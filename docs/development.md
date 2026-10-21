# Development

How the repository is organised, how to build and test each part, how helpers implement phone
services through the daemon, how to add a service, and how to debug. Read
[Architecture](architecture.md) first for the big picture, and [CONTRIBUTING.md](../CONTRIBUTING.md)
for the documentation and privacy rules every change follows.

- [Repository layout](#repository-layout)
- [Building and testing](#building-and-testing)
- [Running things locally](#running-things-locally)
- [How helpers implement services](#how-helpers-implement-services)
- [Adding a service](#adding-a-service)
- [Protocol pitfalls](#protocol-pitfalls)
- [Debugging](#debugging)

## Repository layout

```
smartconnect-linux/
├── src/                  smartconnectd: daemon + CLI (binary), library crate `smartconnect`
│   ├── main.rs           options, subcommands, task startup
│   ├── server.rs         TLS/WebSocket server, session loop, shared Daemon state, reconnect
│   ├── control.rs        control socket ($XDG_RUNTIME_DIR/smartconnectd.sock)
│   ├── discovery.rs      mDNS (avahi), /readyfor/mdns discovery frames, connect_now
│   ├── ble_connect.rs    Bluetooth LE wake (GATT central), adapter power watcher (read-only)
│   ├── reconnect.rs      auto-reconnect loop, phone presence probe, resume/network watcher
│   ├── ble.rs            experimental BLE peripheral (disabled with --no-ble)
│   ├── identity.rs       ids, self-signed certificate, fingerprint
│   ├── invite.rs         pairing token and QR payload
│   ├── pairing.rs        paired-phone store (paired.json)
│   ├── proto.rs          envelope, advertised serviceVersions / caps / tile cap map
│   ├── clipboard.rs      service 9 + wl-clipboard
│   ├── notifications.rs  services 5/37 → org.freedesktop.Notifications, reply window
│   ├── status.rs         phone status for front-ends
│   ├── sms.rs            service 204 (+ 100)
│   ├── pc_lock.rs        service 22
│   ├── hotspot.rs        service 13
│   ├── webcam.rs         service 3 request detection (the media lives in webcam/)
│   ├── app_icons.rs      service 17 app list → background icon download (the client lives in fex/)
│   └── helpers.rs        supervision of the stream host, Cross Control, display and webcam helpers
├── tests/handshake.rs    plays the phone's side of pairing against an in-process daemon
├── mirror/               smartconnect-mirror: webrtcbin viewer, stream host, app list
├── crosscontrol/         smartconnect-crosscontrol: Wayland edge strip + pointer lock
├── display/              smartconnect-display: phone as display (capture, session, input)
├── webcam/               smartconnect-webcam: phone camera as webcam
├── files/                smartconnect-files: phone storage as a FUSE mount, and serve (a PC
│                         folder for the phone), both NFSv3 over mutual TLS
├── fex/                  smartconnect-fex: client for the phone's fex transfer server (lib + CLI)
├── gui/                  smartconnect-gui: GTK4/libadwaita app
├── noctalia-plugin/      Noctalia Shell plugin (QML)
├── desktop/              reply.qml (embedded into the daemon); mirror_prototype.py (old Python
│                         prototype of the viewer, superseded by mirror/)
├── packaging/            install.sh, uninstall.sh, systemd user unit, launcher + icon,
│                         arch/PKGBUILD
├── docs/                 user and developer documentation; docs/protocol/ = wire protocol
├── .notes/               design log: what / how / why for each change (index.md)
└── CLAUDE.md             the project rules (documentation + privacy), also for coding agents
```

## Building and testing

The repository root is a Cargo workspace. The daemon is the root package; the helpers and the GTK
app are members. They share one `Cargo.lock` and one target directory:

| Package | Directory | Binary | Build-time system libraries |
|---|---|---|---|
| `smartconnect-linux` | `.` | `smartconnectd` | `dbus-1` (BlueZ via `bluer`), a C compiler (`ring`) |
| `smartconnect-mirror` | `mirror/` | `smartconnect-mirror` | GStreamer core, base (video, gl, sdp), bad (webrtc) |
| `smartconnect-crosscontrol` | `crosscontrol/` | `smartconnect-crosscontrol` | `wayland-client` |
| `smartconnect-display` | `display/` | `smartconnect-display` | GStreamer (webrtc, sdp), `wayland-client` |
| `smartconnect-webcam` | `webcam/` | `smartconnect-webcam` | GStreamer (webrtc, sdp) |
| `smartconnect-files` | `files/` | `smartconnect-files` | a C compiler (`ring`). `fuser` is built without default features, so no libfuse headers; `fusermount3` (fuse3) is needed at run time only |
| `smartconnect-fex` | `fex/` | `smartconnect-fex` (and the `smartconnect_fex` library the daemon links) | a C compiler (`ring`) |
| `smartconnect-gui` | `gui/` | `smartconnect-gui` | GTK 4.18+, libadwaita 1.7+ |

```sh
cargo build --release --workspace            # everything
cargo test --release --workspace
cargo clippy --release --workspace
cargo build --release -p smartconnect-mirror # one package (or run cargo inside its directory)
```

- Keep every build **warning-free**.
- Rust 1.88 or newer (edition 2024, let-chains; `gui/` is edition 2021).
- Profiles live in the root `Cargo.toml` only (cargo ignores them in members): `rsa` is optimised
  even in debug builds, and `smartconnect-gui` is stripped in release builds.
- `cargo build` in the root without `--workspace` or `-p` builds only the daemon.
- Shared dependencies get the union of the features the members ask for. For example zbus is built
  with both the daemon's `tokio` backend and the display helper's `async-io` one; zbus 5.19 and
  later picks the backend at run time (tokio only inside a tokio runtime), which is why both
  manifests require `5.19`.
- All eight binaries land next to each other in `target/release/` (or your `CARGO_TARGET_DIR` /
  `.cargo/config.toml` `target-dir`). The daemon and the GTK app look for helpers next to
  themselves before `PATH`, so a daemon or GTK app started from there uses the helpers of the same
  build without installing anything. `SMARTCONNECT_<NAME>_BIN` points one of them at another file
  (see [how the programs find each other](installation.md#how-the-programs-find-each-other)). Never
  hard-code a build directory in code or defaults.
- A git-ignored `.cargo/config.toml` is a good place for a per-checkout `target-dir`, for example
  when the source tree sits on a slow filesystem.
- `packaging/install.sh --link` installs a build and links the Noctalia plugin to the checkout;
  `--no-build` reinstalls without running cargo; `--dry-run` shows what it would do.
- The first daemon start generates an RSA-2048 key. The `rsa` profile override above is why that
  doesn't take minutes in a debug build.

What the tests cover:

| Where | What |
|---|---|
| `tests/handshake.rs` | An in-process daemon on a random port, driven like the phone drives it: parse the QR, pin the certificate by fingerprint, upgrade `/readyfor/`, send the hello, check the verify reply and the service 25 tile message, the key exchange, clipboard negotiation, a wrong token being dropped, the token staying valid for its lifetime plus reconnect without a token, and the Connect-button trigger on `/readyfor/mdns`. |
| `src/*.rs` unit tests | Discovery frame layout (a known-answer vector), avahi output parsing, clipboard negotiation, echo suppression and truncation, notification bodies, fingerprints and icon names, Hotspot and PC lock message shapes, SMS request shapes and thread grouping, helper binary lookup and lock detection. |
| `display/` | Unit tests for input packet decoding, the touch gesture state machine and output parsing. Live tests are `#[ignore]`d. `capture_smoke_*` needs a running niri session (`cargo test --release capture_smoke -- --ignored --nocapture --test-threads=1`), and `smoke_move_pointer` moves the real cursor and needs `CURSOR_LOCATOR` set to a cursor-finding script. |
| `mirror/` | Unit tests for offer trimming, invite handling (repeated, headless action clicks, phone-view invites), window resize requests and ICE candidate selection. |
| `webcam/` | Unit tests for the message shapes, offer parsing and trimming, candidate filtering and loopback device selection. |
| `gui/` | Unit tests for display options ↔ helper flags, `display.json`/`gui.json`, session log parsing, a stand-in display session (start, detect, stop) and the helper lookup order. |
| `crosscontrol/` | No automated tests yet. |

None of these replace a test against a real phone: say in the PR what you tried on a device.

## Running things locally

- **Daemon in the foreground:** stop the service (`systemctl --user stop smartconnectd`), then
  `RUST_LOG=debug cargo run --release --bin smartconnectd -- --no-ble`. It uses the normal state
  directory, so your existing pairing keeps working.
- **A throwaway identity:** `--state-dir /tmp/sc-test --name "Test PC"` makes the phone see a
  different PC with its own pairing. Use it to test pairing flows without touching your real
  pairing, and remove the extra entry on the phone afterwards.
- **Helpers by hand:** start the daemon with `SMARTCONNECT_HELPERS=0` so it doesn't start its own
  stream host, then run `cargo run --release -p smartconnect-mirror -- --host`. A Cross Control
  client started by hand takes over from a running one anyway.
- **GTK app:** `cargo run --release -p smartconnect-gui`. It connects to whichever daemon owns the
  socket.
- **Noctalia plugin:** install it with `packaging/install.sh --link` (or link it by hand, see
  [Installation](installation.md#noctalia-plugin)), so the plugin directory is a symlink into the
  checkout and editing the QML is enough. Reload the plugin to pick up changes. Its helpers are
  found on `PATH`; point its two binary settings at your build directory to test a build you
  haven't installed.

## How helpers implement services

A phone service doesn't need code in the daemon. The daemon relays (see
[the relay model](architecture.md#the-relay-model)):

- `{"cmd":"watch","services":[N]}` streams `{"data":…,"service":N}` lines for every message the phone
  sends on service `N`;
- `{"cmd":"send","service":N,"data":{…}}` sends `data` to the phone on service `N`, wrapped in the
  envelope.

So a helper is a process with two control-socket connections. This is the minimal shape (Rust std,
the same structure as `mirror/` and `crosscontrol/`):

```rust
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use serde_json::{json, Value};

const SVC: i64 = 42; // the service this helper implements

fn main() -> anyhow::Result<()> {
    let path = std::path::Path::new(&std::env::var("XDG_RUNTIME_DIR")?).join("smartconnectd.sock");

    // 1. Subscribe first, so no answer from the phone is missed.
    let watch = UnixStream::connect(&path)?;
    (&watch).write_all(format!("{}\n", json!({"cmd": "watch", "services": [SVC]})).as_bytes())?;

    // 2. A second connection for commands. Drain its replies, or the socket buffer eventually fills up.
    let mut cmd = UnixStream::connect(&path)?;
    let drain = cmd.try_clone()?;
    std::thread::spawn(move || for _ in BufReader::new(drain).lines() {});

    for line in BufReader::new(watch).lines() {
        let v: Value = serde_json::from_str(&line?)?;
        if v["service"] != SVC {
            continue; // the first line is {"ok":true,"message":"watching"}
        }
        if v["data"]["cmd"] == 0 {
            let reply = json!({"cmd": "send", "service": SVC, "data": {"ver": "1", "cmd": 1}});
            cmd.write_all(format!("{reply}\n").as_bytes())?;
        }
    }
    Ok(()) // the daemon closed the stream (it stopped or restarted)
}
```

Notes:

- Every watcher sees every frame of its services, so another instance of your helper sees them
  too. Filter by session id where the protocol has one (`display/` does this), or make the helper
  single-instance (`crosscontrol/` uses a `flock`, `mirror --host` probes its socket).
- When the daemon restarts, the watch stream ends. Exit cleanly or reconnect.
- Small, stateless services that react to the phone can live inside the daemon instead. Subscribe
  to `daemon.from_phone` and answer on `daemon.to_phone`, as `pc_lock.rs` and `hotspot.rs` do, and
  start the task from `main.rs`.

## Adding a service

1. **Document the protocol first** in `docs/protocol/<service>.md`: commands, payload shapes,
   sequences, pitfalls, each fact tagged OBSERVED or INFERRED. Use placeholders for every value
   (see [CONTRIBUTING.md](../CONTRIBUTING.md#privacy-rules)).
2. **Choose where it runs.** Media, Wayland or GStreamer work goes in a helper crate. Small
   request/response logic goes in a daemon module.
3. **Implement it.**
   - In the daemon: a `run(daemon)` task on `from_phone`/`to_phone`, spawned in `main.rs`.
   - As a helper: a new crate using `watch`/`send` as shown above.
4. **Advertise it** in `src/proto.rs`, and only once the PC side works:
   - `service_versions()`: the phone only brings a service up if the PC lists it in the handshake
     reply.
   - `service_caps()`: per-service capability levels, where the protocol needs them.
   - `device_status_caps()`: only for services with a **tile** on the phone. A tile appears only
     if the service is in both `service_versions()` and this map. Every service 25 message replaces
     the whole map and the phone stores it, so the map must always be complete. The static map uses
     the value 1 ("on"). A helper that needs to flag its own tile (Cross Control marks it "in use")
     sends a service 25 `cap` object through the control socket, and the daemon merges it into the
     full map (see [`send`](control-socket-api.md#send)).
   These take effect on the next connection. `tests/handshake.rs` checks the messages sent right
   after the handshake, so update it.
5. **If the phone can start the feature** (a tile tap), make sure something answers. Add
   supervision to `src/helpers.rs`, following `answer_cross_control` or `answer_webcam`: spawn the
   helper on the phone's request when none is running (check its lock first), and reply with an
   error if it can't be started, so the phone doesn't just time out.
6. **Front-end hooks:** new control commands go in `control.rs` (`json_command`) and must be
   documented in [control-socket-api.md](control-socket-api.md). GTK and Noctalia controls go in
   `gui/` and `noctalia-plugin/`.
7. **Tests:** unit tests for message shapes and parsing, plus a handshake test if the session
   changes.
8. **Documentation, in the same change:** a `.notes/<feature>.md` entry (what/how/why) plus a line
   in `.notes/index.md`, the feature's section in [usage.md](usage.md) with its verification
   status, the README feature matrix, and a line in `CHANGELOG.md` under `[Unreleased]`.

## Protocol pitfalls

Mistakes that cost the most debugging time so far. The full details are in the
[protocol pages](protocol/README.md).

- **The phone's JSON parsing is strict about types and fails silently.** Sending `0` where the
  phone expects a boolean makes its parser throw inside a background task. Nothing is logged, and
  the feature times out about 15 s later. Copy the exact types.
- **Some messages have lasting effects.** Clipboard `msgId` 101 disables the clipboard for this PC
  on the phone persistently. Never send clipboard PULL for text, msgIds 13–17 or
  `shareHubMountInfo`. A plaintext discovery frame (type 2) makes the phone unpair.
- **Tiles depend on the service 25 `cap` map**, not only on `serviceVersions`, and each message
  replaces the whole map.
- **Identity fields must be stable and non-empty.** An empty `sn` once made the phone's database
  refuse to commit the pairing. Recovering needed a new PC identity.
- **ICE:** offer only LAN host candidates. The phone tries every candidate you send, including
  Docker and VPN addresses.
- **Video:** the phone lists H.265 first. Trim the offer to H.264 before answering.
- **Stream instance id:** the phone drops all stream sessions when a start request carries a
  different `instanceId`. Every mirror process shares one id per login session.

## Debugging

| Tool | How |
|---|---|
| Daemon log | `RUST_LOG=debug` (or `RUST_LOG=smartconnect=debug,info`); `journalctl --user -u smartconnectd -f` under systemd. |
| Session logs | Start the daemon with `--session-log`: `~/.local/share/smartconnect-linux/sessions/*.jsonl` then gets every frame in and out of a session, with secrets and personal text redacted (`server.rs::redact`). [jq recipes](troubleshooting.md#where-the-logs-are). |
| Live traffic | `echo '{"cmd":"watch","services":[15]}' \| socat -t 86400 - UNIX-CONNECT:$XDG_RUNTIME_DIR/smartconnectd.sock` |
| Poking the phone | `send` on the control socket. Read [the warnings](control-socket-api.md#send) first. |
| Phone side | `adb shell setprop log.tag.MotoDesktopLog DEBUG`, restart Smart Connect, then `adb logcat` ([details](troubleshooting.md#phone-side-debug-logs)). This is how silent phone-side failures were found. |
| GStreamer | `GST_DEBUG='3,webrtc*:5'` on the viewer, display or webcam helper. The viewer also honours `MIRROR_DECODER` and `MIRROR_SINK`. |
| Helper output | Daemon-started helpers log to `$XDG_RUNTIME_DIR/smartconnectd/<name>.log`. Run them in a terminal for live output. |
| Phone display | `smartconnect-display --test-pattern` (no capture or input). `SMARTCONNECT_DISPLAY_DEBUG_RESIZE=1280x720` simulates a resolution request from the phone after 15 s, `SMARTCONNECT_DISPLAY_DEBUG_ROTATE=2` a turn to portrait (with `--follow-rotation`). The last SDP offer and answer are in `$XDG_RUNTIME_DIR/smartconnect-display.{offer,answer}.sdp`. To test the GTK app or the plugin without a phone, point `SMARTCONNECT_DISPLAY_BIN` (or the plugin's helper setting) at a stand-in script, as described in `.notes/display-frontend-ui.md`. |

Logs and captures from your own phone contain personal data. Never commit them or paste them into
issues unredacted (see [CONTRIBUTING.md](../CONTRIBUTING.md#privacy-rules)).
