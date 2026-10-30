//! Which local ICE candidates are offered to the phone.
//!
//! The phone only reaches this PC over the LAN it already uses for the control connection, and
//! every extra candidate (docker bridges, VPN tunnels, a second NIC) is one more pair it tries and
//! times out on. In order of preference:
//!
//! 1. `$SMARTCONNECT_ICE_ADDRESSES`: comma-separated IPs and/or IPv4 CIDRs, or `all`.
//! 2. The daemon's `pcAddress` (the address the phone's control connection reached).
//! 3. Private IPv4 (10/8, 172.16/12, 192.168/16) on interfaces that don't look virtual.
//!
//! Only UDP candidates are offered (the phone disables TCP candidates).

use std::net::{IpAddr, Ipv4Addr};

pub const ENV: &str = "SMARTCONNECT_ICE_ADDRESSES";

/// Interface name prefixes skipped by the private-address fallback.
const VIRTUAL_IFACES: &[&str] = &[
    "lo",
    "docker",
    "br-",
    "veth",
    "virbr",
    "vnet",
    "tun",
    "tap",
    "wg",
    "tailscale",
    "zt",
    "vmnet",
    "vboxnet",
    "podman",
    "cni",
    "flannel",
    "cali",
    "lxc",
    "lxd",
    "incus",
    "nordlynx",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    Any,
    Addr(IpAddr),
    /// IPv4 network and prefix length.
    Net(Ipv4Addr, u8),
}

impl Rule {
    fn matches(&self, ip: IpAddr) -> bool {
        match *self {
            Rule::Any => true,
            Rule::Addr(a) => a == ip,
            Rule::Net(net, len) => match ip {
                IpAddr::V4(v4) => {
                    let mask = if len == 0 {
                        0
                    } else {
                        u32::MAX << (32 - u32::from(len))
                    };
                    u32::from(v4) & mask == u32::from(net) & mask
                }
                IpAddr::V6(_) => false,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub rules: Vec<Rule>,
    /// Where the rules came from, for the log.
    pub why: String,
}

impl Filter {
    /// Offer every UDP candidate.
    #[cfg(test)]
    pub fn any(why: &str) -> Filter {
        Filter {
            rules: vec![Rule::Any],
            why: why.to_owned(),
        }
    }

    /// Any RFC 1918 address (no interface check): the last resort when nothing else matched.
    pub fn private_ranges() -> Filter {
        Filter {
            rules: vec![
                Rule::Net(Ipv4Addr::new(10, 0, 0, 0), 8),
                Rule::Net(Ipv4Addr::new(172, 16, 0, 0), 12),
                Rule::Net(Ipv4Addr::new(192, 168, 0, 0), 16),
            ],
            why: "any private IPv4 address".into(),
        }
    }

    /// Picks the rules: `env` (the variable's value) over `pc_address` over the local private
    /// addresses (`ifaces`, `None` when they could not be listed).
    pub fn choose(
        env: Option<&str>,
        pc_address: Option<&str>,
        ifaces: Option<&[(String, IpAddr)]>,
    ) -> Filter {
        if let Some(v) = env.filter(|v| !v.trim().is_empty()) {
            match parse_list(v) {
                Ok(rules) => {
                    return Filter {
                        rules,
                        why: format!("{ENV}={v}"),
                    };
                }
                Err(e) => eprintln!("{ENV}: {e}; ignoring it"),
            }
        }
        if let Some(ip) = pc_address.and_then(normalize) {
            return Filter {
                rules: vec![Rule::Addr(ip)],
                why: format!("{ip}, the address the phone reached (daemon pcAddress)"),
            };
        }
        let Some(ifaces) = ifaces else {
            return Filter::private_ranges();
        };
        let rules: Vec<Rule> = ifaces
            .iter()
            .filter(|(name, ip)| {
                !is_virtual_iface(name) && matches!(ip, IpAddr::V4(v4) if is_rfc1918(*v4))
            })
            .map(|(_, ip)| Rule::Addr(*ip))
            .collect();
        if rules.is_empty() {
            return Filter::private_ranges();
        }
        let list: Vec<String> = ifaces
            .iter()
            .filter(|(_, ip)| rules.contains(&Rule::Addr(*ip)))
            .map(|(n, ip)| format!("{ip} ({n})"))
            .collect();
        Filter {
            rules,
            why: format!("private LAN address(es) {}", list.join(", ")),
        }
    }

    /// From the environment, else the daemon's `pcAddress` (asked only then), else this machine's
    /// interfaces.
    pub fn resolve(pc_address: impl FnOnce() -> Option<String>) -> Filter {
        let env = std::env::var(ENV).ok();
        if let Some(v) = env.as_deref()
            && let Ok(rules) = parse_list(v)
        {
            return Filter {
                rules,
                why: format!("{ENV}={v}"),
            };
        }
        Filter::choose(
            env.as_deref(),
            pc_address().as_deref(),
            local_addrs().as_deref(),
        )
    }

    /// Whether to offer this `candidate:…` line to the phone.
    pub fn allows(&self, candidate: &str) -> bool {
        match candidate_addr(candidate) {
            Some((proto, ip)) => {
                proto.eq_ignore_ascii_case("udp") && self.rules.iter().any(|r| r.matches(ip))
            }
            None => false,
        }
    }
}

/// `"10.0.0.0/8, 192.168.1.20"` or `"all"`.
pub fn parse_list(v: &str) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    for item in v.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()) {
        if item.eq_ignore_ascii_case("all") || item == "*" {
            rules.push(Rule::Any);
        } else if let Some((net, len)) = item.split_once('/') {
            let net: Ipv4Addr = net
                .parse()
                .map_err(|_| format!("{item}: not an IPv4 network"))?;
            let len: u8 = len
                .parse()
                .ok()
                .filter(|l| *l <= 32)
                .ok_or_else(|| format!("{item}: bad prefix length"))?;
            rules.push(Rule::Net(net, len));
        } else {
            rules.push(Rule::Addr(
                normalize(item).ok_or_else(|| format!("{item}: not an IP address"))?,
            ));
        }
    }
    if rules.is_empty() {
        return Err("empty".into());
    }
    Ok(rules)
}

/// An address as the daemon or a candidate writes it: IPv4-mapped IPv6 becomes IPv4; brackets and
/// a `%scope` suffix are dropped.
pub fn normalize(s: &str) -> Option<IpAddr> {
    let s = s.trim().trim_start_matches('[').trim_end_matches(']');
    let s = s.split('%').next()?;
    match s.parse::<IpAddr>().ok()? {
        IpAddr::V6(v6) => Some(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)),
        v4 => Some(v4),
    }
}

/// (transport, address) of `candidate:<foundation> <component> <transport> <priority> <address> <port> typ …`
/// (with or without the `a=` prefix).
pub fn candidate_addr(c: &str) -> Option<(String, IpAddr)> {
    let c = c.trim().trim_start_matches("a=");
    let mut f = c.strip_prefix("candidate:")?.split_whitespace();
    let (_foundation, _component, proto, _priority, addr) =
        (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
    Some((proto.to_owned(), normalize(addr)?))
}

pub fn is_rfc1918(ip: Ipv4Addr) -> bool {
    ip.is_private()
}

pub fn is_virtual_iface(name: &str) -> bool {
    VIRTUAL_IFACES.iter().any(|p| name.starts_with(p))
}

/// (interface, address) of every configured address, or `None` if getifaddrs fails.
pub fn local_addrs() -> Option<Vec<(String, IpAddr)>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below with freeifaddrs.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list returned by getifaddrs, valid until freeifaddrs.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
            continue;
        }
        // SAFETY: ifa_name is a NUL-terminated string owned by the list.
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: ifa_addr points to a sockaddr whose family says which struct it really is.
        let ip = unsafe {
            match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                        sin.sin_addr.s_addr,
                    ))))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    Some(IpAddr::V6(std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
                }
                _ => None,
            }
        };
        if let Some(ip) = ip {
            out.push((name, ip));
        }
    }
    // SAFETY: frees the list allocated by getifaddrs above; no references into it remain.
    unsafe { libc::freeifaddrs(head) };
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "candidate:1 1 UDP 2015363327 192.168.1.20 47432 typ host";

    fn ifaces() -> Vec<(String, IpAddr)> {
        [
            ("lo", "127.0.0.1"),
            ("wlan0", "192.168.1.20"),
            ("wlan0", "fe80::1"),
            ("docker0", "172.17.0.1"),
            ("br-1a2b", "172.18.0.1"),
            ("wg0", "10.8.0.2"),
            ("enp3s0", "10.0.0.5"),
            ("tailscale0", "100.64.0.1"),
        ]
        .iter()
        .map(|(n, a)| ((*n).to_owned(), a.parse().unwrap()))
        .collect()
    }

    #[test]
    fn parses_candidate_lines() {
        assert_eq!(
            candidate_addr(HOST),
            Some(("UDP".into(), "192.168.1.20".parse().unwrap()))
        );
        assert_eq!(
            candidate_addr("a=candidate:2 1 tcp 1015022079 10.0.0.5 9 typ host tcptype active")
                .map(|c| c.0),
            Some("tcp".into())
        );
        assert_eq!(
            candidate_addr("candidate:3 1 UDP 1 fe80::1%wlan0 5000 typ host")
                .unwrap()
                .1,
            "fe80::1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            candidate_addr("candidate:4 1 UDP 1 abcd.local 5000 typ host"),
            None
        );
        assert_eq!(candidate_addr("garbage"), None);
    }

    #[test]
    fn pc_address_wins_over_interfaces() {
        let f = Filter::choose(None, Some("::ffff:10.0.0.5"), Some(&ifaces()));
        assert_eq!(f.rules, vec![Rule::Addr("10.0.0.5".parse().unwrap())]);
        assert!(f.allows("candidate:1 1 UDP 2015363327 10.0.0.5 4000 typ host"));
        assert!(!f.allows(HOST));
        // TCP is never offered.
        assert!(!f.allows("candidate:1 1 TCP 2015363327 10.0.0.5 9 typ host tcptype passive"));
    }

    #[test]
    fn fallback_is_private_ipv4_on_real_interfaces() {
        let f = Filter::choose(None, None, Some(&ifaces()));
        assert_eq!(
            f.rules,
            vec![
                Rule::Addr("192.168.1.20".parse().unwrap()),
                Rule::Addr("10.0.0.5".parse().unwrap())
            ]
        );
        assert!(f.allows(HOST));
        assert!(!f.allows("candidate:1 1 UDP 1 172.17.0.1 4000 typ host")); // docker0
        assert!(!f.allows("candidate:1 1 UDP 1 10.8.0.2 4000 typ host")); // wg0
        assert!(!f.allows("candidate:1 1 UDP 1 100.64.0.1 4000 typ host")); // tailscale
        // An unparsable daemon address falls through to the interfaces.
        assert_eq!(
            Filter::choose(None, Some("not-an-ip"), Some(&ifaces()))
                .rules
                .len(),
            2
        );
    }

    #[test]
    fn last_resort_is_any_private_range() {
        let f = Filter::choose(None, None, None);
        assert_eq!(f, Filter::private_ranges());
        let only_virtual = vec![("docker0".to_owned(), "172.17.0.1".parse().unwrap())];
        let f = Filter::choose(None, None, Some(&only_virtual));
        assert_eq!(f.rules, Filter::private_ranges().rules);
        for (ip, ok) in [
            ("10.1.2.3", true),
            ("172.15.255.255", false),
            ("172.16.0.1", true),
            ("172.31.255.1", true),
            ("172.32.0.1", false),
            ("192.168.0.1", true),
            ("192.169.0.1", false),
            ("8.8.8.8", false),
        ] {
            assert_eq!(
                f.allows(&format!("candidate:1 1 UDP 1 {ip} 4000 typ host")),
                ok,
                "{ip}"
            );
        }
    }

    #[test]
    fn env_override() {
        let f = Filter::choose(Some("all"), Some("192.168.1.20"), Some(&ifaces()));
        assert!(f.allows("candidate:1 1 UDP 1 8.8.8.8 4000 typ srflx"));
        let f = Filter::choose(Some("10.0.0.0/8, 192.168.1.20"), Some("192.168.9.9"), None);
        assert_eq!(f.rules.len(), 2);
        assert!(f.allows("candidate:1 1 udp 1 10.200.0.7 4000 typ host"));
        assert!(f.allows(HOST));
        assert!(!f.allows("candidate:1 1 UDP 1 192.168.9.9 4000 typ host"));
        // A bad value is ignored (falls back to pcAddress).
        let f = Filter::choose(Some("10.0.0.0/40"), Some("192.168.9.9"), None);
        assert_eq!(f.rules, vec![Rule::Addr("192.168.9.9".parse().unwrap())]);
        assert!(parse_list("nope").is_err() && parse_list(" , ").is_err());
        assert_eq!(
            parse_list("0.0.0.0/0").unwrap(),
            vec![Rule::Net(Ipv4Addr::UNSPECIFIED, 0)]
        );
        assert!(Rule::Net(Ipv4Addr::UNSPECIFIED, 0).matches("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn normalizes_addresses() {
        assert_eq!(
            normalize("::ffff:192.168.1.20"),
            Some("192.168.1.20".parse().unwrap())
        );
        assert_eq!(
            normalize("[fe80::1%wlan0]"),
            Some("fe80::1".parse().unwrap())
        );
        assert_eq!(normalize(" 10.0.0.1 "), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(normalize("x"), None);
        assert!(
            is_virtual_iface("docker0")
                && is_virtual_iface("veth12")
                && !is_virtual_iface("wlan0")
                && !is_virtual_iface("enp3s0")
        );
    }

    #[test]
    fn lists_local_addresses() {
        // Every Linux machine has at least loopback.
        assert!(
            local_addrs()
                .unwrap()
                .iter()
                .any(|(n, ip)| n == "lo" && ip.is_loopback())
        );
    }
}
