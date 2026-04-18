use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct PreparedConfig {
    pub config: AppConfig,
    pub secrets: Secrets,
    pub tls: Option<ResolvedTls>,
}

#[derive(Debug, Clone)]
pub struct Secrets {
    pub smtp_password: String,
    pub cloudflare_api_token: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedTls {
    pub fullchain_path: PathBuf,
    pub privkey_path: PathBuf,
    pub source_label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub hostname: String,
    #[serde(default = "default_appname")]
    pub appname: String,
    #[serde(default = "default_max_message_size")]
    pub max_message_size: usize,
    #[serde(default = "default_max_recipients")]
    pub max_recipients: usize,
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u64,
    pub ports: PortsConfig,
    pub auth: AuthConfig,
    pub senders: SenderConfig,
    pub cloudflare: CloudflareConfig,
    pub tls: TlsSettings,
    pub queue: QueueConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PortsConfig {
    pub smtp_25: String,
    pub submission_587: String,
    pub smtps_465: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub username: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SenderConfig {
    pub allowed_domains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudflareConfig {
    pub account_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsSettings {
    pub cert_name: Option<String>,
    pub fullchain_path: Option<PathBuf>,
    pub privkey_path: Option<PathBuf>,
    #[serde(default)]
    pub allow_insecure_auth: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueueConfig {
    pub spool_dir: PathBuf,
    pub max_attempts: u32,
    pub backoff_initial_secs: u64,
    pub backoff_max_secs: u64,
    pub poll_interval_secs: u64,
}

pub fn prepare_config(path: &Path) -> Result<PreparedConfig> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let mut config: AppConfig = toml::from_str(&content)
        .with_context(|| format!("failed to parse TOML in {}", path.display()))?;

    normalize_config(&mut config)?;
    validate_config(&config)?;
    ensure_queue_dirs(&config.queue.spool_dir)?;

    let secrets = load_secrets()?;
    let tls = resolve_tls(&config.tls)?;

    Ok(PreparedConfig {
        config,
        secrets,
        tls,
    })
}

fn normalize_config(config: &mut AppConfig) -> Result<()> {
    config.hostname = config.hostname.trim().to_string();
    config.appname = config.appname.trim().to_string();
    config.auth.username = config.auth.username.trim().to_string();
    config.cloudflare.account_id = config.cloudflare.account_id.trim().to_string();
    config.senders.allowed_domains = config
        .senders
        .allowed_domains
        .iter()
        .map(|domain| domain.trim().trim_start_matches('@').to_ascii_lowercase())
        .collect();

    Ok(())
}

fn validate_config(config: &AppConfig) -> Result<()> {
    if config.hostname.is_empty() {
        bail!("hostname must not be empty");
    }
    if config.appname.is_empty() {
        bail!("appname must not be empty");
    }
    if config.auth.username.is_empty() {
        bail!("auth.username must not be empty");
    }
    if config.cloudflare.account_id.is_empty() {
        bail!("cloudflare.account_id must not be empty");
    }
    if config.senders.allowed_domains.is_empty() {
        bail!("senders.allowed_domains must contain at least one domain");
    }
    if config.max_message_size == 0 {
        bail!("max_message_size must be greater than 0");
    }
    if config.max_recipients == 0 {
        bail!("max_recipients must be greater than 0");
    }
    if config.command_timeout_secs == 0 {
        bail!("command_timeout_secs must be greater than 0");
    }
    if config.queue.max_attempts == 0 {
        bail!("queue.max_attempts must be greater than 0");
    }
    if config.queue.backoff_initial_secs == 0 {
        bail!("queue.backoff_initial_secs must be greater than 0");
    }
    if config.queue.backoff_max_secs < config.queue.backoff_initial_secs {
        bail!("queue.backoff_max_secs must be >= queue.backoff_initial_secs");
    }
    if config.queue.poll_interval_secs == 0 {
        bail!("queue.poll_interval_secs must be greater than 0");
    }

    for bind in [
        &config.ports.smtp_25,
        &config.ports.submission_587,
        &config.ports.smtps_465,
    ] {
        if bind.trim().is_empty() {
            bail!("listener bind addresses must not be empty");
        }
    }

    match (&config.tls.fullchain_path, &config.tls.privkey_path) {
        (Some(_), None) | (None, Some(_)) => {
            bail!("tls.fullchain_path and tls.privkey_path must be set together")
        }
        _ => {}
    }

    Ok(())
}

fn load_secrets() -> Result<Secrets> {
    let smtp_password = env::var("SMTP2CF_SMTP_PASSWORD")
        .context("missing SMTP2CF_SMTP_PASSWORD environment variable")?;
    let cloudflare_api_token = env::var("SMTP2CF_CLOUDFLARE_API_TOKEN")
        .context("missing SMTP2CF_CLOUDFLARE_API_TOKEN environment variable")?;

    if smtp_password.is_empty() {
        bail!("SMTP2CF_SMTP_PASSWORD must not be empty");
    }
    if cloudflare_api_token.is_empty() {
        bail!("SMTP2CF_CLOUDFLARE_API_TOKEN must not be empty");
    }

    Ok(Secrets {
        smtp_password,
        cloudflare_api_token,
    })
}

fn ensure_queue_dirs(spool_dir: &Path) -> Result<()> {
    for dir in [
        spool_dir,
        &spool_dir.join("pending"),
        &spool_dir.join("dead"),
    ] {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create spool directory {}", dir.display()))?;
    }

    let probe_path = spool_dir.join(".write-test");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&probe_path)
        .with_context(|| format!("spool dir is not writable: {}", spool_dir.display()))?;
    file.write_all(b"ok")?;
    file.sync_all()?;
    fs::remove_file(probe_path)?;

    Ok(())
}

pub fn resolve_tls(tls: &TlsSettings) -> Result<Option<ResolvedTls>> {
    if let (Some(fullchain_path), Some(privkey_path)) = (&tls.fullchain_path, &tls.privkey_path) {
        if !fullchain_path.is_file() {
            bail!(
                "configured tls.fullchain_path does not exist: {}",
                fullchain_path.display()
            );
        }
        if !privkey_path.is_file() {
            bail!(
                "configured tls.privkey_path does not exist: {}",
                privkey_path.display()
            );
        }
        return Ok(Some(ResolvedTls {
            fullchain_path: fullchain_path.clone(),
            privkey_path: privkey_path.clone(),
            source_label: "explicit-paths".to_string(),
        }));
    }

    if let Some(cert_name) = tls.cert_name.as_deref() {
        let base = PathBuf::from("/etc/letsencrypt/live").join(cert_name);
        let fullchain_path = base.join("fullchain.pem");
        let privkey_path = base.join("privkey.pem");

        if fullchain_path.is_file() && privkey_path.is_file() {
            return Ok(Some(ResolvedTls {
                fullchain_path,
                privkey_path,
                source_label: format!("certbot:{cert_name}"),
            }));
        }
        return Ok(None);
    }

    Ok(None)
}

pub fn is_allowed_sender_domain(allowed_domains: &[String], address: &str) -> bool {
    let Some((_, domain)) = address.rsplit_once('@') else {
        return false;
    };
    let domain = domain.trim().trim_end_matches('>').to_ascii_lowercase();
    allowed_domains.iter().any(|allowed| allowed == &domain)
}

fn default_appname() -> String {
    "smtp2cf".to_string()
}

fn default_max_message_size() -> usize {
    25 * 1024 * 1024
}

fn default_max_recipients() -> usize {
    50
}

fn default_command_timeout_secs() -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn sender_domain_match_is_case_insensitive() {
        let allowed = vec!["example.com".to_string()];
        assert!(is_allowed_sender_domain(&allowed, "User@Example.com"));
        assert!(!is_allowed_sender_domain(&allowed, "user@other.com"));
    }

    #[test]
    fn resolve_tls_uses_explicit_paths() {
        let dir = tempdir().unwrap();
        let cert = dir.path().join("fullchain.pem");
        let key = dir.path().join("privkey.pem");
        fs::write(&cert, "cert").unwrap();
        fs::write(&key, "key").unwrap();

        let resolved = resolve_tls(&TlsSettings {
            cert_name: None,
            fullchain_path: Some(cert.clone()),
            privkey_path: Some(key.clone()),
            allow_insecure_auth: false,
        })
        .unwrap()
        .unwrap();

        assert_eq!(resolved.fullchain_path, cert);
        assert_eq!(resolved.privkey_path, key);
    }

    #[test]
    fn queue_dirs_are_created_and_writable() {
        let dir = tempdir().unwrap();
        let spool = dir.path().join("spool");
        ensure_queue_dirs(&spool).unwrap();
        assert!(spool.join("pending").is_dir());
        assert!(spool.join("dead").is_dir());
    }
}
