//! Sprite deployment: create Fly.io Sprite, fetch squashfs from S3, run derived lane service.
//!
//! Triggered after lane export + Tigris upload. Returns the public lane RPC URL.

use sprites::{ServiceRequest, SpritesClient};
use tracing::{info, warn};

use crate::tigris;

/// Result of deploying a Sprite for a lane build.
#[derive(Debug, Clone)]
pub struct SpriteDeployResult {
    pub sprite_name: String,
    pub rpc_url: String,
}

/// Deploy a Sprite for the given digest. Assumes squashfs was already uploaded to
/// s3://lane-exports/{digest}/vc-cm-snapshot-release.squashfs
///
/// Returns Ok with the RPC URL on success. Returns Err if Sprite deploy fails.
/// Sprite deploy is best-effort: build/export can succeed even if this fails.
pub async fn deploy_sprite(
    digest: &str,
) -> Result<SpriteDeployResult, Box<dyn std::error::Error + Send + Sync>> {
    let client = create_sprites_client().await?;
    let sprite_name = sprite_name_from_digest(digest);

    // 1. Presigned URL for squashfs in S3
    let squashfs_url = match tigris::presign_squashfs_get(digest, None) {
        Ok(url) => url,
        Err(e) => {
            warn!("Could not generate presigned squashfs URL: {}", e);
            return Err(e);
        }
    };

    // 2. Create Sprite if needed, then always download squashfs.
    // Derive-node requires /data/vc-cm-snapshot.squashfs as a file; do NOT use
    // try_create_sprite_from_squashfs (that uses squashfs as VM image, wrong layout).
    match client.get(&sprite_name).await {
        Ok(info) => {
            info!(
                "Sprite {} already exists (status: {:?})",
                sprite_name, info.status
            );
        }
        Err(_) => {
            client
                .create(&sprite_name)
                .await
                .map_err(|e| e.to_string())?;
            info!("Created sprite: {}", sprite_name);
        }
    }

    // Download squashfs into /data (required for derive-node)
    if let Err(e) = download_squashfs_into_sprite(&client, &sprite_name, &squashfs_url).await {
        warn!(
            "Could not download squashfs into sprite (derive-node may not have rollup data): {}",
            e
        );
        // Continue anyway – service can still start, user may add squashfs manually
    }

    // 3. Create/update service (derived lane node pattern)
    let http_port = lane_rpc_port();
    let request = lane_service_request(http_port);

    create_service_put(&client, &sprite_name, "lane-node", &request).await?;
    info!("Created service 'lane-node' on sprite {}", sprite_name);

    // 4. Make sprite URL public
    update_url_settings_public(&client, &sprite_name).await?;

    // 5. Get sprite URL for response
    let rpc_url = get_sprite_url(&client, &sprite_name).await?;

    Ok(SpriteDeployResult {
        sprite_name: sprite_name.clone(),
        rpc_url,
    })
}

fn sprite_name_from_digest(digest: &str) -> String {
    // Sprites need alphanumeric + hyphen. Use last 12 chars of digest (after sha256:).
    let short = digest
        .trim_start_matches("sha256:")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>();
    format!("lane-{}", if short.is_empty() { "default" } else { &short })
}

fn lane_rpc_port() -> u16 {
    std::env::var("LANE_RPC_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8545)
}

/// Build Docker Compose for core-lane derive-node mode.
/// Uses derive-node (squashfs snapshot) anchored to lane-espresso RPC.
/// DERIVED_DA_ADDRESS must be set (required by derive-node); CORE_RPC_URL defaults to lane-espresso.
fn build_lane_compose() -> String {
    let chain_id = std::env::var("CHAIN_ID").unwrap_or_else(|_| "1281453634".to_string());
    let core_rpc_url = std::env::var("CORE_RPC_URL")
        .unwrap_or_else(|_| "https://lane-espresso.fly.dev/".to_string());
    let derived_da_address = std::env::var("DERIVED_DA_ADDRESS").unwrap_or_else(|_| String::new());
    if derived_da_address.is_empty() {
        warn!("DERIVED_DA_ADDRESS not set; derive-node will fail at container start (set in Fly secrets)");
    }
    let start_block = std::env::var("START_BLOCK").unwrap_or_else(|_| "0".to_string());

    format!(
        r#"services:
  lane-node:
    image: ghcr.io/lanelayer/core-lane/core-lane:latest
    volumes:
      - /data:/data
    ports:
      - "8545:8545"
    environment:
      CHAIN_ID: "{}"
      CORE_RPC_URL: "{}"
      DATA_DIR: "/data"
      DERIVED_DA_ADDRESS: "{}"
      HTTP_HOST: "0.0.0.0"
      HTTP_PORT: "8545"
      ONLY_START: "derive-node"
      START_BLOCK: "{}"
"#,
        chain_id, core_rpc_url, derived_da_address, start_block
    )
}

/// Service request for derived lane node. Runs core-lane via Docker Compose in derive-node mode.
/// Set LANE_SERVICE_CMD/LANE_SERVICE_ARGS to override with a custom command instead.
fn lane_service_request(http_port: u16) -> ServiceRequest {
    if let (Some(cmd), Some(args_str)) = (
        std::env::var("LANE_SERVICE_CMD").ok(),
        std::env::var("LANE_SERVICE_ARGS").ok(),
    ) {
        let args: Vec<String> = args_str.split_whitespace().map(String::from).collect();
        return ServiceRequest {
            cmd,
            args,
            needs: vec![],
            http_port: Some(http_port),
        };
    }

    // Docker Compose pattern: install Docker, write compose, run core-lane (lane-espresso style)
    let service_script = format!(
        r#"set -e
mkdir -p /srv
cat > /srv/docker-compose.yml << 'COMPOSE_EOF'
{compose}
COMPOSE_EOF
if ! command -v docker >/dev/null 2>&1; then
  sudo apt-get update -qq && sudo apt-get install -y -qq ca-certificates curl
  sudo install -m 0755 -d /etc/apt/keyrings
  sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
  sudo chmod a+r /etc/apt/keyrings/docker.asc
  SUITE="$(. /etc/os-release 2>/dev/null && echo "${{UBUNTU_CODENAME:-${{VERSION_CODENAME:-jammy}}}}")"
  for TRY_SUITE in "$SUITE" noble jammy; do
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu ${{TRY_SUITE}} stable" | sudo tee /etc/apt/sources.list.d/docker.list >/dev/null
    if sudo apt-get update -qq 2>/dev/null && sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin 2>/dev/null; then
      break
    fi
  done
fi
if [ ! -S /var/run/docker.sock ]; then
  sudo dockerd &
  until [ -S /var/run/docker.sock ] 2>/dev/null; do sleep 1; done
fi
exec sudo docker compose -f /srv/docker-compose.yml up
"#,
        compose = build_lane_compose()
    );

    ServiceRequest {
        cmd: "sh".to_string(),
        args: vec!["-c".into(), service_script],
        needs: vec![],
        http_port: Some(http_port),
    }
}

async fn create_sprites_client() -> Result<SpritesClient, Box<dyn std::error::Error + Send + Sync>>
{
    if let Ok(token) = std::env::var("SPRITES_TOKEN") {
        return Ok(SpritesClient::new(token));
    }

    let fly_token = std::env::var("FLY_API_TOKEN")
        .map_err(|_| "Set SPRITES_TOKEN or FLY_API_TOKEN for sprite deploy")?;
    let org = std::env::var("SPRITES_ORG")
        .or_else(|_| std::env::var("FLY_ORG"))
        .map_err(|_| "Set SPRITES_ORG or FLY_ORG when using FLY_API_TOKEN")?;

    let token = SpritesClient::create_token(&fly_token, &org, None)
        .await
        .map_err(|e| format!("Sprites token exchange failed: {}", e))?;
    Ok(SpritesClient::new(token))
}

/// Download squashfs from presigned URL into sprite at /data/vc-cm-snapshot.squashfs
async fn download_squashfs_into_sprite(
    client: &SpritesClient,
    sprite_name: &str,
    squashfs_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sprite = client.sprite(sprite_name);

    // Ensure /data exists
    let _ = sprite.command("mkdir").args(["-p", "/data"]).output().await;

    // Try curl first, then wget
    let output = sprite
        .command("curl")
        .args(["-fsSL", "-o", "/data/vc-cm-snapshot.squashfs", squashfs_url])
        .output()
        .await;

    if let Ok(out) = output {
        if out.status == 0 {
            info!("Downloaded squashfs into sprite via curl");
            return Ok(());
        }
    }

    let output = sprite
        .command("wget")
        .args(["-q", "-O", "/data/vc-cm-snapshot.squashfs", squashfs_url])
        .output()
        .await?;

    if output.status == 0 {
        info!("Downloaded squashfs into sprite via wget");
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&output.stderr);
        let out = String::from_utf8_lossy(&output.stdout);
        Err(format!("curl/wget failed: stderr={} stdout={}", err, out).into())
    }
}

/// Create or update service via PUT (Sprites API expects PUT)
async fn create_service_put(
    client: &SpritesClient,
    sprite_name: &str,
    service_name: &str,
    request: &ServiceRequest,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base = client.base_url().trim_end_matches('/');
    let url = format!(
        "{}/v1/sprites/{}/services/{}",
        base, sprite_name, service_name
    );

    let response = reqwest::Client::new()
        .put(&url)
        .header("Authorization", format!("Bearer {}", client.token()))
        .json(request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Sprites service API error ({}): {}", status.as_u16(), body).into());
    }
    Ok(())
}

/// Set sprite URL to public so developers can access the lane RPC
async fn update_url_settings_public(
    client: &SpritesClient,
    sprite_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base = client.base_url().trim_end_matches('/');
    let url = format!("{}/v1/sprites/{}", base, sprite_name);

    let body = serde_json::json!({
        "url_settings": { "auth": "public" }
    });

    let response = reqwest::Client::new()
        .put(&url)
        .header("Authorization", format!("Bearer {}", client.token()))
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Sprites URL settings update failed ({}): {}",
            status.as_u16(),
            body
        )
        .into());
    }
    Ok(())
}

/// Get the public URL for the sprite from the API
async fn get_sprite_url(
    client: &SpritesClient,
    sprite_name: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let base = client.base_url().trim_end_matches('/');
    let url = format!("{}/v1/sprites/{}", base, sprite_name);

    let response = reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bearer {}", client.token()))
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("Failed to get sprite info: {}", response.status()).into());
    }

    let json: serde_json::Value = response.json().await?;
    let rpc_url = json
        .get("url")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("https://{}.sprites.app", sprite_name));

    Ok(rpc_url)
}
