#!/bin/bash
set -e

echo "Checking disk space..."
df -h

echo "Setting overcommit memory policy..."
echo 1 > /proc/sys/vm/overcommit_memory

echo "Setting up volumes..."
mkdir -p /data/docker /data/root
# Mount Tigris bucket for Lane cache and build working dir (uses existing AWS_* / TIGRIS_* creds)
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-$TIGRIS_ACCESS_KEY_ID}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-$TIGRIS_ACCESS_KEY_SECRET}"
if [ -n "${AWS_ACCESS_KEY_ID}" ] && [ -n "${AWS_SECRET_ACCESS_KEY}" ]; then
  BUCKET="${TIGRIS_BUCKET_NAME:-lane-exports}"
  echo "Mounting Tigris bucket ${BUCKET} at /data/root..."
  tigrisfs "${BUCKET}" /data/root &
  sleep 3
  if mountpoint -q /data/root 2>/dev/null; then
    echo "TigrisFS mounted successfully"
  else
    echo "WARN: TigrisFS mount failed, using local /data/root (disk may be tight)"
  fi
else
  echo "WARN: No Tigris creds (AWS_ACCESS_KEY_ID or TIGRIS_ACCESS_KEY_ID) - using local /data/root"
fi
rm -rf /root
ln -s /data/root /root
export HOME=/root
mkdir -p /root

# Lane cache on local disk (avoids I/O errors when Cartesi reads snapshot from Tigris FUSE)
LANE_CACHE_DIR="/data/lane-cache"
mkdir -p "$LANE_CACHE_DIR"
export XDG_CACHE_HOME="$LANE_CACHE_DIR"
# Lane export looks for ~/.cache/lane; use a HOME whose .cache points to the same local cache
export LANE_HOME="/data/lane-home"
mkdir -p "$LANE_HOME"
ln -sfn "$LANE_CACHE_DIR" "$LANE_HOME/.cache"
echo "Lane cache directory (local disk): $LANE_CACHE_DIR (export uses LANE_HOME=$LANE_HOME)"

echo "Configuring Docker..."
mkdir -p /etc/docker
# No insecure-registries: we use the public HTTPS URL (cli-backend-registry.fly.dev)
cat > /etc/docker/daemon.json <<EOF
{}
EOF

echo "Starting Docker daemon in background (debug logging)..."
dockerd --debug --host=unix:///var/run/docker.sock --host=tcp://0.0.0.0:2376 --data-root=/data/docker &

# Background: wait for Docker, then set up buildx.
# The notification server starts immediately below; handlers will wait for Docker when needed.
(
  echo "Waiting for Docker to be ready..."
  timeout=90
  while ! docker info >/dev/null 2>&1; do
    if [ $timeout -le 0 ]; then
      echo "Docker daemon failed to start within 90s"
      exit 1
    fi
    timeout=$((timeout - 1))
    sleep 1
  done
  echo "Docker is ready (background)"
  echo "Setting up docker buildx..."
  docker buildx create --use --name builder 2>/dev/null || true
  echo "Pre-pulling Lane build images..."
  docker pull ghcr.io/lanelayer/lane-snapshot-builder@sha256:5de1cfaea1a33c8cdcee1abd3306ae9a25709a2522fa33c95822a4fc209b7a18 2>/dev/null || true
  docker pull tonistiigi/binfmt:latest 2>/dev/null || true
  echo "Logging into registry..."
  echo "$REGISTRY_PASSWORD" | docker login cli-backend-registry.fly.dev -u lane-container --password-stdin || true
) &

echo "Starting notification server (Docker will be ready in background)..."
export RUST_BACKTRACE=1
export RUST_LOG=info

if [ ! -x /usr/local/bin/notification-server ]; then
  echo "ERROR: /usr/local/bin/notification-server not found or not executable"
  exit 1
fi

echo "=== Binary diagnostics ==="
file /usr/local/bin/notification-server
ls -lah /usr/local/bin/notification-server
echo "=========================="

# Replace this process with the server so it becomes the main process.
# Then Fly logs show the server's output and only the server receives signals.
exec /usr/local/bin/notification-server
