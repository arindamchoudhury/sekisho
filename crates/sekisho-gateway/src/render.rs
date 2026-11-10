//! agentgateway v1.7.0's local config file, rendered from a snapshot.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::money::MicroUsd;
use serde_json::{Map, Value, json};

use crate::snapshot::{EnvRoutes, GatewaySnapshot, ModelRoute, PricedModel};

/// Per-install choices that change the rendered config.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderOptions {
    /// Let requests through when `extAuthz` can't answer (`failureMode: allow`).
    pub fail_open_authz: bool,
    /// Let requests through when the rate-limit service can't answer.
    pub fail_open_ratelimit: bool,
    /// Let streams continue when the `ext_proc` service can't answer.
    pub fail_open_extproc: bool,
    /// Adds a hook-free `/bare/openai` route to the first environment's
    /// OpenAI-format backend. Only for the end-to-end latency test.
    pub bare_route: bool,
}

const PORT: u16 = 3000;
const LISTENER: &str = "sekisho";
const RATE_LIMIT_DOMAIN: &str = "sekisho";
/// Headers a client might carry its Sekisho token in. `Authorization` is
/// replaced by the provider key (`backendAuth`), so it is not listed.
const STRIPPED_HEADERS: [&str; 3] = ["x-api-key", "api-key", "x-goog-api-key"];

/// The YAML agentgateway reads. Deterministic: environments sorted by name,
/// routes by format, catalog entries by provider and model.
pub fn render(snapshot: &GatewaySnapshot, options: &RenderOptions) -> String {
    let mut envs: Vec<&EnvRoutes> = snapshot.environments.iter().collect();
    envs.sort_by(|a, b| a.env.as_str().cmp(b.env.as_str()));

    let mut routes = Vec::new();
    for env in &envs {
        let mut env_routes: Vec<&ModelRoute> = env.routes.iter().collect();
        env_routes.sort_by_key(|r| r.format.path_segment());
        for route in env_routes {
            let segment = route.format.path_segment();
            let mut policies = forwarding_policies();
            policies.extend(hook_policies(&env.env.to_string(), snapshot, options));
            routes.push(json!({
                "name": format!("{}-{segment}", env.env),
                "matches": [route_match(&format!("/env/{}/{segment}", env.env))],
                "policies": policies,
                "backends": [backend(route)],
            }));
        }
    }
    if options.bare_route
        && let Some(route) = envs
            .iter()
            .find_map(|e| e.routes.iter().find(|r| r.format == ApiFormat::OpenAi))
    {
        routes.push(json!({
            "name": "bare-openai",
            "matches": [route_match("/bare/openai")],
            "policies": forwarding_policies(),
            "backends": [backend(route)],
        }));
    }

    let mut doc = Map::new();
    if !snapshot.priced_models.is_empty() {
        doc.insert(
            "config".into(),
            json!({ "modelCatalog": [{ "inline": { "providers": catalog(&snapshot.priced_models) } }] }),
        );
    }
    doc.insert(
        "binds".into(),
        json!([{
            "port": PORT,
            "listeners": [{ "name": LISTENER, "protocol": "HTTP", "routes": routes }],
        }]),
    );
    serde_yaml_ng::to_string(&Value::Object(doc)).expect("a JSON value always serialises to YAML")
}

/// Model calls are POSTs. agentgateway sends the client's method upstream, so
/// a GET would reach the provider (OpenAI lists stored chat completions at
/// `GET /v1/chat/completions`).
fn route_match(prefix: &str) -> Value {
    json!({ "path": { "pathPrefix": prefix }, "method": "POST" })
}

/// Strips client credentials and drops the `/env/<name>/<format>` prefix.
fn forwarding_policies() -> Map<String, Value> {
    let mut policies = Map::new();
    policies.insert(
        "requestHeaderModifier".into(),
        json!({ "remove": STRIPPED_HEADERS }),
    );
    policies.insert("urlRewrite".into(), json!({ "path": { "prefix": "/" } }));
    policies
}

/// `extAuthz` (keys, environment, budget pre-check), the rate-limit service
/// (spend charging) and `ext_proc` (cutting streams on revocation).
fn hook_policies(
    env: &str,
    snapshot: &GatewaySnapshot,
    options: &RenderOptions,
) -> Map<String, Value> {
    let hooks = &snapshot.hooks;
    let mut policies = Map::new();
    policies.insert(
        "extAuthz".into(),
        json!({
            "host": hooks.authz_http,
            "protocol": { "http": {
                // Names the route's environment to the hook; the client's
                // path is not trusted for it.
                "addRequestHeaders": { "x-sekisho-env": cel_string(env) },
                // Exposes the key id the hook verified as `extauthz.key`.
                "metadata": { "key": "response.headers[\"x-sekisho-key\"]" },
            } },
            "includeRequestHeaders": ["authorization"],
            "failureMode": if options.fail_open_authz { "allow" } else { "deny" },
        }),
    );
    policies.insert(
        "remoteRateLimit".into(),
        json!({
            "host": hooks.grpc,
            "domain": RATE_LIMIT_DOMAIN,
            "failureMode": if options.fail_open_ratelimit { "failOpen" } else { "failClosed" },
            "descriptors": [{
                "type": "tokens",
                "entries": [
                    { "key": "env", "value": cel_string(env) },
                    // Not the Authorization header: agentgateway evaluates these
                    // after backendAuth has put the provider key there.
                    { "key": "key", "value": "extauthz.key" },
                    { "key": "secret", "value": cel_string(&hooks.charge_secret) },
                ],
                // The catalog price of the finished request, in micro-USD.
                "cost": "int(llm.cost.total * 1000000.0)",
            }],
        }),
    );
    policies.insert(
        "extProc".into(),
        json!({
            "host": hooks.grpc,
            "failureMode": if options.fail_open_extproc { "failOpen" } else { "failClosed" },
            "processingOptions": {
                "requestBodyMode": "none",
                "responseBodyMode": "fullDuplexStreamed",
            },
        }),
    );
    policies
}

fn backend(route: &ModelRoute) -> Value {
    let provider = match route.kind {
        ProviderKind::Anthropic => json!({ "anthropic": {} }),
        ProviderKind::OpenAi => json!({ "openAI": {} }),
        ProviderKind::Gemini => json!({ "gemini": {} }),
        // The override names the catalog entry this provider's prices sit under.
        ProviderKind::OpenAiCompatible => json!({ "custom": {
            "providerOverride": route.provider.as_str(),
            "formats": [{ "type": "completions" }],
        } }),
    };
    let mut ai = Map::new();
    ai.insert("name".into(), json!(route.provider.as_str()));
    ai.insert("provider".into(), provider);
    let mut policies = Map::new();
    if let Some(url) = &route.base_url {
        let upstream = Upstream::parse(url);
        ai.insert("hostOverride".into(), json!(upstream.host_port));
        // With a host override and no prefix, agentgateway v1.7.0 sends a
        // built-in kind the client's own path, which could be any of the
        // provider's endpoints. A prefix makes it choose the model endpoint.
        if let Some(prefix) = upstream
            .path_prefix
            .or_else(|| default_path_prefix(route.kind))
        {
            ai.insert("pathPrefix".into(), json!(prefix));
        }
        if upstream.tls {
            policies.insert("backendTLS".into(), json!({}));
        }
    }
    policies.insert(
        "ai".into(),
        json!({ "routes": model_endpoints(route.format) }),
    );
    policies.insert("backendAuth".into(), json!({ "key": route.api_key }));
    let models: Vec<String> = route
        .models
        .iter()
        .map(|m| cel_string(m.as_str()))
        .collect();
    policies.insert(
        "authorization".into(),
        json!({ "rules": [{ "require": format!("llm.requestModel in [{}]", models.join(", ")) }] }),
    );
    json!({ "ai": ai, "policies": policies })
}

/// The provider's default base path, which a `pathPrefix` replaces. `None`
/// for OpenAI-compatible providers, which always choose their own path.
fn default_path_prefix(kind: ProviderKind) -> Option<String> {
    match kind {
        ProviderKind::Anthropic | ProviderKind::OpenAi => Some("/v1".into()),
        // Gemini's paths are absolute (`/v1beta/…`).
        ProviderKind::Gemini => Some("/".into()),
        ProviderKind::OpenAiCompatible => None,
    }
}

/// Each format's model endpoints, by path suffix, and how agentgateway
/// handles them. Without these it handles every request as an OpenAI chat
/// completion; any other path still is, and goes to the provider's chat
/// endpoint.
fn model_endpoints(format: ApiFormat) -> Value {
    match format {
        ApiFormat::Anthropic => json!({
            "/v1/messages": "messages",
            "/v1/messages/count_tokens": "anthropicTokenCount",
        }),
        ApiFormat::OpenAi => json!({
            "/v1/chat/completions": "completions",
            "/v1/responses": "responses",
        }),
        ApiFormat::Gemini => json!({
            ":generateContent": "generateContent",
            ":streamGenerateContent": "generateContent",
            ":countTokens": "geminiCountTokens",
        }),
    }
}

/// A CEL string literal. Names are limited to `[A-Za-z0-9._:/-]`, which need
/// no escaping, but JSON quoting keeps it safe regardless.
fn cel_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serialises")
}

/// A provider base URL as agentgateway's `hostOverride` and `pathPrefix`.
struct Upstream {
    host_port: String,
    path_prefix: Option<String>,
    tls: bool,
}

impl Upstream {
    /// `base_url` was checked to be `http://` or `https://` with a host when
    /// the provider was saved.
    fn parse(url: &str) -> Self {
        let (tls, rest) = match url.strip_prefix("https://") {
            Some(rest) => (true, rest),
            None => (false, url.strip_prefix("http://").unwrap_or(url)),
        };
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let has_port = match authority.rfind(']') {
            Some(end) => authority[end..].contains(':'), // [::1] or [::1]:8000
            None => authority.contains(':'),
        };
        let host_port = if has_port {
            authority.to_owned()
        } else {
            format!("{authority}:{}", if tls { 443 } else { 80 })
        };
        let path = path.trim_end_matches('/');
        Self {
            host_port,
            path_prefix: (!path.is_empty()).then(|| path.to_owned()),
            tls,
        }
    }
}

/// Inline catalog entries, under the provider names agentgateway looks
/// prices up by: its own for built-in kinds, and the provider's name for
/// OpenAI-compatible ones (their `providerOverride`).
fn catalog(priced: &[PricedModel]) -> Value {
    let mut providers: BTreeMap<String, BTreeMap<&str, Value>> = BTreeMap::new();
    for m in priced {
        let key = match m.kind {
            ProviderKind::Anthropic => "anthropic".to_owned(),
            ProviderKind::OpenAi => "openai".to_owned(),
            ProviderKind::Gemini => "gcp.gemini".to_owned(),
            ProviderKind::OpenAiCompatible => m.provider.as_str().to_owned(),
        };
        providers.entry(key).or_default().insert(
            m.model.as_str(),
            json!({ "rates": { "input": usd(m.input), "output": usd(m.output) } }),
        );
    }
    let providers: Map<String, Value> = providers
        .into_iter()
        .map(|(key, models)| {
            let models: Map<String, Value> =
                models.into_iter().map(|(m, v)| (m.to_owned(), v)).collect();
            (key, json!({ "models": models }))
        })
        .collect();
    Value::Object(providers)
}

/// An exact USD decimal for agentgateway's catalog: `4500` micro-USD is
/// `"0.0045"`, `3000000` is `"3"`.
pub fn usd(amount: MicroUsd) -> String {
    amount.to_usd_string()
}

static SCHEMA: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(include_str!(
        "../schema/agentgateway-v1.7.0.config.schema.json"
    ))
    .expect("the vendored schema is JSON");
    jsonschema::validator_for(&schema).expect("the vendored schema compiles")
});

/// Checks a config against agentgateway v1.7.0's published schema (draft
/// 2020-12), vendored at build time. Returns one message per violation.
pub fn validate(yaml: &str) -> Result<(), Vec<String>> {
    let doc: Value = serde_yaml_ng::from_str(yaml).map_err(|e| vec![format!("not YAML: {e}")])?;
    let errors: Vec<String> = SCHEMA
        .iter_errors(&doc)
        .map(|e| {
            let path = e.instance_path().to_string();
            format!("{}: {e}", if path.is_empty() { "/" } else { &path })
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
