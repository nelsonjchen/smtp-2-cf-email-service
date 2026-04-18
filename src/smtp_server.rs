use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mailparse::{MailAddr, MailHeaderMap, addrparse, parse_headers};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tracing::{info, warn};

use crate::config::{AppConfig, PreparedConfig, is_allowed_sender_domain};
use crate::queue::QueueStore;
use crate::tls::load_acceptor;

#[derive(Clone)]
struct Runtime {
    config: AppConfig,
    queue: QueueStore,
    smtp_password: String,
    tls_acceptor: Option<TlsAcceptor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenerMode {
    Port25,
    Submission587,
    Smtps465,
}

impl ListenerMode {
    fn bind_addr(self, config: &AppConfig) -> &str {
        match self {
            Self::Port25 => &config.ports.smtp_25,
            Self::Submission587 => &config.ports.submission_587,
            Self::Smtps465 => &config.ports.smtps_465,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Port25 => "smtp-25",
            Self::Submission587 => "submission-587",
            Self::Smtps465 => "smtps-465",
        }
    }

    fn is_implicit_tls(self) -> bool {
        matches!(self, Self::Smtps465)
    }

    fn allows_starttls(self) -> bool {
        matches!(self, Self::Port25 | Self::Submission587)
    }

    fn requires_tls_before_mail(self) -> bool {
        matches!(self, Self::Submission587)
    }
}

#[derive(Debug)]
struct SessionState {
    mode: ListenerMode,
    tls_active: bool,
    authenticated: bool,
    mail_from: Option<String>,
    recipients: Vec<String>,
}

impl SessionState {
    fn new(mode: ListenerMode) -> Self {
        Self {
            mode,
            tls_active: mode.is_implicit_tls(),
            authenticated: false,
            mail_from: None,
            recipients: Vec::new(),
        }
    }

    fn reset_message(&mut self) {
        self.mail_from = None;
        self.recipients.clear();
    }
}

enum Connection {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
    Invalid,
}

impl Connection {
    async fn upgrade_to_tls(&mut self, acceptor: &TlsAcceptor) -> Result<()> {
        let plain = match std::mem::replace(self, Self::Invalid) {
            Self::Plain(stream) => stream,
            Self::Tls(stream) => {
                *self = Self::Tls(stream);
                bail!("connection is already using TLS");
            }
            Self::Invalid => bail!("connection is in an invalid state"),
        };

        let tls_stream = acceptor
            .accept(plain)
            .await
            .context("STARTTLS handshake failed")?;
        *self = Self::Tls(tls_stream);
        Ok(())
    }

    async fn read_line(&mut self, timeout_duration: Duration) -> Result<Option<String>> {
        let mut buf = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = timeout(timeout_duration, self.read_some(&mut byte)).await??;
            if n == 0 {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            buf.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
            if buf.len() > 16 * 1024 {
                bail!("SMTP line exceeded 16 KiB");
            }
        }

        Ok(Some(String::from_utf8(buf)?))
    }

    async fn read_data(
        &mut self,
        timeout_duration: Duration,
        max_message_size: usize,
    ) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let line = self
                .read_line(timeout_duration)
                .await?
                .ok_or_else(|| anyhow!("client disconnected during DATA"))?;

            if line == ".\r\n" || line == ".\n" || line.trim_end() == "." {
                break;
            }

            let line = if line.starts_with("..") {
                &line.as_bytes()[1..]
            } else {
                line.as_bytes()
            };
            data.extend_from_slice(line);

            if data.len() > max_message_size {
                bail!("message exceeded configured max_message_size");
            }
        }
        Ok(data)
    }

    async fn write_line(&mut self, line: &str) -> Result<()> {
        self.write_bytes(format!("{line}\r\n").as_bytes()).await
    }

    async fn write_lines(&mut self, lines: &[String]) -> Result<()> {
        for line in lines {
            self.write_line(line).await?;
        }
        Ok(())
    }

    async fn read_some(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self {
            Self::Plain(stream) => Ok(stream.read(buf).await?),
            Self::Tls(stream) => Ok(stream.read(buf).await?),
            Self::Invalid => bail!("connection is in an invalid state"),
        }
    }

    async fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Plain(stream) => {
                stream.write_all(bytes).await?;
                stream.flush().await?;
            }
            Self::Tls(stream) => {
                stream.write_all(bytes).await?;
                stream.flush().await?;
            }
            Self::Invalid => bail!("connection is in an invalid state"),
        }
        Ok(())
    }
}

pub async fn run_servers(prepared: PreparedConfig, queue: QueueStore) -> Result<()> {
    let tls_acceptor = match prepared.tls.as_ref() {
        Some(tls) => Some(load_acceptor(tls)?),
        None => None,
    };

    let runtime = Arc::new(Runtime {
        config: prepared.config,
        queue,
        smtp_password: prepared.secrets.smtp_password,
        tls_acceptor,
    });

    let mut join_set = tokio::task::JoinSet::new();
    join_set.spawn(run_listener(runtime.clone(), ListenerMode::Port25));
    if runtime.tls_acceptor.is_some() {
        join_set.spawn(run_listener(runtime.clone(), ListenerMode::Submission587));
        join_set.spawn(run_listener(runtime.clone(), ListenerMode::Smtps465));
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => return Err(err),
            Err(err) => return Err(anyhow!("listener task failed: {err}")),
        }
    }

    Ok(())
}

async fn run_listener(runtime: Arc<Runtime>, mode: ListenerMode) -> Result<()> {
    let bind_addr = mode.bind_addr(&runtime.config).to_string();
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    info!(listener = mode.label(), bind_addr, "listener started");

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let runtime = runtime.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(runtime, mode, stream).await {
                warn!(listener = mode.label(), peer = %peer_addr, error = %err, "SMTP session failed");
            }
        });
    }
}

async fn handle_connection(
    runtime: Arc<Runtime>,
    mode: ListenerMode,
    stream: TcpStream,
) -> Result<()> {
    let mut conn = if mode.is_implicit_tls() {
        let acceptor = runtime
            .tls_acceptor
            .as_ref()
            .ok_or_else(|| anyhow!("implicit TLS listener started without a TLS acceptor"))?;
        Connection::Tls(
            acceptor
                .accept(stream)
                .await
                .context("implicit TLS handshake failed")?,
        )
    } else {
        Connection::Plain(stream)
    };

    let mut session = SessionState::new(mode);
    conn.write_line(&format!(
        "220 {} ESMTP {}",
        runtime.config.hostname, runtime.config.appname
    ))
    .await?;

    loop {
        let line = match conn
            .read_line(Duration::from_secs(runtime.config.command_timeout_secs))
            .await?
        {
            Some(line) => line,
            None => return Ok(()),
        };

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }

        let (command, arg) = split_command(line);

        match command.as_str() {
            "EHLO" => handle_ehlo(&mut conn, &runtime, &session).await?,
            "HELO" => {
                conn.write_line(&format!("250 {}", runtime.config.hostname))
                    .await?
            }
            "NOOP" => conn.write_line("250 2.0.0 OK").await?,
            "RSET" => {
                session.reset_message();
                conn.write_line("250 2.0.0 Reset state").await?;
            }
            "QUIT" => {
                conn.write_line("221 2.0.0 Bye").await?;
                return Ok(());
            }
            "STARTTLS" => {
                if session.tls_active {
                    conn.write_line("454 4.3.3 TLS already active").await?;
                    continue;
                }
                if !mode.allows_starttls() || runtime.tls_acceptor.is_none() {
                    conn.write_line("454 4.7.0 TLS not available").await?;
                    continue;
                }
                conn.write_line("220 2.0.0 Ready to start TLS").await?;
                let acceptor = runtime
                    .tls_acceptor
                    .as_ref()
                    .ok_or_else(|| anyhow!("missing TLS acceptor"))?;
                conn.upgrade_to_tls(acceptor).await?;
                session = SessionState::new(mode);
                session.tls_active = true;
            }
            "AUTH" => {
                if !auth_allowed(&runtime, &session) {
                    conn.write_line(
                        "538 5.7.11 Encryption required for requested authentication mechanism",
                    )
                    .await?;
                    continue;
                }
                if session.authenticated {
                    conn.write_line("503 5.5.1 Already authenticated").await?;
                    continue;
                }
                handle_auth(&mut conn, &runtime, arg, &mut session).await?;
            }
            "MAIL" => {
                if let Some(message) = mail_allowed_error(&runtime, &session) {
                    conn.write_line(message).await?;
                    continue;
                }

                let Some(from) = parse_path_arg(arg, "FROM:") else {
                    conn.write_line("501 5.5.4 MAIL FROM is malformed").await?;
                    continue;
                };
                if !is_allowed_sender_domain(&runtime.config.senders.allowed_domains, &from) {
                    conn.write_line("553 5.7.1 Sender domain not allowed")
                        .await?;
                    continue;
                }

                session.mail_from = Some(from);
                session.recipients.clear();
                conn.write_line("250 2.1.0 OK").await?;
            }
            "RCPT" => {
                if let Some(message) = mail_allowed_error(&runtime, &session) {
                    conn.write_line(message).await?;
                    continue;
                }
                if session.mail_from.is_none() {
                    conn.write_line("503 5.5.1 Need MAIL before RCPT").await?;
                    continue;
                }

                let Some(rcpt) = parse_path_arg(arg, "TO:") else {
                    conn.write_line("501 5.5.4 RCPT TO is malformed").await?;
                    continue;
                };
                if session.recipients.len() >= runtime.config.max_recipients {
                    conn.write_line("452 4.5.3 Too many recipients").await?;
                    continue;
                }

                session.recipients.push(rcpt);
                conn.write_line("250 2.1.5 OK").await?;
            }
            "DATA" => {
                if let Some(message) = mail_allowed_error(&runtime, &session) {
                    conn.write_line(message).await?;
                    continue;
                }
                let Some(from) = session.mail_from.clone() else {
                    conn.write_line("503 5.5.1 Need MAIL before DATA").await?;
                    continue;
                };
                if session.recipients.is_empty() {
                    conn.write_line("503 5.5.1 Need RCPT before DATA").await?;
                    continue;
                }

                conn.write_line("354 End data with <CR><LF>.<CR><LF>")
                    .await?;
                let raw = match conn
                    .read_data(
                        Duration::from_secs(runtime.config.command_timeout_secs),
                        runtime.config.max_message_size,
                    )
                    .await
                {
                    Ok(raw) => raw,
                    Err(err) => {
                        conn.write_line(&format!("552 5.3.4 {}", err)).await?;
                        continue;
                    }
                };

                match ensure_header_from_allowed(&runtime.config.senders.allowed_domains, &raw) {
                    Ok(()) => {}
                    Err(err) => {
                        conn.write_line(&format!("553 5.7.1 {}", err)).await?;
                        continue;
                    }
                }

                let id = match runtime
                    .queue
                    .enqueue(from, session.recipients.clone(), raw)
                    .context("failed to enqueue message")
                {
                    Ok(id) => id,
                    Err(err) => {
                        conn.write_line(&format!("451 4.3.0 {}", err)).await?;
                        continue;
                    }
                };

                session.reset_message();
                conn.write_line(&format!("250 2.0.0 queued as <{id}>"))
                    .await?;
            }
            _ => conn.write_line("502 5.5.1 Command not implemented").await?,
        }
    }
}

fn auth_allowed(runtime: &Runtime, session: &SessionState) -> bool {
    session.tls_active || runtime.config.tls.allow_insecure_auth
}

fn mail_allowed_error(runtime: &Runtime, session: &SessionState) -> Option<&'static str> {
    if !session.authenticated {
        return Some("530 5.7.0 Authentication required");
    }
    if session.mode.requires_tls_before_mail() && !session.tls_active {
        return Some("530 5.7.0 Must issue STARTTLS first");
    }
    if session.mode == ListenerMode::Port25
        && !session.tls_active
        && !runtime.config.tls.allow_insecure_auth
    {
        return Some("530 5.7.0 Plaintext port 25 is disabled for mail submission without TLS");
    }
    None
}

async fn handle_ehlo(
    conn: &mut Connection,
    runtime: &Runtime,
    session: &SessionState,
) -> Result<()> {
    let mut lines = vec![
        format!("250-{}", runtime.config.hostname),
        format!("250-SIZE {}", runtime.config.max_message_size),
    ];

    if !session.tls_active && runtime.tls_acceptor.is_some() && session.mode.allows_starttls() {
        lines.push("250-STARTTLS".to_string());
    }
    if auth_allowed(runtime, session) {
        lines.push("250-AUTH PLAIN LOGIN".to_string());
    }
    lines.push("250 HELP".to_string());

    conn.write_lines(&lines).await
}

async fn handle_auth(
    conn: &mut Connection,
    runtime: &Runtime,
    arg: &str,
    session: &mut SessionState,
) -> Result<()> {
    let (mechanism, rest) = split_command(arg);
    let (username, password) = match mechanism.as_str() {
        "PLAIN" => match decode_auth_plain(rest) {
            Ok(creds) => creds,
            Err(err) => {
                conn.write_line(&format!("501 5.5.2 {}", err)).await?;
                return Ok(());
            }
        },
        "LOGIN" => match decode_auth_login(conn, rest).await {
            Ok(creds) => creds,
            Err(err) => {
                conn.write_line(&format!("501 5.5.2 {}", err)).await?;
                return Ok(());
            }
        },
        _ => {
            conn.write_line("504 5.5.4 Unsupported authentication mechanism")
                .await?;
            return Ok(());
        }
    };

    if username == runtime.config.auth.username && password == runtime.smtp_password {
        session.authenticated = true;
        conn.write_line("235 2.7.0 Authentication successful")
            .await?;
    } else {
        conn.write_line("535 5.7.8 Authentication credentials invalid")
            .await?;
    }

    Ok(())
}

fn ensure_header_from_allowed(allowed_domains: &[String], raw: &[u8]) -> Result<()> {
    let (headers, _) = parse_headers(raw).context("failed to parse RFC 5322 headers")?;
    let from_header = headers
        .get_first_value("From")
        .ok_or_else(|| anyhow!("missing From header"))?;
    let addresses = addrparse(&from_header).context("failed to parse From header address")?;

    for entry in addresses.iter() {
        match entry {
            MailAddr::Single(info) => {
                if !is_allowed_sender_domain(allowed_domains, &info.addr) {
                    bail!("From header domain is not allowed");
                }
            }
            MailAddr::Group(group) => {
                for info in &group.addrs {
                    if !is_allowed_sender_domain(allowed_domains, &info.addr) {
                        bail!("From header domain is not allowed");
                    }
                }
            }
        }
    }

    Ok(())
}

fn decode_auth_plain(rest: &str) -> Result<(String, String)> {
    if rest.trim().is_empty() {
        bail!("AUTH PLAIN requires an initial response");
    }

    let decoded = STANDARD
        .decode(rest.trim())
        .context("failed to decode AUTH PLAIN payload")?;
    let parts: Vec<&[u8]> = decoded.split(|byte| *byte == 0).collect();
    let (username, password) = if parts.len() >= 3 {
        (parts[1], parts[2])
    } else if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        bail!("malformed AUTH PLAIN payload");
    };

    Ok((
        String::from_utf8(username.to_vec())?,
        String::from_utf8(password.to_vec())?,
    ))
}

async fn decode_auth_login(conn: &mut Connection, rest: &str) -> Result<(String, String)> {
    let username = if rest.trim().is_empty() {
        conn.write_line("334 VXNlcm5hbWU6").await?;
        decode_b64_line(
            conn.read_line(Duration::from_secs(300))
                .await?
                .ok_or_else(|| anyhow!("client disconnected during AUTH LOGIN"))?,
        )?
    } else {
        decode_b64_line(rest.to_string())?
    };

    conn.write_line("334 UGFzc3dvcmQ6").await?;
    let password = decode_b64_line(
        conn.read_line(Duration::from_secs(300))
            .await?
            .ok_or_else(|| anyhow!("client disconnected during AUTH LOGIN"))?,
    )?;

    Ok((username, password))
}

fn decode_b64_line(line: String) -> Result<String> {
    let decoded = STANDARD
        .decode(line.trim())
        .context("failed to decode base64 SMTP auth line")?;
    Ok(String::from_utf8(decoded)?)
}

fn split_command(line: &str) -> (String, &str) {
    if let Some((command, arg)) = line.split_once(char::is_whitespace) {
        (command.trim().to_ascii_uppercase(), arg.trim())
    } else {
        (line.trim().to_ascii_uppercase(), "")
    }
}

fn parse_path_arg(input: &str, prefix: &str) -> Option<String> {
    let input = input.trim();
    if !input.to_ascii_uppercase().starts_with(prefix) {
        return None;
    }

    let value = input[prefix.len()..]
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_string();

    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mail_path_arguments() {
        assert_eq!(
            parse_path_arg("FROM:<sender@example.com>", "FROM:").unwrap(),
            "sender@example.com"
        );
    }

    #[test]
    fn decodes_auth_plain_payload() {
        let payload = STANDARD.encode(b"\0user\0pass");
        let (username, password) = decode_auth_plain(&payload).unwrap();
        assert_eq!(username, "user");
        assert_eq!(password, "pass");
    }

    #[test]
    fn validates_header_from_domains() {
        let raw = b"From: Sender <sender@example.com>\r\nTo: to@example.net\r\n\r\nHello";
        ensure_header_from_allowed(&["example.com".to_string()], raw).unwrap();
        assert!(ensure_header_from_allowed(&["other.com".to_string()], raw).is_err());
    }
}
