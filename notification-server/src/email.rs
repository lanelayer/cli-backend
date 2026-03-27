use reqwest::Client;
use serde_json::{json, Value};
use tracing::{info, warn};

fn redact_token(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.len() <= 8 {
        return "***".to_string();
    }
    format!("{}...{}", &trimmed[..4], &trimmed[trimmed.len() - 4..])
}

/// Best-effort Resend email sender for lane push lifecycle notifications.
fn resend_config() -> Result<(String, String, Vec<String>), String> {
    let api_key = std::env::var("RESEND_API_KEY")
        .map_err(|_| "RESEND_API_KEY environment variable not set".to_string())?;
    let from = std::env::var("RESEND_FROM_EMAIL")
        .map_err(|_| "RESEND_FROM_EMAIL environment variable not set".to_string())?;
    let to_raw = std::env::var("RESEND_TO_EMAILS")
        .map_err(|_| "RESEND_TO_EMAILS environment variable not set".to_string())?;

    let recipients: Vec<String> = to_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    if recipients.is_empty() {
        return Err("RESEND_TO_EMAILS is empty after parsing".to_string());
    }

    Ok((api_key, from, recipients))
}

fn extract_email_from_analytics_payload(body: &Value) -> Option<String> {
    // Try a few common shapes so we can tolerate minor API contract changes.
    // Examples:
    // - { "email": "..." }
    // - { "verified": true, "email": "..." }
    // - { "data": { "email": "..." } }
    // - { "auth": { "email": "..." } }
    for email in [
        body.get("email").and_then(|v| v.as_str()),
        body.get("data")
            .and_then(|d| d.get("email"))
            .and_then(|v| v.as_str()),
        body.get("auth")
            .and_then(|a| a.get("email"))
            .and_then(|v| v.as_str()),
        body.get("user")
            .and_then(|u| u.get("email"))
            .and_then(|v| v.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        let trimmed = email.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Fetch recipient email from lane-analytics, if available.
///
/// Required env var:
/// - `LANELAYER_ANALYTICS_BASE_URL`
///
/// Optional env vars:
/// - `LANELAYER_ANALYTICS_STATUS_PATH` (defaults to `/api/v1/auth/status`)
/// - `LANELAYER_ANALYTICS_SESSION_QUERY_PARAM` (defaults to `session`)
pub async fn fetch_email_from_analytics(
    session_id: &str,
    bearer_token: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let base_url = match std::env::var("LANELAYER_ANALYTICS_BASE_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim_end_matches('/').to_string(),
        _ => {
            info!(
                "📭 Analytics lookup skipped: LANELAYER_ANALYTICS_BASE_URL not configured (session={})",
                session_id
            );
            return Ok(None);
        }
    };

    let status_path = std::env::var("LANELAYER_ANALYTICS_STATUS_PATH")
        .unwrap_or_else(|_| "/api/v1/auth/status".to_string());
    let session_query_param = std::env::var("LANELAYER_ANALYTICS_SESSION_QUERY_PARAM")
        .unwrap_or_else(|_| "session".to_string());
    let status_url = format!("{}/{}", base_url, status_path.trim_start_matches('/'));

    let analytics_auth_token = bearer_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var("LANELAYER_ANALYTICS_AUTH_TOKEN")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });

    info!(
        "🔎 Analytics lookup: url={} query_param={} session={} auth={}",
        status_url,
        session_query_param,
        session_id,
        match analytics_auth_token.as_deref() {
            Some(t) => format!("bearer({})", redact_token(t)),
            None => "none".to_string(),
        }
    );

    let client = Client::new();
    let mut req = client
        .get(&status_url)
        .query(&[(session_query_param.as_str(), session_id)]);

    if let Some(token) = analytics_auth_token {
        req = req.bearer_auth(token);
    }

    let response = req.send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        warn!(
            "⚠️ Analytics lookup failed: status={} body={}",
            status,
            body.chars().take(240).collect::<String>()
        );
        return Ok(None);
    }

    let body: Value = response.json().await?;
    info!("✅ Analytics lookup succeeded (session={})", session_id);
    let verified = body.get("verified").and_then(|v| v.as_bool());
    if verified == Some(false) {
        warn!(
            "⚠️ Analytics returned verified=false for session={}",
            session_id
        );
        return Ok(None);
    }

    let resolved = extract_email_from_analytics_payload(&body);
    if resolved.is_none() {
        warn!(
            "⚠️ Analytics payload had no usable email fields for session={}",
            session_id
        );
    }
    Ok(resolved)
}

/// Priority:
/// 1) analytics email for provided session_id
/// 2) fallback list from `RESEND_TO_EMAILS` (if configured)
/// 3) no email (skip notification)
pub async fn resolve_recipients(
    session_id: Option<&str>,
    analytics_bearer_token: Option<&str>,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(sid) = session_id {
        if !sid.trim().is_empty() {
            if let Some(email) = fetch_email_from_analytics(sid, analytics_bearer_token).await? {
                info!("📬 Resolved recipient from analytics: {}", email);
                return Ok(vec![email]);
            }
            info!("📭 Analytics did not return an email; trying RESEND_TO_EMAILS fallback");
        }
    }

    // Fall back to static recipients list (useful for dev / ops visibility).
    // If not configured, silently return empty.
    match resend_config() {
        Ok((_api_key, _from, recipients)) => {
            info!(
                "📬 Using RESEND_TO_EMAILS fallback recipients (count={})",
                recipients.len()
            );
            Ok(recipients)
        }
        Err(e) => {
            warn!("⚠️ No fallback recipients configured: {}", e);
            Ok(Vec::new())
        }
    }
}

pub async fn send_lane_push_started_email(
    recipients: &[String],
    original_path: &str,
    registry_path: &str,
    digest: Option<&str>,
    profile: &str,
    platforms: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let digest_text = digest.unwrap_or("n/a");
    let platform_text = if platforms.is_empty() {
        "n/a".to_string()
    } else {
        platforms.join(", ")
    };

    let subject = format!("Lane deployment started: {}", original_path);
    let html = format!(
        "<h2>Lane deployment started</h2>\
         <p><strong>Original:</strong> {}</p>\
         <p><strong>Registry:</strong> {}</p>\
         <p><strong>Digest:</strong> {}</p>\
         <p><strong>Profile:</strong> {}</p>\
         <p><strong>Platforms:</strong> {}</p>",
        original_path, registry_path, digest_text, profile, platform_text
    );

    send_resend_email(recipients, &subject, &html).await
}

pub async fn send_lane_push_success_email(
    recipients: &[String],
    target_image: &str,
    digest: &str,
    lane_rpc_url: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rpc_line = lane_rpc_url.unwrap_or("n/a");

    // Keep the subject "transactional" (no raw URL) to reduce Gmail Promotions classification.
    // Put the RPC URL in the body instead.
    let subject = format!("Lane push success: {}", target_image);
    let html = format!(
        "<h2>Lane push processed successfully</h2>\
         <p><strong>Target Image:</strong> {}</p>\
         <p><strong>Digest:</strong> {}</p>\
         <p><strong>Lane RPC URL:</strong> {}</p>",
        target_image, digest, rpc_line
    );

    send_resend_email(recipients, &subject, &html).await
}

async fn send_resend_email(
    recipients: &[String],
    subject: &str,
    html: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if recipients.is_empty() {
        return Err("No recipients provided".into());
    }
    info!(
        "✉️ Sending email via Resend: recipients={} subject=\"{}\"",
        recipients.join(","),
        subject
    );
    let (api_key, from, _) = resend_config().map_err(|e| e.to_string())?;
    let client = Client::new();

    let payload = json!({
        "from": from,
        "to": recipients,
        "subject": subject,
        "html": html
    });

    let response = client
        .post("https://api.resend.com/emails")
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Resend request failed ({}): {}", status, body).into());
    }

    info!("✅ Resend accepted email request");
    Ok(())
}
