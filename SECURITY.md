# Security Policy

## Supported Versions

Only the current release branch receives security updates and vulnerability patches.

| Version | Supported          |
| ------- | ------------------ |
| 0.2.x   | :white_check_mark: |
| < 0.2.0 | :x:                |

## Reporting a Vulnerability

Please **do not** open public GitHub issues or discussions for security vulnerabilities.

Instead, please use **GitHub Private Vulnerability Reporting**:
1. Navigate to the repository's **Security** tab.
2. Under "Security overview", click **Report a vulnerability**.
3. Provide a clear description of the vulnerability, reproduction steps, affected component, and environment details.
4. Redact all personal data, private keys, device IDs, real phone numbers, and local IP/MAC addresses from any logs before attaching.

Reports are reviewed promptly, and coordinated disclosure will be arranged with patch verification prior to public release.

---

## Threat Model & Security Architecture

`smartconnect-linux` is an open-source, reverse-engineered implementation of the Motorola Smart Connect protocol designed to run as an unprivileged user-space daemon on Linux.

### 1. Unprivileged Execution (Non-Root Daemon)
- The daemon (`smartconnectd`) and its helper processes (`smartconnect-gui`, `smartconnect-mirror`, `smartconnect-crosscontrol`, `smartconnect-display`, `smartconnect-webcam`, `smartconnect-files`, `smartconnect-fex`) run entirely within the user's unprivileged session (`systemd --user`).
- **No root or sudo privileges** are ever required to run the daemon. Helper utilities only interact with standard user-space APIs (Wayland protocols, PipeWire/PulseAudio, GStreamer, D-Bus, FUSE3).

### 2. Network Boundaries & Listeners
- **Control / WebSocket / FEX (TCP 52020):** The primary service port advertised via mDNS (`_smartconnect._tcp` / `_readyfor._tcp`). It accepts initial pairing requests (`/pair`), WebSocket sessions (`/readyfor/`), and FEX file transfers.
- **NFS File Station (TCP 2049):** Used by `smartconnect-files` for reciprocal phone-to-PC file mounting.
- **LAN Enforcement:** Application-layer checks reject incoming connections from non-private IP addresses. However, TCP 52020 and TCP 2049 must **never** be exposed directly to the public Internet or untrusted public Wi-Fi networks.
- **mDNS Discovery:** Advertises hostname, port, and capabilities on the local subnet. Local broadcast discovery is inherently unauthenticated.

### 3. Trust Model & Input Validation
- **Local Machine Users:** Session keys, RSA private keys, pairing databases (`paired.json`), and SQLite timeline databases are stored with mode `0600` inside `~/.local/share/smartconnect-linux/` (directory mode `0700`), isolating the daemon's state from other unprivileged local users.
- **Peer Device (The Phone):** Input received from the phone (clipboard contents, file transfer paths, notifications, share URLs, control frames) is treated as untrusted:
  - File transfers sanitize destination paths to prevent path traversal outside designated downloads/save directories.
  - FUSE file station enforces path containment and prevents symlink escape.
  - Clipboard transfers filter sensitive tokens and prompt for confirmation on multi-step OTP transfers.
  - URL sharing validates URLs and rejects dangerous URI schemes or authority tricks.
- **Host System:** Outgoing inputs to the host (virtual mouse/keyboard injection, display capture) utilize secure Wayland protocols and portals (`xdg-desktop-portal`), requiring explicit user consent or compositor privileges.

### 4. Pairing & Session Cryptography
- **Pairing:** Handshake uses ephemeral RSA key exchange and AES-GCM session encryption. The pairing exchange requires out-of-band confirmation (QR code scan or PIN code entry).
- **Session Transport:** Frame payloads are encrypted with AES-256-GCM.
- **Reconnect Authentication:** In accordance with the Motorola Smart Connect wire protocol specification, reconnect handshakes on `/readyfor/` match the phone's persistent `realDeviceId` against the paired database. Reconnect attempts enforce per-IP exponential backoff jitter to prevent brute-force attacks.

### 5. Deployment Recommendations
- Use firewall rules (`firewalld`, `ufw`, or `nftables`) to restrict TCP 52020 and TCP 2049 to your trusted local network interface (e.g. home or office Wi-Fi).
- On untrusted public networks (airports, cafes, hotels), pause or stop the daemon (`systemctl --user stop smartconnectd`).
- If you lose or retire a paired phone, revoke its pairing immediately in the GUI settings or by removing it with `smartconnectd pair --unpair <device-id>`.
