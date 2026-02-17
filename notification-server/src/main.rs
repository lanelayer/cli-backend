use axum::{
    extract::Json,
    http::{Request, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use tokio::process::Command as TokioCommand;
use tracing::{error, info, warn};

#[derive(Debug, Deserialize)]
struct LaneNotification {
    #[serde(rename = "type")]
    notification_type: String,
    registry_path: String,
    original_path: String,
    timestamp: DateTime<Utc>,
    success: bool,
    profile: String,
    platforms: Vec<String>,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: String,
    timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct NotificationResponse {
    message: String,
    container: String,
    status: String,
    timestamp: DateTime<Utc>,
}

async fn health_handler() -> impl IntoResponse {
    info!("🏥 Health check requested");
    let response = HealthResponse {
        status: "healthy".to_string(),
        timestamp: Utc::now(),
    };

    (StatusCode::OK, Json(response))
}

#[axum::debug_handler]
async fn notify_handler(Json(notification): Json<LaneNotification>) -> impl IntoResponse {
    let timestamp = Utc::now();

    info!("📢 Lane Notification Received:");
    info!("   Type: {}", notification.notification_type);
    info!("   Registry Path: {}", notification.registry_path);
    info!("   Original Path: {}", notification.original_path);
    info!("   Success: {}", notification.success);
    info!("   Profile: {}", notification.profile);
    info!("   Platforms: {:?}", notification.platforms);
    info!("   Timestamp: {}", notification.timestamp);
    if let Some(ref digest) = notification.digest {
        info!("   Digest: {}", digest);
    }

    if notification.success {
        if let Some(digest) = notification.digest {
            let image_with_digest = format!(
                "{}@{}",
                notification
                    .registry_path
                    .split(':')
                    .next()
                    .unwrap_or(&notification.registry_path),
                digest
            );

            info!("🔧 Building with digest-based image: {}", image_with_digest);

            match run_lane_build(&image_with_digest).await {
                Ok(output) => {
                    info!("✅ Lane build completed successfully");
                    info!("Output: {}", output);

                    match run_lane_export_and_upload(&digest).await {
                        Ok(_) => {
                            info!("✅ Lane export completed successfully");

                            let response = NotificationResponse {
                                message: "✅ Notification processed, Lane build and export completed successfully!".to_string(),
                                container: image_with_digest,
                                status: "Success".to_string(),
                                timestamp,
                            };

                            (StatusCode::OK, Json(response))
                        }
                        Err(e) => {
                            warn!("⚠️ Lane export failed: {}", e);

                            let response = NotificationResponse {
                                message: format!("✅ Lane build succeeded but export failed: {}", e),
                                container: image_with_digest,
                                status: "Partial Success".to_string(),
                                timestamp,
                            };

                            (StatusCode::OK, Json(response))
                        }
                    }
                }
                Err(e) => {
                    error!("❌ Lane build failed: {}", e);

                    let response = NotificationResponse {
                        message: format!("❌ Lane build failed: {}", e),
                        container: image_with_digest,
                        status: "Failed".to_string(),
                        timestamp,
                    };

                    (StatusCode::INTERNAL_SERVER_ERROR, Json(response))
                }
            }
        } else {
            warn!("⚠️ No digest provided in notification");

            let response = NotificationResponse {
                message: "⚠️ No digest provided in notification".to_string(),
                container: notification.registry_path,
                status: "Warning".to_string(),
                timestamp,
            };

            (StatusCode::OK, Json(response))
        }
    } else {
        warn!("⚠️ Notification indicates failure");

        let response = NotificationResponse {
            message: "⚠️ Lane push failed".to_string(),
            container: notification.registry_path,
            status: "Failed".to_string(),
            timestamp,
        };

        (StatusCode::OK, Json(response))
    }
}

async fn run_lane_build(
    image_with_digest: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    info!("🚀 Starting Lane build with image: {}", image_with_digest);

    let mut child = TokioCommand::new("lane")
        .args(&["build", "prod", "--image", image_with_digest])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    let status = child.wait().await?;

    if status.success() {
        info!("✅ Lane build completed successfully");
        Ok("Lane build completed successfully".to_string())
    } else {
        let error_msg = format!("Lane build failed with exit code {}", status);
        error!("❌ {}", error_msg);
        Err(error_msg.into())
    }
}

async fn not_found_handler(uri: Uri) -> impl IntoResponse {
    (StatusCode::NOT_FOUND, format!("Not found: {}", uri))
}

async fn logging_middleware(req: Request<axum::body::Body>, next: Next) -> Response {
    info!("🔍 Incoming request: {} {}", req.method(), req.uri());
    let response = next.run(req).await;
    info!("📤 Response status: {}", response.status());
    response
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    info!("🚀 Starting Rust notification server on port 8000");
    info!("📡 Webhook URL: http://localhost:8000/notify");
    info!("🏥 Health check: http://localhost:8000/health");
    info!("⏹️  Press Ctrl+C to stop the server");
    info!("--------------------------------------------------");

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/notify", post(notify_handler))
        .fallback(not_found_handler)
        .layer(middleware::from_fn(logging_middleware));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    info!("✅ Server listening on http://0.0.0.0:8000");

    axum::serve(listener, app).await.unwrap();
}

async fn run_lane_export_and_upload(
    digest: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("📤 Starting Lane export");

    let mut child = TokioCommand::new("lane")
        .args(&["export", "prod", "lane-export-temp"])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    let status = child.wait().await?;

    if !status.success() {
        return Err(format!("Lane export failed with exit code {}", status).into());
    }

    info!("✅ Lane export completed successfully");
    info!("☁️ Starting upload to Tigris S3");

    upload_to_tigris(digest).await?;

    Ok(())
}

async fn upload_to_tigris(digest: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use s3::creds::Credentials;
    use s3::{Bucket, Region};
    use std::path::Path;
    use walkdir::WalkDir;

    let access_key = std::env::var("AWS_ACCESS_KEY_ID")
        .or_else(|_| std::env::var("TIGRIS_ACCESS_KEY_ID"))
        .map_err(|_| "AWS_ACCESS_KEY_ID or TIGRIS_ACCESS_KEY_ID environment variable not set")?;

    let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
        .or_else(|_| std::env::var("TIGRIS_SECRET_ACCESS_KEY"))
        .map_err(|_| {
            "AWS_SECRET_ACCESS_KEY or TIGRIS_SECRET_ACCESS_KEY environment variable not set"
        })?;

    let credentials = Credentials::new(Some(&access_key), Some(&secret_key), None, None, None)?;

    let region = Region::Custom {
        region: "ap-northeast-2".to_string(),
        endpoint: "https://t3.storage.dev".to_string(),
    };

    let bucket = Bucket::new("lane-exports", region, credentials)?;
    let export_dir = "lane-export-temp";

    let export_path = Path::new(export_dir);
    if !export_path.exists() {
        return Err("Export directory 'lane-export-temp' does not exist".into());
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
    bucket: &s3::Bucket,
    file_path: &std::path::Path,
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
