# smartconnect-linux documentation

smartconnect-linux lets the unmodified Motorola Smart Connect app treat a Linux desktop as a Windows
PC. The [README](../README.md) has the pitch, the feature status matrix and a quick start. These
pages go into detail.

## For users

| Page | What it covers |
|---|---|
| [Installation](installation.md) | Dependencies (Arch package names, plus Debian and Fedora hints), `packaging/install.sh`, building by hand, the Arch package, how the programs find each other (`SMARTCONNECT_*_BIN`), the systemd user service, firewall rules, the Noctalia plugin and the GTK app. |
| [Usage](usage.md) | Pairing, connecting and disconnecting (from the phone, the desktop and the CLI), each feature with its verification status (including browsing the phone's files and sharing a PC folder), and the command-line reference for every binary. |
| [Troubleshooting](troubleshooting.md) | Symptom-by-symptom fixes: pairing, missing tiles, the phone not found over Bluetooth, silent mirroring timeouts, phone files (`~/Phone`) and sharing a folder, phone-side debug logs, and where every log file lives. |
| [Security](security.md) | What is exposed on the LAN and locally, how pairing and encryption work, where keys and state are stored (with permissions), what gets logged and redacted, and known gaps. |

## For developers

| Page | What it covers |
|---|---|
| [Architecture](architecture.md) | Processes, the daemon's relay model, the control socket, helper supervision, session lifecycle and data flows, with diagrams. |
| [Control socket API](control-socket-api.md) | The full reference for `$XDG_RUNTIME_DIR/smartconnectd.sock`: every command, request and reply shapes, the `watch`/`subscribe` streams, errors, and examples with `socat` and `nc`. Also covers the stream host's socket. |
| [Development](development.md) | Repository layout, building and testing the Cargo workspace, how helpers implement phone services through the relay, adding a new service, and debugging techniques. |
| [Protocol notes](protocol/README.md) | The Smart Connect wire protocol, one page per service. Facts are tagged OBSERVED or INFERRED. |

Protocol pages by service:
[pairing and discovery](protocol/pairing-and-discovery.md) ·
[device status and tiles](protocol/device-status-and-tiles.md) ·
[clipboard](protocol/clipboard.md) ·
[notifications](protocol/notifications.md) ·
[SMS](protocol/sms.md) ·
[mirroring](protocol/mirroring.md) ·
[app streaming](protocol/app-streaming.md) ·
[Cross Control](protocol/cross-control.md) ·
[PC lock](protocol/pc-lock.md) ·
[Hotspot](protocol/hotspot.md) ·
[phone as display](protocol/phone-as-display.md) ·
[webcam](protocol/webcam.md) ·
[file exchange](protocol/file-exchange.md)

## Elsewhere in the repository

- [CONTRIBUTING.md](../CONTRIBUTING.md): ground rules (documentation and privacy), the PR checklist
  and how to report bugs.
- [CHANGELOG.md](../CHANGELOG.md): user-visible changes.
- [`.notes/`](../.notes/index.md): the design log. Each note records what was built, how and why.
- [`packaging/README.md`](../packaging/README.md): the short guide that `install.sh` copies to
  `~/.local/share/doc/smartconnect-linux/`.

## Where to start

- **New user:** [Installation](installation.md), then the pairing section of [Usage](usage.md#pairing).
- **Something doesn't work:** [Troubleshooting](troubleshooting.md). Collect logs as described in
  [Where the logs are](troubleshooting.md#where-the-logs-are), and remove personal data before you
  share them.
- **Building a front-end or script:** [Control socket API](control-socket-api.md).
- **Adding a phone feature:** [Architecture](architecture.md), [Development](development.md#adding-a-service)
  and the matching [protocol page](protocol/README.md).
