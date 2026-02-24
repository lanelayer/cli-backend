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
cat > /etc/docker/daemon.json <<EOF
{
  "insecure-registries": ["cli-backend-registry.internal:5000"]
}
EOF

echo "Starting Docker daemon in background..."
dockerd --host=unix:///var/run/docker.sock --host=tcp://0.0.0.0:2376 --data-root=/data/docker &

# Background: wait for Docker, then set up buildx and registry login.
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
  if [ -n "$DOCKER_USERNAME" ] && [ -n "$DOCKER_PASSWORD" ] && [ -n "$DOCKER_REGISTRY" ]; then
    echo "Logging into Docker registry..."
    # Use DOCKER_REGISTRY as-is (e.g. cli-backend-registry.internal:5000) so Docker uses insecure-registries and HTTP
    echo "$DOCKER_PASSWORD" | docker login "$DOCKER_REGISTRY" -u "$DOCKER_USERNAME" --password-stdin 2>&1 || true
  fi
) &

echo "Starting notification server (Docker will be ready in background)..."
export RUST_BACKTRACE=1
export RUST_LOG=info

if [ ! -x /usr/local/bin/notification-server ]; then
  echo "ERROR: /usr/local/bin/notification-server not found or not executable"
  exit 1
fi

exec /usr/local/bin/notification-server
