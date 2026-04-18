use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result};
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::config::ResolvedTls;
use crate::tls::load_server_config;

const CERT_POLL_INTERVAL_SECS: u64 = 30;

pub fn spawn_cert_reload_watcher(tls: ResolvedTls) {
    tokio::spawn(async move {
        if let Err(err) = watch_cert_files(tls).await {
            error!(error = %err, "certificate reload watcher stopped");
        }
    });
}

async fn watch_cert_files(tls: ResolvedTls) -> Result<()> {
    let mut current = CertSnapshot::read(&tls.fullchain_path, &tls.privkey_path)?;
    let mut ticker = interval(Duration::from_secs(CERT_POLL_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let next = match CertSnapshot::read(&tls.fullchain_path, &tls.privkey_path) {
            Ok(next) => next,
            Err(err) => {
                warn!(
                    error = %err,
                    cert_path = %tls.fullchain_path.display(),
                    key_path = %tls.privkey_path.display(),
                    "certificate watcher could not read TLS files; keeping current process"
                );
                continue;
            }
        };

        if next == current {
            continue;
        }

        match load_server_config(&tls) {
            Ok(_) => {
                info!(
                    cert_path = %tls.fullchain_path.display(),
                    key_path = %tls.privkey_path.display(),
                    "TLS files changed; restarting process to reload certificate"
                );
                restart_current_process()?;
            }
            Err(err) => {
                warn!(
                    error = %err,
                    cert_path = %tls.fullchain_path.display(),
                    key_path = %tls.privkey_path.display(),
                    "TLS files changed but the new certificate pair is not valid yet"
                );
                continue;
            }
        }

        current = next;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CertSnapshot {
    cert_modified: SystemTime,
    cert_len: u64,
    key_modified: SystemTime,
    key_len: u64,
}

impl CertSnapshot {
    fn read(cert_path: &Path, key_path: &Path) -> Result<Self> {
        let cert = fs::metadata(cert_path)
            .with_context(|| format!("failed to stat certificate {}", cert_path.display()))?;
        let key = fs::metadata(key_path)
            .with_context(|| format!("failed to stat private key {}", key_path.display()))?;

        Ok(Self {
            cert_modified: cert
                .modified()
                .with_context(|| format!("failed to read mtime for {}", cert_path.display()))?,
            cert_len: cert.len(),
            key_modified: key
                .modified()
                .with_context(|| format!("failed to read mtime for {}", key_path.display()))?,
            key_len: key.len(),
        })
    }
}

fn restart_current_process() -> Result<()> {
    let current_exe = env::current_exe().context("failed to determine current executable path")?;
    let args: Vec<_> = env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        let err = Command::new(&current_exe).args(&args).exec();
        Err(anyhow::anyhow!(
            "failed to exec {}: {err}",
            current_exe.display()
        ))
    }

    #[cfg(not(unix))]
    {
        Command::new(&current_exe)
            .args(&args)
            .spawn()
            .with_context(|| format!("failed to spawn {}", current_exe.display()))?;
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use tempfile::tempdir;

    #[test]
    fn snapshot_changes_when_files_change() {
        let dir = tempdir().unwrap();
        let cert = dir.path().join("fullchain.pem");
        let key = dir.path().join("privkey.pem");

        fs::write(&cert, "cert-v1").unwrap();
        fs::write(&key, "key-v1").unwrap();
        let first = CertSnapshot::read(&cert, &key).unwrap();

        sleep(Duration::from_millis(1100));
        fs::write(&cert, "cert-v2").unwrap();
        let second = CertSnapshot::read(&cert, &key).unwrap();

        assert_ne!(first, second);
    }
}
