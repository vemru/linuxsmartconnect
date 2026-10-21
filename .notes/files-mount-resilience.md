# Phone files: a mount that survives reconnects, and stale-mount handling

## What
Bug: the Noctalia *Phone files* tile showed *Unmount files* but **Open** did nothing. The
`smartconnect-files mount` helper had died: the daemon had restarted and the phone reconnected,
which changes the phone's per-session NFS ports. It left a **stale FUSE mount** at `~/Phone`
(`ls` → "Transport endpoint is not connected"), which `mountpoint -q` still reports as mounted, so
`xdg-open` failed silently. Fixed CLI first, then GUI and plugin:

1. **`smartconnect-files mount`** (`files/src/fuse.rs`):
   - Keeps the mount up across a phone reconnect or a daemon restart: re-handshakes and swaps in a
     new NFS connection.
   - Fails fast (`EIO`) while the phone is down.
   - Gives up only after 10 min or when the phone is unpaired.
   - Unmounts cleanly on SIGINT/SIGTERM/SIGHUP.
   - Clears a stale mount at the mountpoint before mounting.
2. **GTK Files page** (`gui/src/files.rs`):
   - Probes the mount (live / offline / stale / not mounted / unresponsive) off the main thread.
   - Clears a stale mount (at startup, while mounted, after the helper exits) and remounts with
     auto-mount.
   - **Open** and the quick folders probe first: they remount-then-open a dead mount, and toast
     "offline" or "not responding".
3. **Noctalia tile** (`Services/SmartConnect.qml`, `Panel.qml`):
   - A probe script with timeouts.
   - Opening the panel adopts a live mount or clears a stale one.
   - **Open** probes, remounts if needed, then runs `xdg-open` as a tracked process.
   - Failures go to `filesStatus`.
   - The helper's exit always clears a stale mount.

## How
### Helper: reconnect supervisor
- `Core` (shared by the FUSE callbacks and a `reconnect` thread):
  - `conn: Mutex<Option<NfsConn>>`: `None` = down, and every call then returns `EIO` at once.
  - `rpc(timeout, async-closure)` runs one call. A timeout or transport error sets `conn = None`,
    records `down` and notifies the supervisor's `Condvar`. The `async { timeout(..) }` wrapper
    matters: creating `tokio::time::timeout` outside `block_on` panics (no reactor). The e2e test
    caught exactly that.
- **Supervisor loop**:
  - While up: wait up to 20 s, then a keep-alive `GETATTR` on the root (8 s timeout).
  - While down: `daemon::phone_state()` (blocking `status` on the control socket) feeds
    `next_step(down_for, phone, attempt_due)` → `Wait | Attempt | GiveUp`.
  - Backoff after a failed attempt: 2, 4, 8, 16, then 30 s.
  - Reconnect = `daemon::handshake` + `connect` (20 s), then:
    - bump `epoch`;
    - reset the root handle;
    - clear `down`.
  - For `--ip` test endpoints (`Source::Fixed`) it reconnects to the same target.
- **Handles**: inodes are keyed by `(parent, name)` and carry the `epoch` their handle came from.
  - `handle(ino)` returns the handle if it is from the current epoch. Otherwise it re-resolves it
    with `LOOKUP(name)` in `handle(parent)` (recursive up to the root).
  - `with_fh` retries once after `NFS3ERR_STALE` (invalidating that inode).
  - So inode numbers the kernel holds stay valid whether or not the phone's handles survive a new
    session.
- **Signals**:
  - `install_signal_handler()` blocks SIGINT/TERM/HUP before any thread starts.
  - A `sigwait` thread calls `unmount_active()`: fuser's `SessionUnmounter`, then `fusermount3 -uz`
    if busy or stale.
  - The session loop returns and the process exits 0.
  - Before the mount is up, the signal just exits.
  - The supervisor's give-up uses the same unmount path and makes `mount()` return an error with
    the reason.

### Stale detection: `statfs`, not `stat`
Found while testing against a real stale mount (helper SIGKILLed):
- **Plain `stat` keeps succeeding** for up to the attribute TTL (30 s): the kernel answers from the
  root's cached attributes. Only calls that reach the server fail with `ENOTCONN`.
- **`statfs`** (`stat -f`) is never cached and fails at once.
- On our live mount fuser answers `statfs` locally (the default reply), so it costs no phone
  round-trip.
- On a frozen helper it hangs, so it always runs with a timeout or off the main thread.

So the stale check is `statfs` everywhere: the helper's `prepare_mountpoint`/`is_stale`, the GUI's
`is_stale`/`classify`, and the plugin's probe and clear scripts. A listing (`read_dir` / `ls`) is
the "deep" check: it reaches the phone, so it tells **offline** (our helper answers `EIO`) apart
from **live**.

| Probe result | Meaning | Front-ends do |
|---|---|---|
| live | listing works | Open opens |
| offline | helper alive, phone away (EIO) | say so; the helper reconnects by itself |
| stale | helper gone (ENOTCONN) | `fusermount3 -uz`, show unmounted, remount if wanted / on Open |
| none | not a mount point | adopted mount released, or remount on Open |
| unresponsive | probe timed out (frozen helper) | say "not responding" |

### GTK Files page
- **Probes** (`check_health`): `probe_async(dir, deep)` runs `gio::spawn_blocking` under
  `glib::future_with_timeout(3 s)`.
  - Shallow (`statfs` + `stat` + mountpoint): every 5 s while mounted.
  - Deep (plus listing and quick-folder existence): when the mount comes up and on every
    Open/quick folder.
  - Quick-folder visibility now comes from the last deep probe: no file-system calls on the main
    thread.
- **Losing the mount** (`apply_health`): a stale or vanished mount goes to `lost_mount`:
  1. reap the helper (SIGTERM if wedged);
  2. `cleanup_mount`;
  3. show the banner;
  4. `maybe_auto_remount`.
- `on_child_exit` always runs `cleanup_mount` (previously skipped for a stale mount, because
  `is_mountpoint` failed on it) and auto-remounts after an unexpected exit, though not after a
  failed mount, to avoid loops.
- `fusermount_unmount` falls back to `-uz`.
- `start_mount` clears a stale mount first (`create_dir_all` failed on one). A live mount by
  another program gets a clear message.

### Noctalia tile
- `filesProbeScript` prints one word, using `stat -f`/`stat`/`mountpoint`/`ls` under `timeout`
  with `LC_ALL=C`.
- `onFilesProbe` drives the state:
  - promote Mounting → Mounted;
  - adopt (`filesExternal`) or release an external mount;
  - clear a stale one;
  - continue a pending Open (`filesOpenAfter`).
- `filesOpener` is a `Process`, so an `xdg-open` failure reaches `filesStatus`.
- `filesHealthTimer` runs a shallow probe every 5 s while mounted.
- The mount wrapper runs `filesClearStaleScript` first. It only clears a mount that `statfs` reports
  as not connected, never someone else's live mount.

## Why
- **Reconnect in the helper rather than respawning it** keeps the mount, the kernel's inode numbers
  and open file-manager windows across a phone hiccup. Respawning would tear the mount down under
  any open directory.
- **Fail fast while down** rather than queueing: a file manager blocked on a hung FUSE call freezes
  the desktop. `EIO` is honest and recovers by itself.
- **(parent, name) inodes plus an epoch** instead of trusting the phone's handles: we can't confirm
  the phone keeps handles stable across sessions, and a wrong handle would serve the wrong file. A
  re-LOOKUP is cheap and lazy (only inodes actually used again).
- **`statfs` over `mountpoint`/`stat`**: the user's bug was exactly that those calls trust a dead
  mount. Measured on a real stale mount: `stat` OK, `mountpoint -q` "mounted", `stat -f` ENOTCONN.

## Verified
- Unit tests: inode epochs and re-lookup, reconnect policy and backoff, daemon `status` parsing,
  `statfs` on plain folders (files crate); `classify` (GUI).
- **e2e** (`cargo test -p smartconnect-files -- --ignored`, needs `/dev/fuse`). It mounts this
  crate's own NFS-over-TLS server through FUSE, then:
  1. kills the server: a listing fails fast;
  2. restarts it: the mount recovers and old inodes resolve;
  3. unmounts like a signal: clean, nothing left in `/proc/mounts`.
- **Manual, with the real binary against that server:**
  - SIGTERM, SIGINT and SIGHUP (the last with a shell inside the mount) each unmount cleanly with
    exit 0.
  - The probe script reports live / unresponsive (SIGSTOPped helper) / stale (SIGKILLed helper).
  - The helper's startup clears a stale mount.
- **Not yet verified with a real phone reconnect**, and the GUI and plugin flows weren't run
  interactively here (no display / shell restart).
