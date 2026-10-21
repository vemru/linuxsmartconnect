# ICE candidate filter for the stream host/viewer — 2026-10-21

## What

`smartconnect-mirror` used to offer the phone only local ICE candidates containing `"192.168."`,
which broke every user on a 10.x or 172.16-31.x LAN (open-source blocker). Now
`mirror/src/ice.rs` (`CandidateFilter`) decides, per session:

1. `SMARTCONNECT_ICE_ADDRESSES` (comma-separated addresses and/or IPv4 CIDRs, or `all`) wins:
   the same variable and syntax as `smartconnect-display` (`display/src/ice.rs`), so one setting
   covers both;
2. else only the daemon's `pcAddress` (status field: the local address of the control WebSocket
   the phone reached), fetched with a plain `status` request when the session is created;
3. else RFC 1918 IPv4 (10/8, 172.16/12, 192.168/16) minus addresses of virtual interfaces
   (docker*, br-*, veth*, virbr*, vnet*, tun*/tap*, wg*, tailscale*, zt*, …) from
   `ip -j -4 addr show`.

Only UDP candidates are offered (the phone disables TCP candidates). The log line
`[sid] new session: … ICE: offering …` says which rule applied.

## Why

- `pcAddress` is the one address known to work: the phone already reached it. Multi-NIC PCs (Wi-Fi
  and Ethernet on the same LAN, several Docker bridges in 172.16/12) no longer leak extra
  candidates.
- The fallback covers an older daemon or a missing status; `ip -j` needs no new crate. Without
  `ip`, all private IPv4 is offered.
- Filtering only changes what we signal; libnice still checks from every local address, so the
  phone can still find us as a peer-reflexive candidate if the filter is too strict.
- The env override is the escape hatch for unusual setups (e.g. a VPN to the phone).

Only `mirror/` was changed here; `display/` has its own `ice.rs` with the same rules (written in
parallel by its owner), which is why the variable name and syntax were aligned with it.

Unit tests: `ice::tests` (address/transport parsing, each rule, the CIDR override, UDP only,
interface names, `ip -j` and status parsing). Untested on a device.
