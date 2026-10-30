//! Which of our ICE candidates to offer the phone (svc 15 cmd 20).
//!
//! The phone tries every candidate it is given, so addresses on container bridges, VPNs and other
//! virtual interfaces only delay ICE or make it settle on a dead path. Precedence:
//! 1. `SMARTCONNECT_ICE_ADDRESSES`: comma-separated addresses and/or IPv4 CIDRs (e.g.
//!    `192.168.1.20,10.0.0.0/8`), or `all` (same variable and syntax as `smartconnect-display`).
//! 2. The PC address the phone's control connection reached (daemon status `pcAddress`).
//! 3. Private IPv4 (10/8, 172.16/12, 192.168/16) not owned by a virtual interface (docker*, br-*,
//!    veth*, virbr*, tailscale*, wg*, …), as listed by `ip -j -4 addr`.
//!
//! Only UDP candidates are offered (the phone disables TCP candidates). Filtering only changes what
//! we *signal*: libnice still runs its checks from every local address, so the phone can also
//! reach us as a peer-reflexive candidate.

use std::collections::HashSet;
use std::net::Ipv4Addr;

use serde_json::Value;

pub const ENV_ALLOW: &str = "SMARTCONNECT_ICE_ADDRESSES";

#[derive(Clone, Debug, PartialEq)]
pub enum CandidateFilter {
    /// `SMARTCONNECT_ICE_ADDRESSES=all`.
    Any,
    /// `SMARTCONNECT_ICE_ADDRESSES` entries (address or IPv4 CIDR).
    List(Vec<String>),
    /// Only the address the phone reached us on.
    Address(String),
    /// Private IPv4 not owned by a virtual interface (whose addresses are listed here).
    PrivateLan { virtual_addrs: HashSet<String> },
}

impl CandidateFilter {
    /// `interfaces` is `(interface name, IPv4 address)`; only needed for the private-LAN fallback.
    pub fn new(
        env_allow: Option<&str>,
        pc_address: Option<&str>,
        interfaces: &[(String, String)],
    ) -> Self {
        if let Some(list) = env_allow.map(str::trim).filter(|s| !s.is_empty()) {
            let entries: Vec<String> = list
                .split([',', ' '])
                .map(|e| e.trim().to_owned())
                .filter(|e| !e.is_empty())
                .collect();
            if entries
                .iter()
                .any(|e| e == "*" || e.eq_ignore_ascii_case("all"))
            {
                return Self::Any;
            }
            if !entries.is_empty() {
                return Self::List(entries);
            }
        }
        if let Some(pc) = pc_address.map(str::trim).filter(|s| !s.is_empty()) {
            return Self::Address(pc.to_owned());
        }
        let virtual_addrs = interfaces
            .iter()
            .filter(|(name, _)| is_virtual_interface(name))
            .map(|(_, a)| a.clone())
            .collect();
        Self::PrivateLan { virtual_addrs }
    }

    /// The filter for a new session: environment, then the daemon's `pcAddress`, then the
    /// interface list (only read when needed).
    pub fn current(pc_address: Option<String>) -> Self {
        let env = std::env::var(ENV_ALLOW).ok();
        let env_set = env.as_deref().is_some_and(|e| !e.trim().is_empty());
        let interfaces = if env_set || pc_address.is_some() {
            Vec::new()
        } else {
            local_interfaces()
        };
        Self::new(env.as_deref(), pc_address.as_deref(), &interfaces)
    }

    /// Whether to send this local candidate (`candidate:… <address> <port> typ …`) to the phone.
    pub fn allows(&self, candidate: &str) -> bool {
        if !candidate_transport(candidate).is_some_and(|t| t.eq_ignore_ascii_case("udp")) {
            return false;
        }
        let Some(addr) = candidate_address(candidate) else {
            return false;
        };
        match self {
            Self::Any => true,
            Self::List(entries) => entries.iter().any(|e| entry_matches(e, addr)),
            Self::Address(pc) => addr == pc,
            Self::PrivateLan { virtual_addrs } => {
                is_private_ipv4(addr) && !virtual_addrs.contains(addr)
            }
        }
    }
}

impl std::fmt::Display for CandidateFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Any => write!(f, "every UDP candidate ({ENV_ALLOW}=all)"),
            Self::List(e) => write!(f, "{ENV_ALLOW}={}", e.join(",")),
            Self::Address(a) => write!(f, "only {a} (the address the phone reached)"),
            Self::PrivateLan { virtual_addrs } => {
                write!(
                    f,
                    "private IPv4 except {} virtual-interface address(es)",
                    virtual_addrs.len()
                )
            }
        }
    }
}

/// Field `n` of an ICE candidate line (`candidate:<foundation> <component> <transport> <priority>
/// <address> <port> typ <type> …`, optionally prefixed with `a=`).
fn candidate_field(candidate: &str, n: usize) -> Option<&str> {
    let line = candidate
        .trim()
        .strip_prefix("a=")
        .unwrap_or(candidate.trim());
    if !line.starts_with("candidate:") {
        return None;
    }
    line.split_whitespace().nth(n)
}

/// The connection address of a candidate line.
pub fn candidate_address(candidate: &str) -> Option<&str> {
    candidate_field(candidate, 4)
}

/// The transport (`UDP`/`TCP`) of a candidate line.
pub fn candidate_transport(candidate: &str) -> Option<&str> {
    candidate_field(candidate, 2).filter(|_| candidate_field(candidate, 4).is_some())
}

/// RFC 1918 IPv4 (10/8, 172.16/12, 192.168/16).
pub fn is_private_ipv4(addr: &str) -> bool {
    addr.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_private())
}

/// Interfaces whose addresses the phone can't usefully reach: loopback, container and VM bridges,
/// VPN and overlay tunnels.
pub fn is_virtual_interface(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "lo",
        "docker",
        "br-",
        "veth",
        "virbr",
        "vnet",
        "vboxnet",
        "vmnet",
        "podman",
        "cni",
        "flannel",
        "cali",
        "lxcbr",
        "lxdbr",
        "incusbr",
        "tun",
        "tap",
        "wg",
        "tailscale",
        "zt",
        "nordlynx",
        "proton",
        "mullvad",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

fn entry_matches(entry: &str, addr: &str) -> bool {
    if let Some((net, bits)) = entry.split_once('/') {
        let (Ok(net), Ok(bits), Ok(ip)) = (
            net.parse::<Ipv4Addr>(),
            bits.parse::<u32>(),
            addr.parse::<Ipv4Addr>(),
        ) else {
            return false;
        };
        if bits > 32 {
            return false;
        }
        let mask = if bits == 0 {
            0
        } else {
            u32::MAX << (32 - bits)
        };
        return u32::from(net) & mask == u32::from(ip) & mask;
    }
    addr == entry
}

/// `ip -j -4 addr show` → `(interface, address)` pairs.
pub fn parse_ip_addr_json(text: &str) -> Vec<(String, String)> {
    let Ok(Value::Array(ifaces)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for iface in &ifaces {
        let Some(name) = iface.get("ifname").and_then(Value::as_str) else {
            continue;
        };
        for a in iface
            .get("addr_info")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(local) = a.get("local").and_then(Value::as_str) {
                out.push((name.to_owned(), local.to_owned()));
            }
        }
    }
    out
}

/// This host's IPv4 addresses by interface (empty when `ip` is unavailable).
pub fn local_interfaces() -> Vec<(String, String)> {
    std::process::Command::new("ip")
        .args(["-j", "-4", "addr", "show"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_ip_addr_json(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// `pcAddress` from the daemon's `status` reply.
pub fn pc_address_from_status(status: &Value) -> Option<String> {
    status
        .get("pcAddress")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(addr: &str) -> String {
        format!("candidate:1 1 UDP 2015363327 {addr} 52342 typ host")
    }

    fn ifaces() -> Vec<(String, String)> {
        [
            ("lo", "127.0.0.1"),
            ("wlan0", "192.168.1.20"),
            ("docker0", "172.17.0.1"),
            ("br-0123abcd", "172.18.0.1"),
            ("tailscale0", "100.64.0.5"),
            ("wg0", "10.8.0.2"),
            ("eth0", "10.0.0.7"),
        ]
        .iter()
        .map(|(n, a)| (n.to_string(), a.to_string()))
        .collect()
    }

    #[test]
    fn address_is_the_fifth_field() {
        assert_eq!(
            candidate_address(&cand("192.168.1.20")),
            Some("192.168.1.20")
        );
        let tcp = "a=candidate:2 1 TCP 1015022079 10.0.0.7 9 typ host tcptype active";
        assert_eq!(candidate_address(tcp), Some("10.0.0.7"));
        assert_eq!(candidate_transport(tcp), Some("TCP"));
        assert_eq!(candidate_address("candidate:1 1 UDP"), None);
        assert_eq!(candidate_transport("candidate:1 1 UDP"), None);
        assert_eq!(candidate_address("garbage 1 2 3 4 5"), None);
    }

    #[test]
    fn only_udp_is_offered() {
        let tcp = "candidate:2 1 TCP 1015022079 10.0.0.7 9 typ host tcptype active";
        assert!(
            !CandidateFilter::Any.allows(tcp),
            "the phone disables TCP candidates"
        );
        assert!(!CandidateFilter::new(None, Some("10.0.0.7"), &[]).allows(tcp));
        assert!(CandidateFilter::new(None, Some("10.0.0.7"), &[]).allows(&cand("10.0.0.7")));
    }

    #[test]
    fn pc_address_wins_over_the_lan_fallback() {
        let f = CandidateFilter::new(None, Some("10.0.0.7"), &ifaces());
        assert_eq!(f, CandidateFilter::Address("10.0.0.7".into()));
        assert!(f.allows(&cand("10.0.0.7")));
        assert!(
            !f.allows(&cand("192.168.1.20")),
            "another LAN address of this PC"
        );
        assert!(!f.allows(&cand("172.17.0.1")));
    }

    #[test]
    fn fallback_is_private_ipv4_without_virtual_interfaces() {
        let f = CandidateFilter::new(None, None, &ifaces());
        assert!(f.allows(&cand("192.168.1.20")));
        assert!(f.allows(&cand("10.0.0.7")), "10/8 LANs work too");
        assert!(
            f.allows(&cand("172.20.4.9")),
            "172.16/12 on a real interface"
        );
        assert!(!f.allows(&cand("172.17.0.1")), "docker0");
        assert!(!f.allows(&cand("172.18.0.1")), "docker user bridge");
        assert!(!f.allows(&cand("10.8.0.2")), "wireguard");
        assert!(
            !f.allows(&cand("100.64.0.5")),
            "tailscale CGNAT is not RFC 1918"
        );
        assert!(!f.allows(&cand("127.0.0.1")));
        assert!(!f.allows(&cand("169.254.3.4")));
        assert!(!f.allows(&cand("fe80::1")));
        assert!(!f.allows(&cand("203.0.113.9")), "public address");
    }

    #[test]
    fn env_override() {
        let f = CandidateFilter::new(
            Some("172.17.0.1, 10.1.0.0/16 192.168.7.40"),
            Some("10.0.0.7"),
            &[],
        );
        assert!(f.allows(&cand("172.17.0.1")));
        assert!(f.allows(&cand("10.1.200.3")));
        assert!(!f.allows(&cand("10.2.0.1")));
        assert!(f.allows(&cand("192.168.7.40")));
        assert!(
            !f.allows(&cand("10.0.0.7")),
            "the override replaces pcAddress"
        );
        assert_eq!(
            CandidateFilter::new(Some(" all "), None, &[]),
            CandidateFilter::Any
        );
        assert_eq!(
            CandidateFilter::new(Some("*"), None, &[]),
            CandidateFilter::Any
        );
        assert!(CandidateFilter::Any.allows(&cand("8.8.8.8")));
        assert_eq!(
            CandidateFilter::new(Some(" , "), Some("10.0.0.7"), &[]),
            CandidateFilter::Address("10.0.0.7".into())
        );
        assert!(
            !CandidateFilter::new(Some("10.0.0.0/40"), None, &[]).allows(&cand("10.0.0.1")),
            "bad CIDR"
        );
    }

    #[test]
    fn virtual_interface_names() {
        for n in [
            "docker0",
            "br-0123456789ab",
            "veth12ab",
            "virbr0",
            "tailscale0",
            "wg0",
            "tun0",
            "lo",
        ] {
            assert!(is_virtual_interface(n), "{n}");
        }
        for n in ["wlan0", "wlp2s0", "eth0", "enp3s0", "br0"] {
            assert!(!is_virtual_interface(n), "{n}");
        }
    }

    #[test]
    fn parses_ip_json_and_status() {
        let json = r#"[{"ifindex":1,"ifname":"lo","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8}]},
                       {"ifindex":3,"ifname":"wlan0","addr_info":[{"family":"inet","local":"192.168.1.20","prefixlen":24}]},
                       {"ifindex":4,"ifname":"docker0","addr_info":[]}]"#;
        assert_eq!(
            parse_ip_addr_json(json),
            vec![
                ("lo".to_owned(), "127.0.0.1".to_owned()),
                ("wlan0".to_owned(), "192.168.1.20".to_owned())
            ]
        );
        assert!(parse_ip_addr_json("not json").is_empty());
        let status = serde_json::json!({ "connected": true, "pcAddress": "192.168.1.20" });
        assert_eq!(
            pc_address_from_status(&status).as_deref(),
            Some("192.168.1.20")
        );
        assert_eq!(
            pc_address_from_status(&serde_json::json!({ "pcAddress": null })),
            None
        );
    }
}
