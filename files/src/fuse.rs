//! Exposes the phone's NFS export as a read-only FUSE filesystem, and keeps it alive across phone
//! reconnects and daemon restarts.
//!
//! FUSE callbacks are synchronous; the NFS client is async over a single TLS connection, so each
//! callback drives one NFS RPC with `Runtime::block_on`. The session loop runs one call at a time,
//! but the `Filesystem` trait takes `&self`, so shared state lives behind mutexes in [`Core`].
//!
//! Performance: every RPC is a round-trip over TLS to the phone, so we minimise them. `opendir`
//! reads the **whole** directory once (following the NFS cookie) and caches it; `readdir` and
//! `readdirplus` then page out of that cache, and `readdirplus` hands the kernel each entry's
//! attributes so `ls -l` / file managers don't `getattr` every file. Attributes are cached by the
//! kernel for [`TTL`], and large read-ahead is requested in `init`.
//!
//! Robustness:
//! - Every RPC has a timeout. A timeout or a transport error drops the connection (a half-sent RPC
//!   desyncs the single stream); from then on calls fail fast with `EIO` instead of hanging.
//! - A supervisor thread notices the drop (or finds it with a periodic keep-alive `GETATTR`), waits
//!   until the daemon reports the phone connected, redoes the `cmd=1001` handshake with backoff (the
//!   phone's NFS ports change per session) and swaps in the new connection — the mount survives a
//!   phone reconnect or a daemon restart. It gives up (and unmounts) only after [`GIVE_UP`] or when
//!   the daemon reports the phone unpaired.
//! - Inodes are keyed by (parent, name) and remember which connection generation their NFS handle
//!   came from; after a reconnect (or an `NFS3ERR_STALE`) a handle is re-resolved by `LOOKUP` from
//!   its parent, so inode numbers the kernel holds stay valid.
//! - SIGINT/SIGTERM/SIGHUP unmount cleanly (see [`install_signal_handler`]); a stale FUSE mount left
//!   at the mountpoint by a crashed run is lazily unmounted before mounting ([`prepare_mountpoint`]).
//!   A tiny watchdog child holds a pipe from this process and unmounts if it closes without a clean
//!   goodbye, so even `SIGKILL` or a panic cannot leave a stale mount behind.
//! - While the phone is away, `getattr` serves the last known attributes (seeded for the root) with
//!   a short TTL instead of `EIO`. File managers stat every entry of the folder they show, so this
//!   keeps the user's home folder readable while the mount waits to reconnect.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    KernelConfig, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request, Session, SessionUnmounter,
};
use nfs3_types::nfs3::{
    GETATTR3args, LOOKUP3args, Nfs3Option, Nfs3Result, READ3args, READDIRPLUS3args, cookieverf3,
    diropargs3, fattr3, filename3, ftype3, nfs_fh3, nfsstat3, nfstime3,
};
use tokio::runtime::Runtime;

use crate::cert::Identity;
use crate::daemon::{self, PhoneState};
use crate::nfs::{MountTarget, NfsConn, connect};

/// How long the kernel may cache attributes and directory entries.
const TTL: Duration = Duration::from_secs(30);
/// While the phone is away, attributes come from the last seen values; refresh them quickly.
const OFFLINE_TTL: Duration = Duration::from_secs(1);
/// Fail an NFS call (and drop the connection) rather than hang if the phone stops answering.
const OP_TIMEOUT: Duration = Duration::from_secs(20);
/// The keep-alive probe's timeout (it should notice a dead link well before a user does).
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// How often an idle, connected mount checks the link.
const KEEPALIVE: Duration = Duration::from_secs(20);
/// How often a disconnected mount asks the daemon whether the phone is back.
const STATUS_POLL: Duration = Duration::from_secs(2);
/// Bound on a reconnect's TLS + MOUNT step (the handshake has its own 10 s bound).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Unmount after the phone has been unreachable this long.
pub const GIVE_UP: Duration = Duration::from_secs(10 * 60);
/// Safety bound on the `opendir` cookie loop (no real phone directory needs this many pages).
const MAX_DIR_PAGES: usize = 4096;

/// Where a new NFS endpoint comes from after a connection loss.
pub enum Source {
    /// Redo the `cmd=1001` handshake through the daemon (the normal case).
    Daemon,
    /// Fixed endpoint from `--ip/--mount-port/--nfs-port/--fp` (testing): reconnect to it as is.
    Fixed(MountTarget),
}

// ── Mountpoint helpers ─────────────────────────────────────────────────────

/// An error that means a FUSE mount whose server is gone ("Transport endpoint is not connected").
pub fn is_stale_error(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOTCONN) | Some(libc::ECONNABORTED)
    )
}

/// `statfs(dir)`. The stale-mount test: unlike `stat`, which the kernel answers from cached
/// attributes for up to the attribute TTL after the helper died, `statfs` always goes to the FUSE
/// server and fails with `ENOTCONN` at once when it is gone. (Our own helper answers it locally,
/// without a round-trip to the phone.)
pub fn statfs(dir: &Path) -> io::Result<()> {
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a valid NUL-terminated path and a zeroed, writable statfs buffer.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut buf) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `dir` is a FUSE mount whose helper is gone.
pub fn is_stale(dir: &Path) -> bool {
    statfs(dir).is_err_and(|e| is_stale_error(&e))
}

/// `fusermount3 -u DIR`, and `-uz` (lazy) if that fails (busy, or already stale).
fn fusermount_unmount(dir: &Path) {
    let ok = Command::new("fusermount3")
        .arg("-u")
        .arg(dir)
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        let _ = Command::new("fusermount3").arg("-uz").arg(dir).output();
    }
}

/// How often the watchdog checks that the mount still answers.
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
/// How long a check may take. One FUSE call may legitimately take up to [`OP_TIMEOUT`] (a read, or
/// a directory page), so a mount that has not answered within this long is really wedged.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(25);

/// Why the watchdog stopped watching.
#[derive(Debug, PartialEq)]
enum WatchEnd {
    /// The helper wrote `done` and closed the pipe: it unmounted on purpose.
    Clean,
    /// The pipe closed without `done`: the helper died (killed, crashed, panicked).
    Crashed,
    /// The helper is alive but the mount stopped answering.
    Wedged,
}

/// Watches the mount helper and lazily unmounts `dir` if the mount stops working.
///
/// The mount helper spawns this as a tiny child and keeps its stdin open for as long as it lives.
/// If the helper is killed (even with SIGKILL), crashes or panics, the pipe closes without the
/// `done` marker and the mount is released — so no "Transport endpoint is not connected" FUSE mount
/// is left in the user's home. A clean exit writes `done` first, so a mount that was already
/// replaced by a new helper at the same path is not touched.
///
/// A helper can also stay alive while its FUSE thread is wedged (a stuck RPC, a deadlock): the mount
/// then answers nothing, and every program that stats the folder — a file manager showing the home
/// folder, for one — hangs. So while the helper is alive this also probes the mount with `statfs`
/// and releases it when it stops answering.
pub fn watchdog(dir: &Path) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin().lock();
        let mut seen = Vec::new();
        let _ = stdin.read_to_end(&mut seen);
        let _ = tx.send(seen);
    });
    match watch(&rx, HEALTH_INTERVAL, || answers(dir, HEALTH_TIMEOUT)) {
        WatchEnd::Clean => {}
        WatchEnd::Crashed => fusermount_unmount(dir),
        WatchEnd::Wedged => {
            tracing::warn!(
                "the mount at {} stopped answering; unmounting it",
                dir.display()
            );
            fusermount_unmount(dir);
        }
    }
}

/// The watchdog loop: wait for the helper's pipe to close, checking `healthy` on every tick.
fn watch(
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    interval: Duration,
    mut healthy: impl FnMut() -> bool,
) -> WatchEnd {
    loop {
        match rx.recv_timeout(interval) {
            Ok(seen) if seen.ends_with(b"done") => return WatchEnd::Clean,
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return WatchEnd::Crashed;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !healthy() {
                    return WatchEnd::Wedged;
                }
            }
        }
    }
}

/// Whether `statfs` on `dir` answers within `timeout`. The call runs on its own thread so a wedged
/// FUSE request cannot block the watchdog too (that thread is abandoned when it times out).
fn answers(dir: &Path, timeout: Duration) -> bool {
    let dir = dir.to_path_buf();
    within(timeout, move || statfs(&dir).is_ok()).unwrap_or(false)
}

/// Runs `f` on a thread and waits up to `timeout`; `None` if it did not finish in time.
fn within<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout).ok()
}

/// Starts the [`watchdog`] child for `mountpoint`; dropping the returned pipe ends it.
fn spawn_watchdog(mountpoint: &Path) -> Option<std::process::ChildStdin> {
    let exe = std::env::current_exe().ok()?;
    let mut child = Command::new(exe)
        .arg("mount-watchdog")
        .arg("--mountpoint")
        .arg(mountpoint)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // The helper's stderr is the mount log; a warning about a released mount belongs there.
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| tracing::warn!("could not start the mount watchdog: {e}"))
        .ok()?;
    child.stdin.take()
}

/// Clears a stale FUSE mount a crashed run left at `dir` (lazy unmount), then creates `dir`.
pub fn prepare_mountpoint(dir: &Path) -> Result<()> {
    if is_stale(dir) {
        tracing::warn!(
            "{} is a stale mount (transport endpoint is not connected); unmounting it first",
            dir.display()
        );
        let _ = Command::new("fusermount3").arg("-uz").arg(dir).output();
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating the mountpoint {}", dir.display()))
}

// ── Shutdown: signals and give-up share one unmount path ───────────────────

/// The live mount, so a signal or the supervisor can unmount it.
static ACTIVE: Mutex<Option<(PathBuf, SessionUnmounter)>> = Mutex::new(None);
/// Set when we unmounted on a signal (a clean exit).
static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// Unmounts the active mount, if any (the session loop then returns). Returns whether one was active.
fn unmount_active() -> bool {
    let Some((dir, mut unmounter)) = ACTIVE.lock().unwrap().take() else {
        return false;
    };
    if unmounter.unmount().is_err() || is_stale(&dir) {
        fusermount_unmount(&dir);
    }
    // A busy mountpoint (a shell `cd`'d into it) refuses a plain unmount: detach it lazily.
    let _ = Command::new("fusermount3").arg("-uz").arg(&dir).output();
    true
}

/// Blocks SIGINT/SIGTERM/SIGHUP in this thread (and so in every thread started after it) and waits
/// for them on a dedicated thread: with a mount active it unmounts (the session loop returns and the
/// program exits cleanly); before the mount is up it just exits. Call before any other thread starts.
pub fn install_signal_handler() {
    // SAFETY: plain sigset manipulation and pthread_sigmask on the calling thread.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                let mut sig: libc::c_int = 0;
                // SAFETY: `set` is a valid, initialised sigset blocked in this thread.
                if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                    continue;
                }
                tracing::info!("signal {sig}: unmounting");
                SIGNALLED.store(true, Ordering::SeqCst);
                if !unmount_active() {
                    std::process::exit(128 + sig);
                }
            }
        })
        .expect("spawning the signal thread");
}

// ── Inodes ──────────────────────────────────────────────────────────────────

struct Node {
    fh: nfs_fh3,
    /// The connection generation `fh` came from; another generation means "re-LOOKUP me".
    epoch: u64,
    parent: u64,
    name: Vec<u8>,
    /// Last attributes seen for this inode. Served while the phone is away so metadata calls from
    /// the parent directory (e.g. a file manager listing the home folder) don't fail with `EIO`.
    attr: Option<FileAttr>,
}

/// Inode numbers by (parent, name), so they survive a reconnect that hands out new file handles.
/// Inode 1 is the root.
struct Inodes {
    next: u64,
    nodes: HashMap<u64, Node>,
    by_path: HashMap<(u64, Vec<u8>), u64>,
}

/// What [`Inodes::current`] knows about an inode's handle.
#[derive(Debug, PartialEq)]
enum Handle {
    Valid(nfs_fh3),
    /// From an older connection or marked stale: look `name` up in `parent` again.
    Relookup {
        parent: u64,
        name: Vec<u8>,
    },
}

impl Inodes {
    fn new(root: nfs_fh3, epoch: u64) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            1,
            Node {
                fh: root,
                epoch,
                parent: 1,
                name: Vec::new(),
                attr: None,
            },
        );
        Self {
            next: 2,
            nodes,
            by_path: HashMap::new(),
        }
    }

    fn current(&self, ino: u64, epoch: u64) -> Option<Handle> {
        let n = self.nodes.get(&ino)?;
        Some(if n.epoch == epoch {
            Handle::Valid(n.fh.clone())
        } else {
            Handle::Relookup {
                parent: n.parent,
                name: n.name.clone(),
            }
        })
    }

    /// The inode for `name` in `parent`, refreshing its handle; a new inode the first time.
    fn intern(&mut self, parent: u64, name: &[u8], fh: nfs_fh3, epoch: u64) -> u64 {
        if let Some(&ino) = self.by_path.get(&(parent, name.to_vec())) {
            if let Some(n) = self.nodes.get_mut(&ino) {
                n.fh = fh;
                n.epoch = epoch;
            }
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_path.insert((parent, name.to_vec()), ino);
        self.nodes.insert(
            ino,
            Node {
                fh,
                epoch,
                parent,
                name: name.to_vec(),
                attr: None,
            },
        );
        ino
    }

    /// Remembers the latest attributes for `ino` (used while the phone is unreachable).
    fn remember(&mut self, ino: u64, attr: FileAttr) {
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.attr = Some(attr);
        }
    }

    /// The last attributes seen for `ino`, if any.
    fn attr(&self, ino: u64) -> Option<FileAttr> {
        self.nodes.get(&ino).and_then(|n| n.attr)
    }

    fn set(&mut self, ino: u64, fh: nfs_fh3, epoch: u64) {
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.fh = fh;
            n.epoch = epoch;
        }
    }

    /// The server said the handle is stale: re-resolve it on next use.
    fn invalidate(&mut self, ino: u64) {
        if let Some(n) = self.nodes.get_mut(&ino).filter(|_| ino != 1) {
            n.epoch = 0;
        }
    }

    /// A new connection: its root handle; every other handle becomes re-resolvable.
    fn reset_root(&mut self, root: nfs_fh3, epoch: u64) {
        self.set(1, root, epoch);
    }
}

// ── Reconnect policy ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Step {
    /// Poll again after [`STATUS_POLL`].
    Wait,
    /// Redo the handshake and reconnect now.
    Attempt,
    /// Unmount and exit with this reason.
    GiveUp(String),
}

/// What a disconnected mount does next, given how long it has been down, what the daemon says about
/// the phone, and whether the backoff since the last failed attempt has passed.
fn next_step(down_for: Duration, phone: PhoneState, attempt_due: bool) -> Step {
    match phone {
        PhoneState::Unpaired => Step::GiveUp("the phone is no longer paired".into()),
        _ if down_for >= GIVE_UP => Step::GiveUp(format!(
            "the phone was unreachable for {} minutes",
            GIVE_UP.as_secs() / 60
        )),
        PhoneState::Connected if attempt_due => Step::Attempt,
        _ => Step::Wait,
    }
}

/// Delay before the next attempt after `failures` failed ones: 2, 4, 8, 16, then 30 s.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(failures.min(5)).min(30))
}

// ── Core ────────────────────────────────────────────────────────────────────

/// State shared by the FUSE callbacks and the supervisor thread.
struct Core {
    rt: Runtime,
    /// `None` while disconnected: calls fail fast with `EIO`.
    conn: Mutex<Option<NfsConn>>,
    inodes: Mutex<Inodes>,
    /// Bumped on every reconnect.
    epoch: AtomicU64,
    /// `Some(since)` while disconnected; the supervisor waits on `wake`.
    down: Mutex<Option<Instant>>,
    wake: Condvar,
    /// Why the supervisor unmounted, if it did.
    give_up: Mutex<Option<String>>,
    uid: u32,
    gid: u32,
}

impl Core {
    /// Runs one NFS call on the live connection under `timeout`. `None` when disconnected — and a
    /// timeout or transport error disconnects (the supervisor then reconnects).
    fn rpc<T, E: std::fmt::Debug>(
        &self,
        timeout: Duration,
        f: impl AsyncFnOnce(&mut NfsConn) -> Result<T, E>,
    ) -> Option<T> {
        let mut guard = self.conn.lock().unwrap();
        let conn = guard.as_mut()?;
        // The timeout is created inside the runtime (`async` block): building it outside panics.
        let why = match self
            .rt
            .block_on(async { tokio::time::timeout(timeout, f(conn)).await })
        {
            Ok(Ok(v)) => return Some(v),
            Ok(Err(e)) => format!("{e:?}"),
            Err(_) => format!("no answer in {} s", timeout.as_secs()),
        };
        tracing::warn!(
            "lost the NFS connection to the phone ({why}); reconnecting in the background"
        );
        *guard = None;
        drop(guard);
        let mut down = self.down.lock().unwrap();
        down.get_or_insert_with(Instant::now);
        self.wake.notify_all();
        None
    }

    /// The current NFS handle of `ino`, re-resolved by `LOOKUP` from its parent when it predates the
    /// connection (or was marked stale).
    fn handle(&self, ino: u64) -> Result<nfs_fh3, Errno> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let (parent, name) = match self.inodes.lock().unwrap().current(ino, epoch) {
            None => return Err(Errno::ENOENT),
            Some(Handle::Valid(fh)) => return Ok(fh),
            Some(Handle::Relookup { .. }) if ino == 1 => return Err(Errno::EIO),
            Some(Handle::Relookup { parent, name }) => (parent, name),
        };
        let dir = self.handle(parent)?;
        let args = LOOKUP3args {
            what: diropargs3 {
                dir,
                name: filename3::from(name),
            },
        };
        match self.rpc(OP_TIMEOUT, async move |c: &mut NfsConn| {
            c.lookup(&args).await
        }) {
            Some(Nfs3Result::Ok(ok)) => {
                self.inodes
                    .lock()
                    .unwrap()
                    .set(ino, ok.object.clone(), epoch);
                Ok(ok.object)
            }
            Some(Nfs3Result::Err(_)) => Err(Errno::ENOENT), // gone on the phone meanwhile
            None => Err(Errno::EIO),
        }
    }

    /// Runs `op` with `ino`'s handle; on `NFS3ERR_STALE` re-resolves the handle once and retries.
    fn with_fh<T, E>(
        &self,
        ino: u64,
        op: impl Fn(nfs_fh3) -> Option<Nfs3Result<T, E>>,
    ) -> Result<Nfs3Result<T, E>, Errno> {
        for attempt in 0..2 {
            let fh = self.handle(ino)?;
            match op(fh) {
                None => return Err(Errno::EIO),
                Some(Nfs3Result::Err((nfsstat3::NFS3ERR_STALE, _))) if attempt == 0 => {
                    self.inodes.lock().unwrap().invalidate(ino)
                }
                Some(r) => return Ok(r),
            }
        }
        Err(Errno::ESTALE)
    }

    /// Checks an idle connection with a `GETATTR` on the root (a failure disconnects).
    fn probe(&self) {
        let root = match self
            .inodes
            .lock()
            .unwrap()
            .current(1, self.epoch.load(Ordering::SeqCst))
        {
            Some(Handle::Valid(fh)) => fh,
            _ => return,
        };
        let _ = self.rpc(PROBE_TIMEOUT, async move |c: &mut NfsConn| {
            c.getattr(&GETATTR3args { object: root }).await
        });
    }

    /// New handshake (or the fixed target), new connection, new generation.
    fn reconnect(&self, id: &Identity, source: &Source) -> Result<()> {
        let target = match source {
            Source::Daemon => self.rt.block_on(daemon::handshake(&id.fingerprint))?,
            Source::Fixed(t) => t.clone(),
        };
        let conn = self
            .rt
            .block_on(async { tokio::time::timeout(CONNECT_TIMEOUT, connect(id, &target)).await })
            .map_err(|_| anyhow!("connecting to the phone's NFS server timed out"))??;
        let root = conn.root_nfs_fh3();
        let mut inodes = self.inodes.lock().unwrap();
        let mut slot = self.conn.lock().unwrap();
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        inodes.reset_root(root, epoch);
        *slot = Some(conn);
        *self.down.lock().unwrap() = None;
        Ok(())
    }

    /// The supervisor loop: keep-alive while connected, reconnect while not, give up when hopeless.
    fn supervise(&self, id: Identity, source: Source) {
        let mut failures = 0u32;
        let mut next_attempt = Instant::now();
        loop {
            let down_since = {
                let guard = self.down.lock().unwrap();
                if guard.is_none() {
                    *self.wake.wait_timeout(guard, KEEPALIVE).unwrap().0
                } else {
                    *guard
                }
            };
            let Some(since) = down_since else {
                self.probe();
                continue;
            };
            let phone = match source {
                Source::Daemon => daemon::phone_state(),
                Source::Fixed(_) => PhoneState::Connected,
            };
            match next_step(since.elapsed(), phone, Instant::now() >= next_attempt) {
                Step::Wait => std::thread::sleep(STATUS_POLL),
                Step::Attempt => match self.reconnect(&id, &source) {
                    Ok(()) => {
                        tracing::info!(
                            "reconnected to the phone (down {} s)",
                            since.elapsed().as_secs()
                        );
                        failures = 0;
                    }
                    Err(e) => {
                        failures += 1;
                        next_attempt = Instant::now() + backoff(failures);
                        tracing::warn!(
                            "reconnect failed ({e:#}); retrying in {} s",
                            backoff(failures).as_secs()
                        );
                    }
                },
                Step::GiveUp(why) => {
                    tracing::warn!("giving up: {why}; unmounting");
                    *self.give_up.lock().unwrap() = Some(why);
                    unmount_active();
                    return;
                }
            }
        }
    }
}

// ── Mount ───────────────────────────────────────────────────────────────────

/// One cached directory entry, with its attributes ready for `readdirplus`.
struct DirEntry {
    ino: u64,
    name: Vec<u8>,
    attr: FileAttr,
}

enum DirErr {
    Stale,
    Io,
}

pub struct PhoneFs {
    core: Arc<Core>,
    /// Directory listings cached between `opendir` and `releasedir`, keyed by the dir file handle.
    dirs: Mutex<HashMap<u64, Arc<Vec<DirEntry>>>>,
    next_handle: AtomicU64,
}

/// Mounts the phone's export at `mountpoint` (read-only) and runs until unmounted (by the user, a
/// signal, or the supervisor giving up — the latter is an error).
pub fn mount(id: Identity, source: Source, target: &MountTarget, mountpoint: &Path) -> Result<()> {
    mount_impl(id, source, target, mountpoint, true)
}

/// [`mount`], with the watchdog child optional (tests run in the test binary, not this helper).
fn mount_impl(
    id: Identity,
    source: Source,
    target: &MountTarget,
    mountpoint: &Path,
    watchdog: bool,
) -> Result<()> {
    let rt = Runtime::new().context("starting the async runtime")?;
    let conn = rt.block_on(connect(&id, target))?;
    let root = conn.root_nfs_fh3();
    // SAFETY: getuid/getgid cannot fail.
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mut inodes = Inodes::new(root, 1);
    // Seed the root's attributes: while the phone is away, `getattr` serves them from this cache
    // instead of failing, so file managers browsing the parent (e.g. the home folder) don't show
    // "Input/output error".
    inodes.remember(1, dir_placeholder(1, uid, gid));
    let core = Arc::new(Core {
        rt,
        conn: Mutex::new(Some(conn)),
        inodes: Mutex::new(inodes),
        epoch: AtomicU64::new(1),
        down: Mutex::new(None),
        wake: Condvar::new(),
        give_up: Mutex::new(None),
        uid,
        gid,
    });
    let fs = PhoneFs {
        core: core.clone(),
        dirs: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(1),
    };
    // AutoUnmount would need AllowOther/AllowRoot (system `user_allow_other`); instead we unmount
    // ourselves on a signal or when giving up, and the watchdog below covers a crash or SIGKILL.
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::FSName("smartconnect-phone".into()),
        MountOption::Subtype("smartconnect".into()),
        MountOption::DefaultPermissions,
    ];
    let mut session = Session::new(fs, mountpoint, &config)
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;
    *ACTIVE.lock().unwrap() = Some((mountpoint.to_path_buf(), session.unmount_callable()));
    tracing::info!("mounted the phone's storage at {}", mountpoint.display());
    // Held until this function returns: if the helper dies first (SIGKILL, panic, OOM), the pipe
    // closes without the `done` marker and the watchdog unmounts, so no stale FUSE mount is left.
    let mut watchdog_pipe = if watchdog {
        spawn_watchdog(mountpoint)
    } else {
        None
    };

    let sup = core.clone();
    std::thread::Builder::new()
        .name("reconnect".into())
        .spawn(move || sup.supervise(id, source))
        .context("starting the reconnect thread")?;

    let result = session.run();
    ACTIVE.lock().unwrap().take();
    // The session is over: make sure the mount is really released before telling the watchdog this
    // was a clean exit. (`-uz` is harmless when it is already gone.)
    fusermount_unmount(mountpoint);
    if let Some(mut pipe) = watchdog_pipe.take() {
        use std::io::Write;
        let _ = pipe.write_all(b"done");
    }
    if let Some(why) = core.give_up.lock().unwrap().clone() {
        return Err(anyhow!("unmounted: {why}"));
    }
    if SIGNALLED.load(Ordering::SeqCst) {
        return Ok(());
    }
    result.with_context(|| format!("the FUSE session at {} ended", mountpoint.display()))
}

impl PhoneFs {
    /// Reads a whole directory (following the NFS cookie across calls), interning inodes and building
    /// ready-to-serve entries. Includes `.` and `..`. Retries once with a fresh handle when stale.
    fn fetch_dir(&self, dir_ino: u64) -> Result<Vec<DirEntry>, Errno> {
        for attempt in 0..2 {
            let dir = self.core.handle(dir_ino)?;
            match self.fetch_dir_once(&dir, dir_ino) {
                Ok(v) => return Ok(v),
                Err(DirErr::Stale) if attempt == 0 => {
                    self.core.inodes.lock().unwrap().invalidate(dir_ino)
                }
                Err(_) => return Err(Errno::EIO),
            }
        }
        Err(Errno::ESTALE)
    }

    fn fetch_dir_once(&self, dir: &nfs_fh3, dir_ino: u64) -> Result<Vec<DirEntry>, DirErr> {
        let core = &self.core;
        let epoch = core.epoch.load(Ordering::SeqCst);
        let mut out = vec![
            DirEntry {
                ino: dir_ino,
                name: b".".to_vec(),
                attr: dir_placeholder(dir_ino, core.uid, core.gid),
            },
            DirEntry {
                ino: 1,
                name: b"..".to_vec(),
                attr: dir_placeholder(1, core.uid, core.gid),
            },
        ];
        let mut cookie: u64 = 0;
        let mut verf = cookieverf3::default();
        for _ in 0..MAX_DIR_PAGES {
            let args = READDIRPLUS3args {
                dir: dir.clone(),
                cookie,
                cookieverf: verf,
                dircount: 32768,
                maxcount: 131072,
            };
            let ok = match core.rpc(OP_TIMEOUT, async move |c: &mut NfsConn| {
                c.readdirplus(&args).await
            }) {
                Some(Nfs3Result::Ok(ok)) => ok,
                Some(Nfs3Result::Err((nfsstat3::NFS3ERR_STALE, _))) => return Err(DirErr::Stale),
                _ => return Err(DirErr::Io),
            };
            verf = ok.cookieverf;
            let eof = ok.reply.eof;
            let entries = ok.reply.entries.into_inner();
            if entries.is_empty() {
                break;
            }
            let mut last = cookie;
            for e in entries {
                last = e.cookie;
                let name = e.name.0.as_ref().to_vec();
                if name == b"." || name == b".." {
                    continue;
                }
                let Nfs3Option::Some(fh) = e.name_handle else {
                    continue; // no handle → we can't address it; skip
                };
                let mut inodes = core.inodes.lock().unwrap();
                let ino = inodes.intern(dir_ino, &name, fh, epoch);
                let attr = match e.name_attributes {
                    Nfs3Option::Some(a) => attr_of(ino, &a, core.uid, core.gid),
                    Nfs3Option::None => reg_placeholder(ino, core.uid, core.gid),
                };
                inodes.remember(ino, attr);
                drop(inodes);
                out.push(DirEntry { ino, name, attr });
            }
            if eof || last == cookie {
                break; // done, or the server isn't advancing the cookie
            }
            cookie = last;
        }
        Ok(out)
    }

    /// The cached listing for an open directory handle, or a fresh fetch as a fallback.
    fn dir_entries(&self, fh: u64, ino: u64) -> Option<Arc<Vec<DirEntry>>> {
        if let Some(e) = self.dirs.lock().unwrap().get(&fh).cloned() {
            return Some(e);
        }
        self.fetch_dir(ino).ok().map(Arc::new)
    }
}

impl Filesystem for PhoneFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> Result<(), std::io::Error> {
        // Ask the kernel to read ahead generously, so large file reads are fewer, bigger round-trips.
        let _ = config.set_max_readahead(1 << 20);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let core = &self.core;
        let epoch = core.epoch.load(Ordering::SeqCst);
        let name = name.as_bytes().to_vec();
        let res = core.with_fh(parent.0, |dir| {
            let args = LOOKUP3args {
                what: diropargs3 {
                    dir,
                    name: filename3::from(name.clone()),
                },
            };
            core.rpc(OP_TIMEOUT, async move |c: &mut NfsConn| {
                c.lookup(&args).await
            })
        });
        match res {
            Ok(Nfs3Result::Ok(ok)) => {
                let Nfs3Option::Some(a) = ok.obj_attributes else {
                    reply.error(Errno::EIO);
                    return;
                };
                let mut inodes = core.inodes.lock().unwrap();
                let ino = inodes.intern(parent.0, &name, ok.object, epoch);
                let attr = attr_of(ino, &a, core.uid, core.gid);
                inodes.remember(ino, attr);
                drop(inodes);
                reply.entry(&TTL, &attr, Generation(0));
            }
            Ok(Nfs3Result::Err(_)) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let core = &self.core;
        // Disconnected: serve the last known attributes instead of EIO. File managers stat every
        // entry of the folder they show, so failing here made the whole home folder unreadable
        // while the phone was away (and the mount was still up waiting to reconnect).
        if core.conn.lock().unwrap().is_none() {
            return match core.inodes.lock().unwrap().attr(ino.0) {
                Some(attr) => reply.attr(&OFFLINE_TTL, &attr),
                None => reply.error(Errno::EIO),
            };
        }
        let res = core.with_fh(ino.0, |object| {
            core.rpc(OP_TIMEOUT, async move |c: &mut NfsConn| {
                c.getattr(&GETATTR3args { object }).await
            })
        });
        match res {
            Ok(Nfs3Result::Ok(ok)) => {
                let attr = attr_of(ino.0, &ok.obj_attributes, core.uid, core.gid);
                core.inodes.lock().unwrap().remember(ino.0, attr);
                reply.attr(&TTL, &attr);
            }
            Ok(Nfs3Result::Err(_)) => reply.error(Errno::EIO),
            Err(e) => {
                // The connection dropped during this call: fall back to the cached attributes.
                if e == Errno::EIO
                    && let Some(attr) = core.inodes.lock().unwrap().attr(ino.0)
                {
                    return reply.attr(&OFFLINE_TTL, &attr);
                }
                reply.error(e);
            }
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.fetch_dir(ino.0) {
            Ok(entries) => {
                let h = self.next_handle.fetch_add(1, Ordering::Relaxed);
                self.dirs.lock().unwrap().insert(h, Arc::new(entries));
                reply.opened(FileHandle(h), FopenFlags::empty());
            }
            Err(e) => reply.error(e),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.dirs.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(entries) = self.dir_entries(fh.0, ino.0) else {
            reply.error(Errno::EIO);
            return;
        };
        for (i, e) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(
                INodeNo(e.ino),
                (i as u64) + 1,
                e.attr.kind,
                OsStr::from_bytes(&e.name),
            ) {
                break; // the reply buffer is full
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let Some(entries) = self.dir_entries(fh.0, ino.0) else {
            reply.error(Errno::EIO);
            return;
        };
        for (i, e) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(
                INodeNo(e.ino),
                (i as u64) + 1,
                OsStr::from_bytes(&e.name),
                &TTL,
                &e.attr,
                Generation(0),
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let core = &self.core;
        let res = core.with_fh(ino.0, |file| {
            core.rpc(OP_TIMEOUT, async move |c: &mut NfsConn| {
                c.read(&READ3args {
                    file,
                    offset,
                    count: size,
                })
                .await
            })
        });
        match res {
            Ok(Nfs3Result::Ok(ok)) => reply.data(ok.data.as_ref()),
            Ok(Nfs3Result::Err(_)) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }
}

fn attr_of(ino: u64, a: &fattr3, uid: u32, gid: u32) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: systime(&a.atime),
        mtime: systime(&a.mtime),
        ctime: systime(&a.ctime),
        crtime: systime(&a.ctime),
        kind: ftype_to_kind(a.type_),
        perm: (a.mode & 0o7777) as u16,
        nlink: a.nlink.max(1),
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// A stand-in attribute for a directory entry (`.`/`..` or one the server gave no attrs for); the
/// kernel refreshes it with `getattr` when it actually needs it.
fn dir_placeholder(ino: u64, uid: u32, gid: u32) -> FileAttr {
    placeholder(ino, FileType::Directory, 0o755, 2, uid, gid)
}

fn reg_placeholder(ino: u64, uid: u32, gid: u32) -> FileAttr {
    placeholder(ino, FileType::RegularFile, 0o644, 1, uid, gid)
}

fn placeholder(ino: u64, kind: FileType, perm: u16, nlink: u32, uid: u32, gid: u32) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn systime(t: &nfstime3) -> SystemTime {
    UNIX_EPOCH + Duration::new(t.seconds as u64, t.nseconds)
}

fn ftype_to_kind(t: ftype3) -> FileType {
    match t {
        ftype3::NF3DIR => FileType::Directory,
        ftype3::NF3LNK => FileType::Symlink,
        ftype3::NF3BLK => FileType::BlockDevice,
        ftype3::NF3CHR => FileType::CharDevice,
        ftype3::NF3SOCK => FileType::Socket,
        ftype3::NF3FIFO => FileType::NamedPipe,
        _ => FileType::RegularFile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fh(b: &[u8]) -> nfs_fh3 {
        nfs_fh3 {
            data: b.to_vec().into(),
        }
    }

    #[test]
    fn stale_errors() {
        assert!(is_stale_error(&io::Error::from_raw_os_error(
            libc::ENOTCONN
        )));
        assert!(!is_stale_error(&io::Error::from_raw_os_error(libc::ENOENT)));
        assert!(!is_stale_error(&io::Error::other("x")));
        // A plain folder and a missing one are not stale.
        let dir = std::env::temp_dir();
        assert!(statfs(&dir).is_ok() && !is_stale(&dir));
        assert!(!is_stale(&dir.join("sc-files-no-such-dir")));
    }

    #[test]
    fn a_wedged_health_check_times_out_instead_of_blocking() {
        let start = Instant::now();
        let hung = within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(5));
            true
        });
        assert_eq!(hung, None);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(within(Duration::from_secs(5), || true), Some(true));
    }

    #[test]
    fn a_working_folder_answers_the_health_check() {
        let dir = std::env::temp_dir();
        assert!(answers(&dir, Duration::from_secs(5)));
        assert!(!answers(
            &dir.join("sc-files-no-such-dir"),
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn the_watchdog_tells_a_clean_exit_from_a_crash() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(b"done".to_vec()).unwrap();
        assert_eq!(
            watch(&rx, Duration::from_millis(10), || true),
            WatchEnd::Clean
        );

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(b"bye".to_vec()).unwrap();
        assert_eq!(
            watch(&rx, Duration::from_millis(10), || true),
            WatchEnd::Crashed
        );

        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        assert_eq!(
            watch(&rx, Duration::from_millis(10), || true),
            WatchEnd::Crashed
        );
    }

    #[test]
    fn a_helper_that_stays_alive_but_stops_answering_is_released() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut checks = 0;
        let end = watch(&rx, Duration::from_millis(10), || {
            checks += 1;
            checks < 3
        });
        assert_eq!(end, WatchEnd::Wedged);
        assert_eq!(checks, 3, "the health check runs while the pipe is open");
        drop(tx);
    }

    #[test]
    fn inodes_survive_a_reconnect() {
        let mut ino = Inodes::new(fh(b"root1"), 1);
        let dcim = ino.intern(1, b"DCIM", fh(b"dcim1"), 1);
        let photo = ino.intern(dcim, b"a.jpg", fh(b"a1"), 1);
        assert_eq!(ino.current(photo, 1), Some(Handle::Valid(fh(b"a1"))));

        // New connection: root is fresh, the rest must be looked up again by (parent, name).
        ino.reset_root(fh(b"root2"), 2);
        assert_eq!(ino.current(1, 2), Some(Handle::Valid(fh(b"root2"))));
        assert_eq!(
            ino.current(photo, 2),
            Some(Handle::Relookup {
                parent: dcim,
                name: b"a.jpg".to_vec()
            })
        );
        // The same path keeps its inode number even with a new handle.
        assert_eq!(ino.intern(1, b"DCIM", fh(b"dcim2"), 2), dcim);
        assert_eq!(ino.current(dcim, 2), Some(Handle::Valid(fh(b"dcim2"))));
        // NFS3ERR_STALE: re-resolve; the root is never invalidated.
        ino.invalidate(dcim);
        ino.invalidate(1);
        assert!(matches!(
            ino.current(dcim, 2),
            Some(Handle::Relookup { .. })
        ));
        assert!(matches!(ino.current(1, 2), Some(Handle::Valid(_))));
        assert_eq!(ino.current(999, 2), None);
    }

    /// End to end through the kernel: mount our own NFS-over-TLS server (the PC→phone half of this
    /// crate) with FUSE, kill the server, check calls fail fast instead of hanging, restart it, check
    /// the mount recovers by itself, then unmount the way a signal does. Needs `/dev/fuse` and
    /// `fusermount3`: `cargo test -p smartconnect-files -- --ignored`.
    #[test]
    #[ignore]
    fn mount_survives_a_server_restart() {
        use crate::server;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let base = std::env::temp_dir().join(format!("sc-files-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let share = base.join("share");
        std::fs::create_dir_all(share.join("docs")).unwrap();
        std::fs::write(share.join("hello.txt"), b"hello").unwrap();
        std::fs::write(share.join("docs/note.md"), b"# note").unwrap();
        let server_id = Identity::load_or_create(&base.join("server-id")).unwrap();
        let client_id = Identity::load_or_create(&base.join("client-id")).unwrap();
        let pinned = server::new_pinned();
        pinned
            .write()
            .unwrap()
            .insert(client_id.fingerprint.clone());

        // The "phone": a server on its own runtime, so dropping the runtime kills it and its sockets.
        let start = |port: u16| {
            let rt = Runtime::new().unwrap();
            let (srv_share, srv_pinned) = (share.clone(), pinned.clone());
            let srv_id = Identity::load_or_create(&base.join("server-id")).unwrap();
            let port = rt.block_on(async move {
                let (listener, port) = server::bind(port).await.unwrap();
                let srv = server::NfsServer::new(
                    &srv_id,
                    srv_share,
                    "/PhoneShare".into(),
                    srv_pinned,
                    false,
                    false,
                    false,
                )
                .unwrap();
                tokio::spawn(srv.serve(listener));
                port
            });
            (rt, port)
        };
        let (server_rt, port) = start(0);
        let target = MountTarget {
            ip: "127.0.0.1".into(),
            mount_port: port,
            nfs_port: port,
            fingerprint: server_id.fingerprint.clone(),
            path: "/PhoneShare".into(),
        };
        let mp = base.join("mnt");
        prepare_mountpoint(&mp).unwrap();
        let (t2, mp2) = (target.clone(), mp.clone());
        let mounted = std::thread::spawn(move || {
            mount_impl(client_id, Source::Fixed(t2), &target, &mp2, false)
        });
        let wait_for = |what: &str, f: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(45);
            while !f() {
                assert!(Instant::now() < deadline, "timed out waiting for: {what}");
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        wait_for("the mount", &|| {
            std::fs::read(mp.join("hello.txt")).is_ok_and(|b| b == b"hello")
        });
        // Manual checks of front-end probes against a live mount: SC_E2E_PAUSE_SECS=10 holds it here.
        if let Some(secs) = std::env::var("SC_E2E_PAUSE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            eprintln!(
                "e2e mount live at {} for {secs} s; server port {port}, ids in {}",
                mp.display(),
                base.display()
            );
            std::thread::sleep(Duration::from_secs(secs));
        }

        // The server goes away: a directory listing (never served from a kernel cache) fails, quickly.
        drop(server_rt);
        let t0 = Instant::now();
        assert!(
            std::fs::read_dir(mp.join("docs"))
                .and_then(|d| d.collect::<io::Result<Vec<_>>>())
                .is_err()
        );
        assert!(
            t0.elapsed() < OP_TIMEOUT + Duration::from_secs(2),
            "a dead link must not hang"
        );

        // It comes back on the same port: the mount reconnects and old inodes resolve again.
        let (_server_rt2, _) = start(port);
        wait_for("the reconnect", &|| {
            std::fs::read(mp.join("docs/note.md")).is_ok_and(|b| b == b"# note")
        });
        assert!(std::fs::read_dir(&mp).unwrap().count() >= 2);

        // Unmount like SIGTERM does; the session returns cleanly and nothing stale is left.
        SIGNALLED.store(true, Ordering::SeqCst);
        assert!(unmount_active());
        mounted.join().unwrap().unwrap();
        assert!(
            std::fs::metadata(&mp).is_ok(),
            "the mountpoint is a plain, reachable dir again"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reconnect_policy() {
        let s = Duration::from_secs;
        // Phone back and backoff over: try now.
        assert_eq!(next_step(s(5), PhoneState::Connected, true), Step::Attempt);
        // Backoff not over, phone not connected, or the daemon down: keep waiting.
        assert_eq!(next_step(s(5), PhoneState::Connected, false), Step::Wait);
        assert_eq!(next_step(s(5), PhoneState::Disconnected, true), Step::Wait);
        assert_eq!(next_step(s(5), PhoneState::DaemonDown, true), Step::Wait);
        // Unpaired, or down too long: give up.
        assert!(matches!(
            next_step(s(5), PhoneState::Unpaired, true),
            Step::GiveUp(_)
        ));
        assert!(matches!(
            next_step(GIVE_UP, PhoneState::Connected, true),
            Step::GiveUp(_)
        ));
        // Backoff grows and is capped.
        assert_eq!(
            [1, 2, 3, 4, 5, 9].map(|f| backoff(f).as_secs()),
            [2, 4, 8, 16, 30, 30]
        );
    }
}
