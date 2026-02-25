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
use tokio::time::{sleep, Duration};
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
                                message: format!(
                                    "✅ Lane build succeeded but export failed: {}",
                                    e
                                ),
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
    wait_for_docker().await?;
    info!("🚀 Starting Lane build with image: {}", image_with_digest);

    let mut child = TokioCommand::new("lane")
        .args(["build", "prod", "--image", image_with_digest])
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

/// Wait for Docker daemon to be ready (e.g. after start.sh started it in background).
/// Times out after 90 seconds.
async fn wait_for_docker() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    const MAX_WAIT: Duration = Duration::from_secs(90);
    const POLL_INTERVAL: Duration = Duration::from_secs(1);
    let deadline = tokio::time::Instant::now() + MAX_WAIT;

    while tokio::time::Instant::now() < deadline {
        let output = TokioCommand::new("docker").arg("info").output().await?;
        if output.status.success() {
            info!("Docker is ready");
            return Ok(());
        }
        sleep(POLL_INTERVAL).await;
    }
    Err("Docker did not become ready within 90 seconds".into())
}

async fn logging_middleware(req: Request<axum::body::Body>, next: Next) -> Response {
    info!("🔍 Incoming request: {} {}", req.method(), req.uri());
    let response = next.run(req).await;
    info!("📤 Response status: {}", response.status());
    response
}

fn main() {
    // CRITICAL: Write to stderr FIRST, before anything else (visible in fly logs)
    use std::io::Write;
    let _ = std::io::stderr().write_all(b"MAIN_STARTED\n");
    let _ = std::io::stderr().flush();
    let _ = std::io::stderr().write_all(b"[INIT] Process starting...\n");
    let _ = std::io::stderr().write_all(format!("[INIT] PID: {}\n", std::process::id()).as_bytes());
    let _ = std::io::stderr().flush();

    // Set panic hook early
    std::panic::set_hook(Box::new(|panic_info| {
        use std::io::Write;
        let _ = std::io::stderr().write_all(b"[PANIC HOOK] ");
        let _ = std::io::stderr().write_all(format!("{:?}", panic_info).as_bytes());
        let _ = std::io::stderr().write_all(b"\n");
        let _ = std::io::stderr().flush();
    }));

    let _ = std::io::stderr().write_all(b"[INIT] Panic hook set\n");
    let _ = std::io::stderr().flush();

    // Run the async main
    let _ = std::io::stderr().write_all(b"[INIT] Starting tokio runtime...\n");
    let _ = std::io::stderr().flush();

    tokio::runtime::Runtime::new()
        .expect("Failed to create tokio runtime")
        .block_on(async_main());
}

async fn async_main() {
    use std::io::Write;
    let _ = std::io::stderr().write_all(b"[ASYNC] Entered async_main\n");
    let _ = std::io::stderr().flush();

    // Retry bind: on Fly/Firecracker the network may not be ready immediately.
    const BIND_RETRY: std::time::Duration = std::time::Duration::from_secs(30);
    const BIND_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
    let deadline = std::time::Instant::now() + BIND_RETRY;
    let listener = loop {
        match tokio::net::TcpListener::bind("0.0.0.0:8000").await {
            Ok(l) => {
                let _ = std::io::stderr().write_all(b"[ASYNC] Bound to 0.0.0.0:8000\n");
                let _ = std::io::stderr().write_all(b"LISTENING_ON_8000\n");
                let _ = std::io::stderr().flush();
                break l;
            }
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    let _ = std::io::stderr()
                        .write_all(format!("[ASYNC] Failed to bind after 30s: {}\n", e).as_bytes());
                    let _ = std::io::stderr().flush();
                    std::process::exit(1);
                }
                let _ = std::io::stderr()
                    .write_all(format!("[ASYNC] Bind failed, retrying: {}\n", e).as_bytes());
                let _ = std::io::stderr().flush();
                tokio::time::sleep(BIND_INTERVAL).await;
            }
        }
    };

    let _ = std::io::stderr().write_all(b"[ASYNC] Initializing tracing and router...\n");
    let _ = std::io::stderr().flush();

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("🚀 Starting Rust notification server on port 8000");
    info!("📡 Webhook URL: http://localhost:8000/notify");
    info!("🏥 Health check: http://localhost:8000/health");

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/notify", post(notify_handler))
        .fallback(not_found_handler)
        .layer(middleware::from_fn(logging_middleware));

    info!("✅ Server listening on http://0.0.0.0:8000");

    const SIGTERM_GRACE_SECS: u64 = 60;
    let shutdown_signal = async {
        use std::io::Write;
        use tokio::signal;
        let start = std::time::Instant::now();
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let wait_for_shutdown = async {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
            let sigterm_loop = async {
                loop {
                    sigterm.recv().await;
                    if start.elapsed() >= Duration::from_secs(SIGTERM_GRACE_SECS) {
                        let _ = std::io::stderr().write_all(b"[SIGNAL] Received SIGTERM (after grace period), exiting\n");
                        let _ = std::io::stderr().flush();
                        break;
                    }
                    let _ = std::io::stderr().write_all(b"[SIGNAL] Ignoring SIGTERM (grace period)\n");
                    let _ = std::io::stderr().flush();
                }
            };
            tokio::select! {
                _ = ctrl_c => {
                    let _ = std::io::stderr().write_all(b"[SIGNAL] Received Ctrl+C\n");
                    let _ = std::io::stderr().flush();
                }
                _ = sigterm_loop => {}
            }
        };

        #[cfg(not(unix))]
        let wait_for_shutdown = async {
            ctrl_c.await;
            let _ = std::io::stderr().write_all(b"[SIGNAL] Received Ctrl+C\n");
            let _ = std::io::stderr().flush();
        };

        wait_for_shutdown.await
    };

    let _ = std::io::stderr().write_all(b"[ASYNC] Starting server with graceful shutdown...\n");
    let _ = std::io::stderr().write_all(b"ACCEPTING_CONNECTIONS\n");
    let _ = std::io::stderr().flush();

    // Handle serve errors gracefully
    let server = axum::serve(listener, app);

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                let error_msg = format!("❌ Server error: {}", e);
                let _ = std::io::stderr().write_all(error_msg.as_bytes());
                let _ = std::io::stderr().write_all(b"\n");
                let _ = std::io::stderr().flush();
                error!("{}", error_msg);
                std::process::exit(1);
            }
        },
        _ = shutdown_signal => {
            let _ = std::io::stderr().write_all(b"[SHUTDOWN] Received shutdown signal, exiting gracefully\n");
            let _ = std::io::stderr().flush();
            info!("🛑 Shutting down gracefully...");
        },
    }
}

async fn run_lane_export_and_upload(
    digest: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("📤 Starting Lane export");

    let mut child = TokioCommand::new("lane")
        .args(["export", "prod", "lane-export-temp"])
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
