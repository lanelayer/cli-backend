# CLI Backend - Server-Side Docker → Squashfs/Cartesi Pipeline

This repository contains the server-side infrastructure for converting Docker containers into squashfs images and Cartesi machine snapshots using the Lane CLI.

## Architecture

The system consists of two main services deployed on Fly.io:

1. **Docker Registry** (`docker-registry/`) - A Docker registry that receives pushed images
2. **Notification Server** (`notification-server/`) - A Rust service that:
   - Receives webhooks when images are pushed
   - Runs `lane build` to convert Docker → squashfs/Cartesi
   - Runs `lane export` to extract the artifacts
   - Uploads exports to Tigris S3 (`s3://lane-exports/{digest}/`)
   - Deploys a Fly.io Sprite with the squashfs and returns the public lane RPC URL (when `SPRITES_TOKEN` is configured)

## Prerequisites

- Fly.io account and CLI (`flyctl`)
- GitHub Actions with `FLY_API_TOKEN` secret
- AWS/Tigris S3 credentials for storing exports
- Lane CLI (`@lanelayer/cli`) installed in the notification server container

## Initial Setup

### 1. Create Fly.io Apps

First, create the two Fly.io apps:

```bash
# Create Docker Registry app
cd docker-registry
flyctl apps create cli-backend-registry

# Create Notification Server app
cd ../notification-server
flyctl apps create cli-backend-notification-server
```

### 2. Set Fly.io Secrets

Set secrets for both apps using `flyctl secrets set`:

**Docker Registry secrets:**
```bash
cd docker-registry
flyctl secrets set REGISTRY_HTTP_SECRET="$(openssl rand -base64 32)"
```

**Notification Server secrets:**
```bash
cd notification-server
flyctl secrets set \
  AWS_ACCESS_KEY_ID="your-aws-key" \
  AWS_SECRET_ACCESS_KEY="your-aws-secret" \
  TIGRIS_ACCESS_KEY_ID="your-tigris-key" \
  TIGRIS_SECRET_ACCESS_KEY="your-tigris-secret"
```
(The registry is public; no registry credentials are needed.)

**Sprite deployment (optional):** After lane export uploads to S3, the server can deploy a Fly.io Sprite for the derived lane. Set either:
- `SPRITES_TOKEN` – Sprites API token (from sprites.dev/account), or
- `FLY_API_TOKEN` + `SPRITES_ORG` (or `FLY_ORG`) – for token exchange

Also set `DERIVED_DA_ADDRESS` (required for derive-node mode; the Sprite will fail to start without it). The Sprite uses derive-node mode anchored to `https://lane-espresso.fly.dev/` by default (`CORE_RPC_URL`).

If not set, Sprite deploy is skipped and `lane_rpc_url` is omitted from the response.

### 3. Create Persistent Volume (for Docker Registry)

```bash
flyctl volumes create registry_data --size 10 --app cli-backend-registry
```

### 4. Configure GitHub Secrets

Add these secrets to your GitHub repository:

- `FLY_API_TOKEN` - Your Fly.io API token (get with `flyctl auth token`)
- `GHCR_TOKEN` - GitHub Container Registry token (for pushing images). If not set, the workflow uses `GITHUB_TOKEN`.
- `REGISTRY_HTPASSWD` - Registry auth: store a **generated htpasswd line** as this secret (see below).

#### Registry auth: store a generated htpasswd as a GitHub secret

The Docker registry image is built with auth from the `REGISTRY_HTPASSWD` secret. Use a single generated htpasswd line so credentials stay out of the repo and CI uses them safely.

1. **Generate one htpasswd line** (choose a username and strong password):
   ```bash
   htpasswd -Bbn YOUR_REGISTRY_USER 'YOUR_REGISTRY_PASSWORD'
   ```
   Copy the single line it prints (e.g. `lane-container:$2y$05$...`).

2. **Add it as a GitHub secret**:  
   Repo → **Settings** → **Secrets and variables** → **Actions** → **New repository secret**  
   - Name: `REGISTRY_HTPASSWD`  
   - Value: paste the htpasswd line from step 1.

The workflow requires `REGISTRY_HTPASSWD` to be set to build the registry image; it does not generate htpasswd in CI.

## CI/CD Deployment

The GitHub Actions workflow automatically:

1. **Builds** both Docker images and pushes to GHCR
2. **Deploys** Docker Registry to Fly.io
3. **Deploys** Notification Server to Fly.io

### Manual Deployment

You can also deploy manually:

```bash
# Deploy Docker Registry
cd docker-registry
flyctl deploy

# Deploy Notification Server
cd ../notification-server
flyctl deploy
```

## How It Works

### Public registry URL

The Docker registry is available at **`https://cli-backend-registry.fly.dev`**. Use this host in image names when pushing and in the webhook payload.

- **Push**: Requires auth. Run `docker login cli-backend-registry.fly.dev` with the htpasswd user/password (same credentials as in `REGISTRY_HTPASSWD`), then tag and push e.g. `cli-backend-registry.fly.dev/my-repo/myimage:tag`.
- **Pull**: No auth. The notification server (and anyone else) pulls from this URL without credentials.

The notification server does not hardcode the registry; it uses the `registry_path` from the webhook. For production, the client (e.g. Lane CLI) must push to this registry and send that same image path in `registry_path` so the server can pull the image.

### User Workflow

1. User runs `lane build prod` locally → creates deterministic Docker image
2. User runs `lane push` (with registry set to `cli-backend-registry.fly.dev`) → pushes to Docker registry + sends webhook to notification server
3. Notification server receives webhook at `POST /notify`
4. Server runs `lane build prod --image <image-with-digest>` → converts to squashfs/Cartesi (pulls from `registry_path`)
5. Server runs `lane export prod lane-export-temp` → extracts artifacts
6. Server uploads all files from `lane-export-temp/` to `s3://lane-exports/{digest}/`

### Output Location

All squashfs and Cartesi machine snapshots are uploaded to:
```
s3://lane-exports/{digest}/{filename}
```

Where `{digest}` is the Docker image digest from the push notification.

## API Endpoints

### Notification Server

- `GET /health` - Health check endpoint
- `POST /notify` - Webhook endpoint for Lane CLI push notifications

Expected payload (use the public registry host in `registry_path` for production):
```json
{
  "type": "push",
  "registry_path": "cli-backend-registry.fly.dev/my-repo/image:tag",
  "original_path": "image:tag",
  "timestamp": "2024-01-01T00:00:00Z",
  "success": true,
  "profile": "prod",
  "platforms": ["linux/riscv64"],
  "digest": "sha256:...",
  "session_id": "optional-session-id"
}
```

Response includes `lane_rpc_url` when Sprite deployment succeeds (optional, requires `SPRITES_TOKEN`).

### Optional email notifications (Resend)

The notification server can send lifecycle emails for lane push processing:
- Processing started (before `lane build`)
- Processing succeeded (after build + export path succeeds)

Set these environment variables on the notification server app:
- `RESEND_API_KEY`
- `RESEND_FROM_EMAIL` (example: `Lane Bot <noreply@yourdomain.com>`)
- `RESEND_TO_EMAILS` (comma-separated recipients)
- `LANELAYER_ANALYTICS_BASE_URL` (optional; if set and `session_id` is provided in webhook payload, notification-server fetches recipient email from analytics)
- `LANELAYER_ANALYTICS_AUTH_TOKEN` (optional; when set, notification-server queries `/api/v1/auth/email/{session_id}` which requires auth; when missing, it falls back to the public `/api/v1/auth/status`)
- `LANELAYER_ANALYTICS_EMAIL_PATH` (optional; defaults to `/api/v1/auth/email/{session_id}`; `{session_id}` is replaced in the string)
- `LANELAYER_ANALYTICS_STATUS_PATH` (optional; defaults to `/api/v1/auth/status`)
- `LANELAYER_ANALYTICS_SESSION_QUERY_PARAM` (optional; defaults to `session`; used for `/api/v1/auth/status?session=<session_id>`)

## Development

### Building Locally

```bash
# Build notification server
cd notification-server
cargo build --release

# Build Docker images
docker build -t docker-registry:local ./docker-registry
docker build -t notification-server:local ./notification-server
```

### Run CI checks locally

Before pushing, run the same Rust checks as CI to avoid failures:

```bash
./scripts/ci.sh
```

This runs `cargo check`, `cargo clippy`, and `cargo fmt --check` in `notification-server/`.

### Testing Locally

You can test the notification endpoint locally:

```bash
# Start notification server locally (requires Docker daemon)
cd notification-server
cargo run

# In another terminal, test the notification endpoint
curl -X POST http://localhost:8000/notify \
  -H "Content-Type: application/json" \
  -d '{
    "type": "push",
    "registry_path": "cli-backend-registry.fly.dev/my-repo/test:latest",
    "original_path": "test:latest",
    "timestamp": "2024-01-01T00:00:00Z",
    "success": true,
    "profile": "prod",
    "platforms": ["linux/riscv64"],
    "digest": "sha256:test123"
  }'
```

## Troubleshooting

### Docker build: "unexpected commit digest" / "failed precondition"

If you see an error like:
```
failed to compute cache key: failed commit on ref "layer-sha256:...": unexpected commit digest ... expected sha256:...: failed precondition
```
this is usually **BuildKit cache corruption**. Fix it by clearing the build cache and rebuilding without cache:

```bash
docker builder prune -af
cd notification-server
docker build --no-cache -t notification-server:local .
```

For `fly deploy`, if the same error appears in CI or when building remotely, the deploy may need to run without cache once (e.g. push an empty commit or use your provider’s “clear cache” option if available).

### Fly.io Deployment Issues

- Ensure `FLY_API_TOKEN` is set correctly in GitHub secrets
- Check app names match in `fly.toml` files
- Verify secrets are set: `flyctl secrets list`

### Lane build "can't fetch the container" / registry login

The lane build runs `lane build prod --image <image>` and **pulls** that image from your registry. The notification server logs into the registry in **start.sh** (background) using `REGISTRY_PASSWORD`. If that login fails or hasn’t finished when a build runs, the lane build can fail with a fetch/pull error.

**How to verify and test:**

1. **Check startup logs**  
   After deploy, in Fly logs you should see either:
   - `REGISTRY_LOGIN_SUCCEEDED` (from the background setup), or  
   - `REGISTRY_LOGIN_FAILED (lane build may fail to pull container)`  
   So you can show in logs whether login succeeded at startup.

2. **Check right before each build**  
   When a notification is processed, the server logs one of:
   - `Registry login ready (lane build can pull from registry)` — login completed before the build.
   - `Registry login not confirmed within 60s (lane build may fail to fetch container)` — build started before login or login failed; the following lane build may fail to pull.

3. **Reproduce "can't fetch container"**  
   - Set a wrong or empty `REGISTRY_PASSWORD` in Fly secrets and redeploy. Trigger a push so the notification server runs a lane build. You should see `REGISTRY_LOGIN_FAILED` in logs and the lane build failing to pull the image.  
   - Or trigger a notification very soon after deploy (race): if the build runs before the background login finishes, you may see "Registry login not confirmed within 60s" and then a pull failure.

4. **Ensure login completes before builds**  
   The server now waits up to 60 seconds for the background login to complete (sentinel file `/tmp/registry-login-done`) before starting the lane build, so the race window is reduced. If you still see pull failures, confirm `REGISTRY_PASSWORD` is set correctly in Fly secrets for the notification server app.

5. **Test pull from inside the app (fly ssh console)**  
   To confirm whether the Fly machine can pull the image at all (same environment as the lane build), use **`fly ssh console`** so you land on the **existing** app machine where Docker is running.

   **Important:** Use **`fly ssh console`**, not `fly console`. `fly console` creates a **new ephemeral machine** that does **not** run `start.sh`, so Docker is never started there and you will see *"Cannot connect to the Docker daemon"*. `fly ssh console` connects to the already-running app machine where `start.sh` has started `dockerd` in the background.

   ```bash
   # 1. Open a shell on the RUNNING app machine (where start.sh already started Docker)
   fly ssh console -a cli-backend-notification-server

   # 2. Confirm you're on the app: you should see dockerd and notification-server in process list (procps is installed in the app image)
   ps aux | grep -E 'dockerd|notification-server'

   # 3. (Optional) If you need to log in first (same creds as REGISTRY_PASSWORD)
   # echo "$REGISTRY_PASSWORD" | docker login cli-backend-registry.fly.dev -u lane-container --password-stdin

   # 4. Pull the exact image that failed (replace with your image@sha256:...)
   docker pull cli-backend-registry.fly.dev/sample-python@sha256:f087d6a313238eded9bf156cf138958ba9bfa51d9e2e3542b45972edc0cb8677
   ```

   The ephemeral machine from `fly console` is minimal (no `ps`, no Docker); you can't usefully install tools there. Use `fly ssh console` to get the real app machine. The app image includes `procps` so `ps` works there. If you still see "Cannot connect to the Docker daemon" after using `fly ssh console`, check Fly logs for "Docker is ready" and "Starting Docker daemon"; if `dockerd` failed to start on the app machine, the logs may show why. If the pull works in the console, the problem may be timing (build ran before login) or how lane invokes Docker.

### Buildx OCI Export Error

The notification server needs a buildx builder with `docker-container` driver. The `start.sh` script creates one, but if issues persist, you may need to configure it manually in the Fly.io app.

### S3 Upload Fails

Check that:
- AWS/Tigris credentials are set correctly in Fly.io secrets
- S3 bucket `lane-exports` exists
- Network connectivity to S3 endpoint (`https://t3.storage.dev`)

## License

[Your License Here]
