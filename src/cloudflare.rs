use anyhow::{Context, Result, anyhow};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::queue::QueueEnvelope;

#[derive(Clone)]
pub struct CloudflareClient {
    http: reqwest::Client,
    account_id: String,
    api_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryDecision {
    Delivered,
    Retry(String),
    Dead(String),
}

#[derive(Debug, Serialize)]
pub(crate) struct SendRawRequest {
    from: String,
    recipients: Vec<String>,
    mime_message: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: u32,
    message: String,
}

impl CloudflareClient {
    pub fn new(account_id: String, api_token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .context("failed to build reqwest client")?;

        Ok(Self {
            http,
            account_id,
            api_token,
        })
    }

    pub async fn deliver(&self, envelope: &QueueEnvelope) -> Result<DeliveryDecision> {
        let body = build_send_raw_request(envelope)?;
        let url = format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/email/sending/send_raw",
            self.account_id
        );

        let response = match self
            .http
            .post(url)
            .bearer_auth(&self.api_token)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => return Ok(DeliveryDecision::Retry(err.to_string())),
        };

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            return Ok(DeliveryDecision::Retry(format!(
                "Cloudflare HTTP {status}: {}",
                truncate_body(&text)
            )));
        }

        if !status.is_success() {
            return Ok(DeliveryDecision::Dead(format!(
                "Cloudflare HTTP {status}: {}",
                truncate_body(&text)
            )));
        }

        let parsed: ApiResponse = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse Cloudflare response body: {text}"))?;

        if parsed.success {
            return Ok(DeliveryDecision::Delivered);
        }

        if parsed.errors.iter().any(|err| err.code == 10004) {
            return Ok(DeliveryDecision::Retry(format_api_errors(&parsed.errors)));
        }

        Ok(DeliveryDecision::Dead(format_api_errors(&parsed.errors)))
    }
}

pub(crate) fn build_send_raw_request(envelope: &QueueEnvelope) -> Result<SendRawRequest> {
    Ok(SendRawRequest {
        from: envelope.envelope_from.clone(),
        recipients: envelope.recipients.clone(),
        mime_message: String::from_utf8(envelope.raw_mime()?)
            .map_err(|_| anyhow!("raw MIME message is not valid UTF-8"))?,
    })
}

fn format_api_errors(errors: &[ApiError]) -> String {
    if errors.is_empty() {
        return "Cloudflare returned success=false without error details".to_string();
    }

    errors
        .iter()
        .map(|err| format!("{}: {}", err.code, err.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn truncate_body(body: &str) -> String {
    const MAX: usize = 240;
    if body.len() <= MAX {
        body.to_string()
    } else {
        format!("{}...", &body[..MAX])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn request_body_uses_raw_mime() {
        let envelope = QueueEnvelope {
            id: "1".into(),
            envelope_from: "sender@example.com".into(),
            recipients: vec!["to@example.net".into()],
            raw_mime_b64: base64::engine::general_purpose::STANDARD
                .encode(b"From: sender@example.com\r\nTo: to@example.net\r\n\r\nHello"),
            attempt_count: 0,
            next_attempt_at: 0,
            last_error: None,
            created_at: 0,
        };

        let request = build_send_raw_request(&envelope).unwrap();
        assert_eq!(request.from, "sender@example.com");
        assert_eq!(request.recipients, vec!["to@example.net"]);
        assert!(request.mime_message.contains("From: sender@example.com"));
    }
}
