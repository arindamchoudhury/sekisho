//! `sekisho serve` settings, read from `SEKISHO_*` environment variables.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use sekisho_core::access::{GROUP_RULE, is_group_reference};
use sekisho_core::keys::hash_secret;
use sekisho_core::secret_box::SecretBox;
use sekisho_gateway::{HookTargets, RenderOptions};
use sekisho_store::StoreOptions;
use url::Url;

use crate::app::Settings;
use crate::catalog::CatalogSettings;

/// The bounds of `SEKISHO_CATALOG_SYNC_INTERVAL`: often enough to matter,
/// rarely enough not to hammer Git hosts.
const SYNC_EVERY_MIN: Duration = Duration::from_secs(60);
const SYNC_EVERY_MAX: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A whole number and a unit: `90s`, `30m`, `1h`, `1d`.
fn duration(value: &str) -> Result<Duration, String> {
    let rule = "use a whole number and a unit, s, m, h or d, such as 30m or 1h";
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit()).ok_or(rule)?;
    let (number, unit) = value.split_at(split);
    let number: u64 = number.parse().map_err(|_| rule)?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(rule.to_owned()),
    };
    Ok(Duration::from_secs(
        number.checked_mul(seconds).ok_or(rule)?,
    ))
}

/// Shortest accepted bootstrap or gateway token.
const MIN_TOKEN_CHARS: usize = 16;
/// How long `GET /internal/gateway/config` waits for a newer version.
pub const LONG_POLL: Duration = Duration::from_secs(30);
/// How often the publisher re-renders without a change.
pub const REPUBLISH: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is not set")]
    Missing(&'static str),
    #[error("{name} is invalid: {reason}")]
    Invalid { name: &'static str, reason: String },
}

/// Where Sekisho reaches Dex.
#[derive(Clone, Debug)]
pub struct DexSettings {
    /// `SEKISHO_DEX_ISSUER`: Dex's issuer, as browsers reach it.
    pub issuer: Url,
    /// `SEKISHO_DEX_URL`: where Sekisho reaches Dex's HTTP; by default the
    /// issuer's origin.
    pub http_url: Url,
    /// `SEKISHO_DEX_GRPC`: Dex's gRPC API, `host:port`.
    pub grpc: String,
}

pub struct ServerConfig {
    /// May contain a password, so `Debug` leaves it out.
    pub database_url: String,
    /// `SEKISHO_DATABASE_MAX_CONNECTIONS`, the pool's size: 1 to 1000,
    /// default 20.
    pub database_max_connections: u32,
    pub secret_box: SecretBox,
    pub listen: SocketAddr,
    pub hooks_http_listen: SocketAddr,
    pub hooks_grpc_listen: SocketAddr,
    pub dex: DexSettings,
    pub settings: Settings,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("listen", &self.listen)
            .field("hooks_http_listen", &self.hooks_http_listen)
            .field("hooks_grpc_listen", &self.hooks_grpc_listen)
            .field("dex", &self.dex)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// An absolute `http` or `https` URL with a host and no user information,
/// query or fragment.
fn web_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|e| format!("not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err("use an http or https URL".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not carry a user name or password".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not have a query or fragment".to_owned());
    }
    Ok(url)
}

/// `host:port`, as tonic dials it over plain HTTP/2.
fn host_port(value: &str) -> Result<String, String> {
    let shape = "use host:port, such as dex:5557";
    let (host, port) = value.rsplit_once(':').ok_or(shape)?;
    if host.is_empty() || host.contains('/') || host.contains('@') || port.parse::<u16>().is_err() {
        return Err(shape.to_owned());
    }
    Ok(value.to_owned())
}

/// The secret agentgateway's rate-limit calls carry (see `HookTargets`),
/// derived from the gateway token so every replica agrees on it. Whoever has
/// the gateway token can read the rendered config anyway.
fn charge_secret(gateway_token: &str) -> String {
    hash_secret(&format!(
        "sekisho charge secret
{gateway_token}"
    ))
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect()
}

impl ServerConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads settings through `get`, so tests need not touch the process
    /// environment. An empty value counts as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let var = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        let required = |name: &'static str| var(name).ok_or(ConfigError::Missing(name));
        let invalid = |name: &'static str, reason: String| ConfigError::Invalid { name, reason };
        let addr = |name: &'static str, default: &str| {
            var(name)
                .unwrap_or_else(|| default.to_owned())
                .parse::<SocketAddr>()
                .map_err(|e| invalid(name, e.to_string()))
        };
        let token = |name: &'static str| {
            let token = required(name)?;
            if token.chars().count() < MIN_TOKEN_CHARS {
                return Err(invalid(
                    name,
                    format!("use at least {MIN_TOKEN_CHARS} characters"),
                ));
            }
            Ok(token)
        };

        let database_url = required("SEKISHO_DATABASE_URL")?;
        let database_max_connections = match var("SEKISHO_DATABASE_MAX_CONNECTIONS") {
            None => StoreOptions::default().max_connections,
            Some(v) => v
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=1000).contains(n))
                .ok_or_else(|| {
                    invalid(
                        "SEKISHO_DATABASE_MAX_CONNECTIONS",
                        "use a whole number from 1 to 1000".into(),
                    )
                })?,
        };
        let secret_box = SecretBox::from_base64_key(&required("SEKISHO_ENCRYPTION_KEY")?)
            .map_err(|e| invalid("SEKISHO_ENCRYPTION_KEY", e.to_string()))?;
        let bootstrap_token = token("SEKISHO_BOOTSTRAP_TOKEN")?;
        let gateway_token = token("SEKISHO_GATEWAY_TOKEN")?;
        if gateway_token == bootstrap_token {
            return Err(invalid(
                "SEKISHO_GATEWAY_TOKEN",
                "must differ from SEKISHO_BOOTSTRAP_TOKEN".into(),
            ));
        }
        let mut public_url = web_url(&required("SEKISHO_PUBLIC_URL")?)
            .map_err(|r| invalid("SEKISHO_PUBLIC_URL", r))?;
        let path = public_url.path().trim_end_matches('/').to_owned();
        public_url.set_path(&path);
        let issuer = web_url(&required("SEKISHO_DEX_ISSUER")?)
            .map_err(|r| invalid("SEKISHO_DEX_ISSUER", r))?;
        let http_url = match var("SEKISHO_DEX_URL") {
            Some(v) => web_url(&v).map_err(|r| invalid("SEKISHO_DEX_URL", r))?,
            None => Url::parse(&issuer.origin().ascii_serialization())
                .map_err(|e| invalid("SEKISHO_DEX_ISSUER", e.to_string()))?,
        };
        let dex = DexSettings {
            issuer,
            http_url,
            grpc: host_port(&required("SEKISHO_DEX_GRPC")?)
                .map_err(|r| invalid("SEKISHO_DEX_GRPC", r))?,
        };
        // Sekisho's groups, and identity providers' groups named
        // <connector>/<group>, such as github/acme:admins.
        let admin_groups: Vec<String> = var("SEKISHO_ADMIN_GROUPS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(str::to_owned)
            .collect();
        if let Some(bad) = admin_groups.iter().find(|g| !is_group_reference(g)) {
            return Err(invalid(
                "SEKISHO_ADMIN_GROUPS",
                format!("{bad}: {GROUP_RULE}"),
            ));
        }
        let hooks = HookTargets {
            authz_http: required("SEKISHO_HOOKS_HTTP_TARGET")?,
            grpc: required("SEKISHO_HOOKS_GRPC_TARGET")?,
            charge_secret: charge_secret(&gateway_token),
        };

        let mut render = RenderOptions::default();
        for hook in var("SEKISHO_HOOKS_FAIL_OPEN")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
        {
            match hook {
                "authz" => render.fail_open_authz = true,
                "ratelimit" => render.fail_open_ratelimit = true,
                "extproc" => render.fail_open_extproc = true,
                other => {
                    return Err(invalid(
                        "SEKISHO_HOOKS_FAIL_OPEN",
                        format!("unknown hook {other:?}; use authz, ratelimit or extproc"),
                    ));
                }
            }
        }
        render.bare_route = match var("SEKISHO_E2E_BARE_ROUTE").as_deref() {
            None | Some("0") => false,
            Some("1") if cfg!(feature = "e2e") => true,
            Some("1") => {
                return Err(invalid(
                    "SEKISHO_E2E_BARE_ROUTE",
                    "the bare route is only for the end-to-end tests: it serves a provider \
                     with its key and no key or budget checks, so only a build with the \
                     e2e feature can turn it on"
                        .to_owned(),
                ));
            }
            Some(other) => {
                return Err(invalid(
                    "SEKISHO_E2E_BARE_ROUTE",
                    format!("use 1 or 0, not {other:?}"),
                ));
            }
        };

        let mut catalog = CatalogSettings::default();
        if let Some(value) = var("SEKISHO_CATALOG_SYNC_INTERVAL") {
            let every =
                duration(&value).map_err(|r| invalid("SEKISHO_CATALOG_SYNC_INTERVAL", r))?;
            if !(SYNC_EVERY_MIN..=SYNC_EVERY_MAX).contains(&every) {
                return Err(invalid(
                    "SEKISHO_CATALOG_SYNC_INTERVAL",
                    "use 1m to 7d".to_owned(),
                ));
            }
            catalog.sync_every = every;
        }
        catalog.allow_plain_http = match var("SEKISHO_E2E_PLAIN_HTTP").as_deref() {
            None | Some("0") => false,
            Some("1") if cfg!(feature = "e2e") => true,
            Some("1") => {
                return Err(invalid(
                    "SEKISHO_E2E_PLAIN_HTTP",
                    "plain HTTP to Git hosts, registries and agents is only for the                      end-to-end tests' mock, so only a build with the e2e feature can turn it on"
                        .to_owned(),
                ));
            }
            Some(other) => {
                return Err(invalid(
                    "SEKISHO_E2E_PLAIN_HTTP",
                    format!("use 1 or 0, not {other:?}"),
                ));
            }
        };

        Ok(Self {
            database_url,
            database_max_connections,
            secret_box,
            listen: addr("SEKISHO_LISTEN", "0.0.0.0:8080")?,
            hooks_http_listen: addr("SEKISHO_HOOKS_HTTP_LISTEN", "0.0.0.0:9080")?,
            hooks_grpc_listen: addr("SEKISHO_HOOKS_GRPC_LISTEN", "0.0.0.0:9081")?,
            dex,
            settings: Settings {
                bootstrap_token,
                gateway_token,
                hooks,
                render,
                long_poll: LONG_POLL,
                republish: REPUBLISH,
                public_url,
                admin_groups,
                catalog,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";

    fn required() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("SEKISHO_DATABASE_URL", "postgres://sekisho@db/sekisho"),
            ("SEKISHO_ENCRYPTION_KEY", KEY),
            ("SEKISHO_BOOTSTRAP_TOKEN", "bootstrap-token-0123456789"),
            ("SEKISHO_GATEWAY_TOKEN", "gateway-token-0123456789"),
            ("SEKISHO_HOOKS_HTTP_TARGET", "sekisho:9080"),
            ("SEKISHO_HOOKS_GRPC_TARGET", "sekisho:9081"),
            ("SEKISHO_PUBLIC_URL", "https://sekisho.example.com/"),
            ("SEKISHO_DEX_ISSUER", "https://sekisho.example.com/dex"),
            ("SEKISHO_DEX_GRPC", "dex:5557"),
        ])
    }

    fn load(vars: &HashMap<&'static str, &'static str>) -> Result<ServerConfig, ConfigError> {
        ServerConfig::from_lookup(|name| vars.get(name).map(|v| v.to_string()))
    }

    fn error(vars: &HashMap<&'static str, &'static str>) -> String {
        load(vars).unwrap_err().to_string()
    }

    #[test]
    fn defaults_apply() {
        let c = load(&required()).unwrap();
        assert_eq!(c.database_url, "postgres://sekisho@db/sekisho");
        assert_eq!(c.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(c.hooks_http_listen.to_string(), "0.0.0.0:9080");
        assert_eq!(c.hooks_grpc_listen.to_string(), "0.0.0.0:9081");
        assert_eq!(c.settings.hooks.authz_http, "sekisho:9080");
        assert_eq!(c.settings.hooks.grpc, "sekisho:9081");
        assert_eq!(c.settings.render, RenderOptions::default());
        assert_eq!(c.settings.long_poll, std::time::Duration::from_secs(30));
        assert_eq!(c.database_max_connections, 20);
    }

    #[test]
    fn the_pool_size_is_set_or_refused() {
        let mut vars = required();
        vars.insert("SEKISHO_DATABASE_MAX_CONNECTIONS", "50");
        assert_eq!(load(&vars).unwrap().database_max_connections, 50);
        for bad in ["0", "-1", "lots", "1001"] {
            vars.insert("SEKISHO_DATABASE_MAX_CONNECTIONS", bad);
            assert!(
                error(&vars).contains("SEKISHO_DATABASE_MAX_CONNECTIONS"),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_missing_value_names_its_variable() {
        for name in required().keys() {
            let mut vars = required();
            vars.remove(name);
            assert!(error(&vars).contains(name), "{name}");
            vars.insert(name, "");
            assert!(error(&vars).contains(name), "{name} empty");
        }
    }

    #[test]
    fn fail_open_and_bare_route_map_to_render_options() {
        let mut vars = required();
        vars.insert("SEKISHO_HOOKS_FAIL_OPEN", "authz, extproc");
        let render = load(&vars).unwrap().settings.render;
        assert_eq!(
            render,
            RenderOptions {
                fail_open_authz: true,
                fail_open_ratelimit: false,
                fail_open_extproc: true,
                bare_route: false
            }
        );
        vars.insert("SEKISHO_HOOKS_FAIL_OPEN", "ratelimit");
        vars.insert("SEKISHO_E2E_BARE_ROUTE", "0");
        let render = load(&vars).unwrap().settings.render;
        assert!(render.fail_open_ratelimit && !render.fail_open_authz && !render.bare_route);

        vars.insert("SEKISHO_HOOKS_FAIL_OPEN", "authz,everything");
        assert!(error(&vars).contains("SEKISHO_HOOKS_FAIL_OPEN"));
        vars.insert("SEKISHO_HOOKS_FAIL_OPEN", "");
        vars.insert("SEKISHO_E2E_BARE_ROUTE", "yes");
        assert!(error(&vars).contains("SEKISHO_E2E_BARE_ROUTE"));
    }

    /// The bare route serves a provider with its key and no checks, so only a
    /// build for the end-to-end tests (`--features e2e`) can turn it on.
    #[test]
    #[cfg(not(feature = "e2e"))]
    fn the_bare_route_needs_an_e2e_build() {
        let mut vars = required();
        vars.insert("SEKISHO_E2E_BARE_ROUTE", "1");
        let message = error(&vars);
        assert!(
            message.contains("SEKISHO_E2E_BARE_ROUTE") && message.contains("e2e"),
            "{message}"
        );
    }

    #[test]
    #[cfg(feature = "e2e")]
    fn an_e2e_build_can_turn_on_the_bare_route() {
        let mut vars = required();
        vars.insert("SEKISHO_E2E_BARE_ROUTE", "1");
        assert!(load(&vars).unwrap().settings.render.bare_route);
    }

    /// Plain HTTP to Git hosts, registries and agents is for the
    /// end-to-end tests' mock alone.
    #[test]
    #[cfg(not(feature = "e2e"))]
    fn plain_http_needs_an_e2e_build() {
        let mut vars = required();
        vars.insert("SEKISHO_E2E_PLAIN_HTTP", "1");
        let message = error(&vars);
        assert!(
            message.contains("SEKISHO_E2E_PLAIN_HTTP") && message.contains("e2e"),
            "{message}"
        );
        vars.insert("SEKISHO_E2E_PLAIN_HTTP", "0");
        assert!(!load(&vars).unwrap().settings.catalog.allow_plain_http);
    }

    #[test]
    #[cfg(feature = "e2e")]
    fn an_e2e_build_can_turn_on_plain_http() {
        let mut vars = required();
        vars.insert("SEKISHO_E2E_PLAIN_HTTP", "1");
        assert!(load(&vars).unwrap().settings.catalog.allow_plain_http);
    }

    #[test]
    fn the_catalog_sync_interval_is_read_and_bounded() {
        let mut vars = required();
        let catalog = load(&vars).unwrap().settings.catalog;
        assert_eq!(catalog.sync_every, Duration::from_secs(3600));
        assert!(!catalog.allow_plain_http);
        assert_eq!(catalog.git_program, std::path::PathBuf::from("git"));
        for (value, seconds) in [
            ("30m", 1800),
            ("2h", 7200),
            ("90s", 90),
            ("1d", 86_400),
            ("1h", 3600),
        ] {
            vars.insert("SEKISHO_CATALOG_SYNC_INTERVAL", value);
            assert_eq!(
                load(&vars).unwrap().settings.catalog.sync_every,
                Duration::from_secs(seconds),
                "{value}"
            );
        }
        for bad in ["59s", "8d", "60", "1x", "-1h", "h", "1.5h", ""] {
            vars.insert("SEKISHO_CATALOG_SYNC_INTERVAL", bad);
            if bad.is_empty() {
                // An empty value counts as unset.
                assert!(load(&vars).is_ok());
                continue;
            }
            assert!(
                error(&vars).contains("SEKISHO_CATALOG_SYNC_INTERVAL"),
                "{bad}"
            );
        }
    }

    #[test]
    fn invalid_values_name_their_variable_without_echoing_secrets() {
        let mut vars = required();
        vars.insert("SEKISHO_LISTEN", "not-an-address");
        assert!(error(&vars).contains("SEKISHO_LISTEN"));

        let mut vars = required();
        vars.insert("SEKISHO_ENCRYPTION_KEY", "c2hvcnQ=");
        let msg = error(&vars);
        assert!(
            msg.contains("SEKISHO_ENCRYPTION_KEY") && !msg.contains("c2hvcnQ="),
            "{msg}"
        );

        for name in ["SEKISHO_BOOTSTRAP_TOKEN", "SEKISHO_GATEWAY_TOKEN"] {
            let mut vars = required();
            vars.insert(name, "short");
            let msg = error(&vars);
            assert!(msg.contains(name) && !msg.contains("short"), "{msg}");
        }
        let mut vars = required();
        vars.insert("SEKISHO_GATEWAY_TOKEN", "bootstrap-token-0123456789");
        assert!(error(&vars).contains("SEKISHO_GATEWAY_TOKEN")); // must differ from the bootstrap token
    }

    #[test]
    fn new_settings_are_required_and_checked() {
        let c = load(&required()).unwrap();
        // The trailing slash goes; Dex is reached at the issuer's origin by
        // default; no admin groups.
        assert_eq!(
            c.settings.public_url.as_str(),
            "https://sekisho.example.com/"
        );
        assert_eq!(c.dex.issuer.as_str(), "https://sekisho.example.com/dex");
        assert_eq!(c.dex.http_url.as_str(), "https://sekisho.example.com/");
        assert_eq!(c.dex.grpc, "dex:5557");
        assert!(c.settings.admin_groups.is_empty());

        let mut vars = required();
        vars.insert("SEKISHO_PUBLIC_URL", "https://sekisho.example.com/sekisho/");
        vars.insert("SEKISHO_DEX_URL", "http://dex:5556");
        vars.insert("SEKISHO_ADMIN_GROUPS", " platform , github/acme:admins ,");
        let c = load(&vars).unwrap();
        assert_eq!(
            c.settings.public_url.as_str(),
            "https://sekisho.example.com/sekisho"
        );
        assert_eq!(c.dex.http_url.as_str(), "http://dex:5556/");
        assert_eq!(
            c.settings.admin_groups,
            vec!["platform", "github/acme:admins"]
        );

        for (name, value) in [
            ("SEKISHO_PUBLIC_URL", "https://sekisho.example.com/?x=1"),
            ("SEKISHO_PUBLIC_URL", "https://sekisho.example.com/#top"),
            ("SEKISHO_PUBLIC_URL", "ftp://sekisho.example.com/"),
            ("SEKISHO_PUBLIC_URL", "sekisho.example.com"),
            ("SEKISHO_PUBLIC_URL", "https://user:pw@sekisho.example.com/"),
            ("SEKISHO_DEX_ISSUER", "dex"),
            ("SEKISHO_DEX_ISSUER", "https://dex.example.com/?x=1"),
            ("SEKISHO_DEX_URL", "file:///dex"),
            ("SEKISHO_DEX_GRPC", "dex"),
            ("SEKISHO_DEX_GRPC", "http://dex:5557"),
            ("SEKISHO_DEX_GRPC", "dex:port"),
            // An identity provider's group needs its connector (D124).
            ("SEKISHO_ADMIN_GROUPS", "platform,acme:admins"),
            ("SEKISHO_ADMIN_GROUPS", "Platform"),
            ("SEKISHO_ADMIN_GROUPS", "github/"),
        ] {
            let mut vars = required();
            vars.insert(name, value);
            let message = error(&vars);
            assert!(message.contains(name), "{name}={value}: {message}");
        }
    }

    #[test]
    fn debug_hides_secrets() {
        let config = load(&required()).unwrap();
        let shown = format!("{config:?}");
        for secret in [
            "bootstrap-token-0123456789",
            "gateway-token-0123456789",
            KEY,
            config.settings.hooks.charge_secret.as_str(),
        ] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[test]
    fn charge_secret_is_derived_from_the_gateway_token() {
        let secret = load(&required()).unwrap().settings.hooks.charge_secret;
        assert_eq!(secret.len(), 64);
        assert!(secret.bytes().all(|c| c.is_ascii_hexdigit()));
        assert!(!secret.contains("gateway-token"));
        // Every replica derives the same one; another token, another secret.
        assert_eq!(
            load(&required()).unwrap().settings.hooks.charge_secret,
            secret
        );
        let mut vars = required();
        vars.insert("SEKISHO_GATEWAY_TOKEN", "another-gateway-token-0123456789");
        assert_ne!(load(&vars).unwrap().settings.hooks.charge_secret, secret);
    }
}
