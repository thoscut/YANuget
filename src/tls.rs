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

/// Subject alternative names for a generated certificate: `localhost` plus the
/// host of the configured base URL, if any.
pub fn certificate_sans(base_url: Option<&str>) -> Vec<String> {
    match base_url.and_then(host_of) {
        Some(host) if host != "localhost" => vec![host, "localhost".to_string()],
        _ => vec!["localhost".to_string()],
    }
}

/// Extract the host portion of a base URL (dropping scheme, path and port).
pub fn host_of(base_url: &str) -> Option<String> {
    let after_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    let host = after_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    (!host.is_empty()).then(|| host.to_string())
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

/// What [`check_pair`] found in a usable certificate/key pair.
#[derive(Debug, Default)]
pub struct PairCheck {
    /// Certificates in the chain file (the server's own first).
    pub chain_len: usize,
    /// The names asked about that the certificate does not cover. A client
    /// that reaches the server by one of them refuses the connection.
    pub uncovered: Vec<String>,
}

/// Check, without starting a server, that `cert` and `key` are a pair TLS
/// can serve: both parse, the key is one rustls can sign with, and it belongs
/// to the first certificate of the chain. Each of `hosts` (a DNS name or an
/// IP address) is checked against the certificate's subject alternative
/// names, as a client would.
///
/// The error is a sentence for the operator.
pub fn check_pair(
    cert: &Path,
    key: &Path,
    hosts: &[String],
) -> std::result::Result<PairCheck, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .and_then(|certs| certs.collect())
        .map_err(|e| format!("TLS certificate {}: {e}", cert.display()))?;
    let Some(leaf) = chain.first().cloned() else {
        return Err(format!(
            "TLS certificate {} holds no certificate",
            cert.display()
        ));
    };
    let private =
        PrivateKeyDer::from_pem_file(key).map_err(|e| format!("TLS key {}: {e}", key.display()))?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&private)
        .map_err(|e| format!("TLS key {}: {e}", key.display()))?;
    let chain_len = chain.len();
    let certified = rustls::sign::CertifiedKey::new(chain, signing);
    match certified.keys_match() {
        Ok(()) => {}
        Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch)) => {
            return Err(format!(
                "TLS key {} does not belong to the certificate {} (the first certificate \
                 in the file must be the server's own)",
                key.display(),
                cert.display()
            ));
        }
        // rustls cannot tell for this key type; the handshake will.
        Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::Unknown)) => {}
        Err(e) => return Err(format!("TLS certificate {}: {e}", cert.display())),
    }

    let parsed = rustls::server::ParsedCertificate::try_from(&leaf)
        .map_err(|e| format!("TLS certificate {}: {e}", cert.display()))?;
    let uncovered = hosts
        .iter()
        .filter(|host| {
            let name = host.trim_start_matches('[').trim_end_matches(']');
            match ServerName::try_from(name) {
                Ok(name) => rustls::client::verify_server_name(&parsed, &name).is_err(),
                Err(_) => true,
            }
        })
        .cloned()
        .collect();
    Ok(PairCheck {
        chain_len,
        uncovered,
    })
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

    #[test]
    fn host_of_strips_scheme_path_and_port() {
        assert_eq!(
            host_of("https://nuget.example.com").as_deref(),
            Some("nuget.example.com")
        );
        assert_eq!(
            host_of("https://nuget.example.com:8443/feed").as_deref(),
            Some("nuget.example.com")
        );
        assert_eq!(host_of("host-only").as_deref(), Some("host-only"));
        assert_eq!(host_of("https://"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn certificate_sans_include_localhost_and_host() {
        assert_eq!(certificate_sans(None), vec!["localhost".to_string()]);
        assert_eq!(
            certificate_sans(Some("https://feed.example.com")),
            vec!["feed.example.com".to_string(), "localhost".to_string()]
        );
        // A localhost base URL is not duplicated.
        assert_eq!(
            certificate_sans(Some("http://localhost:5000")),
            vec!["localhost".to_string()]
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

    fn write_pair(dir: &Path, name: &str, sans: &[&str]) -> (PathBuf, PathBuf) {
        let sans: Vec<String> = sans.iter().map(|s| s.to_string()).collect();
        let (cert, key) = generate_self_signed(&sans).unwrap();
        let cert_path = dir.join(format!("{name}.crt"));
        let key_path = dir.join(format!("{name}.key"));
        std::fs::write(&cert_path, cert).unwrap();
        std::fs::write(&key_path, key).unwrap();
        (cert_path, key_path)
    }

    #[test]
    fn a_matching_pair_passes_and_names_are_checked_like_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_pair(dir.path(), "a", &["nuget.example.com", "nuget", "10.0.0.5"]);
        let hosts: Vec<String> = [
            "nuget.example.com",
            "nuget",
            "10.0.0.5",
            "other.example.com",
            "10.0.0.6",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let check = check_pair(&cert, &key, &hosts).unwrap();
        assert_eq!(check.chain_len, 1);
        assert_eq!(check.uncovered, vec!["other.example.com", "10.0.0.6"]);
    }

    #[test]
    fn a_key_from_another_pair_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = write_pair(dir.path(), "a", &["localhost"]);
        let (_, other_key) = write_pair(dir.path(), "b", &["localhost"]);
        let err = check_pair(&cert, &other_key, &[]).unwrap_err();
        assert!(err.contains("does not belong to the certificate"), "{err}");
    }

    #[test]
    fn files_that_are_not_pem_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_pair(dir.path(), "a", &["localhost"]);
        let junk = dir.path().join("junk");
        std::fs::write(&junk, "not a certificate").unwrap();
        assert!(check_pair(&junk, &key, &[])
            .unwrap_err()
            .contains("holds no certificate"));
        assert!(check_pair(&cert, &junk, &[]).is_err());
        assert!(check_pair(&dir.path().join("missing"), &key, &[]).is_err());
    }
}
