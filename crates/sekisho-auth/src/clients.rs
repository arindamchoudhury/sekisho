//! The OAuth clients Sekisho knows. Release 1 pre-registers its CLI; the web
//! UI's client comes with plan 7, and MCP clients with plan 5.

use std::net::{Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// The CLI's client ID.
pub const CLI: &str = "sekisho-cli";

pub const AUTHORIZATION_CODE: &str = "authorization_code";
pub const REFRESH_TOKEN: &str = "refresh_token";
pub const DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// A registered public client (no secret: PKCE stands in for one).
#[derive(Debug, PartialEq, Eq)]
pub struct Client {
    pub id: &'static str,
    /// Shown to people, for example on the device confirmation page.
    pub name: &'static str,
    /// The grant types it may use at the token endpoint.
    pub grants: &'static [&'static str],
    redirects: Redirects,
}

/// Which redirect URIs a client may name.
#[derive(Debug, PartialEq, Eq)]
enum Redirects {
    /// `http://127.0.0.1:<port>/<path>` or `http://[::1]:<port>/<path>`
    /// (RFC 8252 section 7.3), any port.
    Loopback,
}

impl Client {
    pub fn allows(&self, grant: &str) -> bool {
        self.grants.contains(&grant)
    }
}

static CLIENTS: [Client; 1] = [Client {
    id: CLI,
    name: "Sekisho CLI",
    grants: &[AUTHORIZATION_CODE, REFRESH_TOKEN, DEVICE_CODE],
    redirects: Redirects::Loopback,
}];

/// The registered client with this ID.
pub fn find(id: &str) -> Option<&'static Client> {
    CLIENTS.iter().find(|c| c.id == id)
}

/// Whether `client` may be sent to `uri`. The URI must already be in the
/// form Sekisho sends back (no IPv4 shorthand, no default port, no odd
/// escapes), so what was checked is exactly what is redirected to.
pub fn redirect_allowed(client: &Client, uri: &str) -> bool {
    match client.redirects {
        Redirects::Loopback => loopback(uri),
    }
}

/// `http`, a loopback IP literal and an explicit port; no user information,
/// query or fragment. Never `localhost`, which a hosts file can point
/// anywhere.
fn loopback(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    let loopback_host = match url.host() {
        Some(Host::Ipv4(ip)) => ip == Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(ip)) => ip == Ipv6Addr::LOCALHOST,
        _ => false,
    };
    url.as_str() == uri
        && url.scheme() == "http"
        && loopback_host
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_is_registered_with_every_grant() {
        let cli = find(CLI).unwrap();
        for grant in [AUTHORIZATION_CODE, REFRESH_TOKEN, DEVICE_CODE] {
            assert!(cli.allows(grant), "{grant}");
        }
        assert!(!cli.allows("client_credentials"));
        assert!(find("other").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn only_loopback_literals_with_a_port() {
        let cli = find(CLI).unwrap();
        for ok in [
            "http://127.0.0.1:43123/callback",
            "http://127.0.0.1:9/",
            "http://[::1]:9/cb",
            "http://127.0.0.1:65535/a/b",
        ] {
            assert!(redirect_allowed(cli, ok), "{ok}");
        }
        for refused in [
            "http://localhost:8000/cb",
            "https://127.0.0.1:9/cb",
            "https://127.0.0.1/cb",
            "http://127.0.0.1.evil.com/cb",
            "http://user@127.0.0.1:9/cb",
            "http://user:pw@127.0.0.1:9/cb",
            "http://127.0.0.1:9/cb#x",
            "http://127.0.0.1:9/cb#",
            "http://127.0.0.1:9/cb?x=1",
            "http://127.0.0.1:9/cb?",
            "http://10.0.0.1:9/cb",
            "http://127.0.0.2:9/cb",
            "http://127.1:9/cb",
            "http://0x7f000001:9/cb",
            "http://127.0.0.1/cb",
            "http://127.0.0.1:80/cb",
            "http://127.0.0.1:9",
            "HTTP://127.0.0.1:9/cb",
            "http://127.0.0.1:9\\@evil.com/cb",
            "http://[::ffff:127.0.0.1]:9/cb",
            "//127.0.0.1:9/cb",
            "/cb",
            "",
        ] {
            assert!(!redirect_allowed(cli, refused), "{refused}");
        }
    }
}
