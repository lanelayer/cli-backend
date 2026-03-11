//! Tigris S3: upload exports and presigned URLs for squashfs

use s3::creds::Credentials;
use s3::{Bucket, Region};
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
