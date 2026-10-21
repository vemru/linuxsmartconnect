//! The **PC→phone** NFS server: serves a local directory to the phone over fingerprint-pinned
//! mutual TLS (the mirror of [`crate::tls`]'s client).
//!
//! The phone is the NFS *client* here; the PC exports a folder read-only. One TLS listener handles
//! everything — MOUNT, NFSv3 and PORTMAP are all multiplexed on each connection by
//! [`nfs3_server`]'s core. We:
//!   1. build a [`rustls::ServerConfig`] that presents our self-signed cert ([`cert::Identity`]) and
//!      pins the phone's client cert by `SHA-256(DER)` fingerprint (never a CA chain), mirroring the
//!      client's [`crate::tls::FingerprintVerifier`] on the server side;
//!   2. accept TCP, complete the TLS handshake, and hand the stream to
//!      [`nfs3_server`]'s `process_socket`, driven by a [`LocalFs`] read-only VFS.
//!
//! The pinned fingerprint set is shared with [`crate::responder`]: it adds the phone's `fp` (taken
//! from the phone's `type:0` handshake) before replying `type:1`, so the phone's cert is already
//! trusted by the time it connects. See `docs/protocol/file-exchange.md`.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use nfs3_server::test_reexports::{RPCContext, process_socket};
use nfs3_server::vfs::adapters::ReadOnlyAdapter;

use crate::cert::{Identity, fingerprint_of};
use crate::localfs::LocalFs;
use crate::tls::verify_signature;

/// The set of phone client-cert fingerprints we currently accept (uppercase hex `SHA-256(DER)`).
/// Shared with the handshake responder, which inserts a phone's `fp` before sending the reply.
pub type PinnedFps = Arc<RwLock<HashSet<String>>>;

/// Creates an empty pinned-fingerprint set.
pub fn new_pinned() -> PinnedFps {
    Arc::new(RwLock::new(HashSet::new()))
}

/// Verifies the phone's **client** certificate by its SHA-256 fingerprint only (no CA chain, no
/// hostname) — the server-side counterpart of [`crate::tls::FingerprintVerifier`]. The handshake
/// signature is checked against the pinned cert's own public key (webpki rejects the phone's X.509
/// v1 cert), proving the peer holds the matching private key.
#[derive(Debug)]
struct PinnedClientVerifier {
    pinned: PinnedFps,
    provider: Arc<CryptoProvider>,
    no_hints: Vec<DistinguishedName>,
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.no_hints
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let got = fingerprint_of(end_entity.as_ref());
        let pinned = self.pinned.read().unwrap();
        if pinned.iter().any(|fp| fp.eq_ignore_ascii_case(&got)) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "phone client certificate fingerprint is not pinned (no matching handshake)".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_signature(cert, message, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_signature(cert, message, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Builds the rustls server config: present our cert, require and pin the phone's client cert.
fn server_config(id: &Identity, pinned: PinnedFps) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedClientVerifier {
        pinned,
        provider: provider.clone(),
        no_hints: Vec::new(),
    });
    let cert_chain = vec![CertificateDer::from(id.cert_der.clone())];
    let key = PrivatePkcs8KeyDer::from(id.key_der.clone());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("selecting TLS protocol versions")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, key.into())
        .context("installing our server certificate")?;
    Ok(Arc::new(config))
}

/// A bound, ready-to-run PC→phone NFS-over-TLS server.
pub struct NfsServer {
    acceptor: TlsAcceptor,
    vfs: Arc<ReadOnlyAdapter<LocalFs>>,
    export_label: String,
}

impl NfsServer {
    /// Builds the server for `share_dir`, exported under `export_label` (the `path` we advertise in
    /// the handshake, e.g. `/PhoneShare`). `pinned` is the shared set the responder adds phone fps to.
    pub fn new(
        id: &Identity,
        share_dir: std::path::PathBuf,
        export_label: String,
        pinned: PinnedFps,
    ) -> Result<Self> {
        let fs = LocalFs::new(share_dir).context("opening the shared directory")?;
        let vfs = Arc::new(ReadOnlyAdapter::new(fs));
        let config = server_config(id, pinned)?;
        Ok(Self { acceptor: TlsAcceptor::from(config), vfs, export_label })
    }

    /// Accepts connections forever, running one NFS session per TLS stream. Returns only on a fatal
    /// listener error.
    pub async fn serve(self, listener: TcpListener) -> Result<()> {
        // One base context (shared VFS + a single file-handle generation + transaction tracker);
        // cloned per connection so handles stay valid across the phone's connections.
        let base = RPCContext::test_ctx(&self.export_label, self.vfs.clone());
        // Our real root handle ([generation][root id]); the phone's all-zero root handle is mapped
        // onto it by `PhoneStream`.
        let root_fh: Arc<[u8]> = Arc::from(base.root_dir().data.as_ref());
        loop {
            let (tcp, peer) = listener.accept().await.context("accepting a TLS connection")?;
            let _ = tcp.set_nodelay(true);
            let acceptor = self.acceptor.clone();
            let ctx = base.clone();
            let root_fh = root_fh.clone();
            tokio::spawn(async move {
                let tls = match acceptor.accept(tcp).await {
                    Ok(s) => s,
                    Err(e) => {
                        // Usually a fingerprint that isn't pinned yet, or a stray probe.
                        debug!("TLS handshake from {peer} failed: {e}");
                        return;
                    }
                };
                info!("phone connected for file sharing from {peer}");
                let tls = PhoneStream::new(tls, root_fh, trace_enabled(), peer);
                if let Err(e) = process_socket(tls, ctx).await {
                    debug!("NFS session with {peer} ended: {e}");
                }
            });
        }
    }
}

/// Whether the opt-in wire trace is on (`SMARTCONNECT_FILES_TRACE=1`).
fn trace_enabled() -> bool {
    std::env::var_os("SMARTCONNECT_FILES_TRACE").is_some_and(|v| v != "0")
}

/// The decrypted stream from the phone, with one compatibility fix applied at the RPC record layer.
///
/// The phone (like Lenovo's own NFS servers) treats a **64-byte all-zero file handle** as the export
/// root and never calls MOUNT to learn ours — it goes straight to NFS `NULL`/`ACCESS`/`READDIRPLUS`
/// on the zero handle. `nfs3_server` handles are `[8-byte generation][id]`, so a zero handle would be
/// rejected as `NFS3ERR_STALE`. This adapter reassembles RPC record fragments and, for an NFS call
/// whose first argument is that zero handle, substitutes our real root handle (fixing the record
/// length). Writes pass through. With `SMARTCONNECT_FILES_TRACE=1` both directions are hex-dumped at
/// `debug` level.
struct PhoneStream<S> {
    inner: S,
    root_fh: Arc<[u8]>,
    /// Raw bytes from the phone not yet assembled into a complete record fragment.
    inbuf: Vec<u8>,
    /// Rewritten bytes ready for the NFS core, and how much of it has been handed out.
    outbuf: Vec<u8>,
    outpos: usize,
    trace: bool,
    peer: std::net::SocketAddr,
}

impl<S> PhoneStream<S> {
    fn new(inner: S, root_fh: Arc<[u8]>, trace: bool, peer: std::net::SocketAddr) -> Self {
        Self { inner, root_fh, inbuf: Vec::new(), outbuf: Vec::new(), outpos: 0, trace, peer }
    }
}

const NFS_PROGRAM: u32 = 100_003;
const ZERO_ROOT_LEN: usize = 64;

/// Length (marker included) of the first complete record fragment in `buf`, if one is buffered.
fn complete_fragment(buf: &[u8]) -> Option<usize> {
    let marker = u32::from_be_bytes(buf.get(..4)?.try_into().ok()?);
    let len = (marker & 0x7fff_ffff) as usize;
    (buf.len() >= 4 + len).then_some(4 + len)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn pad4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// If `frag` (marker + body) is a single-fragment NFS call whose first argument is the 64-byte
/// all-zero handle, returns the fragment with `root_fh` in its place; otherwise `None`.
fn rewrite_zero_root(frag: &[u8], root_fh: &[u8]) -> Option<(Vec<u8>, u32)> {
    let marker = be32(frag, 0)?;
    if marker & 0x8000_0000 == 0 {
        return None; // only single-fragment records (the phone's calls are small)
    }
    let body = &frag[4..];
    // xid, msg_type (0 = CALL), rpcvers, prog, vers, proc
    if be32(body, 4)? != 0 || be32(body, 12)? != NFS_PROGRAM {
        return None;
    }
    let proc_ = be32(body, 20)?;
    let cred_len = be32(body, 28)? as usize;
    let verf_at = 32 + pad4(cred_len);
    let verf_len = be32(body, verf_at + 4)? as usize;
    let args = verf_at + 8 + pad4(verf_len);
    if be32(body, args)? as usize != ZERO_ROOT_LEN {
        return None;
    }
    let fh = body.get(args + 4..args + 4 + ZERO_ROOT_LEN)?;
    if fh.iter().any(|&b| b != 0) {
        return None;
    }
    let mut new_body = Vec::with_capacity(body.len());
    new_body.extend_from_slice(&body[..args]);
    new_body.extend_from_slice(&(root_fh.len() as u32).to_be_bytes());
    new_body.extend_from_slice(root_fh);
    new_body.resize(new_body.len() + pad4(root_fh.len()) - root_fh.len(), 0);
    new_body.extend_from_slice(&body[args + 4 + ZERO_ROOT_LEN..]);
    let mut out = Vec::with_capacity(4 + new_body.len());
    out.extend_from_slice(&(0x8000_0000 | new_body.len() as u32).to_be_bytes());
    out.extend_from_slice(&new_body);
    Some((out, proc_))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for PhoneStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        let this = &mut *self;
        loop {
            // Hand out rewritten bytes first.
            if this.outpos < this.outbuf.len() {
                let n = (this.outbuf.len() - this.outpos).min(buf.remaining());
                buf.put_slice(&this.outbuf[this.outpos..this.outpos + n]);
                this.outpos += n;
                return Poll::Ready(Ok(()));
            }
            // Then assemble the next complete fragment, rewriting it if needed.
            if let Some(n) = complete_fragment(&this.inbuf) {
                let frag: Vec<u8> = this.inbuf.drain(..n).collect();
                if this.trace {
                    debug!("trace {} phone→PC {}B: {}", this.peer, frag.len(), hex(&frag[..frag.len().min(512)]));
                }
                this.outbuf = match rewrite_zero_root(&frag, &this.root_fh) {
                    Some((rewritten, proc_)) => {
                        debug!("mapped the phone's zero root handle onto ours (NFS proc {proc_})");
                        rewritten
                    }
                    None => frag,
                };
                this.outpos = 0;
                continue;
            }
            // Need more input from the phone.
            let mut tmp = [0u8; 16 * 1024];
            let mut rb = tokio::io::ReadBuf::new(&mut tmp);
            match std::pin::Pin::new(&mut this.inner).poll_read(cx, &mut rb) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {
                    let got = rb.filled();
                    if got.is_empty() {
                        // EOF: flush any partial fragment as-is, then report EOF.
                        if this.inbuf.is_empty() {
                            return Poll::Ready(Ok(()));
                        }
                        this.outbuf = std::mem::take(&mut this.inbuf);
                        this.outpos = 0;
                        continue;
                    }
                    this.inbuf.extend_from_slice(got);
                }
            }
        }
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for PhoneStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let res = std::pin::Pin::new(&mut self.inner).poll_write(cx, data);
        if self.trace {
            if let std::task::Poll::Ready(Ok(n)) = res {
                debug!("trace {} PC→phone {}B: {}", self.peer, n, hex(&data[..n.min(512)]));
            }
        }
        res
    }

    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Binds the NFS-over-TLS listener on all interfaces at `port` (0 = pick a free port) and returns it
/// together with the actual port.
pub async fn bind(port: u16) -> Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("binding the NFS-over-TLS listener on port {port}"))?;
    let actual = listener.local_addr().context("reading the listener address")?.port();
    if actual != port && port != 0 {
        warn!("requested port {port} unavailable; bound {actual} instead");
    }
    Ok((listener, actual))
}

#[cfg(test)]
mod zero_root_tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// The phone's real NFS ACCESS call on the 64-byte all-zero root handle (captured on a device,
    /// stamp anonymised), as one RPC record fragment.
    fn phone_access_on_zero_root() -> Vec<u8> {
        let mut body = String::new();
        body += "545ebfb7 00000000 00000002 000186a3 00000003 00000004"; // xid CALL v2 NFS v3 ACCESS
        body += "00000001 00000020 00000000 00000009 6c6f63616c686f7374000000"; // AUTH_UNIX "localhost"
        body += "00000000 00000000 00000000"; // uid gid gids
        body += "00000000 00000000"; // verf AUTH_NONE
        body += "00000040";
        body += &"00".repeat(64); // the all-zero root handle
        body += "00000001"; // access: READ
        let body = unhex(&body.replace(' ', ""));
        assert_eq!(body.len(), 0x90, "matches the captured record length");
        let mut frag = (0x8000_0000u32 | body.len() as u32).to_be_bytes().to_vec();
        frag.extend_from_slice(&body);
        frag
    }

    #[test]
    fn zero_root_handle_is_mapped_onto_ours() {
        let root: Vec<u8> = (1..=16).collect();
        let frag = phone_access_on_zero_root();
        assert_eq!(complete_fragment(&frag), Some(frag.len()));
        let (out, proc_) = rewrite_zero_root(&frag, &root).expect("should rewrite");
        assert_eq!(proc_, 4, "ACCESS");
        // 48 bytes shorter (64-byte handle → 16-byte handle), marker updated, last-fragment bit kept.
        assert_eq!(out.len(), frag.len() - 48);
        let marker = u32::from_be_bytes(out[..4].try_into().unwrap());
        assert_eq!(marker, 0x8000_0000 | (out.len() as u32 - 4));
        // The handle is now ours, and the trailing access mask survived.
        let args = out.len() - 4 - (4 + 16);
        assert_eq!(&out[args..args + 4], &16u32.to_be_bytes());
        assert_eq!(&out[args + 4..args + 20], root.as_slice());
        assert_eq!(&out[out.len() - 4..], &1u32.to_be_bytes());
    }

    #[test]
    fn other_records_pass_through() {
        let root: Vec<u8> = (1..=16).collect();
        // NFS NULL (no arguments) is not rewritten.
        let null = unhex("80000048545ebfb50000000000000002000186a300000003000000000000000100000020000000000000000a6c6f63616c686f737400000000000000000000000000000000");
        assert!(rewrite_zero_root(&null, &root).is_none());
        // A real (non-zero) handle is left alone.
        let mut frag = phone_access_on_zero_root();
        let at = frag.len() - 4 - 64;
        frag[at] = 7;
        assert!(rewrite_zero_root(&frag, &root).is_none());
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end loopback test: our NFS-over-TLS **server** ([`NfsServer`] + [`LocalFs`]) against
    //! our own NFS-over-TLS **client** ([`crate::tls::TlsNfsConnector`] + `nfs3_client`), with each
    //! side pinning the other by SHA-256 cert fingerprint — exactly the mutual-TLS + NFSv3 path the
    //! phone will drive, minus the daemon handshake relay (which is covered by `responder`'s tests).
    use std::path::PathBuf;

    use nfs3_client::Nfs3ConnectionBuilder;
    use nfs3_client::nfs3_types::nfs3::{Nfs3Result, READ3args, READDIRPLUS3args, diropargs3, filename3, LOOKUP3args};

    use crate::cert::Identity;
    use crate::tls::{TlsNfsConnector, client_config};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("scfiles-it-{}-{tag}-{:?}", std::process::id(), std::time::SystemTime::now()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn phone_style_client_mounts_reads_and_lists_over_tls() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        // A share with a file and a subdirectory.
        let share = tmp("share");
        std::fs::write(share.join("hello.txt"), b"hello from the PC").unwrap();
        std::fs::create_dir(share.join("docs")).unwrap();
        std::fs::write(share.join("docs").join("note.md"), b"# note").unwrap();

        // Server identity + the phone-side (client) identity, each pinned by the other.
        let server_id = Identity::load_or_create(&tmp("server-id")).unwrap();
        let client_id = Identity::load_or_create(&tmp("client-id")).unwrap();

        let pinned = super::new_pinned();
        pinned.write().unwrap().insert(client_id.fingerprint.clone());

        let (listener, port) = super::bind(0).await.unwrap();
        let srv = super::NfsServer::new(&server_id, share, "/PhoneShare".into(), pinned).unwrap();
        tokio::spawn(srv.serve(listener));

        // Client: present our client cert, pin the server's fp — the mirror of the phone.
        let config = client_config(&client_id, &server_id.fingerprint).unwrap();
        let connector = TlsNfsConnector::new(config);
        let mut conn = Nfs3ConnectionBuilder::new(connector, "127.0.0.1", "/PhoneShare")
            .mount_port(port)
            .nfs3_port(port)
            .connect_from_privileged_port(false)
            .mount()
            .await
            .expect("mount over mutual TLS");

        // List the root: we should see both top-level names.
        let root = conn.root_nfs_fh3();
        let args = READDIRPLUS3args { dir: root.clone(), cookie: 0, cookieverf: Default::default(), dircount: 8192, maxcount: 32768 };
        let names: Vec<String> = match conn.readdirplus(&args).await.unwrap() {
            Nfs3Result::Ok(ok) => ok
                .reply
                .entries
                .into_inner()
                .into_iter()
                .map(|e| String::from_utf8_lossy(e.name.0.as_ref()).into_owned())
                .collect(),
            Nfs3Result::Err((c, _)) => panic!("readdirplus failed: {c:?}"),
        };
        assert!(names.contains(&"hello.txt".to_string()), "got {names:?}");
        assert!(names.contains(&"docs".to_string()), "got {names:?}");

        // LOOKUP + READ the file, verifying the bytes round-trip over the wire.
        let look = LOOKUP3args { what: diropargs3 { dir: root, name: filename3::from(b"hello.txt".to_vec()) } };
        let fh = match conn.lookup(&look).await.unwrap() {
            Nfs3Result::Ok(ok) => ok.object,
            Nfs3Result::Err((c, _)) => panic!("lookup failed: {c:?}"),
        };
        let read = READ3args { file: fh, offset: 0, count: 4096 };
        let data = match conn.read(&read).await.unwrap() {
            Nfs3Result::Ok(ok) => ok.data.as_ref().to_vec(),
            Nfs3Result::Err((c, _)) => panic!("read failed: {c:?}"),
        };
        assert_eq!(&data, b"hello from the PC");

        conn.unmount().await.ok();
    }

    #[tokio::test]
    async fn unpinned_client_is_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let share = tmp("share2");
        std::fs::write(share.join("x"), b"x").unwrap();

        let server_id = Identity::load_or_create(&tmp("server-id2")).unwrap();
        let client_id = Identity::load_or_create(&tmp("client-id2")).unwrap();

        // Note: the client's fp is NOT inserted into the pinned set.
        let pinned = super::new_pinned();
        let (listener, port) = super::bind(0).await.unwrap();
        let srv = super::NfsServer::new(&server_id, share, "/PhoneShare".into(), pinned).unwrap();
        tokio::spawn(srv.serve(listener));

        let config = client_config(&client_id, &server_id.fingerprint).unwrap();
        let connector = TlsNfsConnector::new(config);
        let res = Nfs3ConnectionBuilder::new(connector, "127.0.0.1", "/PhoneShare")
            .mount_port(port)
            .nfs3_port(port)
            .connect_from_privileged_port(false)
            .mount()
            .await;
        assert!(res.is_err(), "an unpinned client certificate must be refused");
    }
}
