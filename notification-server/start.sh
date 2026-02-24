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

# Configure Docker to allow insecure registries (for internal HTTP registry)
# Note: We'll configure this via daemon.json, but dockerd needs to be restarted to pick it up
# For now, we'll handle the login error gracefully and continue
mkdir -p /etc/docker
cat > /etc/docker/daemon.json <<EOF
{
  "insecure-registries": ["cli-backend-registry.internal:5000"]
}
EOF

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
    # Try to login, but don't fail if it doesn't work (registry might be HTTP)
    if echo "$DOCKER_PASSWORD" | docker login "$DOCKER_REGISTRY" -u "$DOCKER_USERNAME" --password-stdin 2>&1; then
        echo "Successfully logged into Docker registry"
    else
        echo "WARNING: Failed to log into Docker registry (this may be expected for HTTP registries)"
        echo "Docker operations may still work if the registry allows anonymous access"
    fi
fi

echo "Starting notification server..."

# Verify binary exists and is executable
if [ ! -f /usr/local/bin/notification-server ]; then
    echo "ERROR: Binary not found at /usr/local/bin/notification-server"
    exit 1
fi

if [ ! -x /usr/local/bin/notification-server ]; then
    echo "ERROR: Binary is not executable"
    exit 1
fi

echo "Binary found and executable"

echo "Checking for missing libraries..."
ldd /usr/local/bin/notification-server 2>&1 || echo "ldd failed"

echo "Verifying binary can execute..."
/usr/local/bin/notification-server --version 2>&1 || echo "Version check failed (expected if no --version flag)"

echo "Checking if port 8000 is available..."
if command -v netstat >/dev/null 2>&1; then
    netstat -tuln | grep :8000 || echo "Port 8000 appears to be free"
fi

echo "Running notification server (stdout/stderr will be shown)..."
echo "=========================================="

# Force unbuffered output and enable backtraces
export RUST_BACKTRACE=1
export RUST_LOG=info
export PYTHONUNBUFFERED=1  # In case any Python scripts are involved

# Test if we can actually run the binary and see output
echo "Testing binary with timeout (5 seconds)..."
timeout 5 /usr/local/bin/notification-server 2>&1 || {
    TEST_EXIT=$?
    echo "Binary test exited with code: $TEST_EXIT"
    if [ $TEST_EXIT -eq 124 ]; then
        echo "✅ Binary is running (timeout reached - this is good!)"
    else
        echo "❌ Binary exited immediately with code: $TEST_EXIT"
        echo "This suggests the binary is not working correctly"
        echo "Attempting to run with strace to see what's happening..."
        timeout 3 strace -e trace=all /usr/local/bin/notification-server 2>&1 | head -100 || true
        echo "--- End of strace output ---"
        echo "Checking if binary is actually executable..."
        ls -la /usr/local/bin/notification-server
        echo "Checking file type..."
        file /usr/local/bin/notification-server
        echo "Checking for missing dynamic libraries..."
        ldd /usr/local/bin/notification-server 2>&1 || echo "ldd failed or static binary"
    fi
}

echo "=========================================="
echo "Starting notification server for real..."
echo "PID of this shell: $$"
echo "About to start notification-server..."

# Final verification - try to run the binary with a timeout to see if it starts
echo "Final verification: Testing if binary can start (2 second test)..."
timeout 2 /usr/local/bin/notification-server > /tmp/notification-test.log 2>&1 &
TEST_PID=$!
sleep 2
if kill -0 $TEST_PID 2>/dev/null; then
    echo "✅ Binary started successfully (killing test process)"
    kill $TEST_PID 2>/dev/null || true
    wait $TEST_PID 2>/dev/null || true
else
    TEST_EXIT=$?
    echo "⚠️ Binary test exited with code: $TEST_EXIT"
    echo "Test output:"
    cat /tmp/notification-test.log || echo "No test output captured"
    echo "--- End of test output ---"
fi

echo "Current working directory: $(pwd)"
echo "PATH: $PATH"
echo "LD_LIBRARY_PATH: ${LD_LIBRARY_PATH:-not set}"

# Check if stdbuf is available, use it if possible
if command -v stdbuf >/dev/null 2>&1; then
    echo "Using stdbuf for unbuffered output"
    echo "About to exec notification-server (this will replace this shell process)"
    echo "If you see this message after exec, something went wrong"
    # Don't redirect stderr to stdout here - let stdbuf handle it separately
    # This ensures our debug output goes to stderr properly
    exec stdbuf -oL -eL /usr/local/bin/notification-server
else
    echo "stdbuf not available, using direct execution"
    echo "About to exec notification-server (this will replace this shell process)"
    echo "If you see this message after exec, something went wrong"
    # Use exec to replace shell process (required by Fly.io)
    # Don't redirect - let stdout and stderr go to their natural destinations
    exec /usr/local/bin/notification-server
fi