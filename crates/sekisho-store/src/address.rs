//! Where a provider's base URL may point (design section 9.3, as in Obot).
//!
//! The host is resolved when the provider is saved and every address it
//! resolves to is checked. agentgateway makes the connection, so this is a
//! save-time check only; plan 8's egress rules also block metadata addresses
//! from agentgateway.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// A lookup that takes longer counts as one that doesn't resolve.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The host and port a base URL sends requests to, split the way the gateway
/// renderer splits it (`Upstream::parse` in `sekisho-gateway`): no query or
/// fragment, the authority up to the first `/`, the host after the last `@`.
/// `None` unless the URL is `http://` or `https://` with a host and, if
/// given, a numeric port.
pub(crate) fn host_and_port(url: &str) -> Option<(&str, u16)> {
    let (rest, default_port) = match url.strip_prefix("https://") {
        Some(rest) => (rest, 443),
        None => (url.strip_prefix("http://")?, 80),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let authority = rest.split('/').next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => {
            let (host, after) = bracketed.split_once(']')?;
            (host, after)
        }
        // The port keeps its `:`, as after a bracketed host.
        None => match authority.rfind(':') {
            Some(colon) => authority.split_at(colon),
            None => (authority, ""),
        },
    };
    let port = match port {
        "" => default_port,
        _ => port.strip_prefix(':')?.parse().ok()?,
    };
    (!host.is_empty()).then_some((host, port))
}

/// Checks every address `url`'s host resolves to:
/// - loopback and private network addresses need `allow_private`;
/// - link-local, unspecified and cloud metadata addresses are never allowed;
/// - IPv4-mapped IPv6 addresses count as IPv4;
/// - a host that doesn't resolve needs `allow_private`, since it may resolve
///   only on the gateway's network.
///
/// Errors name an address but never the URL or its host.
pub async fn check_destination(url: &str, allow_private: bool) -> Result<(), String> {
    let (host, port) =
        host_and_port(url).ok_or("use an http:// or https:// URL with a host and a valid port")?;
    let literal = host.strip_suffix('.').unwrap_or(host).parse::<IpAddr>();
    let addresses: Vec<IpAddr> = match literal {
        Ok(ip) => vec![ip],
        Err(_) => match tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
        {
            Ok(Ok(found)) => found.map(|a| a.ip()).collect(),
            Ok(Err(_)) | Err(_) => Vec::new(),
        },
    };
    if addresses.is_empty() {
        return if allow_private {
            Ok(())
        } else {
            Err(
                "the host doesn't resolve from the Sekisho server; if it resolves only on \
                 the gateway's network, turn on allow_private_network"
                    .to_owned(),
            )
        };
    }
    let classes: Vec<(IpAddr, Class)> = addresses
        .into_iter()
        .map(|ip| {
            let ip = as_ipv4_if_mapped(ip);
            (ip, classify(ip))
        })
        .collect();
    if let Some((ip, Class::Never(what))) = classes
        .iter()
        .find(|(_, class)| matches!(class, Class::Never(_)))
    {
        return Err(format!("{ip} is {what}, which can't be used"));
    }
    if !allow_private
        && let Some((ip, Class::Private(what))) = classes
            .iter()
            .find(|(_, class)| matches!(class, Class::Private(_)))
    {
        return Err(format!("{ip} is {what}, which needs allow_private_network"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Public,
    /// Allowed with `allow_private_network`.
    Private(&'static str),
    /// Never allowed.
    Never(&'static str),
}

const METADATA: &str = "a cloud metadata address";
const LINK_LOCAL: &str = "a link-local address";
const UNSPECIFIED: &str = "an unspecified address";
const LOOPBACK: &str = "a loopback address";
const PRIVATE: &str = "a private network address";

/// AWS, Azure and GCP's instance metadata (169.254.169.254), Alibaba Cloud's
/// (100.100.100.200) and Azure's wire server (168.63.129.16).
const METADATA_V4: [Ipv4Addr; 3] = [
    Ipv4Addr::new(169, 254, 169, 254),
    Ipv4Addr::new(100, 100, 100, 200),
    Ipv4Addr::new(168, 63, 129, 16),
];
/// AWS's IPv6 instance metadata.
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

fn as_ipv4_if_mapped(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// Metadata addresses are checked first: two of them sit inside ranges that
/// are otherwise private (100.64.0.0/10, fc00::/7).
fn classify(ip: IpAddr) -> Class {
    match ip {
        IpAddr::V4(a) => {
            let [first, second, ..] = a.octets();
            if METADATA_V4.contains(&a) {
                Class::Never(METADATA)
            } else if a.is_unspecified() {
                Class::Never(UNSPECIFIED)
            } else if a.is_link_local() {
                Class::Never(LINK_LOCAL)
            } else if a.is_loopback() {
                Class::Private(LOOPBACK)
            } else if a.is_private() || (first == 100 && (second & 0xc0) == 64) {
                Class::Private(PRIVATE) // with 100.64.0.0/10, carrier-grade NAT
            } else {
                Class::Public
            }
        }
        IpAddr::V6(a) => {
            let first = a.segments()[0];
            if a == METADATA_V6 {
                Class::Never(METADATA)
            } else if a.is_unspecified() {
                Class::Never(UNSPECIFIED)
            } else if (first & 0xffc0) == 0xfe80 {
                Class::Never(LINK_LOCAL)
            } else if a.is_loopback() {
                Class::Private(LOOPBACK)
            } else if (first & 0xfe00) == 0xfc00 {
                Class::Private(PRIVATE) // fc00::/7, unique local
            } else {
                Class::Public
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_split_like_the_renderer() {
        for (url, want) in [
            ("http://mock:8000/v1", Some(("mock", 8000))),
            ("https://example.com", Some(("example.com", 443))),
            ("http://example.com/", Some(("example.com", 80))),
            ("http://[::1]:8000", Some(("::1", 8000))),
            ("http://[::1]", Some(("::1", 80))),
            ("http://u:p@10.1.2.3:9/x", Some(("10.1.2.3", 9))),
            ("http://a@b@c/x", Some(("c", 80))),
            (
                "http://203.0.113.10?@169.254.169.254",
                Some(("203.0.113.10", 80)),
            ),
            ("http://h#@169.254.169.254", Some(("h", 80))),
            ("http://h:abc", None),
            ("http://h:", None),
            ("http://[::1", None),
            ("http://[::1]x", None),
            ("http://", None),
            ("http:///x", None),
            ("ftp://h", None),
        ] {
            assert_eq!(host_and_port(url), want, "{url}");
        }
    }

    #[test]
    fn addresses_are_classified() {
        let class = |s: &str| classify(s.parse().unwrap());
        assert_eq!(class("203.0.113.10"), Class::Public);
        assert_eq!(class("100.63.255.255"), Class::Public);
        assert_eq!(class("100.128.0.0"), Class::Public);
        assert_eq!(class("100.64.0.0"), Class::Private(PRIVATE));
        assert_eq!(class("100.127.255.255"), Class::Private(PRIVATE));
        assert_eq!(class("172.31.255.255"), Class::Private(PRIVATE));
        assert_eq!(class("172.32.0.0"), Class::Public);
        assert_eq!(class("127.1.2.3"), Class::Private(LOOPBACK));
        assert_eq!(class("169.254.0.1"), Class::Never(LINK_LOCAL));
        assert_eq!(class("100.100.100.200"), Class::Never(METADATA));
        assert_eq!(class("::"), Class::Never(UNSPECIFIED));
        assert_eq!(class("fd00:ec2::254"), Class::Never(METADATA));
        assert_eq!(class("fd00:ec2::253"), Class::Private(PRIVATE));
        assert_eq!(class("febf::1"), Class::Never(LINK_LOCAL));
        assert_eq!(class("fec0::1"), Class::Public);
        assert_eq!(class("2001:db8::1"), Class::Public);
        assert_eq!(
            classify(as_ipv4_if_mapped("::ffff:127.0.0.1".parse().unwrap())),
            Class::Private(LOOPBACK)
        );
    }
}
