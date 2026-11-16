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

/// A provider base URL as agentgateway v1.7.0 reads it: `url::Url::parse`,
/// then its host, port and path (`apply_base_url` in `types/local.rs`).
/// agentgateway refuses the whole config file if one `baseUrl` doesn't
/// parse, so the store checks with the same parser, the address check uses
/// the host agentgateway will connect to, and the renderer writes the URL
/// from these parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseUrl {
    pub tls: bool,
    /// Lowercased, punycoded, percent-decoded; an IPv4 address in any form
    /// agentgateway accepts (`0xa9.254.169.254`) is read as one.
    pub host: url::Host<String>,
    pub port: u16,
    /// Without a trailing `/`; empty for none.
    pub path: String,
}

impl BaseUrl {
    /// `host:port`, an IPv6 host in brackets.
    pub fn host_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Why a base URL was refused. The messages never quote the URL: user info
/// is a credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseUrlError {
    /// Not `http://` or `https://` with a host.
    NotHttp,
    /// User info, a query or a fragment, which agentgateway refuses.
    Extras,
    /// Doesn't parse (a bad IP address, host or port), or holds a
    /// backslash, whitespace or a control character, which the parser
    /// silently reads differently from how an admin does.
    Unreadable,
}

impl BaseUrlError {
    pub fn message(self) -> &'static str {
        match self {
            BaseUrlError::NotHttp => "use an http:// or https:// URL with a host",
            BaseUrlError::Extras => "use a URL without user name, password, query or fragment",
            BaseUrlError::Unreadable => {
                "agentgateway can't read this URL: check the host and the port, and use / not \\"
            }
        }
    }
}

/// A base URL as a provider is saved with it: everything agentgateway
/// refuses is an error.
pub fn parse_base_url(raw: &str) -> Result<BaseUrl, BaseUrlError> {
    read_base_url(raw, false)
}

/// A base URL saved before the store refused user info, a query or a
/// fragment: those are dropped, as the renderer always did, and anything
/// agentgateway can't read is still an error.
pub fn read_saved_base_url(raw: &str) -> Result<BaseUrl, BaseUrlError> {
    read_base_url(raw, true)
}

fn read_base_url(raw: &str, drop_extras: bool) -> Result<BaseUrl, BaseUrlError> {
    if raw
        .chars()
        .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
    {
        return Err(BaseUrlError::Unreadable);
    }
    let tls = if raw.starts_with("https://") {
        true
    } else if raw.starts_with("http://") {
        false
    } else {
        return Err(BaseUrlError::NotHttp);
    };
    let url = url::Url::parse(raw).map_err(|e| match e {
        url::ParseError::EmptyHost => BaseUrlError::NotHttp,
        _ => BaseUrlError::Unreadable,
    })?;
    if !drop_extras
        && (!url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some())
    {
        return Err(BaseUrlError::Extras);
    }
    let host = url.host().ok_or(BaseUrlError::NotHttp)?.to_owned();
    let port = url.port_or_known_default().ok_or(BaseUrlError::NotHttp)?;
    Ok(BaseUrl {
        tls,
        host,
        port,
        path: url.path().trim_end_matches('/').to_owned(),
    })
}

/// Checks every address `url`'s host resolves to:
/// - loopback and private network addresses need `allow_private`;
/// - link-local, unspecified and cloud metadata addresses are never allowed;
/// - IPv4-mapped IPv6 addresses count as IPv4;
/// - a host that doesn't resolve needs `allow_private`, since it may resolve
///   only on the gateway's network.
///
/// Errors name an address but never the URL or its host.
pub async fn check_destination(url: &BaseUrl, allow_private: bool) -> Result<(), String> {
    let addresses: Vec<IpAddr> = match &url.host {
        url::Host::Ipv4(ip) => vec![IpAddr::V4(*ip)],
        url::Host::Ipv6(ip) => vec![IpAddr::V6(*ip)],
        url::Host::Domain(host) => match tokio::time::timeout(
            LOOKUP_TIMEOUT,
            tokio::net::lookup_host((host.as_str(), url.port)),
        )
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
    fn base_urls_are_read_as_agentgateway_reads_them() {
        let ok = |url: &str| parse_base_url(url).map(|b| (b.host_port(), b.path, b.tls));
        let read = |host_port: &str, path: &str, tls: bool| {
            Ok((host_port.to_owned(), path.to_owned(), tls))
        };
        assert_eq!(ok("http://mock:8000/v1"), read("mock:8000", "/v1", false));
        assert_eq!(ok("https://example.com"), read("example.com:443", "", true));
        assert_eq!(ok("http://example.com/"), read("example.com:80", "", false));
        assert_eq!(ok("http://[::1]:8000"), read("[::1]:8000", "", false));
        assert_eq!(
            ok("http://Ollama.LAN:11434/v1/"),
            read("ollama.lan:11434", "/v1", false)
        );
        // The forms that hide an address: decoded as agentgateway decodes them.
        assert_eq!(
            ok("http://0xa9.254.169.254/v1"),
            read("169.254.169.254:80", "/v1", false)
        );
        assert_eq!(
            ok("http://%31%36%39.254.169.254/v1"),
            read("169.254.169.254:80", "/v1", false)
        );
        for (url, error) in [
            ("http://u:p@10.1.2.3:9/x", BaseUrlError::Extras),
            ("http://203.0.113.10?@169.254.169.254", BaseUrlError::Extras),
            ("http://h#@169.254.169.254", BaseUrlError::Extras),
            ("http://h/v1?", BaseUrlError::Extras),
            (r"http://169.254.169.254\x.com/v1", BaseUrlError::Unreadable),
            ("http://h x/v1", BaseUrlError::Unreadable),
            ("http://10.0.0.256/v1", BaseUrlError::Unreadable),
            ("http://foo|bar:8000/v1", BaseUrlError::Unreadable),
            ("http://xn--a.com/v1", BaseUrlError::Unreadable),
            ("http://h:abc", BaseUrlError::Unreadable),
            // agentgateway can't read a second colon either.
            ("http://ollama:11434:11434/v1", BaseUrlError::Unreadable),
            ("http://h:99999", BaseUrlError::Unreadable),
            ("http://[::1", BaseUrlError::Unreadable),
            ("http://", BaseUrlError::NotHttp),
            ("ftp://h", BaseUrlError::NotHttp),
            ("mock:8000", BaseUrlError::NotHttp),
        ] {
            assert_eq!(parse_base_url(url), Err(error), "{url}");
        }
        // Rows saved before the store refused the extras: dropped when read.
        let saved = read_saved_base_url("https://u:p@h.example/v1?x=1#f").unwrap();
        assert_eq!(
            (saved.host_port(), saved.path),
            ("h.example:443".into(), "/v1".into())
        );
        assert_eq!(
            read_saved_base_url("http://10.0.0.256/v1"),
            Err(BaseUrlError::Unreadable)
        );
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
