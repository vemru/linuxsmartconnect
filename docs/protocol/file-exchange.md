# File exchange: "fex", tunnelled NFS, and Share Hub

**Status in this project:** phone→PC **file browsing is implemented and verified on a device**
(`smartconnect-files`, a read-only FUSE mount over NFSv3-on-mutual-TLS). The reverse, **PC→phone
sharing (phone mounts the PC), is now implemented but not yet verified with a phone**
(`smartconnect-files serve` — a read-only NFSv3 *server* over the same mutual TLS; see
[PC→phone: `serve`](#pcphone-sharing-serve-implemented-pending-live-test) below). Writing to the
phone is not built. The **fex pull client is implemented and verified on a device** for the app
icons the phone offers on service 17 ([`fex/`](../../fex/), `smartconnect-fex`); clipboard images,
which use the same transfer server, are not wired to it yet. This page documents
the three mechanisms Smart Connect uses to move file *bytes* — file browsing, clipboard
images/files, drag-and-drop and "send files" — reconstructed from the phone app (JADX) and the
Windows binaries (Ghidra), and confirmed live for the File Station / NFS (phone→PC) path.

> **Live-verified correction:** the File Station handshake (`cmd=1001`) is **serviceId 168 on both
> sides**, not 30 — serviceId 30 is the separate **Share Hub** handler (`cmd 5001`); a `cmd=1001`
> sent to 30 is handled by `ShareHubSharing` and dropped. The phone's cert is X.509 **v1**, which
> webpki rejects, so the handshake signature is verified against the pinned cert's public key
> directly (`files/src/tls.rs`).

Tags: **OBSERVED** = read in decompiled code / constants / symbols · **INFERRED** = deduced ·
**UNKNOWN** = named but not recovered. Secrets and addresses are placeholders: `<token>`,
`<fingerprint>`, `<device-id>`, `192.168.1.20`.

> **Big picture.** Earlier (strings-only) notes assumed everything rode a proprietary QUIC tunnel.
> Decompiling the phone shows that **on a LAN both features can avoid the proprietary transport**:
> the NFS file path defaults to a plain **mutual-TLS-over-TCP** mode (`SOCKET_SSL`) — and the **PC
> chooses it**, so the native QUIC tunnel is never required for file access. Clipboard-image bytes go
> over a TCP port the phone advertises, where it runs a small **HTTPS + WebSocket** server (§3). Both
> are implemented in pure Rust in this project. See
> [Implementation readiness](#implementation-readiness).

## The three mechanisms

| Mechanism | Control plane | Byte transport | Used for |
|---|---|---|---|
| **Tunnelled NFSv3** (File Station) | WS **service 168 ↔ 168**: the `cmd=1001` handshake (VERIFIED on a device), plus a `ServerInfo` advert | NFSv3 over either plain **mutual-TLS TCP** (`SOCKET_SSL`) or the native **dt2socket** QUIC tunnel (`SOCKET_DT2`) | Browsing the phone's files, and the phone browsing the PC's |
| **fex / FileEx** | WS **service 2** (`SERVICE_ID_FILE_EX`) | On a LAN: an **HTTPS + WebSocket** server on a TCP port the sender advertises (§3). Remote: the native Mesh P2P tunnel | Clipboard images/files, drag-and-drop, "send files" |
| **Share Hub (control)** | WS **service 30** (`cmd 5001` MOUNT) | — (names exports for an NFS mount) | A separate phone-side path for sharing chosen PC directories; documented, not used by this project (`serve` needs only the 168 handshake) |

The clipboard image **signalling** rides service 9 and is in
[clipboard.md](clipboard.md#image-and-file-clips); the bytes it refers to move over fex, below.

---

## 1. Tunnelled NFSv3 file access

The phone runs a genuine **NFSv3 (RFC 1813)** server (`com.lenovo.nfsserver.*`); the PC is the NFS
client. For PC→phone sharing the roles reverse (PC exports, phone mounts). This is standard NFS, with
a few Lenovo extensions, carried over one of two transports.

### NFS protocol — OBSERVED, standard RFC 1813

- **Programs:** `PORTMAP = 100000` (v2), `MOUNT = 100005` (v3), `NFS = 100003` (v3). The server
  rejects any NFS RPC version other than 3.
- **Standard procedures** (NFS program, dispatched strictly by number): `0 NULL, 1 GETATTR, 2 SETATTR,
  3 LOOKUP, 4 ACCESS, 5 READLINK, 6 READ, 7 WRITE, 8 CREATE, 9 MKDIR, 10 SYMLINK, 11 MKNOD, 12 REMOVE,
  13 RMDIR, 14 RENAME, 15 LINK, 16 READDIR, 17 READDIRPLUS, 18 FSSTAT, 19 FSINFO, 20 PATHCONF,
  21 COMMIT`. MOUNT: `0 NULL, 1 MNT, 3 UMNT, 4 UMNTALL, 5 EXPORT`. PORTMAP: `0 NULL, 1 SET, 2 UNSET,
  3 GETPORT, 4 DUMP, 5 CALLIT`.
- **Lenovo custom NFS procedures** (22–29, optional — a stock client never needs them): `22 READTHUMBNAIL,
  23 READCATEGORYDIR, 24 GETATTRPATH, 25 READPATH, 26 CREATEPATH, 27 REMOVEBATCH, 29 FILECHANGENOTIFY`.
  These add read/stat/create **by path**, batch delete, thumbnails, and a change-notify stream.
- **Root file handle:** `MNT(sharePath)` returns `mountstat 0`, then a **64-byte all-zero** file handle
  (v3; 32 bytes for v1), then zero auth flavours. File handles are standard length-prefixed, 4-byte
  padded `nfs_fh3`.
- **A portmap quirk:** `GETPORT` returns an error result on every 4th call (OBSERVED). The ports are
  already in the advert, so a client can skip portmap.

### The advert — `ServerInfo` (OBSERVED)

The exposing side sends a `ServerInfo` over the control channel (per device id):

| Field | Meaning |
|---|---|
| `mPortMapPort`, `mMountPort`, `mNfsPort`, `mNfsDataPort` | the **runtime** ports for each program (the `ServerInfo` defaults 1025/1058/2049/0 are placeholders, overwritten at start). They are the local ports of the transport sockets. |
| `serverFp` | the server's own cert **SHA-256 fingerprint** — the peer pins this |
| `sharePath` | the exported root (phone: primary external storage) |
| `nfsVersion` | a Lenovo feature-version field (default 1), **not** the NFS RPC version (which is 3) |
| `nfsDependency` | capability bits; `& 16` (`CAPABILITY_NFS_DATA_CHANNEL`) opens a 4th "data" socket |
| `dt2type` | **the transport selector** (see below) |
| `localHost`, `g_nUID`, `g_nGID` | bind address and the uid/gid the server reports |

The device id is the external map key, not a `ServerInfo` field.

### Transport — two modes, selected by `dt2type` (OBSERVED)

`Dt2SocketJni$SOCKET_TYPE`: `SOCKET_TCP = 0`, `SOCKET_MULTITCP = 1`, **`SOCKET_SSL = 2`**,
**`SOCKET_DT2 = 3`**, `SOCKET_MAX = 4`.

- **`SOCKET_SSL = 2` — plain mutual TLS over TCP, LAN-reachable, no native code.** The server binds a
  real `SSLServerSocket` on `localHost:<port>` (the device's LAN IP) with **both** `needClientAuth`
  and `wantClientAuth`. Security is pure `javax.net.ssl`: `SSLContextFactory` + `MyTrustManager` do
  **fingerprint pinning** — the server verifies the client's cert by `SHA-256(DER)` uppercase-hex,
  compared case-insensitively against the fingerprints it was told; it does **not** check a CA chain.
  `serverFp` is the server's ephemeral self-signed cert fingerprint. **This mode is fully
  reimplementable in Rust (rustls + a custom verifier).**
- **`SOCKET_DT2 = 3` — the native QUIC "DataTunnel20" tunnel.** The TLS/encryption is done **inside**
  the native `dt2socket` lib (`QuicSocket`, `3.1.7+quic`); Java only hands it the cert/key PEM and
  `encrypt=true` and then reads/writes already-encrypted bytes. The socket is addressed by
  `(deviceId, port)` and exposes a local loopback port the RPC stack binds to. **Not reimplementable
  from the phone bytecode alone** — needs the native lib (Ghidra) or the shipped PC `dt2socket.dll`.
- `SOCKET_TCP`/`SOCKET_MULTITCP` are plain TCP without TLS.

### Fingerprint exchange (OBSERVED)

Each side generates a self-signed cert, computes `SHA-256(DER)` (uppercase hex), and sends it over
the already-authenticated control channel. The receiver registers the peer's fingerprint
(`NfsServer.addClientFp(<device-id>, <fingerprint>)`, into both the Java pin map used by `SOCKET_SSL`
and the native pin map used by `SOCKET_DT2`). The server advertises its own as `ServerInfo.serverFp`.

### The `cmd=1001` handshake (OBSERVED) — and the PC chooses the transport

File browsing is set up by one handshake on the File Station WS plane (service 168), routed through
`FileStationManager.onReceiveMessage` → `ProtocolMsgHandler.onReceiveMessage(deviceId, networkType,
cmd, msg)`. `cmd == 1001` is the NFS handshake; its JSON `type` picks the direction:

- **`type = 0`** — the sender is the NFS *client*, the receiver becomes the *server*. So **the PC
  sends `type:0` to browse the phone** (phone starts its NFS server).
- **`type = 1`** — a server handshake: the sender is the server. So **the PC sends `type:1` to share
  PC files** (phone mounts the PC).

**The PC chooses the transport, and SSL is the default.** The phone reads `dt2type` from the
handshake, defaults it to `2` (SSL), clamps anything that isn't `3` to `2`, and forces `2` for
relayed links (`networkType` 8/9) and the `127.0.0.1` loopback. So **omitting `dt2type`, or sending
`2`, makes the phone serve NFS over `SOCKET_SSL`** — plain mutual-TLS TCP, no native tunnel. Only an
explicit `dt2type:3` on a real LAN IP selects the native QUIC path. (`fp` vs `fp2` in the handshake
pick which cert the phone pins: SSL uses `fp`, DT2 uses `fp2`.)

#### Envelope (OBSERVED)

`cmd` is **not** a key in the handshake JSON — it is a field of an inner `StationData` frame, and the
handshake JSON is carried (double-encoded, as a string) in that frame's `data`. The outer frame is the
usual connectivity `WsData`:

```jsonc
// outer WsData (serialized keys): serviceId = destination
{ "version": "1", "deviceId": "<src device id>", "serviceId": 168, "srcServiceId": 168,
  "senderId": <n>, "msgId": <n>,
  "data": {                          // inner StationData
    "ver": "1.0", "cmd": 1001,
    "data": "{\"type\":0,\"dt2type\":2,\"nfs_dependency\":24,\"fp\":\"<pc cert fp>\"}"  // handshake JSON, as a string
  } }
```

**File Station is serviceId 168 on both sides** (VERIFIED on device): the PC sends `cmd=1001` to
`serviceId 168` and the phone replies on `serviceId 168`. Do **not** send it to serviceId 30 — that is
the separate **Share Hub** handler (`ShareHubSharing`, `cmd 5001`, §2 below); a `cmd=1001` sent there
is dropped. Include our `fp` in the request (the phone registers it via `addClientFp` to pin our
client cert for the mutual-TLS NFS connection).

#### Request JSON — PC browses the phone (`type:0`), OBSERVED keys

```json
{ "type": 0, "dt2type": 2, "nfs_dependency": 24 }
```
`nfs_dependency` is a capability bitmask (bits: `REMOVE_BATCH=1, HEIC_TO_JPEG=2, DNG_TO_JPEG=4,
PHOTO_LIST_JSON=8, NFS_DATA_CHANNEL=16`); Windows sends `24` (`8|16`). Bit 16 makes the phone return
`nfsDataPort` (the write-back channel); drop it for read-only. The phone pins our cert by the `fp` we
sent in an earlier step of the continuity flow (SSL uses `fp`, DT2 uses `fp2`).

#### Reply JSON — phone → PC (`type:1`), OBSERVED keys (note the irregular casing)

```json
{ "type": 1, "path": "/sdcard", "port": <portmap>, "nfsport": <nfs>, "nfsDataPort": <data?>,
  "fp": "<phone server cert fingerprint>", "deviceId": "<phone device id>", "timestamp": <ms>,
  "screen_state": true, "ip": "<phone lan ip>", "nfs_version": 8, "nfs_capability": 25, "dt2type": 2 }
```
Keys are exactly: `nfsport` (lowercase), `nfsDataPort` (camelCase, present only if the request set bit
16), `nfs_version`/`nfs_capability`/`nfs_dependency` (snake_case). `nfs_version = 8` and
`nfs_capability = 25` are hard-coded literals; `nfs_version` is the Lenovo feature version, not the RPC
version (which is 3). Export root is `/sdcard`.

The PC then presents the cert whose fingerprint it announced, pins the phone's `fp`, opens a TLS TCP
connection to `ip:nfsport`, and runs MOUNT `MNT("/sdcard") → NFSv3` (`port` is the portmap port, which
can be skipped since the ports are in the reply).

**Per-session endpoint (OBSERVED on a device).** The `port`/`nfsport` the phone answers with are
valid for that control session only. After the phone reconnects (or the daemon restarts and the phone
connects again), the old NFS connection is dead and the old ports no longer accept it; a new
`cmd=1001` handshake returns new ones. A client that wants to outlive a reconnect must redo the
handshake rather than reconnect to the remembered ports. `smartconnect-files mount` does this; it
also re-resolves NFS file handles from the previous session by `LOOKUP` from the root rather than
assuming they survive (INFERRED as necessary; the root handle is re-taken from the new `MNT`).

Dispatch note: `cmd` values on this File Station channel include `1001` (handshake), `2001/2002`
(sync/delete), `2003` (unmount), `2008` (screen-state), and smart-share `103–112`; `1001` is the only
mount-setup command, sub-dispatched by `type` (0 client / 1 server).

### Sequence

```mermaid
sequenceDiagram
  participant PC as PC (= smartconnect-linux)
  participant Ph as Phone
  Note over PC,Ph: both connected on the File Station WS plane (serviceId 168)
  PC->>Ph: cmd=1001 {type:0, dt2type:2, fp:<PC cert fp>}   (browse the phone)
  Ph-->>PC: cmd=1001 {type:1, path:"/sdcard", nfs_port, port, fp:<phone cert fp>, nfs_version:8, cap:25}
  Note over PC,Ph: SOCKET_SSL (default) — mutual-TLS TCP, each pins the other's cert fp
  PC->>Ph: TLS connect phone-ip:nfs_port (present cert matching sent fp; pin phone fp)
  PC->>Ph: MOUNT v3 MNT("/sdcard") -> 64-byte zero root FH
  PC->>Ph: NFS v3: GETATTR/FSINFO, LOOKUP/READDIRPLUS, READ
```

### PC→phone sharing: `serve` (IMPLEMENTED, pending live test)

The reverse direction reuses the **same** `cmd=1001` handshake and the **same** `SOCKET_SSL`
mutual-TLS NFS transport, with the roles swapped: the **PC is the NFS server** and the **phone is
the client**. `smartconnect-files serve [--sharedir DIR]` implements it.

- **Trigger (OBSERVED).** A connected phone *auto-sends* `cmd=1001` with `type:0` on serviceId 168
  (`{"type":0, "dt2type":3, "fp":"<phone client-cert fp>", "nfs_version":8, …}`) — it is asking the
  receiver to become the NFS server. No PC-side enable step is needed, in either direction: being
  connected is enough (VERIFIED for browsing the phone).
- **Reply (OBSERVED shape; our values).** The PC answers on serviceId 168 with `cmd=1001` `type:1`:
  ```jsonc
  { "type": 1, "path": "/PhoneShare", "port": <P>, "nfsport": <P>, "nfsDataPort": <P>,
    "fp": "<PC server-cert fp, uppercase hex>", "ip": "<PC LAN IP>",
    "nfs_version": 8, "nfs_capability": 25, "dt2type": 2,
    "deviceId": "<any>", "timestamp": <ms>, "screen_state": true }
  ```
  We **force `dt2type:2` (SSL)** so the phone connects over plain mutual-TLS TCP — the native QUIC
  tunnel (`dt2type:3`) is not implementable, and replying `2` selects SSL (the phone clamps non-`3`
  to `2`). We advertise the **same port** for `port`/`nfsport`/`nfsDataPort`: one TLS listener
  handles portmap + MOUNT + NFS. The PC's LAN IP comes from the daemon status (`pcAddress`), an
  `--ip` override, or a local private IPv4.
- **Pinning.** The PC takes the phone's `fp` from the `type:0` request and pins it (the phone's
  client cert is accepted only by that `SHA-256(DER)` fingerprint, verified exactly like the client
  side — webpki is bypassed because the phone's cert is X.509 v1). The phone pins the PC's `fp` from
  the reply. Then the phone opens a mutual-TLS TCP connection to `ip:nfsport` and speaks standard
  NFSv3 + MOUNT (+ PORTMAP), which the PC serves **read-only** over the shared directory.

```mermaid
sequenceDiagram
  participant Ph as Phone
  participant PC as PC (= smartconnect-files serve)
  Note over PC,Ph: both connected on the File Station WS plane (serviceId 168)
  Ph->>PC: cmd=1001 {type:0, fp:<phone client-cert fp>}   (auto-sent, ~1/s)
  Note over PC: pin the phone fp; reply (throttled)
  PC-->>Ph: cmd=1001 {type:1, path:"/PhoneShare", port/nfsport=<P>, fp:<PC cert fp>, ip, dt2type:2}
  Note over PC,Ph: SOCKET_SSL — mutual-TLS TCP, each pins the other's cert fp
  Ph->>PC: TLS connect PC-ip:<P> (present phone cert matching sent fp; pin PC fp)
  Ph->>PC: MOUNT v3 MNT("/PhoneShare") -> root FH
  Ph->>PC: NFS v3: GETATTR/FSINFO, LOOKUP/READDIRPLUS, READ (read-only)
```

**Implementation (OBSERVED in our code).** The NFS server is `nfs3_server` 0.11 driven over our own
TLS stream (`process_socket` fed a `tokio_rustls::server::TlsStream`); a local-directory read-only
VFS (`files/src/localfs.rs`) maps file handles ↔ paths/inodes and answers
lookup/getattr/read/readdir(plus)/readlink/fsinfo. `files/src/server.rs` builds the rustls
`ServerConfig` with a fingerprint-pinning `ClientCertVerifier`; `files/src/responder.rs` drives the
handshake over the daemon control socket. The full server + mutual-TLS + NFS round-trip (mount,
list, read, and rejection of an unpinned client) is covered by loopback unit tests. **Not yet
verified against a real phone** — the handshake *trigger* for this direction is confirmed by capture,
but the phone's exact reply-field expectations and whether it uses portmap are unconfirmed on the
wire.

---

## 2. Share Hub control plane (service 30 ↔ 168)

Share Hub negotiates **which** directories are exported, then an NFS mount carries the bytes. It is
**separate from** the File Station `cmd=1001` handshake (§1), which needs no Share Hub step in either
direction; this project doesn't use Share Hub. Reconstructed from the phone (the PC is the peer and
the NFS server for this path). Command values below are **OBSERVED** where the
phone emits them, **INFERRED** where only declared.

### Envelope and commands

Every message is a Gson `WsData` frame: `{ver, serviceId (destination), srcServiceId (source),
senderId, deviceId, msgId, data}`. The phone's Share Hub handler is **service 30**; the PC's is
addressed as **service 168** (a request is `srcServiceId 30 → serviceId 168`; the reply swaps them).

Command ids are **strings** on the wire, inside the bean:

| Constant | Value | Tag |
|---|---|---|
| `VERSION` (`ver`) | `"1"` | OBSERVED |
| `CMD_MOUNT` (`cmd`) | `"5001"` | OBSERVED |
| `CMD_UNMOUNT` | `5002` | INFERRED (declared, unused in this build) |
| `CMD_DOWNLOAD` | `5003` | INFERRED |
| `CMD_EDIT` | `5004` | INFERRED |

Only **MOUNT** is actually driven by the phone client; UNMOUNT/DOWNLOAD/EDIT are declared but dead in
this build.

### MOUNT request / response (OBSERVED)

```jsonc
// phone -> PC : WsData{ srcServiceId:30, serviceId:168, data: MountRequest }
{ "ver": "1", "cmd": "5001", "sid": "1000",
  "paths": ["<base64(NO_WRAP) of an absolute path>", ...], "extra": null }

// PC -> phone : WsData{ srcServiceId:168, serviceId:30, data: Response }
{ "ver": "1", "cmd": "5001", "sid": "1000",
  "paths":      ["<base64 echo of each requested path>"],
  "sharePaths": ["<base64 of the NFS export path for each>"],
  "retCodes":   [0, ...],   // 0 = mounted OK; non-zero = failed (ERROR_* codes)
  "extra": "" }
```

- `sid` is a correlation id (a string holding an int ≥ 1000); the reply must echo it.
- All paths are **Base64 with Android flag 2 = `NO_WRAP`**.
- The reply arrays must be **equal length** (`paths`, `sharePaths`, `retCodes`), or the phone drops
  the whole response.
- The phone runs a **strict path-safety check** on every decoded `path` and `sharePath`: it rejects
  empty strings, `../` / `%2e%2e` traversal, NUL bytes, anything outside a character allowlist, a
  `FilenameUtils.normalize()` failure, and reserved Windows device names (`CON`, `PRN`, `AUX`, `NUL`,
  `COM1…`, `LPT1…`). The PC must emit `sharePath` values that pass this, or the phone silently skips
  them.
- On `retCode == 0` the phone builds a `DocumentsContract` document URI
  `authority = Constants.NFS_AUTHORITY`, document id `"<serverIp>:<sharePath>"` (serverIp from the
  NFS advert), i.e. it mounts the PC's NFS export at that path.

`Response.ERROR_*` has 11 codes (`ERROR_NO_ERROR = 0` confirmed behaviourally; the others —
`ERROR_SHARE_*`, `ERROR_UNSHARE_*`, `ERROR_START_*`, `ERROR_STOP_*` — are named but their integer
values were inlined and are **UNKNOWN**).

> Ids 201 and 202 also appear in the Windows UI as internal WebView page numbers. On the wire, the
> phone-facing file-sharing ids are **168** (File Station, both sides) and **30** (Share Hub).

---

## 3. fex / FileEx (service 2)

fex moves clipboard images/files, drag-and-drop and "send files". Smart Connect drives it through the
cgo export `Fex_SendFiles` / `fileTransfer_SendFilesJsonAsync` (PC `winapi.dll`; the phone's JNI
counterpart is `api.Api.fileTransfer_*`), wrapping the Go package
`lenovo.com/sdks/mesh@v0.3.455/cloud-broker/filetransfer` for the cloud path. The Java/APK layer
only (a) picks a port and (b) advertises it; the transfer itself is native Go. On a LAN it is a small
HTTPS + WebSocket protocol, recovered below and implemented in [`fex/`](../../fex/).

### Transport choice — LAN is plain TCP (OBSERVED)

`R4ConnectivityManager.getServicePort(deviceId, protocol)` branches on
`Api.tunnel_IsConnected(deviceId)`:

- **No P2P tunnel (typical LAN):** returns a **direct local TCP port**, a random free port in
  **`[50000, 59999]`** (OBSERVED live: every offer used a new port in that range). The peer connects
  straight to `phone-ip:<port>` over the LAN.
- **P2P tunnel connected (remote):** the same port is reverse-mapped over the native Mesh tunnel
  (`Api.tunnel_Reverse`).

So on a LAN, **fex bytes travel over a direct TCP connection** to an advertised port — not a WebRTC
data channel. On that connection the phone runs a TLS (HTTPS + WebSocket) server, below. Clipboard
uses the TCP branch.

### The advert — `FileExSendData` over service 2 (OBSERVED)

For a clipboard image the phone sends `WsData{serviceId: 2, data: FileExSendData}`:

`FileExSendData { requestId:String, serviceId:int, port:int, fingerprint:String, token:String,
text:String, autoDownload:bool, specifiedPath:bool, filePath:String, errorcode:int }`. For a
clipboard image it carries `requestId = "5"`, `serviceId = 9` (clipboard), `port = <tcp port>`,
`fingerprint = <fingerprint>` — `token` empty, no `fexId`, no `autoDownload`. The PC keys on
`serviceId == 2` and connects to `phone-ip:port`, pinning `fingerprint`. (Related beans in the fex
family: `FileExReceiveData` adds `fexId` + `token` for the receive/PC→phone direction;
`FileExData`/`FileExPc2PhoneBeforeParamsData` are richer variants carrying `ip`/`cmd`.)

### The LAN pull protocol: HTTPS + a WebSocket of text frames (OBSERVED on a device)

Recovered from a live phone (offers triggered at will with service 17 `cmd 2`, the phone's own
`FileTransfer` debug log, and requests made against its server) and the Go symbol and string tables
of the phone's library. Implemented in [`fex/`](../../fex/src/client.rs) and verified end to end for
the service 17 app-icon transfer.

**Who serves.** The *sender* is the server. To send files the phone's Go library
(`FileTransfer/sender`, called through `Api.fileTransfer_SendFiles…`) zips them into its cache as
`<taskId>.zip`, starts an HTTPS server on a random port, and only then reports `onReady(deviceId,
fingerprint, token, port)`, which the Java side turns into the advert (`FileExSendData` on service
2, or the service 17 `cmd 1` app list). The receiver (the PC) connects and pulls.

**TLS.** Plain TLS on the advertised port, no client certificate (OBSERVED: TLS 1.3, ALPN offers
`h2` and `http/1.1`). The server certificate is a fresh self-signed RSA-2048 certificate per
transfer (subject `C=CN, O=Lenovo, OU=MBG, CN=Lenovo`, ~60-day validity). The advert's
`fingerprint` is **lowercase hex `SHA-256(certificate DER)`** (OBSERVED: matches byte for byte), so
the client pins exactly that certificate. Note the WebSocket needs HTTP/1.1, so offer ALPN
`http/1.1` only.

**Routes** (Go `net/http` mux; anything else is `404 page not found`):

| Route | Purpose | Tag |
|---|---|---|
| `GET /websocket` (WebSocket upgrade) | Control channel for one transfer | OBSERVED |
| `GET /download?taskId=<id>[&token=<token>]` | The whole transfer as one zip (`application/zip`, `Accept-Ranges: bytes`, `Range` honoured with `206`) | OBSERVED |
| `GET /downloadFile?taskId=<id>&token=<token>&index=<n>` | Per-file download of the library's other ("direct", unzipped) send mode; answers `does not exist` for a zip task | OBSERVED (route), INFERRED (use) |

An unknown task id on `/download` still gets `200 text/plain` with `download -> taskId <id> does not
exist!`, so check the content, not just the status (OBSERVED).

**Frames.** Each WebSocket text message is one frame:

```text
<kind>:<id>:<name>[?<json object>]
kind = action | ack          id = no whitespace, e.g. Server1001, Client1001
name = versionNegotiation | sendRequest | status | message
```

The phone parses frames with a regex of exactly that shape (OBSERVED in its string table). The
server numbers its actions `Server1001`, `Server1002`, … with one counter for the life of the
server; an `ack` repeats the id of the action it answers. Client actions use `Client<n>` (OBSERVED
accepted: `Client1001`).

**Sequence** (all OBSERVED on a device; payload shapes with placeholders):

```text
PC → phone   TLS (pin = advert fingerprint), GET /websocket, Upgrade: websocket
phone → PC   action:Server1001:versionNegotiation?{"version":1}
PC → phone   ack:Server1001:versionNegotiation?{"version":1}
phone → PC   action:Server1002:sendRequest?{"id":"<taskId>","senderId":"<device-id>","senderName":"",
               "fileName":"<first file>","fileCount":<n>,"mimeType":"*/*","totalSize":<bytes>,
               "file":[{"name":"<file>","mimeType":"image/png","size":17399,"dir":false,"index":0}, …]}
PC → phone   ack:Server1002:sendRequest?{}
PC → phone   (new TLS connection) GET /download?taskId=<taskId>&token=<token>
phone → PC   200 application/zip, Content-Length = totalSize
PC → phone   action:Client1001:status?{"taskId":"<taskId>","type":1,"reason":""}
phone → PC   ack:Client1001:status
```

- `totalSize` is the size of the **zip**, not the sum of the files (OBSERVED: 1 926 928 vs 1 889 780).
- The zip is written in streaming mode: general-purpose flag bit 3 (sizes and CRC in a data
  descriptor), zeros in the local headers, real sizes in the central directory. Icon entries use
  method 0 (Store) (OBSERVED). Entry names are the plain file names, no directories.
- `status.type` (the phone's `server.Status{TaskId, Type, Reason}`): **0** fires the sender's
  `onProgress` with 0 bytes done, **1** fires `onComplete`, **2** and **3** fire `onFailure` with
  `code` = the type and the `reason` text (all OBSERVED by sending each value). This project sends 1
  on success and 2 when its download fails after accepting the offer.
- `sendRequest` is acked with `{}`; the phone does not look at the ack body (OBSERVED: it proceeds
  and drops the request from its pending map).
- The `message` frame name exists in the grammar but was not seen in a zip transfer.

**What the token and fingerprint do** (OBSERVED by experiment): the fingerprint authenticates the
*phone* to the PC (it travels over the already-authenticated control WebSocket). The `token` is
**not checked** by `/download`: a missing or wrong token still returns the zip; the `taskId` (sent
only inside the TLS WebSocket) is what selects the file. The PC's library still sends `&token=` (the
format string exists), so this client does too. Whether `/websocket` restricts the peer address was
not tested.

**What is *not* on this path.** The earlier notes' X25519 → HKDF (`gkhm256v2`) → AEAD with Ed25519
offer and chunk signatures belong to the Lenovo Mesh `cloud-broker/filetransfer` package (its
`file_offer`/`offer_response`, blob and WebRTC code), i.e. the cloud / P2P-tunnel transfer. None of
it appears on the LAN direct path: confidentiality and server authentication there are just TLS 1.3
plus the pinned certificate (OBSERVED: the whole exchange above works with nothing else). Likewise no
binary frame header or magic: the frames are WebSocket text messages.

**Reuse for clipboard images (INFERRED).** A clipboard image's service 2 `FileExSendData` carries
`port` + `fingerprint` (and an empty `token`) from the same `fileTransfer_SendFilesJsonAsync`
family, so it should be pulled with the same client (`smartconnect_fex::client::pull`, or
`smartconnect-fex pull --host … --port … --fingerprint …`). Not yet verified: whether that send
uses the zip mode above or the direct mode behind `/downloadFile` (one request per `index`), which
would need a small addition to the client.

See [clipboard.md](clipboard.md#image-and-file-clips) for the service-9 signalling that precedes this.

---

## Implementation readiness

| Part | Status | Notes |
|---|---|---|
| Share Hub control (svc 30 MOUNT) | **Documented, not used** | Exact JSON, `CMD_MOUNT=5001`, base64 NO_WRAP, path-safety rules all OBSERVED; neither direction of File Station needs it |
| NFSv3 server + client logic | **Ready** | Standard RFC 1813; programs/procs/root-FH OBSERVED; custom procs optional |
| Cert fingerprint exchange + pinning | **Ready** | `SHA-256(DER)` uppercase hex, over the control channel, OBSERVED |
| NFS transport `SOCKET_SSL` (mutual-TLS TCP) | **Ready — confirmed default** | The phone defaults `dt2type` to `2` and clamps non-`3` to `2`; the PC selects it. Pure TLS, reimplementable with rustls + a custom fingerprint verifier |
| `cmd=1001` NFS handshake (svc 168) | **Implemented** | Phone→PC verified on a device (`smartconnect-files mount`); the PC's `type:1` reply for `serve` is built, not verified with a phone |
| NFS transport `SOCKET_DT2` (QUIC tunnel) | **Not needed** | Only if the PC asks for `dt2type:3`; smartconnect-linux simply won't |
| fex LAN transport (which socket) | **Ready** | TCP to an advertised `[50000,59999]` port, OBSERVED |
| fex pull (TLS pin, WebSocket frames, `/download` zip, `status`) | **Implemented, verified on a device** | [`fex/`](../../fex/): `smartconnect-fex` + the daemon's app-icon fetch; see [§3](#the-lan-pull-protocol-https--a-websocket-of-text-frames-observed-on-a-device) |
| fex for clipboard images | **Client ready, not wired** | Same server family (INFERRED); zip vs `/downloadFile` mode to confirm with one image clip |

### Verdict: file access is implementable in pure Rust on a LAN

The transport question is **resolved**. The phone's NFS data path defaults to **`SOCKET_SSL`**
(NFSv3 over plain mutual-TLS TCP, SHA-256 fingerprint-pinned) and the **PC chooses it** via the
`cmd=1001` handshake — the native QUIC "dt2socket" tunnel is only used if the PC explicitly requests
`dt2type:3`, which this project will not. So file browsing is buildable with no native code: an
**NFSv3 client** (mount + browse + read the phone's `/sdcard`) and, for PC→phone, an **NFSv3 server**
answering the same service 168 handshake — both behind **rustls** with a custom verifier that pins the peer's
cert fingerprint and presents a self-signed cert whose fingerprint was announced in the handshake.

### What is still open

1. **The write path.** The mount is read-only: NFS writes to the phone (and the `nfs_dependency`
   bit-16 data channel they may need) are not built.
2. **A live `serve` test.** The PC→phone server answers the phone's own `type:0` on service 168 and
   passes loopback tests, but no phone has mounted it yet. Unconfirmed on the wire: which reply
   fields the phone insists on, and whether it uses portmap.
3. **fex for clipboard images.** The client is done (§3) but isn't wired to service 9 clips; one
   image clip will show whether the phone serves it as a zip or through `/downloadFile`.
4. The one still-inferred NFS literal is `PROTOCOL_TCP`/`UDP` (almost certainly `"tcp"`/`"udp"`).

### Driving the handshake by hand

`smartconnect-files list` does the whole handshake through the daemon and prints the phone's export
root; no flags are needed. To watch the raw frames instead, use the control socket's generic
`send` and `watch` (the generic `send` sets `srcServiceId` equal to `serviceId`, which is what File
Station wants):

```jsonc
// watch for the phone's File Station reply
{"cmd":"watch","services":[168]}
// send the cmd=1001 type:0 request (data = the StationData frame; its data = the handshake JSON string)
{"cmd":"send","service":168,"data":{"ver":"1.0","cmd":1001,"data":"{\"type\":0,\"dt2type\":2,\"nfs_dependency\":24,\"fp\":\"<pc cert fp>\"}"}}
```

The phone replies on service 168 with a `StationData` whose `data` parses to the `type:1` JSON
(`ip`, `nfsport`, `port`, `fp`). `smartconnect-files fingerprint` prints the `fp` to send.

## See also

- [clipboard.md](clipboard.md#image-and-file-clips) — the service-9 signalling for an image/file clip.
- [README.md](README.md#service-ids) — the service-id table.
- [../../TODO.md](../../TODO.md) — the implementation backlog.
