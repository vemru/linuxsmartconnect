# smartconnect-linux: rules for every change

This project is published as open source: a Linux peer for Motorola phones running the unmodified
Smart Connect app. Documentation and privacy are part of the definition of done.

## Documentation: update it in the same change as the code

| Where | What goes there |
|---|---|
| `.notes/<feature>.md` | Design log: **What** changed, **How** (approach, key files/functions), **Why** (reasoning, alternatives, constraints). Add a Mermaid diagram when a flow or state machine is easier to see than read. Keep `.notes/index.md` in sync, one line per note. |
| `docs/` | User-facing docs (installation, usage, troubleshooting, architecture, control-socket API, development, security). Update every page the change affects. |
| `docs/protocol/` | Wire protocol per service: commands, payload shapes, sequences, gotchas. Tag facts **OBSERVED** (seen on the wire or in code) or **INFERRED**. |
| `README.md` | Feature matrix (status of each feature) and quick start. |
| `CHANGELOG.md` | One line per user-visible change under `## [Unreleased]`. |

A feature isn't done until its docs say how to use it, what is verified, and what isn't.

## Never put in the repo

- Raw research material: packet captures, logcats, ETL traces, memory dumps, Ghidra/JADX output,
  Motorola APKs or Windows binaries, Smart Connect `config.json` files.
- Real secrets or personal data: pairing keys, tokens, certificate fingerprints, device IDs and
  serials, MAC/IP addresses, phone numbers, contact names, notification or SMS text, Wi-Fi
  SSIDs/passwords. Use placeholders such as `<token>`, `<device-id>`, `192.168.1.20`.
- Copied decompiled code. Describe behaviour in your own words; naming a class or method is fine
  when it helps a reader find it.

## Build and test

The repo root is a Cargo workspace: the daemon is the root package (`smartconnectd`), and
`mirror/`, `crosscontrol/`, `display/`, `webcam/`, `files/`, `fex/` and `gui/` are members sharing one `Cargo.lock`
and one target directory (profiles go in the root `Cargo.toml` only). Run
`cargo build --release --workspace` and `cargo test --release --workspace` (`-p <package>` while
iterating on one crate), and keep builds warning-free. `packaging/install.sh` builds and installs
everything (`--dry-run` to preview).

Programs find each other by `$SMARTCONNECT_<NAME>_BIN`, then the directory of the running binary,
then `PATH` (the Noctalia plugin: its settings, then `PATH`). Never hard-code a home or build
directory path in code, defaults or docs.
