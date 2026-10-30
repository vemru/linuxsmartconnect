//! Connects to the phone's NFSv3 server over mutual TLS and lists a directory.
//!
//! This is the core the FUSE layer will sit on. The ports and the phone's fingerprint come from the
//! `cmd=1001` File Station handshake (done by the daemon); here they are inputs so the path can be
//! driven directly for testing against a phone.

use anyhow::{Context, Result};
use nfs3_client::Nfs3Connection;
use nfs3_client::Nfs3ConnectionBuilder;
use nfs3_client::tokio::TokioIo;
use nfs3_types::nfs3::{Nfs3Result, READDIRPLUS3args};
use tokio::net::TcpStream;

use crate::cert::Identity;
use crate::tls::{TlsNfsConnector, client_config};

/// The concrete NFS-over-TLS connection type (NFSv3 carried over our pinned-mutual-TLS stream).
pub type NfsConn = Nfs3Connection<TokioIo<tokio_rustls::client::TlsStream<TcpStream>>>;

/// Where the phone's NFS server is, as returned by the handshake.
#[derive(Clone)]
pub struct MountTarget {
    pub ip: String,
    pub mount_port: u16,
    pub nfs_port: u16,
    /// The phone server certificate fingerprint to pin.
    pub fingerprint: String,
    /// Export path, e.g. `/sdcard`.
    pub path: String,
}

/// Opens the fingerprint-pinned mutual-TLS connection and mounts the phone's NFS export.
pub async fn connect(id: &Identity, t: &MountTarget) -> Result<NfsConn> {
    // Make sure a process-default crypto provider exists (some rustls/tokio-rustls paths look it up).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = client_config(id, &t.fingerprint)?;
    let connector = TlsNfsConnector::new(config);
    Nfs3ConnectionBuilder::new(connector, &t.ip, &t.path)
        .mount_port(t.mount_port)
        .nfs3_port(t.nfs_port)
        .connect_from_privileged_port(false)
        .mount()
        .await
        .context("mounting the phone's NFS export over TLS")
}

/// Mounts the phone's export over TLS and prints the names in its root directory.
pub async fn list_root(id: &Identity, t: &MountTarget) -> Result<()> {
    let mut conn = connect(id, t).await?;
    let root = conn.root_nfs_fh3();
    let args = READDIRPLUS3args {
        dir: root,
        cookie: 0,
        cookieverf: Default::default(),
        dircount: 8192,
        maxcount: 32768,
    };
    match conn
        .readdirplus(&args)
        .await
        .context("READDIRPLUS on the root")?
    {
        Nfs3Result::Ok(ok) => {
            for entry in ok.reply.entries.into_inner() {
                println!("{}", String::from_utf8_lossy(entry.name.0.as_ref()));
            }
        }
        Nfs3Result::Err((code, _)) => {
            anyhow::bail!("the phone returned an NFS error for READDIRPLUS: {code:?}");
        }
    }

    conn.unmount().await.ok();
    Ok(())
}
