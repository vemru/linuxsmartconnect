//! A read-only NFSv3 view of a local directory, for **PC→phone** sharing.
//!
//! This is the mirror of the client side ([`crate::fuse`]): here the PC is the NFS *server* and the
//! phone is the client. We implement [`nfs3_server`]'s read-only filesystem trait
//! ([`NfsReadFileSystem`]) over one local directory (the "share dir"); the server core
//! ([`nfs3_server::tcp::process_socket`]) turns that into MOUNT + NFSv3 + PORTMAP on the wire, and
//! [`crate::server`] carries it over the phone's fingerprint-pinned mutual-TLS transport.
//!
//! File identity: every object under the share dir is given a stable 64-bit id (an inode number)
//! the first time it is seen, interned in both directions (`id → path`, `path → id`). The id is the
//! NFS `fileid3` and the payload of the opaque [`PhoneHandle`] the server wraps into a file handle.
//! Inode 1 is the share root.
//!
//! Hidden entries (names starting with `.`) are invisible to the phone by default: not listed, not
//! found by LOOKUP, and skipped by the category listings. Sharing a home folder would otherwise hand
//! the phone `~/.ssh` and every app's private data. [`LocalFs::show_hidden`] turns them back on.
//!
//! Category listings ([`LocalFs::category`]) answer Lenovo's custom `READCATEGORYDIR` call (NFS
//! procedure 23), which Smart Connect's Files tab uses for *All files*, *Recents*, *Images* and the
//! rest; see `crate::router`.
//!
//! Safety: lookups only ever join a **single** path component to a known directory, and `..` is
//! clamped at the root, so every interned path stays inside the share dir. Symlinks are not exposed
//! at all (LOOKUP and READDIRPLUS skip them), READ opens with `O_NOFOLLOW`, and batch/thumbnail
//! paths are walked component by component and canonicalized, so a link to `/etc` or another user's
//! home can never be traversed, read, trashed or thumbnailed through this server.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nfs3_server::fs_util::metadata_to_fattr3;
use nfs3_server::nfs3_types::nfs3::{fattr3, filename3, nfspath3, nfsstat3};
use nfs3_server::nfs3_types::xdr_codec::Opaque;
use nfs3_server::vfs::{
    DirEntryPlus, FileHandle, NextResult, NfsReadFileSystem, ReadDirPlusIterator,
};

/// The root directory is always inode 1.
const ROOT_ID: u64 = 1;

/// Bytes of our part of a file handle. `nfs3_server` prefixes every handle with its 8-byte
/// generation number, so handles on the wire are exactly 64 bytes.
const HANDLE_LEN: usize = 56;

/// A file handle carrying the inode id, padded so the wire handle is **64 bytes**. The phone's NFS
/// client accepts only 0- or 64-byte handles: any other length flags its XDR decoder as failed, the
/// rest of the reply is mis-parsed and the call is retried forever (seen as a READDIRPLUS loop with
/// 16-byte handles).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhoneHandle([u8; HANDLE_LEN]);

impl PhoneHandle {
    pub fn new(id: u64) -> Self {
        let mut bytes = [0u8; HANDLE_LEN];
        bytes[..8].copy_from_slice(&id.to_be_bytes());
        Self(bytes)
    }

    pub fn as_u64(&self) -> u64 {
        u64::from_be_bytes(self.0[..8].try_into().expect("8-byte prefix"))
    }
}

impl FileHandle for PhoneHandle {
    fn len(&self) -> usize {
        HANDLE_LEN
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }
}

impl std::fmt::Debug for PhoneHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PhoneHandle").field(&self.as_u64()).finish()
    }
}

/// Interning tables mapping between 64-bit ids and absolute paths under the share dir.
struct Inner {
    by_id: HashMap<u64, PathBuf>,
    by_path: HashMap<PathBuf, u64>,
    next: u64,
}

impl Inner {
    fn intern(&mut self, path: PathBuf) -> u64 {
        if let Some(&id) = self.by_path.get(&path) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.by_path.insert(path.clone(), id);
        self.by_id.insert(id, path);
        id
    }
}

/// A read-only NFS filesystem backed by a local directory. Clones share the id tables (the router
/// keeps one for the category listings while the NFS core owns another).
#[derive(Clone)]
pub struct LocalFs {
    root: PathBuf,
    inner: Arc<RwLock<Inner>>,
    show_hidden: bool,
    /// Recent tree-walk results per category (the phone pages through them with offset/limit).
    walks: Arc<Mutex<HashMap<u32, CachedWalk>>>,
    /// Thumbnails already made, by file and modification time (the phone re-asks while scrolling).
    thumbs: Arc<Mutex<HashMap<ThumbKey, Arc<Vec<u8>>>>>,
}

/// A thumbnail cache key: the file and its modification time.
type ThumbKey = (PathBuf, u64);

/// A tree walk's result and when it was made.
type CachedWalk = (Instant, Arc<Vec<CategoryEntry>>);

/// Whether `name` is a hidden (dot) entry. `.` and `..` are navigation, not entries.
fn is_hidden(name: &[u8]) -> bool {
    name.first() == Some(&b'.') && name != b"." && name != b".."
}

impl LocalFs {
    /// Serves `root` (which must exist and be a directory) read-only over NFS.
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        let mut by_id = HashMap::new();
        let mut by_path = HashMap::new();
        by_id.insert(ROOT_ID, root.clone());
        by_path.insert(root.clone(), ROOT_ID);
        Ok(Self {
            root,
            inner: Arc::new(RwLock::new(Inner { by_id, by_path, next: ROOT_ID + 1 })),
            show_hidden: false,
            walks: Arc::default(),
            thumbs: Arc::default(),
        })
    }

    /// Also shows hidden (dot) files and folders to the phone.
    #[must_use]
    pub fn show_hidden(mut self, show: bool) -> Self {
        self.show_hidden = show;
        self
    }

    fn visible(&self, name: &[u8]) -> bool {
        self.show_hidden || !is_hidden(name)
    }

    fn path_of(&self, id: u64) -> Result<PathBuf, nfsstat3> {
        self.inner.read().unwrap().by_id.get(&id).cloned().ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn intern(&self, path: PathBuf) -> u64 {
        self.inner.write().unwrap().intern(path)
    }
}

impl NfsReadFileSystem for LocalFs {
    type Handle = PhoneHandle;

    fn root_dir(&self) -> PhoneHandle {
        PhoneHandle::new(ROOT_ID)
    }

    async fn lookup(
        &self,
        dirid: &PhoneHandle,
        filename: &filename3<'_>,
    ) -> Result<PhoneHandle, nfsstat3> {
        let dir = self.path_of(dirid.as_u64())?;
        let name = filename.as_ref();
        if name == b"." {
            return Ok(*dirid);
        }
        if name == b".." {
            // Clamp at the share root so `..` can never escape the export.
            if dir == self.root {
                return Ok(PhoneHandle::new(ROOT_ID));
            }
            let parent = dir.parent().unwrap_or(&self.root).to_path_buf();
            return Ok(PhoneHandle::new(self.intern(parent)));
        }
        // Reject anything that isn't a single, safe path component, and hidden names unless shown.
        if name.is_empty() || name.contains(&b'/') || name == b".." || !self.visible(name) {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let child = dir.join(std::ffi::OsStr::from_bytes(name));
        // Don't expose symlinks at all: a lookup only ever resolves to a real entry inside the
        // share, so a link to /etc or to another user's home can never be opened through us.
        let meta = std::fs::symlink_metadata(&child).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if meta.file_type().is_symlink() {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        Ok(PhoneHandle::new(self.intern(child)))
    }

    async fn getattr(&self, id: &PhoneHandle) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id.as_u64())?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        Ok(metadata_to_fattr3(id.as_u64(), &meta))
    }

    async fn read(
        &self,
        id: &PhoneHandle,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let path = self.path_of(id.as_u64())?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if meta.is_dir() {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        // O_NOFOLLOW: even a handle interned before a file became a symlink can't follow it.
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        // fstat the object we actually opened (not the path we stat'ed a moment ago).
        let len = f.metadata().map_err(|_| nfsstat3::NFS3ERR_IO)?.len();
        if offset >= len {
            return Ok((Vec::new(), true));
        }
        f.seek(SeekFrom::Start(offset)).map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let want = u64::from(count).min(len - offset) as usize;
        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(nfsstat3::NFS3ERR_IO),
            }
        }
        buf.truncate(filled);
        let eof = offset + filled as u64 >= len;
        Ok((buf, eof))
    }

    async fn readdirplus(
        &self,
        dirid: &PhoneHandle,
        cookie: u64,
    ) -> Result<impl ReadDirPlusIterator<PhoneHandle>, nfsstat3> {
        let dir = self.path_of(dirid.as_u64())?;
        let meta = std::fs::symlink_metadata(&dir).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if !meta.is_dir() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let rd = std::fs::read_dir(&dir).map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let mut entries: Vec<(u64, Vec<u8>, PathBuf)> = Vec::new();
        for ent in rd {
            let ent = ent.map_err(|_| nfsstat3::NFS3ERR_IO)?;
            if !self.visible(ent.file_name().as_bytes()) {
                continue;
            }
            // Symlinks are not entries the phone may see (see `lookup`).
            if ent.file_type().map_err(|_| nfsstat3::NFS3ERR_IO)?.is_symlink() {
                continue;
            }
            let path = ent.path();
            let id = self.intern(path.clone());
            entries.push((id, ent.file_name().as_bytes().to_vec(), path));
        }
        // Deterministic ordering (by name) so pagination cookies are stable across calls.
        entries.sort_by(|a, b| a.1.cmp(&b.1));

        let start = if cookie == 0 {
            0
        } else {
            match entries.iter().position(|(id, _, _)| *id == cookie) {
                Some(pos) => pos + 1,
                None => return Err(nfsstat3::NFS3ERR_BAD_COOKIE),
            }
        };
        entries.drain(..start.min(entries.len()));
        Ok(LocalFsIterator { entries, index: 0 })
    }

    async fn readlink(&self, id: &PhoneHandle) -> Result<nfspath3<'_>, nfsstat3> {
        let path = self.path_of(id.as_u64())?;
        let target = std::fs::read_link(&path).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        Ok(nfspath3(Opaque::owned(target.as_os_str().as_bytes().to_vec())))
    }
}

/// Paginated directory iterator: each entry's attributes are fetched lazily as it is yielded, so a
/// large directory doesn't stat everything up front.
struct LocalFsIterator {
    entries: Vec<(u64, Vec<u8>, PathBuf)>,
    index: usize,
}

impl ReadDirPlusIterator<PhoneHandle> for LocalFsIterator {
    async fn next(&mut self) -> NextResult<DirEntryPlus<PhoneHandle>> {
        loop {
            let Some((id, name, path)) = self.entries.get(self.index).cloned() else {
                return NextResult::Eof;
            };
            self.index += 1;
            let attr = match std::fs::symlink_metadata(&path) {
                // Some filesystems report DT_UNKNOWN, so re-check here: the phone never sees links.
                Ok(meta) if !meta.file_type().is_symlink() => Some(metadata_to_fattr3(id, &meta)),
                Ok(_) => continue,
                Err(_) => continue, // vanished between readdir and stat — skip it
            };
            return NextResult::Ok(DirEntryPlus {
                fileid: id,
                name: filename3(Opaque::owned(name)),
                cookie: id,
                name_attributes: attr,
                name_handle: Some(PhoneHandle::new(id)),
            });
        }
    }
}

// ── Category listings (Lenovo `READCATEGORYDIR`) ─────────────────────────────────────────────

/// `READCATEGORYDIR` categories, as the phone numbers its Files-tab views.
pub mod category {
    pub const RECENTS: u32 = 0;
    pub const IMAGES: u32 = 1;
    pub const VIDEOS: u32 = 2;
    pub const DOCUMENTS: u32 = 3;
    pub const AUDIO: u32 = 4;
    pub const DOWNLOADS: u32 = 5;
    /// The direct children of one folder ("All files").
    pub const FOLDER: u32 = 6;
    pub const IMAGES_AND_VIDEOS: u32 = 7;
}

/// Entry kinds in a category listing (the phone's own server's values).
pub mod kind {
    pub const OTHER: u32 = 0;
    pub const FOLDER: u32 = 1;
    pub const IMAGE: u32 = 2;
    pub const VIDEO: u32 = 4;
    pub const AUDIO: u32 = 8;
    pub const DOCUMENT: u32 = 16;
}

/// "Recent" means modified within this long (the phone's own server uses one month).
const RECENT_WINDOW: Duration = Duration::from_secs(30 * 24 * 3600);
/// A tree walk for the media/recent categories stops after this many entries or this depth, so a
/// huge shared folder can't stall the phone's request.
const WALK_MAX_ENTRIES: usize = 200_000;
const WALK_MAX_DEPTH: usize = 16;
/// How long a tree walk is reused while the phone pages through it.
const WALK_CACHE_FOR: Duration = Duration::from_secs(30);

/// One entry of a category listing.
#[derive(Clone, Debug, PartialEq)]
pub struct CategoryEntry {
    pub id: u64,
    /// Path relative to the share root, with a leading `/` (e.g. `/notes/readme.txt`).
    pub path: String,
    /// The parent's relative path (`/` for top-level entries).
    pub parent: String,
    pub name: String,
    pub size: u64,
    /// Modification time, in seconds since the epoch.
    pub modified: u64,
    pub kind: u32,
}

/// The kind of a file from its extension.
fn kind_of(name: &str) -> u32 {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "bmp" | "svg" | "tif" | "tiff" | "avif" => kind::IMAGE,
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "3gp" | "wmv" | "flv" => kind::VIDEO,
        "mp3" | "flac" | "ogg" | "oga" | "opus" | "m4a" | "wav" | "aac" | "wma" => kind::AUDIO,
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "xls" | "xlsx" | "ods" | "csv" | "ppt" | "pptx"
        | "odp" | "epub" => kind::DOCUMENT,
        _ => kind::OTHER,
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

impl LocalFs {
    /// The share-relative form of `path` (`/` for the root).
    fn relative(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        format!("/{}", rel.to_string_lossy())
    }

    fn entry(&self, path: PathBuf, meta: &std::fs::Metadata) -> CategoryEntry {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parent = path.parent().map_or_else(|| "/".to_owned(), |p| self.relative(p));
        let kind = if meta.is_dir() { kind::FOLDER } else { kind_of(&name) };
        CategoryEntry {
            path: self.relative(&path),
            parent,
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: mtime_secs(meta),
            kind,
            name,
            id: self.intern(path),
        }
    }

    /// Answers `READCATEGORYDIR`: entries `offset..offset+limit` of `category`. Category
    /// [`category::FOLDER`] lists the folder `dir_id` (folders first, then by name); the others
    /// search the whole share, newest first. Unknown categories are empty.
    pub fn category(&self, dir_id: u64, cat: u32, offset: u32, limit: u32) -> Result<Vec<CategoryEntry>, nfsstat3> {
        let all = if cat == category::FOLDER {
            Arc::new(self.folder(&self.path_of(dir_id)?)?)
        } else {
            self.walk_cached(cat)
        };
        Ok(all.iter().skip(offset as usize).take(limit as usize).cloned().collect())
    }

    fn folder(&self, dir: &Path) -> Result<Vec<CategoryEntry>, nfsstat3> {
        let rd = std::fs::read_dir(dir).map_err(|_| nfsstat3::NFS3ERR_NOTDIR)?;
        let mut out: Vec<CategoryEntry> = rd
            .filter_map(Result::ok)
            .filter(|e| self.visible(e.file_name().as_bytes()))
            .filter_map(|e| {
                let path = e.path();
                let meta = std::fs::symlink_metadata(&path).ok()?;
                (meta.is_dir() || meta.is_file()).then(|| self.entry(path, &meta))
            })
            .collect();
        out.sort_by(|a, b| {
            (b.kind == kind::FOLDER)
                .cmp(&(a.kind == kind::FOLDER))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    fn walk_cached(&self, cat: u32) -> Arc<Vec<CategoryEntry>> {
        if let Some((at, list)) = self.walks.lock().unwrap().get(&cat)
            && at.elapsed() < WALK_CACHE_FOR
        {
            return list.clone();
        }
        let list = Arc::new(self.walk(cat));
        self.walks.lock().unwrap().insert(cat, (Instant::now(), list.clone()));
        list
    }

    /// Files under the share (or its Downloads folder) that belong to `cat`, newest first. Doesn't
    /// follow symlinks or enter hidden folders (unless shown).
    fn walk(&self, cat: u32) -> Vec<CategoryEntry> {
        let wanted = |kind: u32, modified: u64| -> bool {
            match cat {
                category::RECENTS => {
                    let cutoff = SystemTime::now().checked_sub(RECENT_WINDOW).unwrap_or(UNIX_EPOCH);
                    modified >= cutoff.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
                }
                category::IMAGES => kind == kind::IMAGE,
                category::VIDEOS => kind == kind::VIDEO,
                category::DOCUMENTS => kind == kind::DOCUMENT,
                category::AUDIO => kind == kind::AUDIO,
                category::DOWNLOADS => true,
                category::IMAGES_AND_VIDEOS => kind == kind::IMAGE || kind == kind::VIDEO,
                _ => false,
            }
        };
        let start = if cat == category::DOWNLOADS {
            match ["Downloads", "Download"].iter().map(|d| self.root.join(d)).find(|p| p.is_dir()) {
                Some(dir) => dir,
                None => return Vec::new(),
            }
        } else {
            self.root.clone()
        };
        let mut out = Vec::new();
        let mut stack = vec![(start, 0usize)];
        let mut visited = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for ent in rd.filter_map(Result::ok) {
                visited += 1;
                if visited > WALK_MAX_ENTRIES {
                    stack.clear();
                    break;
                }
                if !self.visible(ent.file_name().as_bytes()) {
                    continue;
                }
                let path = ent.path();
                let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                if meta.is_dir() {
                    if depth + 1 < WALK_MAX_DEPTH {
                        stack.push((path, depth + 1));
                    }
                } else if meta.is_file() {
                    let kind = kind_of(&ent.file_name().to_string_lossy());
                    if wanted(kind, mtime_secs(&meta)) {
                        out.push(self.entry(path, &meta));
                    }
                }
            }
        }
        out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
        out
    }
}

// ── Batch remove and thumbnails (Lenovo `REMOVEBATCH` / `READTHUMBNAIL`) ──────────────────────

/// Longest side of a thumbnail we send, in pixels.
const THUMB_SIZE: u32 = 320;
/// Images larger than this get no thumbnail (decoding them would stall the phone's grid).
const THUMB_MAX_FILE: u64 = 64 * 1024 * 1024;
/// Thumbnails kept in memory; the cache is simply cleared when full.
const THUMB_CACHE_MAX: usize = 1000;
/// A video thumbnail is one frame grabbed by `ffmpeg`; give up after this long.
const VIDEO_THUMB_TIMEOUT: Duration = Duration::from_secs(10);

/// A JPEG of one frame of `video` (about 1 s in, or the first frame of a shorter clip), scaled to
/// fit [`THUMB_SIZE`], made by `ffmpeg` if it is installed.
fn video_thumbnail(video: &Path) -> Option<Vec<u8>> {
    let scale = format!("scale={THUMB_SIZE}:{THUMB_SIZE}:force_original_aspect_ratio=decrease");
    ["1", "0"].iter().find_map(|at| {
        let mut child = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-ss", at, "-i"])
            .arg(video)
            .args(["-frames:v", "1", "-vf", &scale, "-q:v", "5", "-f", "image2pipe", "-c:v", "mjpeg", "-"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            stdout.read_to_end(&mut out).map(|_| out)
        });
        let deadline = Instant::now() + VIDEO_THUMB_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let out = reader.join().ok()?.ok()?;
        (status?.success() && !out.is_empty()).then_some(out)
    })
}

impl LocalFs {
    /// Maps a path the phone sends in a batch call back to a file under the share: it may be
    /// share-relative (`/notes/a.txt`), carry our export label (`/alice/notes/a.txt`), or the
    /// phone's document-id form (`192.168.1.20:/alice/notes/a.txt`). Refuses `..`, hidden components
    /// (unless shown), the share root itself, and anything that doesn't exist.
    pub fn resolve_phone_path(&self, path: &str, label: &str) -> Option<PathBuf> {
        let path = match path.split_once(":/") {
            Some((host, rest)) if !host.contains('/') => format!("/{rest}"),
            _ => path.to_owned(),
        };
        let label = label.trim_end_matches('/');
        let rel = match path.strip_prefix(label) {
            Some(rest) if !label.is_empty() && (rest.is_empty() || rest.starts_with('/')) => rest,
            _ => path.as_str(),
        };
        let mut out = self.root.clone();
        let mut depth = 0;
        for part in rel.split('/').filter(|p| !p.is_empty() && *p != ".") {
            if part == ".." || !self.visible(part.as_bytes()) {
                return None;
            }
            out.push(part);
            let meta = std::fs::symlink_metadata(&out).ok()?;
            if meta.file_type().is_symlink() {
                return None;
            }
            depth += 1;
        }
        if depth == 0 {
            return None;
        }
        // Defence in depth: the walked path must still canonicalize inside the share.
        let canon = out.canonicalize().ok()?;
        canon.starts_with(&self.root).then_some(canon)
    }

    /// Moves each path to the desktop Trash (`gio trash`), so a deletion from the phone can be
    /// undone. Returns 1 (moved) or 0 (refused or failed) per path, in order.
    pub fn trash(&self, paths: &[String], label: &str) -> Vec<u8> {
        let results = paths
            .iter()
            .map(|p| {
                let Some(target) = self.resolve_phone_path(p, label) else { return 0 };
                let ok = std::process::Command::new("gio")
                    .arg("trash")
                    .arg("--")
                    .arg(&target)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .is_ok_and(|o| o.status.success());
                if ok {
                    tracing::info!("moved {} to the Trash at the phone's request", target.display());
                } else {
                    tracing::warn!("could not move {} to the Trash", target.display());
                }
                u8::from(ok)
            })
            .collect();
        self.walks.lock().unwrap().clear(); // the category searches are stale now
        results
    }

    /// A JPEG thumbnail (longest side at most [`THUMB_SIZE`]) of the image or video at `path`, or
    /// `None` for anything else (or a video when `ffmpeg` isn't installed).
    pub fn thumbnail(&self, path: &str, label: &str) -> Option<Arc<Vec<u8>>> {
        let file = self.resolve_phone_path(path, label)?;
        // symlink_metadata: never follow a link that replaced the file after the walk.
        let meta = std::fs::symlink_metadata(&file).ok()?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return None;
        }
        let key = (file.clone(), mtime_secs(&meta));
        if let Some(hit) = self.thumbs.lock().unwrap().get(&key) {
            return Some(hit.clone());
        }
        let jpeg = match kind_of(&file.file_name()?.to_string_lossy()) {
            kind::IMAGE if meta.len() <= THUMB_MAX_FILE => image_thumbnail(&file)?,
            kind::VIDEO => video_thumbnail(&file)?,
            _ => return None,
        };
        let jpeg = Arc::new(jpeg);
        let mut cache = self.thumbs.lock().unwrap();
        if cache.len() >= THUMB_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, jpeg.clone());
        Some(jpeg)
    }
}

/// A JPEG of `file` scaled to fit [`THUMB_SIZE`].
fn image_thumbnail(file: &Path) -> Option<Vec<u8>> {
    let img = image::ImageReader::open(file).ok()?.with_guessed_format().ok()?.decode().ok()?;
    let thumb = img.thumbnail(THUMB_SIZE, THUMB_SIZE).into_rgb8();
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
    thumb.write_with_encoder(enc).ok()?;
    Some(out)
}

/// The share folder inside `home`.
fn share_dir_under(home: &Path) -> PathBuf {
    home.join("SmartConnect")
}

/// Default share dir: `~/SmartConnect`, created on first use, with hidden entries invisible to the
/// phone unless `--include-hidden` (see [`LocalFs::show_hidden`]).
///
/// It used to be all of `$HOME`. A narrow, obvious folder keeps the rest of the home directory
/// (`.ssh`, browser profiles, other apps' state) out of the phone's view even if a hidden-entry
/// check is ever missed, and an app created for this purpose also can't accidentally expose
/// unrelated files. An explicit `--sharedir` (or the GUI's folder choice) is unchanged.
pub fn default_share_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    share_dir_under(&home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfs3_server::nfs3_types::nfs3::ftype3;

    fn fname(s: &str) -> filename3<'static> {
        filename3(Opaque::owned(s.as_bytes().to_vec()))
    }

    async fn drain(
        mut it: impl ReadDirPlusIterator<PhoneHandle>,
    ) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        loop {
            match it.next().await {
                NextResult::Ok(e) => out.push((e.cookie, e.name.as_ref().to_vec())),
                NextResult::Eof => break,
                NextResult::Err(e) => panic!("iterator error: {e:?}"),
            }
        }
        out
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("localfs-test-{}", std::process::id()));
        let d = base.join(format!("{:?}", std::time::SystemTime::now()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn hidden_entries_are_invisible_unless_shown() {
        let dir = tempdir();
        std::fs::write(dir.join("visible.txt"), b"x").unwrap();
        std::fs::create_dir(dir.join(".ssh")).unwrap();
        std::fs::write(dir.join(".ssh").join("id_ed25519"), b"secret").unwrap();

        let fs = LocalFs::new(dir.clone()).unwrap();
        let names: Vec<_> = drain(fs.readdirplus(&fs.root_dir(), 0).await.unwrap()).await.into_iter().map(|e| e.1).collect();
        assert_eq!(names, vec![b"visible.txt".to_vec()]);
        assert!(matches!(fs.lookup(&fs.root_dir(), &fname(".ssh")).await, Err(nfsstat3::NFS3ERR_NOENT)));
        assert!(fs.category(ROOT_ID, category::FOLDER, 0, 100).unwrap().iter().all(|e| !e.name.starts_with('.')));
        assert!(fs.category(ROOT_ID, category::RECENTS, 0, 100).unwrap().iter().all(|e| !e.path.contains("/.")));

        let fs = LocalFs::new(dir.clone()).unwrap().show_hidden(true);
        assert!(fs.lookup(&fs.root_dir(), &fname(".ssh")).await.is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn phone_paths_resolve_inside_the_share_only() {
        let dir = tempdir();
        std::fs::create_dir(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes").join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join(".secret"), b"x").unwrap();
        let fs = LocalFs::new(dir.clone()).unwrap();
        let want = Some(fs.root.join("notes").join("a.txt"));
        assert_eq!(fs.resolve_phone_path("/notes/a.txt", "/share"), want);
        assert_eq!(fs.resolve_phone_path("/share/notes/a.txt", "/share"), want);
        assert_eq!(fs.resolve_phone_path("192.168.1.20:/share/notes/a.txt", "/share"), want);
        assert_eq!(fs.resolve_phone_path("/notes/../notes/a.txt", "/share"), None, "no ..");
        assert_eq!(fs.resolve_phone_path("/.secret", "/share"), None, "hidden");
        assert_eq!(fs.resolve_phone_path("/", "/share"), None, "not the root");
        assert_eq!(fs.resolve_phone_path("/share", "/share"), None, "not the root");
        assert_eq!(fs.resolve_phone_path("/missing.txt", "/share"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_share_is_a_smartconnect_folder_under_home() {
        assert_eq!(share_dir_under(Path::new("/home/alice")), PathBuf::from("/home/alice/SmartConnect"));
    }

    #[tokio::test]
    async fn symlinks_are_not_exposed_or_followed() {
        let outside = tempdir();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        image::RgbImage::from_pixel(20, 10, image::Rgb([0, 200, 0])).save(outside.join("pic.png")).unwrap();
        let dir = tempdir();
        std::fs::write(dir.join("innocent.txt"), b"ok").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("secret.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("pic.png"), dir.join("pic.png")).unwrap();

        let fs = LocalFs::new(dir.clone()).unwrap();
        let root = fs.root_dir();
        // LOOKUP refuses a symlink, whether it points at a file or a directory.
        assert_eq!(fs.lookup(&root, &fname("secret.txt")).await.unwrap_err(), nfsstat3::NFS3ERR_NOENT);
        assert_eq!(fs.lookup(&root, &fname("escape")).await.unwrap_err(), nfsstat3::NFS3ERR_NOENT);
        // READDIRPLUS doesn't list it.
        let names: Vec<_> = drain(fs.readdirplus(&root, 0).await.unwrap()).await.into_iter().map(|e| e.1).collect();
        assert_eq!(names, vec![b"innocent.txt".to_vec()]);
        // A handle that already points at one can't be read through.
        let id = fs.inner.write().unwrap().intern(dir.join("secret.txt"));
        assert_eq!(fs.read(&PhoneHandle::new(id), 0, 100).await.err(), Some(nfsstat3::NFS3ERR_IO));
        // Batch paths and thumbnails refuse symlinks, including in a parent component.
        assert_eq!(fs.resolve_phone_path("/secret.txt", "/share"), None);
        assert_eq!(fs.resolve_phone_path("/escape/secret.txt", "/share"), None);
        assert_eq!(fs.resolve_phone_path("/pic.png", "/share"), None);
        assert!(fs.thumbnail("/pic.png", "/share").is_none());
        // Category listings skip it too.
        let listed: Vec<_> = fs.category(ROOT_ID, category::FOLDER, 0, 100).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(listed, ["innocent.txt"]);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn thumbnails_are_small_jpegs_of_images_only() {
        let dir = tempdir();
        image::RgbImage::from_pixel(1000, 500, image::Rgb([200, 30, 30])).save(dir.join("big.png")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"not an image").unwrap();
        let fs = LocalFs::new(dir.clone()).unwrap();
        let jpeg = fs.thumbnail("/big.png", "/share").expect("a thumbnail");
        let back = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (THUMB_SIZE, THUMB_SIZE / 2));
        assert!(fs.thumbnail("/notes.txt", "/share").is_none());
        // Cached: the same bytes come back.
        assert!(Arc::ptr_eq(&fs.thumbnail("/big.png", "/share").unwrap(), &fs.thumbnail("/big.png", "/share").unwrap()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn video_thumbnails_come_from_ffmpeg() {
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        let dir = tempdir();
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=10:duration=2", "-pix_fmt", "yuv420p"])
            .arg(dir.join("clip.mp4"))
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("ffmpeg can't encode a test clip here; skipping");
            return;
        }
        let fs = LocalFs::new(dir.clone()).unwrap();
        let jpeg = fs.thumbnail("/clip.mp4", "/share").expect("a video thumbnail");
        let back = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (THUMB_SIZE, 180));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn category_listings() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("Pictures/2026")).unwrap();
        std::fs::create_dir(dir.join("Downloads")).unwrap();
        std::fs::write(dir.join("b.pdf"), b"pdf").unwrap();
        std::fs::write(dir.join("A.txt"), b"text").unwrap();
        std::fs::write(dir.join("Pictures/2026/cat.JPG"), b"jpg").unwrap();
        std::fs::write(dir.join("Downloads/song.mp3"), b"mp3").unwrap();

        let fs = LocalFs::new(dir.clone()).unwrap();
        // All files: folders first, then names case-insensitively; paths are share-relative.
        let top = fs.category(ROOT_ID, category::FOLDER, 0, 100).unwrap();
        let names: Vec<_> = top.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Downloads", "Pictures", "A.txt", "b.pdf"]);
        assert_eq!(top[0].kind, kind::FOLDER);
        assert_eq!((top[2].path.as_str(), top[2].parent.as_str(), top[2].size), ("/A.txt", "/", 4));
        assert_eq!(top[3].kind, kind::DOCUMENT);
        // Paging.
        let page: Vec<_> = fs.category(ROOT_ID, category::FOLDER, 1, 2).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(page, ["Pictures", "A.txt"]);
        // A subfolder by its id.
        let pics = top.iter().find(|e| e.name == "Pictures").unwrap().id;
        assert_eq!(fs.category(pics, category::FOLDER, 0, 10).unwrap()[0].path, "/Pictures/2026");
        // Searches across the tree.
        let images = fs.category(ROOT_ID, category::IMAGES, 0, 10).unwrap();
        assert_eq!((images.len(), images[0].path.as_str(), images[0].parent.as_str()), (1, "/Pictures/2026/cat.JPG", "/Pictures/2026"));
        assert_eq!(fs.category(ROOT_ID, category::DOWNLOADS, 0, 10).unwrap()[0].name, "song.mp3");
        assert_eq!(fs.category(ROOT_ID, category::RECENTS, 0, 10).unwrap().len(), 4, "all four files are new");
        assert!(fs.category(ROOT_ID, 99, 0, 10).unwrap().is_empty());
        assert!(matches!(fs.category(12345, category::FOLDER, 0, 10), Err(nfsstat3::NFS3ERR_STALE)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn lists_reads_and_navigates() {
        let dir = tempdir();
        std::fs::write(dir.join("a.txt"), b"hello world").unwrap();
        std::fs::write(dir.join("b.txt"), b"second").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("c.txt"), b"nested").unwrap();

        let fs = LocalFs::new(dir.clone()).unwrap();
        let root = fs.root_dir();
        assert_eq!(root.as_u64(), ROOT_ID);

        // readdirplus lists all three top-level names, sorted.
        let listing = drain(fs.readdirplus(&root, 0).await.unwrap()).await;
        let names: Vec<_> = listing.iter().map(|(_, n)| String::from_utf8_lossy(n).into_owned()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);

        // lookup + getattr on a file.
        let a = fs.lookup(&root, &fname("a.txt")).await.unwrap();
        let attr = fs.getattr(&a).await.unwrap();
        assert_eq!(attr.type_, ftype3::NF3REG);
        assert_eq!(attr.size, 11);

        // read respects offset/count and flags EOF.
        let (bytes, eof) = fs.read(&a, 0, 5).await.unwrap();
        assert_eq!(&bytes, b"hello");
        assert!(!eof);
        let (bytes, eof) = fs.read(&a, 6, 100).await.unwrap();
        assert_eq!(&bytes, b"world");
        assert!(eof);

        // descend into the subdirectory, then `..` returns to the root.
        let sub = fs.lookup(&root, &fname("sub")).await.unwrap();
        assert_eq!(fs.getattr(&sub).await.unwrap().type_, ftype3::NF3DIR);
        let subc = drain(fs.readdirplus(&sub, 0).await.unwrap()).await;
        assert_eq!(subc.len(), 1);
        let dotdot = fs.lookup(&sub, &fname("..")).await.unwrap();
        assert_eq!(dotdot.as_u64(), ROOT_ID);

        // `..` at the root is clamped to the root (no escape).
        let clamp = fs.lookup(&root, &fname("..")).await.unwrap();
        assert_eq!(clamp.as_u64(), ROOT_ID);

        // a missing name is ENOENT.
        assert_eq!(fs.lookup(&root, &fname("nope")).await.unwrap_err(), nfsstat3::NFS3ERR_NOENT);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pagination_cookie_resumes() {
        let dir = tempdir();
        for i in 0..5 {
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }
        let fs = LocalFs::new(dir.clone()).unwrap();
        let root = fs.root_dir();

        let full = drain(fs.readdirplus(&root, 0).await.unwrap()).await;
        assert_eq!(full.len(), 5);

        // Resume after the second entry; we should get exactly the last three, in the same order.
        let cookie = full[1].0;
        let rest = drain(fs.readdirplus(&root, cookie).await.unwrap()).await;
        assert_eq!(rest, full[2..].to_vec());

        // An unknown cookie is a bad cookie.
        assert_eq!(
            fs.readdirplus(&root, u64::MAX).await.err(),
            Some(nfsstat3::NFS3ERR_BAD_COOKIE)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
