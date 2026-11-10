//! `sekisho serve` settings, read from `SEKISHO_*` environment variables.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use sekisho_core::keys::hash_secret;
use sekisho_core::secret_box::SecretBox;
use sekisho_gateway::{HookTargets, RenderOptions};

use crate::app::Settings;

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

pub struct ServerConfig {
    /// May contain a password, so `Debug` leaves it out.
    pub database_url: String,
    pub secret_box: SecretBox,
    pub listen: SocketAddr,
    pub hooks_http_listen: SocketAddr,
    pub hooks_grpc_listen: SocketAddr,
    pub settings: Settings,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("listen", &self.listen)
            .field("hooks_http_listen", &self.hooks_http_listen)
            .field("hooks_grpc_listen", &self.hooks_grpc_listen)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
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

        Ok(Self {
            database_url,
            secret_box,
            listen: addr("SEKISHO_LISTEN", "0.0.0.0:8080")?,
            hooks_http_listen: addr("SEKISHO_HOOKS_HTTP_LISTEN", "0.0.0.0:9080")?,
            hooks_grpc_listen: addr("SEKISHO_HOOKS_GRPC_LISTEN", "0.0.0.0:9081")?,
            settings: Settings {
                bootstrap_token,
                gateway_token,
                hooks,
                render,
                long_poll: LONG_POLL,
                republish: REPUBLISH,
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
