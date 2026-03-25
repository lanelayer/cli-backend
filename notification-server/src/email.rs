use reqwest::Client;
use serde_json::{json, Value};

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

/// Fetch the recipient email from lanelayer-analytics, if available.
///
/// Required env var for lookup:
/// - `LANELAYER_ANALYTICS_BASE_URL`
///
/// Optional env vars:
/// - `LANELAYER_ANALYTICS_AUTH_TOKEN` (optional; when set, uses `/api/v1/auth/email/{session_id}` first)
/// - `LANELAYER_ANALYTICS_EMAIL_PATH` (optional; defaults to `/api/v1/auth/email/{session_id}`; `{session_id}` replaced)
/// - `LANELAYER_ANALYTICS_STATUS_PATH` (optional; defaults to `/api/v1/auth/status`)
/// - `LANELAYER_ANALYTICS_SESSION_QUERY_PARAM` (optional; defaults to `session`; used for `/api/v1/auth/status?session=...`)
pub async fn fetch_email_from_analytics(
    session_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let base_url = match std::env::var("LANELAYER_ANALYTICS_BASE_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim_end_matches('/').to_string(),
        _ => return Ok(None),
    };

    // The newer endpoint returns the email as *plain text* and requires auth.
    // In lanelayer-analytics this is:
    //   GET /api/v1/auth/email/:session_id
    // with `Authorization: Bearer <auth_token>` (or `?auth_token=...`).
    let auth_token = std::env::var("LANELAYER_ANALYTICS_AUTH_TOKEN")
        .ok()
        .and_then(|v| {
            let t = v.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });

    let email_path_template = std::env::var("LANELAYER_ANALYTICS_EMAIL_PATH")
        .unwrap_or_else(|_| "/api/v1/auth/email/{session_id}".to_string());

    if let Some(token) = auth_token.as_ref() {
        let email_path = email_path_template.replace("{session_id}", session_id);
        let email_url = format!("{}/{}", base_url, email_path.trim_start_matches('/'));

        let response = Client::new()
            .get(&email_url)
            .bearer_auth(token)
            .query(&[("auth_token", token)])
            .send()
            .await?;

        if response.status().is_success() {
            let email_text = response.text().await.unwrap_or_default();
            let trimmed = email_text.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
    }

    // Fallback: the status endpoint is public and returns JSON with `{ verified, email }`:
    //   GET /api/v1/auth/status?session=<session_id>
    let status_path = std::env::var("LANELAYER_ANALYTICS_STATUS_PATH")
        .unwrap_or_else(|_| "/api/v1/auth/status".to_string());
    let session_query_param = std::env::var("LANELAYER_ANALYTICS_SESSION_QUERY_PARAM")
        .unwrap_or_else(|_| "session".to_string());

    let status_url = format!("{}/{}", base_url, status_path.trim_start_matches('/'));

    let response = Client::new()
        .get(&status_url)
        .query(&[(session_query_param.as_str(), session_id)])
        .send()
        .await?;

    if !response.status().is_success() {
        return Ok(None);
    }

    let body: Value = response.json().await?;

    // If the API includes an explicit "verified" gate, respect it (when present).
    let verified = body.get("verified").and_then(|v| v.as_bool());
    if verified == Some(false) {
        return Ok(None);
    }

    Ok(extract_email_from_analytics_payload(&body))
}

/// Priority:
/// 1) analytics email for provided session_id
/// 2) `RESEND_TO_EMAILS` fallback list
pub async fn resolve_recipients(
    session_id: Option<&str>,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(sid) = session_id {
        if !sid.trim().is_empty() {
            if let Some(email) = fetch_email_from_analytics(sid).await? {
                return Ok(vec![email]);
            }
        }
    }
    let (_, _, recipients) = resend_config().map_err(|e| e.to_string())?;
    Ok(recipients)
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

    Ok(())
}
