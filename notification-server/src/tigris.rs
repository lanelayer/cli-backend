//! Tigris S3: upload exports and presigned URLs for squashfs

use s3::creds::Credentials;
use s3::{Bucket, Region};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{info, warn};
use walkdir::WalkDir;

const BUCKET_NAME: &str = "lane-exports";
const REGION: &str = "ap-northeast-2";
const ENDPOINT: &str = "https://t3.storage.dev";

/// Squashfs filename produced by lane export (primary artifact for derived lane).
/// Override with SQUASHFS_FILENAME env var if your export uses a different name.
pub fn squashfs_filename() -> String {
    std::env::var("SQUASHFS_FILENAME").unwrap_or_else(|_| "vc-cm-snapshot.squashfs".to_string())
}

fn tigris_credentials() -> Result<Credentials, String> {
    let access_key = std::env::var("AWS_ACCESS_KEY_ID")
        .or_else(|_| std::env::var("TIGRIS_ACCESS_KEY_ID"))
        .map_err(|_| "AWS_ACCESS_KEY_ID or TIGRIS_ACCESS_KEY_ID environment variable not set")?;

    let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
        .or_else(|_| std::env::var("TIGRIS_SECRET_ACCESS_KEY"))
        .map_err(|_| {
            "AWS_SECRET_ACCESS_KEY or TIGRIS_SECRET_ACCESS_KEY environment variable not set"
        })?;

    Credentials::new(Some(&access_key), Some(&secret_key), None, None, None)
        .map_err(|e| e.to_string())
}

fn bucket() -> Result<Bucket, String> {
    let credentials = tigris_credentials()?;
    let region = Region::Custom {
        region: REGION.to_string(),
        endpoint: ENDPOINT.to_string(),
    };
    Bucket::new(BUCKET_NAME, region, credentials).map_err(|e| e.to_string())
}

fn sprite_index_bucket_name() -> String {
    std::env::var("SPRITE_INDEX_BUCKET").unwrap_or_else(|_| BUCKET_NAME.to_string())
}

fn sprite_index_prefix() -> String {
    std::env::var("SPRITE_INDEX_PREFIX").unwrap_or_else(|_| "sprites/chains".to_string())
}

fn sprite_chain_id() -> String {
    std::env::var("CHAIN_ID").unwrap_or_else(|_| "1281453634".to_string())
}

fn sprite_index_key(chain_id: &str) -> String {
    format!(
        "{}/{}/active_sprites.json",
        sprite_index_prefix().trim_end_matches('/'),
        chain_id
    )
}

fn sprite_index_bucket() -> Result<Bucket, String> {
    let credentials = tigris_credentials()?;
    let region = Region::Custom {
        region: REGION.to_string(),
        endpoint: ENDPOINT.to_string(),
    };
    Bucket::new(&sprite_index_bucket_name(), region, credentials).map_err(|e| e.to_string())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SpriteIndexRecord {
    pub sprite_name: String,
    pub rpc_url: String,
    pub do_poll_url: String,
    pub status: String,
    pub digest: String,
    pub last_changed_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActiveSpritesIndex {
    pub version: u8,
    pub chain_id: String,
    pub updated_at: String,
    pub sprites: Vec<SpriteIndexRecord>,
}

/// Upload all files from export_dir to s3://lane-exports/{digest}/
pub async fn upload_to_tigris(
    digest: &str,
    export_dir: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bucket = bucket()?;
    let export_path = Path::new(export_dir);
    if !export_path.exists() {
        return Err(format!("Export directory '{}' does not exist", export_dir).into());
    }

    let mut uploaded_count = 0;
    let mut error_count = 0;

    for entry in WalkDir::new(export_path)
        .min_depth(1)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();

        if path.is_file() {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("Invalid filename")?;

            let s3_key = format!("{}/{}", digest, filename);

            info!("Uploading {} to s3://lane-exports/{}", filename, s3_key);

            match upload_file(&bucket, path, &s3_key).await {
                Ok(_) => {
                    info!("Successfully uploaded {}", filename);
                    uploaded_count += 1;
                }
                Err(e) => {
                    warn!("Failed to upload {}: {}", filename, e);
                    error_count += 1;
                }
            }
        }
    }

    info!(
        "Upload complete! Successfully uploaded: {} files to s3://lane-exports/{}",
        uploaded_count, digest
    );
    if error_count > 0 {
        warn!("Failed to upload: {} files", error_count);
    }

    Ok(())
}

async fn upload_file(
    bucket: &Bucket,
    file_path: &Path,
    s3_key: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metadata = tokio::fs::metadata(file_path)
        .await
        .map_err(|e| format!("Failed to get metadata for file: {:?}: {}", file_path, e))?;
    let file_size = metadata.len();

    info!(
        "Uploading file: {} (size: {} bytes)",
        file_path.display(),
        file_size
    );

    if file_size > 5 * 1024 * 1024 {
        info!(
            "Using multipart upload for large file: {}",
            file_path.display()
        );
    }

    let content = tokio::fs::read(file_path)
        .await
        .map_err(|e| format!("Failed to read file: {:?}: {}", file_path, e))?;

    let response = bucket.put_object(s3_key, &content).await.map_err(|e| {
        format!(
            "Failed to upload to s3://{}/{}: {}",
            bucket.name(),
            s3_key,
            e
        )
    })?;

    if response.status_code() == 200 {
        Ok(())
    } else {
        Err(format!("Upload failed with status code: {}", response.status_code()).into())
    }
}

/// Generate a presigned GET URL for the squashfs at s3://lane-exports/{digest}/{filename}.
/// Expires in 1 hour. Caller can pass custom filename or use default (vc-cm-snapshot.squashfs, overridable via SQUASHFS_FILENAME env).
pub fn presign_squashfs_get(
    digest: &str,
    filename: Option<&str>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let bucket = bucket()?;
    let filename = filename.map(String::from).unwrap_or_else(squashfs_filename);
    let s3_key = format!("{}/{}", digest, filename);
    let presigned = bucket.presign_get(&s3_key, 3600, None)?;
    Ok(presigned)
}

/// Upsert a sprite as active in chain-scoped index:
/// s3://{SPRITE_INDEX_BUCKET}/{SPRITE_INDEX_PREFIX}/{CHAIN_ID}/active_sprites.json
pub async fn upsert_active_sprite(
    sprite_name: &str,
    rpc_url: &str,
    digest: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bucket = sprite_index_bucket()?;
    let chain_id = sprite_chain_id();
    let key = sprite_index_key(&chain_id);
    let now = chrono::Utc::now().to_rfc3339();

    let existing = bucket.get_object(&key).await;
    let mut index = match existing {
        Ok(response) if response.status_code() == 200 => {
            match serde_json::from_slice::<ActiveSpritesIndex>(response.bytes()) {
                Ok(parsed) => parsed,
                Err(e) => {
                    warn!(
                        "Failed parsing existing sprite index JSON at {} (reinitializing): {}",
                        key, e
                    );
                    ActiveSpritesIndex {
                        version: 1,
                        chain_id: chain_id.clone(),
                        updated_at: now.clone(),
                        sprites: vec![],
                    }
                }
            }
        }
        Ok(response) if response.status_code() == 404 => ActiveSpritesIndex {
            version: 1,
            chain_id: chain_id.clone(),
            updated_at: now.clone(),
            sprites: vec![],
        },
        Ok(response) => {
            return Err(format!(
                "failed reading sprite index {}, status {}",
                key,
                response.status_code()
            )
            .into());
        }
        Err(e) if e.to_string().contains("HTTP 404") || e.to_string().contains("NoSuchKey") => {
            ActiveSpritesIndex {
                version: 1,
                chain_id: chain_id.clone(),
                updated_at: now.clone(),
                sprites: vec![],
            }
        }
        Err(e) => return Err(e.into()),
    };

    if index.chain_id != chain_id {
        index.chain_id = chain_id.clone();
    }

    if index.version == 0 {
        index.version = 1;
    }

    let do_poll_url = format!("{}/do_poll", rpc_url.trim_end_matches('/'));
    let mut found = false;
    for record in &mut index.sprites {
        if record.sprite_name == sprite_name
            || record.rpc_url == rpc_url
            || record.do_poll_url == do_poll_url
        {
            record.sprite_name = sprite_name.to_string();
            record.rpc_url = rpc_url.to_string();
            record.do_poll_url = do_poll_url.clone();
            record.status = "active".to_string();
            record.digest = digest.to_string();
            record.last_changed_at = now.clone();
            found = true;
            break;
        }
    }

    if !found {
        index.sprites.push(SpriteIndexRecord {
            sprite_name: sprite_name.to_string(),
            rpc_url: rpc_url.to_string(),
            do_poll_url,
            status: "active".to_string(),
            digest: digest.to_string(),
            last_changed_at: now.clone(),
        });
    }
    index.updated_at = now;

    let payload = serde_json::to_vec_pretty(&index)?;
    let response = bucket.put_object(&key, &payload).await?;
    if response.status_code() != 200 {
        return Err(format!(
            "failed writing sprite index {}, status {}",
            key,
            response.status_code()
        )
        .into());
    }
    info!(
        "Updated sprite index s3://{}/{} with active sprite {}",
        bucket.name(),
        key,
        sprite_name
    );
    Ok(())
}
