# Security hardening before publication

## What

Three gaps found during the documentation review, fixed in the daemon:

1. **Session logs held secrets.** `~/.local/share/smartconnect-linux/sessions/*.jsonl` recorded
   every frame: the verify reply and EXCHANGE_KEY (the per-pair AES key), QR tokens, the hotspot
   passphrase, clipboard text and SMS.
2. **Loose permissions.** The state dir was created with the umask (0755), and `identity.json`,
   `phone-endpoints.json` and `pairing-qr.svg` were 0644. The QR carries a live pairing token, so
   another local user could have paired their own phone while it was valid. `paired.json` and the
   TLS key were already 0600.
3. **Hard-coded `192.168.` ICE filter** in the mirror/app-streaming host and the display helper,
   which broke 10.x and 172.16–31.x networks. The webcam helper was already fine: it uses the
   phone's /24, or private IPv4 when that's unknown.

## How

- `server.rs::redact` now
  - blanks string fields named `key`/`token`/`pwd`/`password`/`passwd`/`secret`/`psk` at any
    depth (as `<redacted N chars>`);
  - keeps only `cmd`/`ver`/`pId` for SMS frames (serviceId or srcServiceId 100/204/205/206);
  - redacts clipboard `text`/`name`/`path`;
  - keeps the existing notification and wallpaper redaction.

  Test: `redact_tests::session_log_drops_keys_tokens_passwords_and_personal_text`.
- `identity::create_private_dir` makes the state dir and the sessions dir 0700.
  `identity::write_private` (0600) is now also used for `identity.json`, the endpoints file and
  the pairing QR, and it re-applies 0600 to files created by older versions.
- `status.pcAddress` is the local IP of the phone's control connection (`tcp.local_addr()`).
  The mirror and display helpers filter their local candidates to it, falling back to private
  ranges minus virtual interfaces.

- **Session logs are opt-in:** `smartconnectd --session-log` (off by default; `server::enable_session_logs`).
  Without it, `SessionLog` is a no-op and the "phone verified" log line says so. Logs written by
  older versions (before redaction) can still contain pair keys: delete
  `~/.local/share/smartconnect-linux/sessions/*.jsonl` if they aren't needed.

## Why

The logs exist to work out unknown commands. Command numbers and payload shapes survive
redaction, and secrets don't belong in them. Field-name-based redaction catches new services
without a per-service list. A remaining gap is documented in `docs/security.md`: reconnects are
accepted on the phone's `realDeviceId`, because it's still unknown whether the phone proves
possession of the pair key when it dials back. Fixing that needs protocol research, since we can't
change what the phone sends.
