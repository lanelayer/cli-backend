#!/bin/bash
set -e

# Deployment script for CLI Web Runtime services
# Usage: ./deploy.sh [environment]

ENVIRONMENT=${1:-production}
COMPOSE_FILE="docker-compose.yml"

echo "🚀 Starting deployment for environment: $ENVIRONMENT"

# Check if .env file exists
if [ ! -f .env ]; then
    echo "⚠️  .env file not found. Copying from .env.example..."
    if [ -f .env.example ]; then
        cp .env.example .env
        echo "📝 Please edit .env file with your configuration before continuing"
        exit 1
    else
        echo "❌ .env.example not found. Please create .env file manually"
        exit 1
    fi
fi

# Load environment variables
set -a
source .env
set +a

echo "📦 Pulling latest images..."
docker-compose -f $COMPOSE_FILE pull || echo "⚠️  Some images may not exist yet, continuing with build..."

echo "🔨 Building images..."
docker-compose -f $COMPOSE_FILE build

echo "🛑 Stopping existing containers..."
docker-compose -f $COMPOSE_FILE down

echo "🚀 Starting services..."
docker-compose -f $COMPOSE_FILE up -d

echo "⏳ Waiting for services to be healthy..."
sleep 10

echo "📊 Service status:"
docker-compose -f $COMPOSE_FILE ps

echo "📋 Recent logs:"
docker-compose -f $COMPOSE_FILE logs --tail=20

echo ""
echo "✅ Deployment complete!"
echo ""
echo "Services:"
echo "  - Docker Registry: http://localhost:${REGISTRY_PORT:-5001}"
echo "  - Notification Server: http://localhost:${NOTIFICATION_PORT:-8000}"
echo ""
echo "Health checks:"
echo "  - Registry: curl http://localhost:${REGISTRY_PORT:-5001}/v2/"
echo "  - Notification: curl http://localhost:${NOTIFICATION_PORT:-8000}/health"

