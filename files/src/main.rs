//! `smartconnect-files` — mounts the phone's storage on Linux over Smart Connect's File Station.
//!
//! The phone exposes its storage as an **NFSv3** server over **mutual TLS** (the `SOCKET_SSL`
//! transport, which is the phone's default and the one the PC selects). This helper:
//!   1. gets a self-signed certificate and its SHA-256 fingerprint ([`cert`]);
//!   2. (via the daemon) performs the `cmd=1001` handshake, which returns the phone's NFS port and
//!      its own cert fingerprint;
//!   3. opens a fingerprint-pinned mutual-TLS connection to that port and speaks NFSv3;
//!   4. presents it as a FUSE filesystem.
//!
//! The wire protocol is documented in `docs/protocol/file-exchange.md`. The reverse direction
//! (**PC→phone**, `serve`) exports a local folder read-only as an NFSv3 server over the same
//! fingerprint-pinned mutual-TLS transport ([`server`], [`localfs`], [`responder`]).

mod cert;
mod daemon;
mod fuse;
mod localfs;
mod nfs;
mod responder;
mod router;
mod server;
mod tls;

use std::path::PathBuf;

use anyhow::{Context, Result};
use cert::Identity;
use nfs::MountTarget;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "smartconnect_files=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str);
    match cmd {
        Some("fingerprint") => {
            let dir = flag(&args, "--state-dir")
                .map(PathBuf::from)
                .unwrap_or_else(Identity::default_dir);
            let id = Identity::load_or_create(&dir).context("loading the TLS identity")?;
            // Printed so the daemon / a capture harness can send it as `fp` in the cmd=1001 handshake.
            println!("{}", id.fingerprint);
            Ok(())
        }
        Some("list") => {
            let dir = flag(&args, "--state-dir")
                .map(PathBuf::from)
                .unwrap_or_else(Identity::default_dir);
            let id = Identity::load_or_create(&dir).context("loading the TLS identity")?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                let target = resolve_target(&args, &id).await?;
                nfs::list_root(&id, &target).await
            })
        }
        Some("mount") => {
            // First, before any thread exists: SIGINT/SIGTERM/SIGHUP unmount cleanly.
            fuse::install_signal_handler();
            let dir = flag(&args, "--state-dir")
                .map(PathBuf::from)
                .unwrap_or_else(Identity::default_dir);
            let id = Identity::load_or_create(&dir).context("loading the TLS identity")?;
            let mountpoint = PathBuf::from(req(&args, "--mountpoint")?);
            // A crashed earlier run may have left a stale FUSE mount there: clear it lazily.
            fuse::prepare_mountpoint(&mountpoint)?;
            // Resolve the phone's NFS endpoint (via the daemon handshake) in a short-lived runtime;
            // fuse::mount then opens its own runtime for the mount session.
            let target = {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(resolve_target(&args, &id))?
            };
            // Test endpoints given on the command line are reused as is on reconnect; otherwise the
            // handshake is redone through the daemon (the phone's ports change per session).
            let source = if flag(&args, "--ip").is_some() {
                fuse::Source::Fixed(target.clone())
            } else {
                fuse::Source::Daemon
            };
            fuse::mount(id, source, &target, &mountpoint)
        }
        // Internal: the mount helper's crash watchdog (see `fuse::watchdog`). It unmounts when
        // its stdin closes, which happens even if the helper is killed with SIGKILL.
        Some("mount-watchdog") => {
            let mountpoint = PathBuf::from(req(&args, "--mountpoint")?);
            fuse::watchdog(&mountpoint);
            Ok(())
        }
        Some("serve") => {
            let dir = flag(&args, "--state-dir")
                .map(PathBuf::from)
                .unwrap_or_else(Identity::default_dir);
            let id = Identity::load_or_create(&dir).context("loading the TLS identity")?;
            let share = flag(&args, "--sharedir")
                .map(PathBuf::from)
                .unwrap_or_else(localfs::default_share_dir);
            std::fs::create_dir_all(&share)
                .with_context(|| format!("creating the shared dir {}", share.display()))?;
            let port: u16 = match flag(&args, "--port") {
                Some(s) => s.parse().context("--port")?,
                None => 2049,
            };
            // The export name the phone sees; by default the folder's own name, e.g. `/alice`.
            let label = flag(&args, "--label").unwrap_or_else(|| {
                format!(
                    "/{}",
                    share
                        .file_name()
                        .map_or("share".into(), |n| n.to_string_lossy())
                )
            });
            let ip_override = flag(&args, "--ip");
            let include_hidden = args.iter().any(|a| a == "--include-hidden");
            let read_only = args.iter().any(|a| a == "--read-only");
            let allow_public = args.iter().any(|a| a == "--allow-public")
                || std::env::var("SMARTCONNECT_ALLOW_PUBLIC")
                    .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let opts = ServeOptions {
                share,
                label,
                port,
                ip_override,
                include_hidden,
                allow_delete: !read_only,
                allow_public,
            };
            rt.block_on(serve(id, dir, opts))
        }
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

const USAGE: &str = "\
smartconnect-files — share files with the phone over Smart Connect File Station

USAGE (phone → PC, browse the phone's storage):
  smartconnect-files fingerprint [--state-dir DIR]
      print our TLS cert fingerprint (the `fp` for the cmd=1001 handshake)
  smartconnect-files list [--path /sdcard] [--state-dir DIR]
      list the phone's export root (handshakes with the phone via the daemon)
  smartconnect-files mount --mountpoint DIR [--path /sdcard] [--state-dir DIR]
      mount the phone's storage (read-only) as a FUSE filesystem at DIR

USAGE (PC → phone, let the phone browse a PC folder):
  smartconnect-files serve [--sharedir DIR] [--include-hidden] [--read-only] [--label /NAME] [--port 2049] [--ip ADDR] [--state-dir DIR]
      share DIR (default: ~/SmartConnect) with the connected phone, which browses it in its
      Files app and in Smart Connect's Files tab. The phone can't change files; deleting from
      the Files tab moves them to your Trash (--read-only refuses that too). Hidden (dot) files
      stay invisible unless --include-hidden. --label defaults to the folder's name

All need smartconnectd running and the phone connected; they perform the cmd=1001
File Station handshake over the daemon control socket. A mount survives phone reconnects
and daemon restarts; Ctrl+C / SIGTERM or `fusermount3 -u DIR` unmounts it.
(For offline mount testing, pass --ip/--mount-port/--nfs-port/--fp to skip the handshake.)
";

/// Runs the PC→phone NFS-over-TLS server plus the handshake responder until Ctrl-C (or a fatal
/// error from either half).
/// What `serve` shares and how (from its command-line flags).
struct ServeOptions {
    share: PathBuf,
    label: String,
    port: u16,
    ip_override: Option<String>,
    include_hidden: bool,
    allow_delete: bool,
    allow_public: bool,
}

async fn serve(id: Identity, state_dir: PathBuf, opts: ServeOptions) -> Result<()> {
    let ServeOptions {
        share,
        label,
        port,
        ip_override,
        include_hidden,
        allow_delete,
        allow_public,
    } = opts;
    // Ensure a process-default crypto provider exists (some rustls paths look it up).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let pinned = server::new_pinned();
    let (listener, actual_port) = server::bind(port).await?;
    let ip = responder::resolve_ip(ip_override).await?;
    let srv = server::NfsServer::new(
        &id,
        share,
        label.clone(),
        pinned.clone(),
        include_hidden,
        allow_delete,
        allow_public,
    )
    .context("building the NFS-over-TLS server")?;
    let params = responder::ReplyParams {
        our_fp: id.fingerprint.clone(),
        ip,
        port: actual_port,
        label,
    };

    let mut serve_task = tokio::spawn(srv.serve(listener));
    let mut responder_task =
        tokio::spawn(async move { responder::run(params, pinned, &state_dir).await });
    responder::announce_tile(true).await;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;

    let result = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C, shutting down the file share");
            Ok(())
        }
        _ = term.recv() => {
            tracing::info!("received SIGTERM, shutting down the file share");
            Ok(())
        }
        r = &mut serve_task => r.context("the NFS server task panicked")?,
        r = &mut responder_task => r.context("the handshake responder task panicked")?,
    };
    responder::announce_tile(false).await;
    result
}

/// Where the phone's NFS server is: from explicit `--ip`/`--mount-port`/`--nfs-port`/`--fp` flags
/// (useful for testing), or — the normal case — from the `cmd=1001` handshake via the daemon.
async fn resolve_target(args: &[String], id: &Identity) -> Result<MountTarget> {
    if let Some(ip) = flag(args, "--ip") {
        return Ok(MountTarget {
            ip,
            mount_port: req(args, "--mount-port")?.parse().context("--mount-port")?,
            nfs_port: req(args, "--nfs-port")?.parse().context("--nfs-port")?,
            fingerprint: req(args, "--fp")?,
            path: flag(args, "--path").unwrap_or_else(|| "/sdcard".into()),
        });
    }
    daemon::handshake(&id.fingerprint)
        .await
        .context("the File Station handshake with the phone failed")
}

/// Returns the value after `name` in `args`, if present (`--flag value` form).
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Like [`flag`], but errors if the flag is missing.
fn req(args: &[String], name: &str) -> Result<String> {
    flag(args, name).with_context(|| format!("missing required argument {name}"))
}
