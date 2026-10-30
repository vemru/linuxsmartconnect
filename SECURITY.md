# Security Policy

## Supported Versions

Only the latest release line receives active security updates and vulnerability patches.

| Version | Supported          |
| ------- | ------------------ |
| 0.2.x   | :white_check_mark: |
| < 0.2.0 | :x:                |

## Reporting a Vulnerability

Please **do not** open public GitHub issues or discussions for security vulnerabilities.

Instead, please use GitHub's private vulnerability reporting:
1. Navigate to the repository's **Security** tab.
2. Under "Security overview", click **Report a vulnerability**.
3. Provide a clear description, reproduction steps, affected component/commit, and environment details.
4. Redact all personal data, private keys, device IDs, and local IP/MAC addresses from any logs before attaching.

Reports are reviewed promptly, and coordinated disclosure will be arranged with patch verification prior to public release.

## Known Limitations

### Reconnect Authentication on `/readyfor/`

During WebSocket reconnection on TCP port 52020 (`/readyfor/`), the daemon verifies an already-paired phone by matching its stable `realDeviceId` against the paired database (`paired.json`) without requiring a cryptographic signature or proof of possession of the AES-GCM pairing key.

- **Protocol Context:** This behavior mirrors the official Motorola Smart Connect wire protocol for interoperability with unmodified mobile handsets.
- **Threat Vector:** An attacker with access to your local Wi-Fi / LAN network who observes or learns the phone's static `realDeviceId` could initiate a reconnect handshake and obtain the session's AES-GCM key from the server response.
- **Existing Mitigations:**
  - **Network Isolation:** Keep port 52020 and mDNS accessible only on trusted private networks. Stop the service or block the port via firewalld / ufw when connected to untrusted public Wi-Fi.
  - **Permissions:** `paired.json`, `key.pem`, and credentials are saved with mode `0600` in a `0700` directory (`~/.local/share/smartconnect-linux/`), preventing access by other local users on the same machine.
  - **Log Redaction:** Frame logs are disabled by default; when enabled (`--session-log`), sensitive fields (`key`, `token`, `pwd`, passwords, message contents, SMS) are automatically redacted.
  - **Revocation:** Removing an untrusted or obsolete device from `paired.json` drops its authorization immediately.
