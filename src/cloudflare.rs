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
    let sanitized = sanitize_raw_mime_for_cloudflare(&envelope.raw_mime()?)?;

    Ok(SendRawRequest {
        from: envelope.envelope_from.clone(),
        recipients: envelope.recipients.clone(),
        mime_message: String::from_utf8(sanitized)
            .map_err(|_| anyhow!("raw MIME message is not valid UTF-8"))?,
    })
}

fn sanitize_raw_mime_for_cloudflare(raw: &[u8]) -> Result<Vec<u8>> {
    let Some((header_bytes, body_bytes)) = split_headers_and_body(raw) else {
        return Err(anyhow!("raw MIME message is missing header/body separator"));
    };

    let header_blocks = parse_header_blocks(header_bytes)?;
    let mut sanitized = Vec::with_capacity(raw.len());

    for block in header_blocks {
        let name = header_name(&block)?;
        if should_strip_header(name) {
            continue;
        }

        for line in block {
            sanitized.extend_from_slice(&line);
            sanitized.extend_from_slice(b"\r\n");
        }
    }

    sanitized.extend_from_slice(b"\r\n");
    sanitized.extend_from_slice(body_bytes);
    Ok(sanitized)
}

fn split_headers_and_body(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
        return Some((&raw[..index], &raw[index + 4..]));
    }

    raw.windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (&raw[..index], &raw[index + 2..]))
}

fn parse_header_blocks(header_bytes: &[u8]) -> Result<Vec<Vec<Vec<u8>>>> {
    let mut blocks: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut current: Option<Vec<Vec<u8>>> = None;

    for line in split_header_lines(header_bytes) {
        if line.is_empty() {
            continue;
        }

        if matches!(line.first(), Some(b' ' | b'\t')) {
            let current = current
                .as_mut()
                .ok_or_else(|| anyhow!("encountered folded header without a preceding header"))?;
            current.push(line.to_vec());
            continue;
        }

        if let Some(block) = current.take() {
            blocks.push(block);
        }
        current = Some(vec![line.to_vec()]);
    }

    if let Some(block) = current {
        blocks.push(block);
    }

    Ok(blocks)
}

fn split_header_lines(header_bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;

    while index < header_bytes.len() {
        match header_bytes[index] {
            b'\r' if header_bytes.get(index + 1) == Some(&b'\n') => {
                lines.push(&header_bytes[start..index]);
                index += 2;
                start = index;
            }
            b'\n' => {
                lines.push(&header_bytes[start..index]);
                index += 1;
                start = index;
            }
            _ => {
                index += 1;
            }
        }
    }

    if start < header_bytes.len() {
        lines.push(&header_bytes[start..]);
    }

    lines
}

fn header_name(block: &[Vec<u8>]) -> Result<&str> {
    let first_line = block
        .first()
        .ok_or_else(|| anyhow!("header block unexpectedly empty"))?;
    let colon = first_line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(|| anyhow!("header is missing ':' separator"))?;
    std::str::from_utf8(&first_line[..colon])
        .map(str::trim)
        .map_err(|_| anyhow!("header name is not valid UTF-8"))
}

fn should_strip_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "arc-authentication-results"
            | "arc-message-signature"
            | "arc-seal"
            | "authentication-results"
            | "bcc"
            | "delivered-to"
            | "dkim-signature"
            | "received"
            | "return-path"
            | "x-gm-features"
            | "x-gm-message-state"
            | "x-gmail-original-message-id"
            | "x-received"
    )
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

    #[test]
    fn sanitizes_folded_transport_headers_without_touching_body() {
        let raw = concat!(
            "Received: by mail-lj1-f178.google.com with SMTP id abc\r\n",
            "        for <to@example.net>; Sat, 18 Apr 2026 19:49:14 -0700 (PDT)\r\n",
            "X-Gm-Message-State: AOJu0Ywg+MlHIHpr4MNBgUQFai0/7mYelgJzkuhyS13gzHMof12XwuwL\r\n",
            "\tWLJfU/V1dFob7vG4+mQ1hjcXELn49hoHf2y8QKkEuwbf5vm+MyEpVaYnSLyFNA2CzQoawEB1bqU\r\n",
            "From: Sender Example <sender@example.com>\r\n",
            "To: recipient@example.net\r\n",
            "Subject: test 123\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/alternative; boundary=\"b\"\r\n",
            "\r\n",
            "--b\r\n",
            "Content-Type: text/plain; charset=\"UTF-8\"\r\n",
            "\r\n",
            "\r\n",
            "--b\r\n",
            "Content-Type: text/html; charset=\"UTF-8\"\r\n",
            "\r\n",
            "<div dir=\"ltr\"><br></div>\r\n",
            "--b--\r\n"
        )
        .as_bytes();

        let sanitized = String::from_utf8(sanitize_raw_mime_for_cloudflare(raw).unwrap()).unwrap();
        assert!(!sanitized.contains("Received: by mail-lj1-f178.google.com"));
        assert!(!sanitized.contains("X-Gm-Message-State:"));
        assert!(sanitized.contains("From: Sender Example <sender@example.com>"));
        assert!(sanitized.contains("<div dir=\"ltr\"><br></div>"));
    }

    #[test]
    fn strips_bcc_before_forwarding() {
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: visible@example.net\r\n",
            "Bcc: hidden@example.net\r\n",
            "Subject: hi\r\n",
            "\r\n",
            "hello\r\n"
        )
        .as_bytes();

        let sanitized = String::from_utf8(sanitize_raw_mime_for_cloudflare(raw).unwrap()).unwrap();
        assert!(!sanitized.contains("Bcc: hidden@example.net"));
        assert!(sanitized.contains("To: visible@example.net"));
        assert!(sanitized.ends_with("\r\n\r\nhello\r\n"));
    }
}
