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
//! NFS `fileid3` and the 8-byte opaque [`FileHandleU64`] the server wraps into a file handle. Inode
//! 1 is the share root.
//!
//! Safety: lookups only ever join a **single** path component to a known directory, and `..` is
//! clamped at the root, so every interned path stays inside the share dir. Symlinks are reported as
//! symlinks (never followed server-side), so a symlink pointing outside the share can't be traversed
//! through this server — the client would have to re-`LOOKUP` the target through our root, which
//! stays inside the share.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::RwLock;

use nfs3_server::fs_util::metadata_to_fattr3;
use nfs3_server::nfs3_types::nfs3::{fattr3, filename3, nfspath3, nfsstat3};
use nfs3_server::nfs3_types::xdr_codec::Opaque;
use nfs3_server::vfs::{
    DirEntryPlus, FileHandleU64, NextResult, NfsReadFileSystem, ReadDirPlusIterator,
};

/// The root directory is always inode 1.
const ROOT_ID: u64 = 1;

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

/// A read-only NFS filesystem backed by a local directory.
pub struct LocalFs {
    root: PathBuf,
    inner: RwLock<Inner>,
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
            inner: RwLock::new(Inner { by_id, by_path, next: ROOT_ID + 1 }),
        })
    }

    fn path_of(&self, id: u64) -> Result<PathBuf, nfsstat3> {
        self.inner.read().unwrap().by_id.get(&id).cloned().ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn intern(&self, path: PathBuf) -> u64 {
        self.inner.write().unwrap().intern(path)
    }
}

impl NfsReadFileSystem for LocalFs {
    type Handle = FileHandleU64;

    fn root_dir(&self) -> FileHandleU64 {
        FileHandleU64::new(ROOT_ID)
    }

    async fn lookup(
        &self,
        dirid: &FileHandleU64,
        filename: &filename3<'_>,
    ) -> Result<FileHandleU64, nfsstat3> {
        let dir = self.path_of(dirid.as_u64())?;
        let name = filename.as_ref();
        if name == b"." {
            return Ok(*dirid);
        }
        if name == b".." {
            // Clamp at the share root so `..` can never escape the export.
            if dir == self.root {
                return Ok(FileHandleU64::new(ROOT_ID));
            }
            let parent = dir.parent().unwrap_or(&self.root).to_path_buf();
            return Ok(FileHandleU64::new(self.intern(parent)));
        }
        // Reject anything that isn't a single, safe path component.
        if name.is_empty() || name.contains(&b'/') || name == b".." {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let child = dir.join(std::ffi::OsStr::from_bytes(name));
        // Don't traverse symlinks: existence is checked without following the final component.
        std::fs::symlink_metadata(&child).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        Ok(FileHandleU64::new(self.intern(child)))
    }

    async fn getattr(&self, id: &FileHandleU64) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id.as_u64())?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        Ok(metadata_to_fattr3(id.as_u64(), &meta))
    }

    async fn read(
        &self,
        id: &FileHandleU64,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let path = self.path_of(id.as_u64())?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if meta.is_dir() {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        let len = meta.len();
        if offset >= len {
            return Ok((Vec::new(), true));
        }
        let mut f = std::fs::File::open(&path).map_err(|_| nfsstat3::NFS3ERR_IO)?;
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
        dirid: &FileHandleU64,
        cookie: u64,
    ) -> Result<impl ReadDirPlusIterator<FileHandleU64>, nfsstat3> {
        let dir = self.path_of(dirid.as_u64())?;
        let meta = std::fs::symlink_metadata(&dir).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if !meta.is_dir() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let rd = std::fs::read_dir(&dir).map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let mut entries: Vec<(u64, Vec<u8>, PathBuf)> = Vec::new();
        for ent in rd {
            let ent = ent.map_err(|_| nfsstat3::NFS3ERR_IO)?;
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

    async fn readlink(&self, id: &FileHandleU64) -> Result<nfspath3<'_>, nfsstat3> {
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

impl ReadDirPlusIterator<FileHandleU64> for LocalFsIterator {
    async fn next(&mut self) -> NextResult<DirEntryPlus<FileHandleU64>> {
        loop {
            let Some((id, name, path)) = self.entries.get(self.index).cloned() else {
                return NextResult::Eof;
            };
            self.index += 1;
            let attr = match std::fs::symlink_metadata(&path) {
                Ok(meta) => Some(metadata_to_fattr3(id, &meta)),
                Err(_) => continue, // vanished between readdir and stat — skip it
            };
            return NextResult::Ok(DirEntryPlus {
                fileid: id,
                name: filename3(Opaque::owned(name)),
                cookie: id,
                name_attributes: attr,
                name_handle: Some(FileHandleU64::new(id)),
            });
        }
    }
}

/// Default share dir: `~/PhoneShare`. Created on demand by the `serve` subcommand.
pub fn default_share_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("PhoneShare")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfs3_server::nfs3_types::nfs3::ftype3;

    fn fname(s: &str) -> filename3<'static> {
        filename3(Opaque::owned(s.as_bytes().to_vec()))
    }

    async fn drain(
        mut it: impl ReadDirPlusIterator<FileHandleU64>,
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
