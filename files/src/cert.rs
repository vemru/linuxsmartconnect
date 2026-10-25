//! The self-signed client certificate and the SHA-256 fingerprint the File Station handshake needs.
//!
//! The phone's NFS data socket uses mutual TLS with **fingerprint pinning**, not a CA chain: during
//! the `cmd=1001` handshake we send the phone the uppercase-hex SHA-256 of our certificate's DER,
//! and when we open the TLS connection we must present exactly that certificate (see
//! `docs/protocol/file-exchange.md`). So the cert is generated once and reused, and its fingerprint
//! is derived the same way the phone computes it (`StringUtils.getCertificateFp`).

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Our TLS identity: the certificate and private key (both DER) and the fingerprint to advertise.
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    /// Uppercase-hex SHA-256 of the certificate DER — what the phone pins (`fp` in the handshake).
    pub fingerprint: String,
}

impl Identity {
    /// Loads the cert+key from `dir`, generating and persisting a fresh one the first time.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
        let cert_path = dir.join("cert.der");
        let key_path = dir.join("key.der");
        let (cert_der, key_der) = if cert_path.exists() && key_path.exists() {
            (
                fs::read(&cert_path).with_context(|| format!("reading {}", cert_path.display()))?,
                fs::read(&key_path).with_context(|| format!("reading {}", key_path.display()))?,
            )
        } else {
            let (cert_der, key_der) = generate()?;
            write_private(&cert_path, &cert_der)?;
            write_private(&key_path, &key_der)?;
            (cert_der, key_der)
        };
        let fingerprint = fingerprint_of(&cert_der);
        Ok(Self { cert_der, key_der, fingerprint })
    }

    /// The default per-user location, next to the daemon's own state.
    pub fn default_dir() -> PathBuf {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
                home.join(".local/share")
            });
        base.join("smartconnect-linux").join("files-identity")
    }
}

/// Generates a fresh self-signed certificate and returns `(cert_der, pkcs8_key_der)`.
fn generate() -> Result<(Vec<u8>, Vec<u8>)> {
    let certified = rcgen::generate_simple_self_signed(vec!["smartconnect-linux".to_string()])
        .context("generating a self-signed certificate")?;
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.signing_key.serialize_der();
    Ok((cert_der, key_der))
}

/// SHA-256 of the certificate DER, uppercase hex, matching the phone's `getCertificateFp`.
pub fn fingerprint_of(cert_der: &[u8]) -> String {
    let digest = Sha256::digest(cert_der);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 700 {}", dir.display()))?;
    Ok(())
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes).with_context(|| format!("writing {}", path.display()))?;
    // Re-apply in case the file already existed with looser modes.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_uppercase_hex_sha256() {
        // SHA-256 of the empty input, uppercase.
        let fp = fingerprint_of(b"");
        assert_eq!(
            fp,
            "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"
        );
        assert_eq!(fp.len(), 64);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }
}
