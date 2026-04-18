use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use mailparse::{MailAddr, MailHeaderMap, addrparse, parse_headers};
use smtpd::{
    AuthData, AuthMach, Error, Response, Session, SmtpConfig, SmtpHandler, SmtpHandlerFactory,
    TlsConfig, TlsMode, async_trait, start_server,
};
use tracing::info;

use crate::config::{AppConfig, PreparedConfig, is_allowed_sender_domain};
use crate::queue::QueueStore;
use crate::tls::load_server_config;

#[derive(Clone)]
struct SharedRuntime {
    config: AppConfig,
    queue: QueueStore,
    smtp_password: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerKind {
    Port25,
    Submission587,
    Smtps465,
}

impl ServerKind {
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
}

#[derive(Clone)]
struct HandlerFactory {
    runtime: Arc<SharedRuntime>,
    kind: ServerKind,
}

struct Handler {
    runtime: Arc<SharedRuntime>,
    kind: ServerKind,
}

pub async fn run_servers(prepared: PreparedConfig, queue: QueueStore) -> Result<()> {
    let tls_config = prepared
        .tls
        .as_ref()
        .map(load_server_config)
        .transpose()?
        .map(TlsConfig::Rustls);

    let runtime = Arc::new(SharedRuntime {
        config: prepared.config,
        queue,
        smtp_password: prepared.secrets.smtp_password,
    });

    let mut join_set = tokio::task::JoinSet::new();
    join_set.spawn(start_smtpd_server(
        runtime.clone(),
        ServerKind::Port25,
        tls_config.clone(),
    ));
    if tls_config.is_some() {
        join_set.spawn(start_smtpd_server(
            runtime.clone(),
            ServerKind::Submission587,
            tls_config.clone(),
        ));
        join_set.spawn(start_smtpd_server(runtime, ServerKind::Smtps465, tls_config));
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => return Err(err),
            Err(err) => return Err(anyhow!("SMTP listener task failed: {err}")),
        }
    }

    Ok(())
}

async fn start_smtpd_server(
    runtime: Arc<SharedRuntime>,
    kind: ServerKind,
    tls_config: Option<TlsConfig>,
) -> Result<()> {
    let bind_addr = kind.bind_addr(&runtime.config).to_string();
    info!(listener = kind.label(), bind_addr, "listener started");

    let config = build_server_config(&runtime.config, kind, tls_config)?;
    let factory = HandlerFactory { runtime, kind };
    start_server(config, factory)
        .await
        .with_context(|| format!("listener {} stopped unexpectedly", kind.label()))?;
    Ok(())
}

fn build_server_config(
    app: &AppConfig,
    kind: ServerKind,
    tls_config: Option<TlsConfig>,
) -> Result<SmtpConfig> {
    let tls_mode = match kind {
        ServerKind::Port25 => match tls_config.clone() {
            Some(config) => TlsMode::Explicit(config),
            None => TlsMode::Disabled,
        },
        ServerKind::Submission587 => TlsMode::Required(
            tls_config
                .clone()
                .ok_or_else(|| anyhow!("submission port requires TLS"))?,
        ),
        ServerKind::Smtps465 => {
            TlsMode::Implicit(tls_config.ok_or_else(|| anyhow!("SMTPS port requires TLS"))?)
        }
    };

    let auth_machs = match kind {
        ServerKind::Port25 if !app.tls.allow_insecure_auth => vec![],
        _ => vec![AuthMach::Plain, AuthMach::Login],
    };

    Ok(SmtpConfig {
        hostname: app.hostname.clone(),
        appname: app.appname.clone(),
        bind_addr: kind.bind_addr(app).to_string(),
        tls_mode,
        max_message_size: Some(app.max_message_size),
        max_recipients: app.max_recipients,
        timeout: std::time::Duration::from_secs(app.command_timeout_secs),
        auth_machs,
        require_auth: !matches!(kind, ServerKind::Port25),
        disable_reverse_dns: true,
        x_client_allowed: None,
    })
}

impl SmtpHandlerFactory for HandlerFactory {
    type Handler = Handler;

    fn new_handler(&self, _session: &Session) -> Self::Handler {
        Handler {
            runtime: self.runtime.clone(),
            kind: self.kind,
        }
    }
}

#[async_trait]
impl SmtpHandler for Handler {
    async fn handle_auth(&mut self, _session: &Session, data: AuthData) -> smtpd::Result {
        let (username, password, _) = data.data();
        if username == self.runtime.config.auth.username && password == self.runtime.smtp_password {
            Ok(Response::Default)
        } else {
            Err(Error::Response(Response::new(
                535,
                "Authentication credentials invalid",
                Some("5.7.8".into()),
            )))
        }
    }

    async fn handle_rcpt(&mut self, session: &Session, _to: &str) -> smtpd::Result {
        if let Some(response) = validate_pre_data(&self.runtime.config, self.kind, session) {
            return Err(Error::Response(response));
        }
        Ok(Response::Default)
    }

    async fn handle_email(&mut self, session: &Session, data: Vec<u8>) -> smtpd::Result {
        if let Some(response) = validate_pre_data(&self.runtime.config, self.kind, session) {
            return Err(Error::Response(response));
        }

        if let Err(err) = ensure_header_from_allowed(&self.runtime.config.senders.allowed_domains, &data)
        {
            return Err(Error::Response(Response::new(
                553,
                err.to_string(),
                Some("5.7.1".into()),
            )));
        }

        match self
            .runtime
            .queue
            .enqueue(session.from.clone(), session.to.clone(), data)
        {
            Ok(id) => Ok(Response::ok(format!("queued as <{id}>"))),
            Err(err) => Err(Error::Response(Response::new(
                451,
                format!("Failed to spool message: {err}"),
                Some("4.3.0".into()),
            ))),
        }
    }
}

fn validate_pre_data(config: &AppConfig, kind: ServerKind, session: &Session) -> Option<Response> {
    if !session.authenticated {
        return Some(Response::reject("Authentication required"));
    }

    if kind == ServerKind::Port25 && !session.is_tls() && !config.tls.allow_insecure_auth {
        return Some(Response::reject(
            "Port 25 does not accept authenticated submission without TLS",
        ));
    }

    if !is_allowed_sender_domain(&config.senders.allowed_domains, &session.from) {
        return Some(Response::new(
            553,
            "Sender domain not allowed",
            Some("5.7.1".into()),
        ));
    }

    None
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
                    return Err(anyhow!("From header domain is not allowed"));
                }
            }
            MailAddr::Group(group) => {
                for info in &group.addrs {
                    if !is_allowed_sender_domain(allowed_domains, &info.addr) {
                        return Err(anyhow!("From header domain is not allowed"));
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_header_from_domains() {
        let raw = b"From: Sender <sender@example.com>\r\nTo: to@example.net\r\n\r\nHello";
        ensure_header_from_allowed(&["example.com".to_string()], raw).unwrap();
        assert!(ensure_header_from_allowed(&["other.com".to_string()], raw).is_err());
    }

    #[test]
    fn port_25_without_tls_and_without_override_is_rejected() {
        let config = AppConfig {
            hostname: "smtp.example.com".into(),
            appname: "smtp2cf".into(),
            max_message_size: 1024,
            max_recipients: 10,
            command_timeout_secs: 10,
            ports: crate::config::PortsConfig {
                smtp_25: "127.0.0.1:25".into(),
                submission_587: "127.0.0.1:587".into(),
                smtps_465: "127.0.0.1:465".into(),
            },
            auth: crate::config::AuthConfig {
                username: "relay@example.com".into(),
            },
            senders: crate::config::SenderConfig {
                allowed_domains: vec!["example.com".into()],
            },
            cloudflare: crate::config::CloudflareConfig {
                account_id: "abc".into(),
            },
            tls: crate::config::TlsSettings {
                cert_name: None,
                fullchain_path: None,
                privkey_path: None,
                allow_insecure_auth: false,
            },
            queue: crate::config::QueueConfig {
                spool_dir: "/tmp".into(),
                max_attempts: 3,
                backoff_initial_secs: 1,
                backoff_max_secs: 2,
                poll_interval_secs: 1,
            },
        };

        let smtp_config = build_server_config(&config, ServerKind::Port25, None).unwrap();
        assert!(smtp_config.auth_machs.is_empty());
    }
}
