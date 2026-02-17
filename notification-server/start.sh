#!/bin/bash

echo "Checking disk space..."
df -h

echo "overcommit memory policy..."
echo 1 > /proc/sys/vm/overcommit_memory

echo "Setting up larger volumes..."
mkdir -p /data/docker /data/root

rm -rf /root
ln -s /data/root /root

export HOME=/root
mkdir -p /root

echo "Starting Docker daemon..."
dockerd --host=unix:///var/run/docker.sock --host=tcp://0.0.0.0:2376 --data-root=/data/docker &

echo "Waiting for Docker to be ready..."
timeout=30
while ! docker info >/dev/null 2>&1; do
    if [ $timeout -le 0 ]; then
        echo "Docker daemon failed to start"
        exit 1
    fi
    timeout=$((timeout-1))
    sleep 1
done

echo "Docker is ready"

echo "Setting up docker buildx..."
docker buildx create --use --name builder || echo "Buildx setup failed, continuing..."

if [ -n "$DOCKER_USERNAME" ] && [ -n "$DOCKER_PASSWORD" ] && [ -n "$DOCKER_REGISTRY" ]; then
    echo "Logging into Docker registry: $DOCKER_REGISTRY"
    echo "$DOCKER_PASSWORD" | docker login "$DOCKER_REGISTRY" -u "$DOCKER_USERNAME" --password-stdin
fi

echo "Starting notification server..."
exec notification-server