# Rung 4 — pairing (QR + control WebSocket) and the BLE blocker

## What was built
A Rust daemon (`smartconnect-linux`) that presents itself to the unmodified Motorola Smart
Connect phone app as a current Windows PC:
- `identity.rs` — persistent device ids + self-signed RSA-2048 cert; fingerprint SHA-256(cert DER),
  exposed as hex (mDNS) and base64 (QR).
- `invite.rs` — one-shot token, QR payload `motorolardpconnection{"wsData":{…},"version":"3.0.0"}`.
- `server.rs` — TLS 1.3/1.2 server, `/readyfor/` WebSocket upgrade, serviceId-0 MANAGER handshake
  (reply `{ver:3,cmd:0,token:"OK",device{realDeviceId,key}}`), per-session JSONL frame log.
- `proto.rs`, `pairing.rs` — envelope + paired-phone store.
- `tests/handshake.rs` — plays the phone side (cert pin, hello, verify reply, token single-use).

## How it was validated against the real phone (motorola edge 60 stylus, app 09.0.3.013.0)
Scanned the daemon's QR; watched the daemon log + a phone logcat (adb). Two gates were found, in order:

### Gate 1 — version match (SOLVED)
`VersionMatchingDialogActivity.getVersionMatchType`: a PC-class peer (os=6/type=4) is shown
"Ready For is now Smart Connect — upgrade your PC app" unless `verCode >= 700000000`.
- The phone's own Android verCode is `9030130` (from `09.0.3.013.0`, 1-digit tail).
- The genuine Windows PC encodes its build `09.0.3.013.000` (3-digit tail) by concatenating the
  components → **`903013000`** (confirmed: Vue store default `verName:"09.0.2.000.000" ↔ verCode:902e6`
  reveals the encoding; `903013000` appears literally in `SmartConnect.exe`).
- FIX: `invite.rs` sends `verCode=903013000`, `verName="09.0.3.013.000"`. After this the phone
  parses the QR fine (logcat: `RemoteClient{… os=6, type=4, scType=2, verCode=903013000 …}`) and
  proceeds to pair. VERIFIED on device.

### RESOLUTION (2026-10-20, later) — pairing HANDSHAKE works (not yet a listed/usable device); BLE was a red herring
Status honestly: the phone authenticates the daemon as a PC (verifies token, reaches `paired:1`), but the
daemon does NOT yet show up as a device in the phone app's list — the session drops ~0.5 s after the
handshake (no service responses) and loops, so the phone never commits it, and the name collides with the
name of the Windows PC. A listed/usable device needs rung 5 (hold the session). Details below.
A Windows HCI-snoop + logcat capture of a genuine successful pair (`evidence-2026-10-20/ble-capture/CAPTURE-NOTES.md`)
showed **first-pair does NOT use Bluetooth at all**. The phone gets everything from the QR and dials the
Wi-Fi WebSocket directly. The real difference between our failing QR and the genuine one:
- genuine `wsData`: **`cmd:1, ver:2, appVer:"09.0.3.013.000"`** (we had `cmd:0, ver:0`, no appVer).
- `cmd:1` → phone "create ws and as client" → dials `wss://<pc>:<wsPort>/readyfor/`. `cmd:0` → phone tries a
  policy exchange that times out → the misleading `code:128 "Receive gatt data timeout"`.
FIX applied in `invite.rs`: QR `wsData` now sends `cmd:1, ver:2, appVer, i3:1`.

Then two follow-ups made it fully pair on-device:
1. **Firewall:** firewalld resets runtime rules on reboot; the phone got `EHOSTUNREACH` until
   `firewall-cmd --add-port=52020/tcp` (use `--permanent`).
2. **Reconnect auth:** the phone pairs on the handshake (sends `svc 0 cmd 1` with `paired:1`), closes that
   connection, then opens its real **session** connection as a paired device — with a credential that is NOT
   the one-time invite token. Fix: `server.rs` no longer consumes the token (valid for its whole TTL), and
   accepts a connection if the invite token is valid **OR** the device's stable `realDeviceId` is already in
   `paired.json` (now keyed by realDeviceId, not the per-connection nonce). Log shows `accepted … reconnect=true`.

STATE: the phone pairs + verifies + reconnects. The control WS still drops after ~0.5 s because the daemon
doesn't answer the phone's post-handshake service messages (`svc 0 cmd 1`, `svc 1 cmd 31` mirror, `svc 16
cmd 14` input) — so the phone loops reconnecting. Holding a stable session = the next phase (service protocol).

BLE is NOT needed for pairing. If ever implemented it's a post-connect, one-way trigger with the **PC as GATT
client/central** connecting to the phone's 0x5246 server (inverted from `ble-negchannel-spec.md`), writing one
encrypted frame to char 0x0722 — low priority. The `src/ble.rs` advertiser/GATT-server we built is the wrong
role and unnecessary; run with `--no-ble`.

### (SUPERSEDED) Gate 2 — BLE GATT required for first pair
After the QR passes, the phone does NOT connect over Wi-Fi first. `PairServiceHelper` hands the PC
its IP policy over a **Bluetooth GATT** channel and waits for the PC to answer:
```
R4ConnectivityManager: handleWsSendResult - srcServiceId is PairServiceHelper, can trigger connection
R4ConnectivityManager: sendGattForIpPolicy  gattData:{"cmd":0,…,"policies":[{"wifi":["<PC ip>"]}],"wsPort":0}
R4ConnectivityManager: handleSendGattResult isSendSuccess:true,  channel:2         ← QR/local channel
R4ConnectivityManager: handleSendGattResult isSendSuccess:false, channel:10, errorCode:1   ← real BLE to PC
PairServiceHelper: handleFailure - code:128, reason:Receive gatt data timeout
```
Phone dialog: **"Unable to pair — Please check that Bluetooth is turned on for both devices."**
The daemon has no Bluetooth, so the GATT exchange times out. (`channel:2` = the QR-bootstrapped
local channel echoing our own fp back; `channel:10` = the BLE link to the PC.) The real Windows app
ships Bluetooth for this (`BtHci.exe` in its install dir). Evidence: `evidence-2026-10-20/pair-attempt-logcat.txt`.

Earlier Wi-Fi-only retries DID reach the daemon's TCP port but sent non-TLS first bytes
(`InvalidContentType`) or nothing — a fallback probe, secondary to the BLE requirement.

## Conclusion / next
First-time pairing requires the daemon to be a BLE GATT peer speaking Motorola's "negChannel"
protocol (advertise the ReadyFor service, answer GattRequest/GattResponse with the IP policy + token),
then the existing Wi-Fi WebSocket carries the session. Linux BLE is feasible here (BlueZ 5.87,
adapter powered, `bluer` crate / D-Bus GATT). The exact GATT service/characteristic UUIDs and the
request/response byte framing are the unknowns — best obtained from a phone Bluetooth HCI snoop log
of a real successful pair with the Windows app (plus static RE of the APK `negChannel`/beans and
`winapi.dll` `framework/discovery/ble`). See `evidence-2026-10-20/re-reports/phone-protocol-findings.md` §2.5
for the GattRequest/GattResponse beans and tentative UUIDs (0xfc9d advertised service; 0x5246/0x5247).
