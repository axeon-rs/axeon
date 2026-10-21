//! Where requests may go: SSRF protection.

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

/// Which destinations a [`Client`](crate::Client) may reach.
///
/// By default ([`Egress::public`]) any host is allowed as long as every
/// address it resolves to is public: private networks, loopback,
/// link-local, cloud metadata services (`169.254.169.254`, Alibaba Cloud's
/// `100.100.100.200`) and other special ranges are refused. This is checked
/// on the addresses actually connected to, for IP-literal URLs, and for
/// every redirect, so a public name that resolves or redirects inward is
/// still refused.
#[derive(Clone, Debug)]
pub struct Egress {
    hosts: Option<Vec<String>>,
    allow_private: bool,
}

impl Egress {
    /// Any host with public addresses. For fetching URLs users provide,
    /// such as a website crawler.
    pub fn public() -> Self {
        Self { hosts: None, allow_private: false }
    }

    /// Only these hosts (and still only public addresses). `*.example.com`
    /// matches subdomains of `example.com`, not `example.com` itself. For
    /// clients of one vendor's API.
    pub fn hosts<S: Into<String>>(hosts: impl IntoIterator<Item = S>) -> Self {
        let hosts = hosts.into_iter().map(|h| normalize(&h.into())).collect();
        Self { hosts: Some(hosts), allow_private: false }
    }

    /// Also allows private and loopback addresses: for calling your own
    /// internal services. Never use it for URLs that come from users.
    pub fn allow_private(mut self) -> Self {
        self.allow_private = true;
        self
    }

    /// Whether `host` (a name or IP literal) may be requested.
    pub(crate) fn check_host(&self, host: &str) -> Result<(), Blocked> {
        let host = normalize(host);
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = bare.parse::<IpAddr>() {
            self.check_ip(ip)?;
        }
        if let Some(hosts) = &self.hosts {
            let allowed = hosts.iter().any(|pattern| match pattern.strip_prefix("*.") {
                Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(suffix) && host[..host.len() - suffix.len()].ends_with('.'),
                None => *pattern == host,
            });
            if !allowed {
                return Err(Blocked(format!("host {} is not in the allow-list", host)));
            }
        }
        Ok(())
    }

    fn check_ip(&self, ip: IpAddr) -> Result<(), Blocked> {
        if self.allow_private || is_public(ip) {
            Ok(())
        } else {
            Err(Blocked(format!("address {} is not public", ip)))
        }
    }

    pub(crate) fn resolver(&self) -> Arc<dyn Resolve> {
        Arc::new(Resolver { egress: self.clone() })
    }
}

fn normalize(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// A request refused by the [`Egress`] policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocked(pub(crate) String);

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "request blocked: {}", self.0)
    }
}

impl std::error::Error for Blocked {}

/// Resolves names, then drops addresses the policy refuses; fails if none
/// are left, so a connection can only go to an allowed address.
struct Resolver {
    egress: Egress,
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let egress = self.egress.clone();
        Box::pin(async move {
            let host = name.as_str().to_string();
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = resolved.iter().copied().filter(|addr| egress.check_ip(addr.ip()).is_ok()).collect();
            if allowed.is_empty() {
                let addresses: Vec<String> = resolved.iter().map(|a| a.ip().to_string()).collect();
                return Err(Box::new(Blocked(format!("{} resolves only to non-public addresses ({})", host, addresses.join(", ")))) as _);
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

/// Whether `ip` is a globally routable unicast address.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // 169.254/16: cloud metadata
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0 // "this network"
        || (a == 100 && (64..128).contains(&b)) // 100.64/10 shared address space, incl. Alibaba Cloud metadata
        || (a == 192 && b == 0 && c == 0) // 192.0.0/24 protocol assignments
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // IPv4 addresses inside IPv6 follow the IPv4 rules.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        // NAT64 (64:ff9b::/96)
        let [a, b] = segments[6].to_be_bytes();
        let [c, d] = segments[7].to_be_bytes();
        return is_public_v4(Ipv4Addr::new(a, b, c, d));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (segments[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (segments[0] & 0xffc0) == 0xfec0 // site-local fec0::/10
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || segments[..6] == [0, 0, 0, 0, 0, 0]) // IPv4-compatible (deprecated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        let blocked = [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.169.254", "100.100.100.200",
            "100.64.0.1", "0.0.0.0", "0.1.2.3", "255.255.255.255", "224.0.0.1", "192.0.2.1", "198.18.0.1", "240.0.0.1",
            "::1", "::", "fc00::1", "fd12:3456::1", "fe80::1", "ff02::1", "2001:db8::1", "::ffff:127.0.0.1",
            "::ffff:169.254.169.254", "64:ff9b::a9fe:a9fe", "::7f00:1",
        ];
        for ip in blocked {
            assert!(!is_public(ip.parse().unwrap()), "{} should be blocked", ip);
        }
        let public = ["8.8.8.8", "1.1.1.1", "47.88.1.1", "100.128.0.1", "172.32.0.1", "2606:4700::1111", "::ffff:8.8.8.8", "64:ff9b::808:808"];
        for ip in public {
            assert!(is_public(ip.parse().unwrap()), "{} should be public", ip);
        }
    }

    #[test]
    fn host_allow_list() {
        let egress = Egress::hosts(["api.vendor.com", "*.aliyuncs.com", "Upper.Example."]);
        for host in ["api.vendor.com", "API.Vendor.COM.", "dashscope.aliyuncs.com", "a.b.aliyuncs.com", "upper.example"] {
            assert!(egress.check_host(host).is_ok(), "{}", host);
        }
        for host in ["vendor.com", "evil-api.vendor.com", "aliyuncs.com", "evilaliyuncs.com", "api.vendor.com.evil.io"] {
            assert!(egress.check_host(host).is_err(), "{}", host);
        }
        // IP literals are checked even without an allow-list.
        let public = Egress::public();
        assert!(public.check_host("8.8.8.8").is_ok());
        assert!(public.check_host("169.254.169.254").is_err());
        assert!(public.check_host("[::1]").is_err());
        assert!(public.check_host("[::ffff:7f00:1]").is_err());
        assert!(Egress::public().allow_private().check_host("127.0.0.1").is_ok());
    }
}
