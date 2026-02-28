#!/bin/bash
set -e

echo "Checking disk space..."
df -h

echo "Setting overcommit memory policy..."
echo 1 > /proc/sys/vm/overcommit_memory

echo "Setting up volumes..."
mkdir -p /data/docker /data/root
rm -rf /root
ln -s /data/root /root
export HOME=/root
mkdir -p /root

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
