mod email;
mod sprite;
mod tigris;

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

use std::sync::OnceLock;

use tokio::process::Command as TokioCommand;
use tokio::sync::Semaphore;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

#[derive(Debug, Deserialize)]
struct LaneNotification {
    #[serde(rename = "type")]
    notification_type: String,
    /// Image reference where lane CLI originally pushed the image (e.g. ttl.sh/...)
    original_path: String,
    /// Image reference in our own registry where we expect to build from
    registry_path: String,
    timestamp: DateTime<Utc>,
    success: bool,
    profile: String,
    platforms: Vec<String>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    lane_rpc_url: Option<String>,
}

fn build_job_semaphore() -> &'static Semaphore {
    // Lane build/export uses global resources (`/root/lane-export-temp`, Docker prune, etc).
    // Serializing background jobs avoids concurrency-induced interference.
    static CELL: OnceLock<Semaphore> = OnceLock::new();
    CELL.get_or_init(|| Semaphore::new(1))
}

async fn docker_pull_image(
    image_with_digest: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    wait_for_docker().await?;

    info!("🐳 Pulling image: {}", image_with_digest);
    let pull_status = TokioCommand::new("docker")
        .args(["pull", image_with_digest])
        .output()
        .await?;

    if !pull_status.status.success() {
        let stderr = String::from_utf8_lossy(&pull_status.stderr);
        return Err(format!("docker pull {} failed: {}", image_with_digest, stderr).into());
    }

    Ok(())
}

async fn tag_and_push_to_registry(
    source_image_with_digest: &str,
    target_image_tag: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    wait_for_docker().await?;
    wait_for_registry_login().await;

    info!(
        "🏷️ Tagging and pushing: {} -> {}",
        source_image_with_digest, target_image_tag
    );

    let tag_status = TokioCommand::new("docker")
        .args(["tag", source_image_with_digest, target_image_tag])
        .output()
        .await?;
    if !tag_status.status.success() {
        let stderr = String::from_utf8_lossy(&tag_status.stderr);
        return Err(format!(
            "docker tag {} {} failed: {}",
            source_image_with_digest, target_image_tag, stderr
        )
        .into());
    }

    let push_status = TokioCommand::new("docker")
        .args(["push", target_image_tag])
        .output()
        .await?;
    if !push_status.status.success() {
        let stderr = String::from_utf8_lossy(&push_status.stderr);
        return Err(format!("docker push {} failed: {}", target_image_tag, stderr).into());
    }

    Ok(())
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
    info!("   Original Path: {}", notification.original_path);
    info!("   Registry Path: {}", notification.registry_path);
    info!("   Success: {}", notification.success);
    info!("   Profile: {}", notification.profile);
    info!("   Platforms: {:?}", notification.platforms);
    info!("   Timestamp: {}", notification.timestamp);
    if let Some(ref digest) = notification.digest {
        info!("   Digest: {}", digest);
    }
    if let Some(ref session_id) = notification.session_id {
        info!("   Session ID: {}", session_id);
    }

    if !notification.success {
        warn!("⚠️ Notification indicates failure");

        let response = NotificationResponse {
            message: "⚠️ Lane push failed".to_string(),
            container: notification.registry_path,
            status: "Failed".to_string(),
            timestamp,
            lane_rpc_url: None,
        };

        return (StatusCode::OK, Json(response));
    }

    let digest = match notification.digest.as_deref() {
        Some(d) => d,
        None => {
            warn!("⚠️ No digest provided in notification");
            let response = NotificationResponse {
                message: "⚠️ No digest provided in notification".to_string(),
                container: notification.registry_path,
                status: "Warning".to_string(),
                timestamp,
                lane_rpc_url: None,
            };
            return (StatusCode::OK, Json(response));
        }
    };

    // Require a sha256 digest so we can derive a stable short ID for the registry image name.
    if !digest.starts_with("sha256:") {
        warn!("No valid sha256 digest in notification (got {})", digest);
        let response = NotificationResponse {
            message: "⚠️ Invalid digest format in notification (expected sha256:...)".to_string(),
            container: notification.original_path,
            status: "Warning".to_string(),
            timestamp,
            lane_rpc_url: None,
        };
        return (StatusCode::OK, Json(response));
    }

    // 1) Get the first 8 characters after "sha256:" (used to name our registry image).
    let short = &digest["sha256:".len()..];
    let short8: String = short.chars().take(8).collect();

    // 2) Source image (where lane CLI pushed the image, e.g. ttl.sh/...)
    let source_image_with_digest = format!(
        "{}@{}",
        notification
            .original_path
            .split(':')
            .next()
            .unwrap_or(&notification.original_path),
        digest
    );

    // 3) Target image in our own registry, derived from digest: cli-backend-registry.fly.dev/lane-<short8>:latest
    let registry_base = std::env::var("LANE_REGISTRY_BASE")
        .unwrap_or_else(|_| "cli-backend-registry.fly.dev".to_string());
    let target_image = format!("{}/lane-{}:latest", registry_base, short8);

    // 4) Resolve recipients BEFORE we do any heavy Docker work.
    let recipients = match email::resolve_recipients(notification.session_id.as_deref()).await {
        Ok(list) => list,
        Err(e) => {
            warn!(
                "⚠️ Failed to resolve recipients from analytics/fallback list: {}",
                e
            );
            Vec::new()
        }
    };

    // 5) Gate the background processing on a successful docker pull.
    // This is the "docker pull the image in the json of the notification" step from feedback.
    if let Err(e) = docker_pull_image(&source_image_with_digest).await {
        error!(
            "❌ Failed to pull source image {}, will not start background deployment: {}",
            source_image_with_digest, e
        );

        let response = NotificationResponse {
            message: format!("❌ Failed to pull image: {}", e),
            container: target_image,
            status: "Failed".to_string(),
            timestamp,
            lane_rpc_url: None,
        };

        return (StatusCode::INTERNAL_SERVER_ERROR, Json(response));
    }

    // Move data into background job and return immediately.
    let recipients_bg = recipients.clone();
    let original_path = notification.original_path;
    let registry_path = notification.registry_path;
    let profile = notification.profile;
    let platforms = notification.platforms;
    let digest_owned = digest.to_string();
    let target_image_bg = target_image.clone();
    let source_image_bg = source_image_with_digest.clone();

    tokio::spawn(async move {
        // Serialize because lane build/export manipulates global docker/export state.
        let _permit = build_job_semaphore().acquire().await;

        // Background step 1: tell user we're starting deployment.
        if let Err(e) = email::send_lane_push_started_email(
            &recipients_bg,
            &original_path,
            &registry_path,
            Some(&digest_owned),
            &profile,
            &platforms,
        )
        .await
        {
            warn!("⚠️ Failed to send lane push started email: {}", e);
        }

        // Background step 2: mirror/tag the pulled image into our stable registry.
        if let Err(e) = tag_and_push_to_registry(&source_image_bg, &target_image_bg).await {
            error!(
                "❌ Failed to tag/push image for lane build: {} (image {})",
                e, target_image_bg
            );
            return;
        }

        // Background step 3: lane build + export + sprite deployment.
        if let Err(e) = run_lane_build(&target_image_bg).await {
            error!("❌ Lane build failed in background job: {}", e);
            return;
        }

        if let Err(e) = run_lane_export_and_upload(&digest_owned, &target_image_bg).await {
            warn!("⚠️ Lane export failed in background job: {}", e);
            return;
        }

        let lane_rpc_url = match sprite::deploy_sprite(&digest_owned).await {
            Ok(result) => {
                info!(
                    "✅ Sprite deployed: {} at {}",
                    result.sprite_name, result.rpc_url
                );
                if let Err(e) = tigris::upsert_active_sprite(
                    &result.sprite_name,
                    &result.rpc_url,
                    &digest_owned,
                )
                .await
                {
                    warn!("⚠️ Failed to upsert sprite active index in Tigris: {}", e);
                }
                Some(result.rpc_url)
            }
            Err(e) => {
                warn!("⚠️ Sprite deploy failed (build/export succeeded): {}", e);
                None
            }
        };

        // Background step 4: email only when RPC is actually available.
        if let Some(ref rpc_url) = lane_rpc_url {
            if let Err(e) = email::send_lane_push_success_email(
                &recipients_bg,
                &target_image_bg,
                &digest_owned,
                Some(rpc_url),
            )
            .await
            {
                warn!("⚠️ Failed to send lane push success email: {}", e);
            }
        } else {
            warn!("Lane build/export succeeded but RPC deploy failed; skipping RPC email");
        }
    });

    let response = NotificationResponse {
        message: "✅ Notification received; lane deployment queued".to_string(),
        container: target_image,
        status: "Queued".to_string(),
        timestamp,
        lane_rpc_url: None,
    };

    (StatusCode::OK, Json(response))
}

async fn log_disk_space(label: &str) {
    match TokioCommand::new("df").args(["-h"]).output().await {
        Ok(out) => info!(
            "💾 Disk space ({}): {}",
            label,
            String::from_utf8_lossy(&out.stdout).trim()
        ),
        Err(e) => warn!("⚠️ Could not check disk space: {}", e),
    }
}

async fn cleanup_docker() {
    info!("🧹 Pruning stopped containers...");
    match TokioCommand::new("docker")
        .args(["container", "prune", "-f"])
        .output()
        .await
    {
        Ok(out) => info!(
            "Container prune: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        ),
        Err(e) => warn!("Failed to prune containers: {}", e),
    }

    info!("🧹 Pruning dangling images...");
    match TokioCommand::new("docker")
        .args(["image", "prune", "-f"])
        .output()
        .await
    {
        Ok(out) => info!(
            "Image prune: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        ),
        Err(e) => warn!("Failed to prune images: {}", e),
    }

    info!("🧹 Pruning build cache...");
    match TokioCommand::new("docker")
        .args(["builder", "prune", "-f"])
        .output()
        .await
    {
        Ok(out) => info!(
            "Builder prune: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        ),
        Err(e) => warn!("Failed to prune build cache: {}", e),
    }
}

async fn log_disk_space_detail(label: &str) {
    for path in &["/", "/data", "/tmp"] {
        match TokioCommand::new("df").args(["-h", path]).output().await {
            Ok(out) => info!(
                "💾 df {} ({}): {}",
                path,
                label,
                String::from_utf8_lossy(&out.stdout).trim()
            ),
            Err(e) => warn!("⚠️ df {} failed: {}", path, e),
        }
    }
}

/// Wait for registry login (start.sh writes /tmp/registry-login-done).
async fn wait_for_registry_login() {
    const MAX_WAIT: Duration = Duration::from_secs(60);
    const POLL: Duration = Duration::from_secs(1);
    let deadline = tokio::time::Instant::now() + MAX_WAIT;
    while tokio::time::Instant::now() < deadline {
        if tokio::fs::try_exists("/tmp/registry-login-done")
            .await
            .unwrap_or(false)
        {
            info!("Registry login ready (lane build can pull from registry)");
            return;
        }
        sleep(POLL).await;
    }
    warn!("Registry login not confirmed within 60s (lane build may fail to fetch container)");
}

async fn run_lane_build(
    image_with_digest: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;

    wait_for_docker().await?;
    wait_for_registry_login().await;
    log_disk_space("before cleanup").await;
    cleanup_docker().await;
    log_disk_space_detail("after cleanup / before lane build").await;
    info!("🚀 Starting Lane build with image: {}", image_with_digest);

    let lane_home = std::env::var("LANE_HOME").unwrap_or_else(|_| "/data/lane-home".to_string());
    let lane_cache =
        std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| "/data/lane-cache".to_string());

    let mut child = TokioCommand::new("lane")
        .args(["build", "prod", "--image", image_with_digest])
        .current_dir("/root")
        .env("HOME", &lane_home)
        .env("XDG_CACHE_HOME", &lane_cache)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let mut stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
    let mut stderr = child.stderr.take().ok_or("Failed to capture stderr")?;

    let (stdout_tx, stdout_rx) = tokio::sync::oneshot::channel();
    let (stderr_tx, stderr_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out).await;
        let _ = stdout_tx.send(out);
    });
    tokio::spawn(async move {
        let mut err = Vec::new();
        let _ = stderr.read_to_end(&mut err).await;
        let _ = stderr_tx.send(err);
    });

    let status = child.wait().await?;

    let stdout_bytes = stdout_rx.await.unwrap_or_default();
    let stderr_bytes = stderr_rx.await.unwrap_or_default();
    let stdout_str = String::from_utf8_lossy(&stdout_bytes);
    let stderr_str = String::from_utf8_lossy(&stderr_bytes);

    log_disk_space("after lane build").await;

    if status.success() {
        info!("✅ Lane build completed successfully");
        Ok("Lane build completed successfully".to_string())
    } else {
        let error_msg = format!(
            "Lane build failed with exit code {}.\nstdout: {}\nstderr: {}",
            status,
            if stdout_str.is_empty() {
                "(empty)"
            } else {
                stdout_str.trim()
            },
            if stderr_str.is_empty() {
                "(empty)"
            } else {
                stderr_str.trim()
            }
        );
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

    let _ = std::io::stderr().write_all(
        b"[ASYNC] Building router and starting server (tracing init in background)...\n",
    );
    let _ = std::io::stderr().flush();

    // Init tracing in background so we don't delay the server from accepting /health.
    // Fly's health check can hit very early; we must be ready as soon as possible.
    tokio::spawn(async {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        std::future::pending::<()>().await;
    });

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/notify", post(notify_handler))
        .fallback(not_found_handler)
        .layer(middleware::from_fn(logging_middleware));

    const GRACE_SECS: u64 = 60;
    let shutdown_signal = async {
        use std::io::Write;
        use tokio::signal;
        let start = std::time::Instant::now();

        #[cfg(unix)]
        let wait_for_shutdown = async {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
            let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
                .expect("failed to install SIGINT handler");

            let on_signal = |name: &str, in_grace: bool| {
                let msg = if in_grace {
                    format!("[SIGNAL] Ignoring {} (grace period)\n", name)
                } else {
                    format!("[SIGNAL] Received {} (after grace period), exiting\n", name)
                };
                let _ = std::io::stderr().write_all(msg.as_bytes());
                let _ = std::io::stderr().flush();
            };

            let sigterm_loop = async {
                loop {
                    sigterm.recv().await;
                    let in_grace = start.elapsed() < Duration::from_secs(GRACE_SECS);
                    on_signal("SIGTERM", in_grace);
                    if !in_grace {
                        break;
                    }
                }
            };
            let sigint_loop = async {
                loop {
                    sigint.recv().await;
                    let in_grace = start.elapsed() < Duration::from_secs(GRACE_SECS);
                    on_signal("SIGINT", in_grace);
                    if !in_grace {
                        break;
                    }
                }
            };
            tokio::select! {
                _ = sigterm_loop => {}
                _ = sigint_loop => {}
            }
        };

        #[cfg(not(unix))]
        let wait_for_shutdown = async {
            signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
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
            match result {
                Ok(()) => {
                    let _ = std::io::stderr().write_all(b"[UNEXPECTED] Server exited without error - exiting with code 1 to trigger restart\n");
                    let _ = std::io::stderr().flush();
                    std::process::exit(1);
                }
                Err(e) => {
                    let error_msg = format!("❌ Server error: {}", e);
                    let _ = std::io::stderr().write_all(error_msg.as_bytes());
                    let _ = std::io::stderr().write_all(b"\n");
                    let _ = std::io::stderr().flush();
                    error!("{}", error_msg);
                    std::process::exit(1);
                }
            }
        },
        _ = shutdown_signal => {
            let _ = std::io::stderr().write_all(b"[SHUTDOWN] Received shutdown signal, exiting gracefully\n");
            let _ = std::io::stderr().flush();
            info!("🛑 Shutting down gracefully...");
        },
    }
}

/// Export output directory. Must be absolute when using current_dir("/root") so upload can find it.
const LANE_EXPORT_DIR: &str = "/root/lane-export-temp";
async fn run_lane_export_and_upload(
    digest: &str,
    image: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("📤 Starting Lane export");

    // Clean up any previous export directory so we start fresh.
    if tokio::fs::metadata(LANE_EXPORT_DIR).await.is_ok() {
        info!("🧹 Removing previous {}...", LANE_EXPORT_DIR);
        tokio::fs::remove_dir_all(LANE_EXPORT_DIR).await.ok();
    }

    let lane_home = std::env::var("LANE_HOME").unwrap_or_else(|_| "/data/lane-home".to_string());
    let lane_cache =
        std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| "/data/lane-cache".to_string());
    let mut child = TokioCommand::new("lane")
        .args(["export", "prod", LANE_EXPORT_DIR, "--image", image])
        .current_dir("/root")
        .env("HOME", &lane_home)
        .env("XDG_CACHE_HOME", &lane_cache)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    let status = child.wait().await?;

    if !status.success() {
        return Err(format!("Lane export failed with exit code {}", status).into());
    }

    info!("✅ Lane export completed successfully");
    info!("☁️ Starting upload to Tigris S3");

    tigris::upload_to_tigris(digest, LANE_EXPORT_DIR).await?;

    info!("🧹 Cleaning up {} after upload...", LANE_EXPORT_DIR);
    tokio::fs::remove_dir_all(LANE_EXPORT_DIR).await.ok();

    Ok(())
}
