use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::cloudflare::{CloudflareClient, DeliveryDecision};
use crate::config::QueueConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEnvelope {
    pub id: String,
    pub envelope_from: String,
    pub recipients: Vec<String>,
    pub raw_mime_b64: String,
    pub attempt_count: u32,
    pub next_attempt_at: u64,
    pub last_error: Option<String>,
    pub created_at: u64,
}

impl QueueEnvelope {
    pub fn raw_mime(&self) -> Result<Vec<u8>> {
        STANDARD
            .decode(self.raw_mime_b64.as_bytes())
            .context("failed to decode spooled raw MIME message")
    }
}

#[derive(Clone)]
pub struct QueueStore {
    inner: Arc<QueueStoreInner>,
}

struct QueueStoreInner {
    config: QueueConfig,
    pending_dir: PathBuf,
    dead_dir: PathBuf,
    sequence: AtomicU64,
}

impl QueueStore {
    pub fn new(config: QueueConfig) -> Result<Self> {
        let pending_dir = config.spool_dir.join("pending");
        let dead_dir = config.spool_dir.join("dead");
        fs::create_dir_all(&pending_dir)?;
        fs::create_dir_all(&dead_dir)?;

        Ok(Self {
            inner: Arc::new(QueueStoreInner {
                config,
                pending_dir,
                dead_dir,
                sequence: AtomicU64::new(1),
            }),
        })
    }

    pub fn enqueue(
        &self,
        envelope_from: String,
        recipients: Vec<String>,
        raw_mime: Vec<u8>,
    ) -> Result<String> {
        let id = self.next_id();
        let now = unix_now();
        let envelope = QueueEnvelope {
            id: id.clone(),
            envelope_from,
            recipients,
            raw_mime_b64: STANDARD.encode(raw_mime),
            attempt_count: 0,
            next_attempt_at: now,
            last_error: None,
            created_at: now,
        };

        let tmp_path = self.inner.pending_dir.join(format!("{id}.tmp"));
        let final_path = self.pending_path(&id);
        let bytes = serde_json::to_vec_pretty(&envelope)?;

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .with_context(|| format!("failed to open temp spool file {}", tmp_path.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp_path, &final_path)?;
        sync_dir(&self.inner.pending_dir)?;

        Ok(id)
    }

    pub fn spawn_delivery_worker(&self, client: CloudflareClient) {
        let queue = self.clone();
        tokio::spawn(async move {
            loop {
                match queue.try_process_one(&client).await {
                    Ok(true) => {}
                    Ok(false) => {
                        sleep(Duration::from_secs(queue.inner.config.poll_interval_secs)).await
                    }
                    Err(err) => {
                        error!(error = %err, "delivery worker iteration failed");
                        sleep(Duration::from_secs(queue.inner.config.poll_interval_secs)).await;
                    }
                }
            }
        });
    }

    async fn try_process_one(&self, client: &CloudflareClient) -> Result<bool> {
        let Some((path, envelope)) = self.next_due_envelope()? else {
            return Ok(false);
        };

        match client.deliver(&envelope).await {
            Ok(DeliveryDecision::Delivered) => {
                fs::remove_file(&path)?;
                info!(message_id = %envelope.id, "message delivered to Cloudflare");
            }
            Ok(DeliveryDecision::Retry(reason)) => {
                self.mark_retry(&path, envelope, &reason)?;
                warn!(reason = %reason, "transient Cloudflare failure; message will be retried");
            }
            Ok(DeliveryDecision::Dead(reason)) => {
                self.move_to_dead(&path, envelope, &reason)?;
                warn!(reason = %reason, "message moved to dead-letter queue");
            }
            Err(err) => {
                self.mark_retry(&path, envelope, &err.to_string())?;
                warn!(error = %err, "unexpected delivery error; message will be retried");
            }
        }

        Ok(true)
    }

    fn next_due_envelope(&self) -> Result<Option<(PathBuf, QueueEnvelope)>> {
        let now = unix_now();
        let mut best: Option<(PathBuf, QueueEnvelope)> = None;

        for entry in fs::read_dir(&self.inner.pending_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }

            let bytes = fs::read(&path)?;
            let envelope: QueueEnvelope = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse spool file {}", path.display()))?;

            if envelope.next_attempt_at > now {
                continue;
            }

            let replace = best
                .as_ref()
                .map(|(_, current)| envelope.next_attempt_at < current.next_attempt_at)
                .unwrap_or(true);

            if replace {
                best = Some((path, envelope));
            }
        }

        Ok(best)
    }

    fn mark_retry(&self, path: &Path, mut envelope: QueueEnvelope, reason: &str) -> Result<()> {
        envelope.attempt_count += 1;
        envelope.last_error = Some(reason.to_string());

        if envelope.attempt_count >= self.inner.config.max_attempts {
            return self.move_to_dead(path, envelope, reason);
        }

        envelope.next_attempt_at = unix_now() + self.backoff_seconds(envelope.attempt_count);
        self.rewrite_pending(path, &envelope)
    }

    fn move_to_dead(&self, path: &Path, mut envelope: QueueEnvelope, reason: &str) -> Result<()> {
        envelope.last_error = Some(reason.to_string());
        let dead_path = self.inner.dead_dir.join(
            path.file_name()
                .ok_or_else(|| anyhow!("invalid spool filename"))?,
        );
        fs::write(&dead_path, serde_json::to_vec_pretty(&envelope)?)?;
        sync_file(&dead_path)?;
        fs::remove_file(path)?;
        sync_dir(&self.inner.dead_dir)?;
        Ok(())
    }

    fn rewrite_pending(&self, path: &Path, envelope: &QueueEnvelope) -> Result<()> {
        let tmp_path = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(envelope)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp_path, path)?;
        sync_dir(&self.inner.pending_dir)?;
        Ok(())
    }

    fn backoff_seconds(&self, attempt_count: u32) -> u64 {
        let exponent = attempt_count.saturating_sub(1).min(16);
        let factor = 1u64 << exponent;
        (self
            .inner
            .config
            .backoff_initial_secs
            .saturating_mul(factor))
        .min(self.inner.config.backoff_max_secs)
    }

    fn pending_path(&self, id: &str) -> PathBuf {
        self.inner.pending_dir.join(format!("{id}.json"))
    }

    fn next_id(&self) -> String {
        let sequence = self.inner.sequence.fetch_add(1, Ordering::Relaxed);
        format!("{}-{}-{}", unix_now(), std::process::id(), sequence)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sync_dir(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    file.sync_all()?;
    Ok(())
}

fn sync_file(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn queue_config(base: &Path) -> QueueConfig {
        QueueConfig {
            spool_dir: base.join("spool"),
            max_attempts: 3,
            backoff_initial_secs: 5,
            backoff_max_secs: 30,
            poll_interval_secs: 1,
        }
    }

    #[test]
    fn enqueue_creates_pending_file() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(queue_config(dir.path())).unwrap();

        let id = queue
            .enqueue(
                "sender@example.com".into(),
                vec!["to@example.net".into()],
                b"Subject: test\r\n\r\nHello".to_vec(),
            )
            .unwrap();

        assert!(queue.pending_path(&id).is_file());
    }

    #[test]
    fn retry_eventually_moves_to_dead() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(queue_config(dir.path())).unwrap();

        let id = queue
            .enqueue(
                "sender@example.com".into(),
                vec!["to@example.net".into()],
                b"Subject: test\r\n\r\nHello".to_vec(),
            )
            .unwrap();

        let path = queue.pending_path(&id);
        let bytes = fs::read(&path).unwrap();
        let envelope: QueueEnvelope = serde_json::from_slice(&bytes).unwrap();

        queue
            .mark_retry(&path, envelope.clone(), "temporary")
            .unwrap();
        let bytes = fs::read(&path).unwrap();
        let envelope: QueueEnvelope = serde_json::from_slice(&bytes).unwrap();
        queue
            .mark_retry(&path, envelope.clone(), "temporary")
            .unwrap();
        let bytes = fs::read(&path).unwrap();
        let envelope: QueueEnvelope = serde_json::from_slice(&bytes).unwrap();
        queue.mark_retry(&path, envelope, "temporary").unwrap();

        assert!(!path.exists());
        assert!(queue.inner.dead_dir.join(format!("{id}.json")).exists());
    }
}
