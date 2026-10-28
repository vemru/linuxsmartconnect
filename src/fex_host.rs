//! Serves our fex offers (clipboard images to the phone) on the daemon's own TLS port.
//!
//! When the PC sends files, it is the fex *server* (`smartconnect_fex::server`): the phone connects
//! to it, opens `GET /websocket` for the control exchange and fetches `GET /download?taskId=…`. A
//! separate port would need its own firewall rule, so these two routes share the control port
//! (52020 by default, already open for pairing): after the TLS handshake [`read_head`] reads the
//! HTTP request head, fex paths go to [`serve`], and everything else continues to the control
//! WebSocket through [`Prefixed`], which replays the bytes already read. The phone pins the
//! certificate by the fingerprint we advertise, which is this listener's own certificate.

use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, bail};
use smartconnect_fex::server::{self, Registry};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tracing::{info, warn};

/// Longest request head we read before routing.
const MAX_HEAD: usize = 16 * 1024;

/// Reads from `s` until the end of an HTTP request head (`\r\n\r\n`) or [`MAX_HEAD`] bytes. Returns
/// everything read, which may run past the head.
pub async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() >= MAX_HEAD {
            bail!("request head over {MAX_HEAD} bytes");
        }
        let n = s.read(&mut chunk).await.context("reading the request head")?;
        if n == 0 {
            bail!("connection closed before a request head");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

/// The request target (path and query) of an HTTP request head.
pub fn target(head: &[u8]) -> Option<&str> {
    let line = head.split(|&b| b == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split(' ');
    let _method = parts.next()?;
    parts.next()
}

/// Whether `target` is one of the fex routes.
pub fn is_fex(target: &str) -> bool {
    matches!(target.split('?').next(), Some("/websocket" | "/download"))
}

/// Whether `peer` is the phone's current session address. fex offers exist only for the connected
/// phone, so a different host — or no session at all — is refused. Addresses are compared
/// canonically so an IPv4-mapped IPv6 peer matches its IPv4 form.
pub fn peer_allowed(phone: Option<IpAddr>, peer: SocketAddr) -> bool {
    phone.is_some_and(|ip| ip.to_canonical() == peer.ip().to_canonical())
}

/// The 403 for a fex request that did not come from the connected phone.
fn forbidden() -> Vec<u8> {
    let body = "forbidden";
    format!("HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        .into_bytes()
}

/// Answers one fex request on `stream` (whose head was `head`), gated on `phone`, the connected
/// phone's address from the control session.
pub async fn serve<S>(registry: &Registry, stream: Prefixed<S>, target: &str, peer: SocketAddr, phone: Option<IpAddr>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = stream;
    if !peer_allowed(phone, peer) {
        warn!(%peer, "fex request from an address that is not the connected phone; refusing");
        stream.write_all(&forbidden()).await?;
        stream.shutdown().await.ok();
        return Ok(());
    }
    if target.starts_with("/download") {
        stream.write_all(&server::download_response(registry, target)).await?;
        stream.shutdown().await.ok();
        return Ok(());
    }
    let ws = tokio_tungstenite::accept_async(stream).await.context("fex WebSocket upgrade")?;
    match server::serve_websocket(ws, registry).await {
        Ok(true) => info!(%peer, "fex: the phone received our files"),
        Ok(false) => warn!(%peer, "fex: the phone reported the transfer as failed"),
        Err(e) => warn!(%peer, "fex: {e:#}"),
    }
    Ok(())
}

/// A stream that first yields `prefix` (bytes already read from `inner`), then reads `inner`.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self { prefix, pos: 0, inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.pos < this.prefix.len() {
            let n = (this.prefix.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smartconnect_fex::server::OutFile;

    fn png() -> OutFile {
        OutFile { name: "clipboard.png".into(), mime_type: "image/png".into(), data: b"\x89PNG\r\n\x1a\nfake".to_vec() }
    }

    #[test]
    fn routes_only_fex_paths() {
        let head = b"GET /download?taskId=abc&token=x HTTP/1.1\r\nHost: pc\r\n\r\n";
        assert_eq!(target(head), Some("/download?taskId=abc&token=x"));
        assert!(is_fex("/download?taskId=abc"));
        assert!(is_fex("/websocket"));
        assert!(!is_fex("/readyfor/"));
        assert!(!is_fex("/readyfor/mdns?x=1"));
        assert!(!is_fex("/websocketx"));
    }

    #[test]
    fn only_the_connected_phones_exact_address_is_allowed() {
        let phone: SocketAddr = "192.168.0.141:45000".parse().unwrap();
        assert!(peer_allowed(Some("192.168.0.141".parse().unwrap()), phone));
        // A different host in the exact same /24 subnet is refused:
        assert!(!peer_allowed(Some("192.168.0.142".parse().unwrap()), phone));
        assert!(!peer_allowed(Some("192.168.0.1".parse().unwrap()), phone));
        assert!(!peer_allowed(None, phone), "with no session there is no phone to serve");
    }

    #[tokio::test]
    async fn a_stranger_in_the_same_subnet_gets_no_websocket_and_no_offer_is_claimed() {
        let registry = Registry::default();
        let t = registry.offer(&[png()], "", "");
        // 192.168.0.142 is a neighbor in the same /24 as phone 192.168.0.141:
        let peer: SocketAddr = "192.168.0.142:5555".parse().unwrap();
        let (mut client, server) = tokio::io::duplex(4096);
        serve(&registry, Prefixed::new(Vec::new(), server), "/websocket", peer, Some("192.168.0.141".parse().unwrap())).await.unwrap();
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 403 Forbidden\r\n"), "{}", String::from_utf8_lossy(&out));
        assert!(registry.has_offers(), "the offer must stay unclaimed");
        assert!(!out.windows(t.zip.len()).any(|w| w == t.zip.as_slice()));
    }

    #[tokio::test]
    async fn a_stranger_in_the_same_subnet_cannot_download_but_the_phone_can() {
        let registry = Registry::default();
        let t = registry.offer(&[png()], "", "");
        let download = format!("/download?taskId={}&token={}", t.task_id, t.token);

        // 192.168.0.142 is a neighbor in the same /24 as phone 192.168.0.141:
        let peer: SocketAddr = "192.168.0.142:5555".parse().unwrap();
        let (mut client, server) = tokio::io::duplex(4096);
        serve(&registry, Prefixed::new(Vec::new(), server), &download, peer, Some("192.168.0.141".parse().unwrap())).await.unwrap();
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 403 Forbidden\r\n"), "{}", String::from_utf8_lossy(&out));
        assert!(!out.ends_with(&t.zip), "the zip must not reach a stranger in the same /24");

        // The phone itself at 192.168.0.141:
        let peer: SocketAddr = "192.168.0.141:45000".parse().unwrap();
        let (mut client, server) = tokio::io::duplex(4096);
        serve(&registry, Prefixed::new(Vec::new(), server), &download, peer, Some("192.168.0.141".parse().unwrap())).await.unwrap();
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        assert!(out.ends_with(&t.zip), "the phone still gets its download");
    }

    #[tokio::test]
    async fn prefixed_replays_what_was_read() {
        let (mut a, b) = tokio::io::duplex(1024);
        a.write_all(b"GET /readyfor/ HTTP/1.1\r\nHost: x\r\n\r\nrest").await.unwrap();
        drop(a);
        let mut b = b;
        let head = read_head(&mut b).await.unwrap();
        assert!(head.starts_with(b"GET /readyfor/"));
        let mut replay = Prefixed::new(head, b);
        let mut all = Vec::new();
        replay.read_to_end(&mut all).await.unwrap();
        assert_eq!(all, b"GET /readyfor/ HTTP/1.1\r\nHost: x\r\n\r\nrest");
    }
}
