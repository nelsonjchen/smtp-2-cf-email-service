use std::fs::File;
use std::io::BufReader;
use anyhow::{Context, Result, anyhow, bail};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::ResolvedTls;

pub fn load_server_config(resolved: &ResolvedTls) -> Result<ServerConfig> {
    let certs = load_certs(&resolved.fullchain_path)?;
    let key = load_private_key(&resolved.privkey_path)?;

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("failed to build rustls server config")
}

fn load_certs(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>> {
    let file =
        File::open(path).with_context(|| format!("failed to open TLS cert {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse TLS cert {}", path.display()))?;

    if certs.is_empty() {
        bail!("no certificates found in {}", path.display());
    }

    Ok(certs)
}

fn load_private_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>> {
    let file =
        File::open(path).with_context(|| format!("failed to open TLS key {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut key_iter = rustls_pemfile::pkcs8_private_keys(&mut reader);

    if let Some(key) = key_iter.next() {
        return key
            .map(Into::into)
            .with_context(|| format!("failed to parse PKCS8 key {}", path.display()));
    }

    let file =
        File::open(path).with_context(|| format!("failed to reopen TLS key {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut rsa_iter = rustls_pemfile::rsa_private_keys(&mut reader);
    if let Some(key) = rsa_iter.next() {
        return key
            .map(Into::into)
            .with_context(|| format!("failed to parse RSA key {}", path.display()));
    }

    Err(anyhow!(
        "no supported private key found in {}",
        path.display()
    ))
}
