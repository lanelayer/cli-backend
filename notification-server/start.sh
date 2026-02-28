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

  # Propagate docker credentials into lane build cache directories.
  # lane-snapshot-builder runs with HOME=<cache-dir> and calls linuxkit, which
  # looks for registry credentials at $HOME/.docker/config.json. Since that
  # path is inside the per-build cache dir (not /root/.docker), we watch for
  # new cache directories and copy the credentials there.
  echo "Starting lane credential propagation daemon..."
  (
    while true; do
      if [ -f /root/.docker/config.json ]; then
        find /root/.cache/lane -mindepth 2 -maxdepth 2 -type d 2>/dev/null | while read -r cache_dir; do
          if [ ! -f "$cache_dir/.docker/config.json" ]; then
            mkdir -p "$cache_dir/.docker"
            cp /root/.docker/config.json "$cache_dir/.docker/config.json"
            echo "Seeded docker credentials -> $cache_dir/.docker/"
          fi
        done
      fi
      sleep 1
    done
  ) &
) &

echo "Starting notification server (Docker will be ready in background)..."
export RUST_BACKTRACE=1
export RUST_LOG=info

if [ ! -x /usr/local/bin/notification-server ]; then
  echo "ERROR: /usr/local/bin/notification-server not found or not executable"
  exit 1
fi

# Replace this process with the server so it becomes the main process.
# Then Fly logs show the server's output and only the server receives signals.
exec /usr/local/bin/notification-server
