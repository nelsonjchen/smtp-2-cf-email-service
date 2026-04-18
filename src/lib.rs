pub mod cloudflare;
pub mod config;
pub mod queue;
pub mod smtp_server;
pub mod tls;

use std::path::Path;

use anyhow::Result;
use tracing::info;

use crate::cloudflare::CloudflareClient;
use crate::config::{PreparedConfig, prepare_config};
use crate::queue::QueueStore;

pub async fn run_check_config(path: &Path) -> Result<()> {
    let prepared = prepare_config(path)?;
    print_config_summary(&prepared);
    Ok(())
}

pub async fn run_serve(path: &Path) -> Result<()> {
    let prepared = prepare_config(path)?;
    print_config_summary(&prepared);

    let queue = QueueStore::new(prepared.config.queue.clone())?;
    let client = CloudflareClient::new(
        prepared.config.cloudflare.account_id.clone(),
        prepared.secrets.cloudflare_api_token.clone(),
    )?;

    queue.spawn_delivery_worker(client);
    smtp_server::run_servers(prepared, queue).await
}

fn print_config_summary(prepared: &PreparedConfig) {
    let tls_mode = if let Some(tls) = &prepared.tls {
        format!(
            "TLS enabled via cert={} cert_path={} key_path={}",
            tls.source_label,
            tls.fullchain_path.display(),
            tls.privkey_path.display()
        )
    } else {
        "TLS not found; only degraded plaintext port 25 will be usable unless allow_insecure_auth=true"
            .to_string()
    };

    info!(
        hostname = %prepared.config.hostname,
        smtp_25 = %prepared.config.ports.smtp_25,
        submission_587 = %prepared.config.ports.submission_587,
        smtps_465 = %prepared.config.ports.smtps_465,
        spool_dir = %prepared.config.queue.spool_dir.display(),
        allowed_domains = ?prepared.config.senders.allowed_domains,
        "{tls_mode}"
    );
}
