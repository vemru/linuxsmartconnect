//! One phone connection to the PC→phone share, routed at the RPC record layer.
//!
//! `nfs3_server` answers the standard NFSv3 procedures but rejects anything else (`PROC_UNAVAIL`)
//! before our code runs. Smart Connect's Files tab also calls three of Lenovo's own procedures:
//! 22 `READTHUMBNAIL` (image previews), 23 `READCATEGORYDIR` (*All files*, *Recents*, …) and 27
//! `REMOVEBATCH` (delete). So instead of handing the TLS stream to `nfs3_server` directly:
//!
//! - a **reader** assembles the phone's RPC records, swaps the phone's all-zero root handle for
//!   ours ([`crate::server::rewrite_zero_root`]), answers the Lenovo procedures itself (each on a
//!   blocking worker, since tree walks and image decoding take time; replies match by xid, so their
//!   order doesn't matter) and forwards every other call to `nfs3_server` over an in-memory pipe;
//! - a **pump** copies `nfs3_server`'s reply records off that pipe;
//! - a **writer** owns the TLS write half and writes whole records only, so our replies and
//!   `nfs3_server`'s never interleave mid-record.
//!
//! ```text
//!  phone ──TLS──▶ reader ──calls──▶ duplex ──▶ nfs3_server
//!    ▲              │ procs 22/23/27               │
//!    └── writer ◀───┴──────────── pump ◀───────────┘
//! ```
//!
//! With `SMARTCONNECT_FILES_TRACE=1` both directions are hex-dumped at `debug` level.

use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::debug;

use crate::localfs::{CategoryEntry, LocalFs};
use crate::server::{be32, hex, pad4, rewrite_zero_root, NFS_PROGRAM, ZERO_ROOT_LEN};

/// Lenovo's NFS procedure numbers (the phone's own server has them as procedures 22–29).
const PROC_READTHUMBNAIL: u32 = 22;
const PROC_READCATEGORYDIR: u32 = 23;
const PROC_REMOVEBATCH: u32 = 27;
/// `READTHUMBNAIL` status when there is no thumbnail (the phone's own server's value).
const THUMB_NONE: u32 = 100;
/// The phone's own server caps one reply at this many entries.
const MAX_ENTRIES: u32 = 5000;
/// Largest RPC record we accept from the phone (its calls are small).
const MAX_RECORD: usize = 1 << 20;

/// Everything one connection needs besides the stream.
#[derive(Clone)]
pub struct Session {
    pub fs: LocalFs,
    /// Our export label (e.g. `/alice`), which the phone may prefix to paths in batch calls.
    pub label: String,
    /// `REMOVEBATCH` moves files to the Trash; without this it is refused.
    pub allow_delete: bool,
    /// Our real root handle (`[generation][root]`, 64 bytes).
    pub root_fh: Arc<[u8]>,
    pub trace: bool,
    pub peer: SocketAddr,
}

/// Runs one phone connection until it closes. `nfs` is the stream end `nfs3_server` serves.
pub async fn run<S, N>(tls: S, nfs: N, s: Session) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    N: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut tls_r, tls_w) = tokio::io::split(tls);
    let (nfs_r, mut nfs_w) = tokio::io::split(nfs);
    let (tx, rx) = mpsc::channel::<Vec<u8>>(64);

    let writer = tokio::spawn(write_records(tls_w, rx, s.trace, s.peer));
    let pump = tokio::spawn(pump_records(nfs_r, tx.clone()));

    let result = async {
        while let Some(frag) = read_record(&mut tls_r).await? {
            if s.trace {
                debug!("trace {} phone→PC {}B: {}", s.peer, frag.len(), hex(&frag[..frag.len().min(512)]));
            }
            let frag = match rewrite_zero_root(&frag, &s.root_fh) {
                Some((rewritten, proc_)) => {
                    debug!("mapped the phone's zero root handle onto ours (NFS proc {proc_})");
                    rewritten
                }
                None => frag,
            };
            if let Some(call) = parse_lenovo_call(&frag) {
                let (s, tx) = (s.clone(), tx.clone());
                tokio::spawn(async move {
                    if let Ok(reply) = tokio::task::spawn_blocking(move || answer(call, &s)).await {
                        let _ = tx.send(reply).await;
                    }
                });
                continue;
            }
            nfs_w.write_all(&frag).await?;
        }
        Ok(())
    }
    .await;

    // Phone gone: close the NFS core's input so it ends, then let the pump and writer drain.
    let _ = nfs_w.shutdown().await;
    drop(tx);
    let _ = pump.await;
    let _ = writer.await;
    result
}

/// Reads one RPC record fragment (marker included), or `None` at a clean end of stream.
async fn read_record<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut marker = [0u8; 4];
    match r.read_exact(&mut marker).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = (u32::from_be_bytes(marker) & 0x7fff_ffff) as usize;
    if len > MAX_RECORD {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("RPC record of {len} bytes")));
    }
    let mut frag = Vec::with_capacity(4 + len);
    frag.extend_from_slice(&marker);
    frag.resize(4 + len, 0);
    r.read_exact(&mut frag[4..]).await?;
    Ok(Some(frag))
}

/// Copies `nfs3_server`'s reply records to the writer.
async fn pump_records<R: AsyncRead + Unpin>(mut r: R, tx: mpsc::Sender<Vec<u8>>) {
    while let Ok(Some(frag)) = read_record(&mut r).await {
        if tx.send(frag).await.is_err() {
            break;
        }
    }
}

/// Writes whole records to the phone, in the order they were queued.
async fn write_records<W: AsyncWrite + Unpin>(mut w: W, mut rx: mpsc::Receiver<Vec<u8>>, trace: bool, peer: SocketAddr) {
    while let Some(frag) = rx.recv().await {
        if trace {
            debug!("trace {peer} PC→phone {}B: {}", frag.len(), hex(&frag[..frag.len().min(512)]));
        }
        if w.write_all(&frag).await.is_err() || w.flush().await.is_err() {
            break;
        }
    }
    let _ = w.shutdown().await;
}

/// A Lenovo-specific call the router answers itself.
#[derive(Debug, PartialEq)]
enum LenovoCall {
    /// `READCATEGORYDIR`: a directory handle (our 64-byte form, after the root rewrite) and paging.
    Category { xid: u32, fh: Vec<u8>, offset: u32, limit: u32, category: u32 },
    /// `READTHUMBNAIL`: an image path (share-relative or prefixed, see [`LocalFs::resolve_phone_path`]).
    Thumbnail { xid: u32, path: String },
    /// `REMOVEBATCH`: a JSON array of paths.
    RemoveBatch { xid: u32, paths_json: String },
}

/// Reads an XDR opaque (length, bytes, padding) at `at`; returns it and the offset after it.
fn opaque(body: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let len = be32(body, at)? as usize;
    Some((body.get(at + 4..at + 4 + len)?, at + 4 + pad4(len)))
}

/// If `frag` is an NFSv3 call to one of Lenovo's procedures we answer: its arguments.
fn parse_lenovo_call(frag: &[u8]) -> Option<LenovoCall> {
    let body = frag.get(4..)?;
    // xid, msg_type (0 = CALL), rpcvers, prog, vers, proc
    if be32(body, 4)? != 0 || be32(body, 12)? != NFS_PROGRAM || be32(body, 16)? != 3 {
        return None;
    }
    let proc_ = be32(body, 20)?;
    if ![PROC_READTHUMBNAIL, PROC_READCATEGORYDIR, PROC_REMOVEBATCH].contains(&proc_) {
        return None;
    }
    let xid = be32(body, 0)?;
    let cred_len = be32(body, 28)? as usize;
    let verf_at = 32 + pad4(cred_len);
    let verf_len = be32(body, verf_at + 4)? as usize;
    let args = verf_at + 8 + pad4(verf_len);
    Some(match proc_ {
        PROC_READCATEGORYDIR => {
            let (fh, at) = opaque(body, args)?;
            LenovoCall::Category {
                xid,
                fh: fh.to_vec(),
                offset: be32(body, at)?,
                limit: be32(body, at + 4)?,
                category: be32(body, at + 8)?,
            }
        }
        PROC_READTHUMBNAIL => {
            // quality (unused: we send one size), then the path
            let (path, _) = opaque(body, args + 4)?;
            LenovoCall::Thumbnail { xid, path: String::from_utf8_lossy(path).into_owned() }
        }
        _ => {
            let (json, _) = opaque(body, args)?;
            LenovoCall::RemoveBatch { xid, paths_json: String::from_utf8_lossy(json).into_owned() }
        }
    })
}

/// Computes the reply record for a Lenovo call (blocking: walks the tree, decodes images).
fn answer(call: LenovoCall, s: &Session) -> Vec<u8> {
    use nfs3_server::nfs3_types::nfs3::nfsstat3;
    match call {
        LenovoCall::Category { xid, fh, offset, limit, category } => {
            let result = handle_id(&fh, &s.root_fh).and_then(|id| s.fs.category(id, category, offset, limit.min(MAX_ENTRIES)));
            match &result {
                Ok(entries) => debug!("READCATEGORYDIR category {category} offset {offset} limit {limit} → {} entries", entries.len()),
                Err(e) => debug!("READCATEGORYDIR category {category} failed: {e:?}"),
            }
            category_reply(xid, result)
        }
        LenovoCall::Thumbnail { xid, path } => match s.fs.thumbnail(&path, &s.label) {
            Some(jpeg) => opaque_reply(xid, 0, Some(jpeg.as_slice())),
            None => opaque_reply(xid, THUMB_NONE, None),
        },
        LenovoCall::RemoveBatch { xid, paths_json } => {
            if !s.allow_delete {
                tracing::info!("refused a delete from the phone (the share is read-only)");
                return opaque_reply(xid, nfsstat3::NFS3ERR_ROFS as u32, None);
            }
            let Ok(paths) = serde_json::from_str::<Vec<String>>(&paths_json) else {
                return opaque_reply(xid, nfsstat3::NFS3ERR_INVAL as u32, None);
            };
            let results = s.fs.trash(&paths, &s.label);
            let json = serde_json::to_string(&results).unwrap_or_else(|_| "[]".into());
            opaque_reply(xid, 0, Some(json.as_bytes()))
        }
    }
}

/// The accepted-reply header (xid, REPLY, MSG_ACCEPTED, AUTH_NONE verifier, SUCCESS).
fn reply_header(xid: u32) -> Vec<u8> {
    let mut body = Vec::new();
    for v in [xid, 1, 0, 0, 0, 0] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body
}

/// Wraps a reply body in a last-fragment record marker.
fn record(body: Vec<u8>) -> Vec<u8> {
    let mut frag = Vec::with_capacity(4 + body.len());
    frag.extend_from_slice(&(0x8000_0000 | body.len() as u32).to_be_bytes());
    frag.extend_from_slice(&body);
    frag
}

/// A reply of a status and, when given, one XDR opaque (thumbnail bytes, remove results).
fn opaque_reply(xid: u32, status: u32, data: Option<&[u8]>) -> Vec<u8> {
    let mut body = reply_header(xid);
    body.extend_from_slice(&status.to_be_bytes());
    if let Some(d) = data {
        body.extend_from_slice(&(d.len() as u32).to_be_bytes());
        body.extend_from_slice(d);
        body.resize(body.len() + pad4(d.len()) - d.len(), 0);
    }
    record(body)
}

/// The inode id inside one of our 64-byte handles, if it carries this server's generation.
fn handle_id(fh: &[u8], root_fh: &[u8]) -> Result<u64, nfs3_server::nfs3_types::nfs3::nfsstat3> {
    use nfs3_server::nfs3_types::nfs3::nfsstat3;
    if fh.len() != ZERO_ROOT_LEN || root_fh.len() < 8 {
        return Err(nfsstat3::NFS3ERR_BADHANDLE);
    }
    if fh[..8] != root_fh[..8] {
        return Err(nfsstat3::NFS3ERR_STALE);
    }
    Ok(u64::from_be_bytes(fh[8..16].try_into().expect("8 bytes")))
}

/// The JSON form of an entry (the phone parses entries as JSON when the server advertises
/// capability 8, which our handshake does).
fn entry_json(e: &CategoryEntry) -> String {
    json!({
        "fileId": e.id.to_string(),
        "path": e.path,
        "parentPath": e.parent,
        "name": e.name,
        "fileSize": e.size,
        "modifyDate": e.modified,
        "createdDate": e.modified,
        "type": e.kind,
        "isLive": 0,
        "iCloud": 0,
    })
    .to_string()
}

/// A `READCATEGORYDIR` reply: `nfsstat3`, then on success `true` + a JSON string per entry and
/// `false`.
fn category_reply(xid: u32, result: Result<Vec<CategoryEntry>, nfs3_server::nfs3_types::nfs3::nfsstat3>) -> Vec<u8> {
    let mut body = reply_header(xid);
    let put = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_be_bytes());
    match result {
        Ok(entries) => {
            put(&mut body, 0); // NFS3_OK
            for e in &entries {
                let text = entry_json(e);
                put(&mut body, 1);
                put(&mut body, text.len() as u32);
                body.extend_from_slice(text.as_bytes());
                body.resize(body.len() + pad4(text.len()) - text.len(), 0);
            }
            put(&mut body, 0);
        }
        Err(stat) => put(&mut body, stat as u32),
    }
    record(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An NFSv3 call shaped like the phone's (AUTH_UNIX "localhost", no verifier) with raw `args`.
    fn call_with(xid: u32, proc_: u32, args: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [xid, 0, 2, NFS_PROGRAM, 3, proc_, 1, 32, 0, 9] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(b"localhost\0\0\0");
        for v in [0u32, 0, 0, 0, 0] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(args);
        let mut frag = (0x8000_0000u32 | b.len() as u32).to_be_bytes().to_vec();
        frag.extend_from_slice(&b);
        frag
    }

    fn xdr_opaque(data: &[u8]) -> Vec<u8> {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(data);
        v.resize(4 + pad4(data.len()), 0);
        v
    }

    fn call(xid: u32, fh: &[u8], offset: u32, limit: u32, category: u32) -> Vec<u8> {
        let mut args = xdr_opaque(fh);
        for v in [offset, limit, category] {
            args.extend_from_slice(&v.to_be_bytes());
        }
        call_with(xid, PROC_READCATEGORYDIR, &args)
    }

    #[test]
    fn parses_the_phones_lenovo_calls() {
        let fh = vec![7u8; 64];
        let parsed = parse_lenovo_call(&call(0x1234, &fh, 0, 500, 6)).unwrap();
        assert_eq!(parsed, LenovoCall::Category { xid: 0x1234, fh, offset: 0, limit: 500, category: 6 });

        let mut args = 1u32.to_be_bytes().to_vec(); // quality
        args.extend(xdr_opaque(b"/Pictures/cat.jpg"));
        assert_eq!(
            parse_lenovo_call(&call_with(5, PROC_READTHUMBNAIL, &args)),
            Some(LenovoCall::Thumbnail { xid: 5, path: "/Pictures/cat.jpg".into() })
        );
        let json = br#"["/a.txt","/b c.pdf"]"#;
        assert_eq!(
            parse_lenovo_call(&call_with(6, PROC_REMOVEBATCH, &xdr_opaque(json))),
            Some(LenovoCall::RemoveBatch { xid: 6, paths_json: String::from_utf8_lossy(json).into_owned() })
        );
        // A standard procedure is left to nfs3_server.
        assert!(parse_lenovo_call(&call_with(1, 17, &xdr_opaque(&[0; 64]))).is_none());
    }

    #[test]
    fn opaque_replies() {
        let ok = opaque_reply(3, 0, Some(b"[1,0]"));
        let body = &ok[4..];
        assert_eq!(be32(body, 24), Some(0));
        assert_eq!(be32(body, 28), Some(5));
        assert_eq!(&body[32..37], b"[1,0]");
        assert_eq!(body.len(), 32 + 8, "padded to 4 bytes");
        let none = opaque_reply(3, THUMB_NONE, None);
        assert_eq!((be32(&none[4..], 24), none.len()), (Some(100), 4 + 28));
    }

    #[test]
    fn reply_is_status_then_json_entries() {
        let e = CategoryEntry {
            id: 3,
            path: "/a.txt".into(),
            parent: "/".into(),
            name: "a.txt".into(),
            size: 5,
            modified: 1_700_000_000,
            kind: 16,
        };
        let frag = category_reply(9, Ok(vec![e]));
        let body = &frag[4..];
        assert_eq!(be32(&frag, 0).unwrap(), 0x8000_0000 | body.len() as u32);
        assert_eq!(&body[..28], &[0, 0, 0, 9, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(be32(body, 28), Some(1), "an entry follows");
        let len = be32(body, 32).unwrap() as usize;
        let v: serde_json::Value = serde_json::from_slice(&body[36..36 + len]).unwrap();
        assert_eq!(v["path"], "/a.txt");
        assert_eq!(v["fileId"], "3");
        assert_eq!(v["type"], 16);
        assert_eq!(v["modifyDate"], 1_700_000_000u64);
        assert_eq!(be32(body, 36 + pad4(len)), Some(0), "end of list");
        assert_eq!(body.len(), 36 + pad4(len) + 4);
        // An error is just the status.
        let err = category_reply(9, Err(nfs3_server::nfs3_types::nfs3::nfsstat3::NFS3ERR_STALE));
        assert_eq!(be32(&err[4..], 24), Some(70));
        assert_eq!(err.len(), 4 + 28);
    }

    #[test]
    fn handle_ids_need_our_generation() {
        let mut root = vec![0u8; 64];
        root[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        root[8..16].copy_from_slice(&1u64.to_be_bytes());
        let mut fh = root.clone();
        fh[8..16].copy_from_slice(&42u64.to_be_bytes());
        assert_eq!(handle_id(&fh, &root), Ok(42));
        fh[0] = 9;
        assert!(handle_id(&fh, &root).is_err());
        assert!(handle_id(&fh[..16], &root).is_err());
    }
}
