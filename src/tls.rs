//! TLS support, with an automatic self-signed certificate fallback.
//!
//! YANuget serves HTTPS by default. When the operator supplies a certificate
//! and key they are used as-is; otherwise a self-signed certificate is
//! generated once and cached under the data directory so it is stable across
//! restarts. Self-signed certificates are convenient for getting started and
//! for internal networks, but clients (e.g. `dotnet`, `choco`) must be
//! configured to trust them — for public feeds, supply a real certificate.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The certificate and key file paths TLS will use.
#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Resolve the certificate/key paths, generating a self-signed pair when none
/// is configured and none has been cached yet.
///
/// * `configured`: explicit `(cert, key)` paths from configuration, if any.
/// * `data_dir`: where the self-signed pair is cached (`{data_dir}/tls`).
/// * `sans`: subject alternative names for a generated certificate.
pub async fn ensure_certificate(
    configured: Option<(PathBuf, PathBuf)>,
    data_dir: &Path,
    sans: &[String],
) -> Result<TlsPaths> {
    if let Some((cert, key)) = configured {
        if !tokio::fs::try_exists(&cert).await.unwrap_or(false) {
            return Err(Error::BadRequest(format!(
                "TLS certificate not found: {}",
                cert.display()
            )));
        }
        if !tokio::fs::try_exists(&key).await.unwrap_or(false) {
            return Err(Error::BadRequest(format!(
                "TLS key not found: {}",
                key.display()
            )));
        }
        return Ok(TlsPaths { cert, key });
    }

    // No certificate configured — use (and if needed create) the cached
    // self-signed pair.
    let dir = data_dir.join("tls");
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let have_both = tokio::fs::try_exists(&cert).await.unwrap_or(false)
        && tokio::fs::try_exists(&key).await.unwrap_or(false);
    if !have_both {
        tokio::fs::create_dir_all(&dir).await?;
        let (cert_pem, key_pem) = generate_self_signed(sans)?;
        tokio::fs::write(&cert, cert_pem).await?;
        // The private key is sensitive; restrict its permissions where possible.
        write_private(&key, key_pem.as_bytes()).await?;
        tracing::warn!(
            cert = %cert.display(),
            "no TLS certificate configured — generated a self-signed one (clients must trust it)"
        );
    }
    Ok(TlsPaths { cert, key })
}

/// Generate a self-signed certificate and return `(cert_pem, key_pem)`.
fn generate_self_signed(sans: &[String]) -> Result<(String, String)> {
    let mut names: Vec<String> = sans.to_vec();
    if names.is_empty() {
        names.push("localhost".to_string());
    }
    let key = rcgen::KeyPair::generate()
        .map_err(|e| Error::Other(anyhow::anyhow!("TLS key generation failed: {e}")))?;
    let cert = rcgen::CertificateParams::new(names)
        .map_err(|e| Error::Other(anyhow::anyhow!("invalid TLS certificate parameters: {e}")))?
        .self_signed(&key)
        .map_err(|e| Error::Other(anyhow::anyhow!("self-signing failed: {e}")))?;
    Ok((cert.pem(), key.serialize_pem()))
}

#[cfg(unix)]
async fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(())
}

#[cfg(not(unix))]
async fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generates_and_caches_self_signed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure_certificate(None, dir.path(), &["localhost".into()])
            .await
            .unwrap();
        assert!(tokio::fs::try_exists(&paths.cert).await.unwrap());
        assert!(tokio::fs::try_exists(&paths.key).await.unwrap());
        let cert_pem = tokio::fs::read_to_string(&paths.cert).await.unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));

        // A second call reuses the cached pair (same bytes).
        let again = ensure_certificate(None, dir.path(), &["localhost".into()])
            .await
            .unwrap();
        assert_eq!(again.cert, paths.cert);
        assert_eq!(
            tokio::fs::read_to_string(&again.cert).await.unwrap(),
            cert_pem
        );
    }

    #[tokio::test]
    async fn missing_configured_cert_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = ensure_certificate(
            Some((dir.path().join("nope.pem"), dir.path().join("no.key"))),
            dir.path(),
            &[],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
    }
}
