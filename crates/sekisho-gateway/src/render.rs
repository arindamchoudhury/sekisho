//! agentgateway v1.7.0's local config file, rendered from a snapshot.
//!
//! One `llm:` block mounted at `/env` serves every environment's model
//! routes: each provider is written once with its credential, and each
//! environment and model gets a model entry matched on `x-sekisho-env`.
//! Its policies run in agentgateway's fixed order on the client's own path:
//! `extAuthz`, the authorization rule, the rate limit, `ext_proc`, then the
//! transformation that sets the environment header and rewrites the path
//! for the model router.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use sekisho_core::formats::{AzureResourceType, HostApi, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::ModelName;
use sekisho_core::pricing::Measure;
use sekisho_store::address::read_saved_base_url;
use sekisho_store::{ProviderCredential, ProviderOptions};
use serde_json::{Map, Value, json};

use crate::snapshot::{GatewaySnapshot, ModelEntry, PricedModel, ProviderEntry};

/// Per-install choices that change the rendered config.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderOptions {
    /// Let requests through when `extAuthz` can't answer (`failureMode: allow`).
    pub fail_open_authz: bool,
    /// Let requests through when the rate-limit service can't answer.
    pub fail_open_ratelimit: bool,
    /// Let streams continue when the `ext_proc` service can't answer.
    pub fail_open_extproc: bool,
    /// Adds a hook-free `/bare/openai` route to the provider of the first
    /// model entry of an OpenAI kind. Only for the end-to-end latency test.
    pub bare_route: bool,
}

/// The only requests that reach a provider: POSTs to the model endpoints
/// of the three client formats, with no query but Claude Code's
/// `beta=true` and the Gemini SDKs' `alt=sse`. A Sekisho token in Gemini's
/// `?key=`, plain or percent-encoded, is refused here. One CEL expression,
/// evaluated on the client's own path before the rewrite.
pub const MODEL_ROUTE_RULE: &str = concat!(
    "request.method == 'POST'",
    " && request.path.matches('^/env/[a-z0-9-]{1,63}/(anthropic/v1/messages(/count_tokens)?",
    "|openai/v1/(chat/completions|responses)",
    "|gemini/v1(alpha|beta)?/models/[A-Za-z0-9._-]+:(generateContent|streamGenerateContent|countTokens))$')",
    " && request.pathAndQuery.matches('^[^?]*([?]beta=true|[?]alt=sse)?$')",
);

const PORT: u16 = 3000;
/// The gateway (and listener) every route attaches to.
const GATEWAY: &str = "sekisho";
/// Where the model routes are mounted: `/env/<name>/anthropic|openai|gemini/…`.
const PATH_PREFIX: &str = "/env";
const RATE_LIMIT_DOMAIN: &str = "sekisho";
/// The header the model entries match on. Set from the path, overwriting
/// any client copy, and removed before the provider.
const ENV_HEADER: &str = "x-sekisho-env";
/// The model the request names, told to the `extAuthz` hook.
const MODEL_HEADER: &str = "x-sekisho-model";
/// The environment as request metadata, which the charges read: the model
/// entry removes the header before the rate-limit descriptors are built.
const ENV_METADATA: &str = "sekisho_env";
/// The environment: the path's second segment (`/env/<name>/…`), which the
/// authorization rule limits to `[a-z0-9-]`.
const ENV_FROM_PATH: &str = "request.path.split(\"/\")[2]";
/// `/env/<name>/<format>/v1/…` to `/env/v1/…`, the standard endpoint under
/// the block's prefix, which the model router strips. Keeps the query.
const PATH_REWRITE: &str =
    "request.pathAndQuery.regexReplace(\"^/env/[a-z0-9-]+/(anthropic|openai|gemini)/\", \"/env/\")";
/// Headers a client might carry its Sekisho token in. The provider's
/// credential is added after the transformation removes them.
const CLIENT_CREDENTIAL_HEADERS: [&str; 4] =
    ["authorization", "x-api-key", "api-key", "x-goog-api-key"];

/// The YAML agentgateway reads. Deterministic: providers sorted by name,
/// model entries by environment name (each environment's models in its own
/// order), catalog entries by key and model.
pub fn render(snapshot: &GatewaySnapshot, options: &RenderOptions) -> String {
    // agentgateway refuses the whole file if one base URL doesn't parse, so
    // a provider saved with one it can't read is left out, with its models:
    // the rest of the config keeps loading.
    let unreadable: Vec<&str> = snapshot
        .providers
        .iter()
        .filter(|p| {
            p.base_url
                .as_deref()
                .is_some_and(|u| Upstream::parse(u).is_none())
        })
        .map(|p| {
            tracing::warn!(
                provider = p.name.as_str(),
                "agentgateway can't read this provider's base URL; its models are left out \
                 of the gateway config until it is fixed"
            );
            p.name.as_str()
        })
        .collect();
    let mut providers: Vec<&ProviderEntry> = snapshot
        .providers
        .iter()
        .filter(|p| !unreadable.contains(&p.name.as_str()))
        .collect();
    providers.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    let mut models: Vec<&ModelEntry> = snapshot
        .models
        .iter()
        .filter(|m| !unreadable.contains(&m.provider.as_str()))
        .collect();
    // Stable, so each environment keeps its own order.
    models.sort_by(|a, b| a.env.as_str().cmp(b.env.as_str()));

    let mut doc = Map::new();
    if !snapshot.priced_models.is_empty() {
        doc.insert(
            "config".into(),
            json!({ "modelCatalog": [{ "inline": { "providers": catalog(&snapshot.priced_models) } }] }),
        );
    }
    doc.insert("gateways".into(), json!({ GATEWAY: { "port": PORT } }));
    let provider_blocks: Vec<Value> = providers.iter().map(|p| provider_block(p)).collect();
    let model_entries: Vec<Value> = models.iter().map(|m| model_entry(m)).collect();
    doc.insert(
        "llm".into(),
        json!({
            "gateways": GATEWAY,
            "pathPrefix": PATH_PREFIX,
            "policies": llm_policies(snapshot, options),
            "providers": provider_blocks,
            "models": model_entries,
        }),
    );
    if options.bare_route
        && let Some(route) = bare_route(&providers, &models)
    {
        doc.insert("routes".into(), json!([route]));
    }
    serde_yaml_ng::to_string(&Value::Object(doc)).expect("a JSON value always serialises to YAML")
}

/// The policies every model route shares, in the order agentgateway runs
/// them: `extAuthz` (keys, environment, budget pre-check), the
/// authorization rule, the rate-limit service (spend charging), `ext_proc`
/// (cutting streams on revocation), then the transformation.
fn llm_policies(snapshot: &GatewaySnapshot, options: &RenderOptions) -> Value {
    let hooks = &snapshot.hooks;
    json!({
        "extAuthz": {
            "host": hooks.authz_http,
            "protocol": { "http": {
                // Overwrite any client copy: the environment from the path,
                // the model from the body or the Gemini path.
                "addRequestHeaders": {
                    ENV_HEADER: ENV_FROM_PATH,
                    MODEL_HEADER: "llm.requestModel",
                },
                // Exposes the key id the hook verified (`none` for a
                // session) as `extauthz.key`, and the person it names as
                // `extauthz.person`.
                "metadata": {
                    "key": "response.headers[\"x-sekisho-key\"]",
                    "person": "response.headers[\"x-sekisho-person\"]",
                },
            } },
            "includeRequestHeaders": ["authorization"],
            "failureMode": if options.fail_open_authz { "allow" } else { "deny" },
        },
        "authorization": { "rules": [{ "require": MODEL_ROUTE_RULE }] },
        "remoteRateLimit": {
            "host": hooks.grpc,
            "domain": RATE_LIMIT_DOMAIN,
            "failureMode": if options.fail_open_ratelimit { "failOpen" } else { "failClosed" },
            "descriptors": spend_descriptors(&hooks.charge_secret),
        },
        "extProc": {
            "host": hooks.grpc,
            "failureMode": if options.fail_open_extproc { "failOpen" } else { "failClosed" },
            "processingOptions": {
                "requestBodyMode": "none",
                "responseBodyMode": "fullDuplexStreamed",
            },
        },
        "transformations": { "request": {
            "set": { ENV_HEADER: ENV_FROM_PATH, ":path": PATH_REWRITE },
            "remove": CLIENT_CREDENTIAL_HEADERS,
            "metadata": { ENV_METADATA: ENV_FROM_PATH },
        } },
    })
}

/// One descriptor per [`Measure`], all with the same entries. agentgateway
/// evaluates the entries when the request starts and each cost when it ends;
/// a cost it can't evaluate leaves its descriptor out. The descriptor
/// without a cost carries the prompt count on the request-side call.
fn spend_descriptors(charge_secret: &str) -> Value {
    Measure::ALL
        .iter()
        .map(|measure| {
            let cost = match measure {
                Measure::Input => Some("llm.inputTokens"),
                Measure::CacheRead => Some("llm.cachedInputTokens"),
                Measure::CacheWrite => Some("llm.cacheCreationInputTokens"),
                Measure::Output => Some("llm.outputTokens"),
                // The catalog price of the finished request, in micro-USD.
                Measure::Catalog => Some("int(llm.cost.total * 1000000.0 + 0.5)"),
                // No cost: agentgateway sends its prompt count when the
                // request starts.
                Measure::Prompt => None,
            };
            let mut descriptor = json!({
                "type": "tokens",
                "entries": [
                    // Not the header: the model entry has removed it by now,
                    // and an entry that can't be evaluated drops the
                    // descriptor silently.
                    { "key": "env", "value": format!("metadata.{ENV_METADATA}") },
                    // Not the Authorization header: agentgateway evaluates these
                    // after the provider's credential has replaced it.
                    { "key": "key", "value": "extauthz.key" },
                    // The person charged: the key's owner, or the session's user.
                    { "key": "person", "value": "extauthz.person" },
                    { "key": "secret", "value": cel_string(charge_secret) },
                    { "key": "model", "value": "llm.requestModel" },
                    // The ID Sekisho's stream hook gave the request, so the
                    // charge can find what the hook counted.
                    { "key": "request", "value": "default(extproc.sekisho_request, \"\")" },
                    { "key": "measure", "value": cel_string(measure.as_str()) },
                ],
            });
            if let Some(cost) = cost {
                descriptor["cost"] = json!(cost);
            }
            descriptor
        })
        .collect()
}

/// A provider in `llm.providers`, which the model entries name. Its
/// `params` and `defaults` reach every entry that names it.
fn provider_block(p: &ProviderEntry) -> Value {
    let provider = match p.kind {
        ProviderKind::Anthropic => json!("anthropic"),
        ProviderKind::OpenAi => json!("openAI"),
        ProviderKind::Gemini => json!("gemini"),
        ProviderKind::Bedrock => json!("bedrock"),
        ProviderKind::Azure => json!("azure"),
        ProviderKind::OpenAiCompatible => json!({ "custom": custom_provider(p) }),
    };
    let mut params = Map::new();
    if let Some(url) = &p.base_url {
        params.insert(
            "baseUrl".into(),
            json!(
                Upstream::parse(url)
                    .expect("unreadable providers are left out")
                    .base_url(p.kind)
            ),
        );
    }
    match (p.kind, &p.options) {
        (ProviderKind::Bedrock, ProviderOptions::Bedrock { region }) => {
            params.insert("awsRegion".into(), json!(region));
        }
        (ProviderKind::Azure, ProviderOptions::Azure { endpoint }) => {
            params.insert("azureResourceName".into(), json!(endpoint.resource_name));
            params.insert(
                "azureResourceType".into(),
                json!(match endpoint.resource_type {
                    AzureResourceType::OpenAi => "openAI",
                    AzureResourceType::Foundry => "foundry",
                }),
            );
            if let Some(project) = &endpoint.project_name {
                params.insert("azureProjectName".into(), json!(project));
            }
        }
        // The store always gives these their setting. agentgateway refuses
        // the whole file without it, so a field the schema refuses keeps
        // such a config from being published.
        (ProviderKind::Bedrock | ProviderKind::Azure, _) => {
            params.insert(
                "unsupported".into(),
                json!(format!("a {} provider needs its setting", p.kind.as_str())),
            );
        }
        _ => {}
    }
    // agentgateway counts each prompt before sending it, for estimating
    // streams that end early.
    params.insert("tokenize".into(), json!(true));
    json!({
        "name": p.name.as_str(),
        "provider": provider,
        "params": params,
        "defaults": { "auth": provider_auth(p) },
    })
}

/// An OpenAI-compatible host: the catalog key its prices sit under (its
/// name when it has none) and the APIs it serves, each passed through.
fn custom_provider(p: &ProviderEntry) -> Value {
    let formats: Vec<Value> = p
        .apis
        .iter()
        .map(|a| {
            let mut format = json!({ "type": match a.api {
                HostApi::ChatCompletions => "completions",
                HostApi::Messages => "messages",
                HostApi::Responses => "responses",
            } });
            if let Some(path) = &a.path {
                format["path"] = json!(path);
            }
            format
        })
        .collect();
    json!({
        "providerOverride": p.catalog_key.as_deref().unwrap_or(p.name.as_str()),
        "formats": formats,
    })
}

/// One environment's model, served by the provider it names, matched on
/// the environment header the transformation set from the path.
fn model_entry(m: &ModelEntry) -> Value {
    json!({
        "id": format!("{}/{}", m.env.as_str(), m.model.as_str()),
        "name": m.model.as_str(),
        "provider": { "reference": m.provider.as_str() },
        "matches": [{ "headers": [{ "name": ENV_HEADER, "value": { "exact": m.env.as_str() } }] }],
        "requestHeaders": { "remove": [ENV_HEADER] },
    })
}

/// How agentgateway authenticates to the provider. Always explicit: with a
/// base URL, agentgateway v1.7.0 skips Bedrock's and Azure's default cloud
/// auth (`proxy/httpproxy.rs`), so a request would go out unsigned. A cloud
/// credential on a kind that doesn't take it (the store refuses such
/// providers) renders a block the schema refuses, so it is never sent.
fn provider_auth(p: &ProviderEntry) -> Value {
    match (&p.credential, p.kind) {
        (ProviderCredential::Key(key), _) => json!({ "key": key }),
        (
            ProviderCredential::AwsAccessKeys {
                access_key_id,
                secret_access_key,
                session_token,
            },
            ProviderKind::Bedrock,
        ) => {
            let mut aws = json!({
                "accessKeyId": access_key_id,
                "secretAccessKey": secret_access_key,
            });
            if let Some(token) = session_token {
                aws["sessionToken"] = json!(token);
            }
            json!({ "aws": aws })
        }
        (
            ProviderCredential::AzureClientSecret {
                tenant_id,
                client_id,
                client_secret,
            },
            ProviderKind::Azure,
        ) => json!({ "azure": { "explicitConfig": { "clientSecret": {
            "tenant_id": tenant_id,
            "client_id": client_id,
            "client_secret": client_secret,
        } } } }),
        // agentgateway signs with the gateway's own AWS credentials, for the
        // region the Bedrock provider names, base URL or not.
        (ProviderCredential::CloudIdentity, ProviderKind::Bedrock) => json!({ "aws": {} }),
        (ProviderCredential::CloudIdentity, ProviderKind::Azure) => {
            json!({ "azure": { "implicit": {} } })
        }
        (credential, kind) => json!({ "unsupported": format!(
            "{} credentials don't go to {} providers",
            credential.kind().as_str(),
            kind.as_str()
        ) }),
    }
}

/// The hook-free route of the end-to-end latency test, to the provider of
/// the first model entry of an OpenAI kind, for that environment's models
/// of that provider. An ordinary route, outside the `llm:` block.
fn bare_route(providers: &[&ProviderEntry], models: &[&ModelEntry]) -> Option<Value> {
    let by_name: HashMap<&str, &ProviderEntry> =
        providers.iter().map(|p| (p.name.as_str(), *p)).collect();
    let (first, provider) = models.iter().find_map(|m| {
        let p = by_name.get(m.provider.as_str())?;
        matches!(
            p.kind,
            ProviderKind::OpenAi | ProviderKind::OpenAiCompatible
        )
        .then_some((m, *p))
    })?;
    let served: Vec<&ModelName> = models
        .iter()
        .filter(|m| m.env == first.env && m.provider == first.provider)
        .map(|m| &m.model)
        .collect();
    Some(json!({
        "gateways": GATEWAY,
        "name": "bare-openai",
        // agentgateway sends the client's method upstream, so a GET would
        // reach the provider (OpenAI lists stored chat completions at
        // `GET /v1/chat/completions`). Model calls are POSTs.
        "matches": [{ "path": { "pathPrefix": "/bare/openai" }, "method": "POST" }],
        "policies": {
            // `Authorization` is replaced by the provider's credential.
            "requestHeaderModifier": { "remove": &CLIENT_CREDENTIAL_HEADERS[1..] },
            "urlRewrite": { "path": { "prefix": "/" } },
        },
        "backends": [bare_backend(provider, &served)],
    }))
}

/// An `ai` backend for an OpenAI or OpenAI-compatible provider. No prompt
/// counting, so the latency test that compares this route measures it
/// together with the hooks.
fn bare_backend(p: &ProviderEntry, models: &[&ModelName]) -> Value {
    let provider = match p.kind {
        ProviderKind::OpenAi => json!({ "openAI": {} }),
        _ => json!({ "custom": custom_provider(p) }),
    };
    let mut ai = Map::new();
    ai.insert("name".into(), json!(p.name.as_str()));
    ai.insert("provider".into(), provider);
    let mut policies = Map::new();
    if let Some(url) = &p.base_url {
        let upstream = Upstream::parse(url).expect("unreadable providers are left out");
        ai.insert("hostOverride".into(), json!(upstream.host_port));
        // With a host override and no prefix, agentgateway v1.7.0 sends a
        // built-in kind the client's own path, which could be any of the
        // provider's endpoints. A prefix makes it choose the model endpoint.
        if let Some(prefix) = upstream
            .path_prefix
            .as_deref()
            .or(default_base_path(p.kind))
        {
            ai.insert("pathPrefix".into(), json!(prefix));
        }
        if upstream.tls {
            policies.insert("backendTLS".into(), json!({}));
        }
    }
    // Without route types agentgateway handles every request as a chat
    // completion.
    policies.insert(
        "ai".into(),
        json!({ "routes": {
            "/v1/chat/completions": "completions",
            "/v1/responses": "responses",
        } }),
    );
    policies.insert("backendAuth".into(), provider_auth(p));
    let models: Vec<String> = models.iter().map(|m| cel_string(m.as_str())).collect();
    policies.insert(
        "authorization".into(),
        json!({ "rules": [{ "require": format!("llm.requestModel in [{}]", models.join(", ")) }] }),
    );
    json!({ "ai": ai, "policies": policies })
}

/// The base path of a built-in Anthropic or OpenAI provider whose base URL
/// has none: agentgateway appends each endpoint to the base URL's path.
/// Gemini's, Bedrock's and Azure's endpoint paths are absolute
/// (`/v1beta/…`, `/model/{id}/converse`, `/openai/v1/…`), and an
/// OpenAI-compatible host's base URL is used as the host documents it.
fn default_base_path(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::Anthropic | ProviderKind::OpenAi => Some("/v1"),
        ProviderKind::Gemini
        | ProviderKind::Bedrock
        | ProviderKind::Azure
        | ProviderKind::OpenAiCompatible => None,
    }
}

/// A CEL string literal. Names are limited to `[A-Za-z0-9._:/-]`, which need
/// no escaping, but JSON quoting keeps it safe regardless.
fn cel_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serialises")
}

/// A provider base URL's parts as agentgateway v1.7.0 reads them: scheme,
/// host and port, and path. Built with the store's own parser
/// ([`read_saved_base_url`]), so what is rendered is what was checked.
struct Upstream {
    host_port: String,
    path_prefix: Option<String>,
    tls: bool,
}

impl Upstream {
    /// `None` for a URL agentgateway can't read, saved before the store
    /// read URLs as agentgateway does. User info, a query and a fragment,
    /// which it refuses, are dropped.
    fn parse(url: &str) -> Option<Self> {
        let url = read_saved_base_url(url).ok()?;
        Some(Self {
            host_port: url.host_port(),
            path_prefix: (!url.path.is_empty()).then_some(url.path),
            tls: url.tls,
        })
    }

    /// `scheme://host:port[path]`, built from the parsed parts only. A
    /// built-in Anthropic or OpenAI provider without a path gets `/v1`.
    fn base_url(&self, kind: ProviderKind) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        let path = self
            .path_prefix
            .as_deref()
            .or(default_base_path(kind))
            .unwrap_or_default();
        format!("{scheme}://{}{path}", self.host_port)
    }
}

/// Inline catalog entries, under the keys agentgateway looks prices up by
/// ([`PricedModel::catalog_key`]), each cache price filled in.
fn catalog(priced: &[PricedModel]) -> Value {
    let mut providers: BTreeMap<&str, BTreeMap<&str, Value>> = BTreeMap::new();
    for m in priced {
        let rates = m.prices.rates(m.cache);
        providers.entry(m.catalog_key.as_str()).or_default().insert(
            m.model.as_str(),
            json!({ "rates": {
                "input": usd(rates.input),
                "output": usd(rates.output),
                "cacheRead": usd(rates.cache_read),
                "cacheWrite": usd(rates.cache_write),
            } }),
        );
    }
    let providers: Map<String, Value> = providers
        .into_iter()
        .map(|(key, models)| {
            let models: Map<String, Value> =
                models.into_iter().map(|(m, v)| (m.to_owned(), v)).collect();
            (key.to_owned(), json!({ "models": models }))
        })
        .collect();
    Value::Object(providers)
}

/// An exact USD decimal for agentgateway's catalog: `4500` micro-USD is
/// `"0.0045"`, `3000000` is `"3"`.
pub fn usd(amount: MicroUsd) -> String {
    amount.to_usd_string()
}

/// One violation without the offending value: the config path, the schema
/// keyword and where it sits in the schema. Unexpected field names are kept,
/// including those inside `anyOf`/`oneOf` branches: names are not secret, and
/// they make typos easy to find.
fn describe(e: &jsonschema::ValidationError<'_>) -> String {
    let path = e.instance_path().to_string();
    let mut message = format!(
        "{}: fails the schema's `{}` rule at {}",
        if path.is_empty() { "/" } else { &path },
        e.kind().keyword(),
        e.schema_path(),
    );
    let mut unexpected = Vec::new();
    unexpected_fields(e, &mut unexpected);
    unexpected.sort_unstable();
    unexpected.dedup();
    if !unexpected.is_empty() {
        message.push_str(&format!(" (unexpected: {})", unexpected.join(", ")));
    }
    message
}

/// Field names the schema didn't expect, at `e` or in its branches.
fn unexpected_fields(e: &jsonschema::ValidationError<'_>, out: &mut Vec<String>) {
    use jsonschema::error::ValidationErrorKind as Kind;
    match e.kind() {
        Kind::AdditionalProperties { unexpected } | Kind::UnevaluatedProperties { unexpected } => {
            out.extend(unexpected.iter().cloned());
        }
        Kind::AnyOf { context }
        | Kind::OneOfNotValid { context }
        | Kind::OneOfMultipleValid { context } => {
            for branch in context.iter().flatten() {
                unexpected_fields(branch, out);
            }
        }
        _ => {}
    }
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
///
/// A config holds decrypted provider keys and the messages are logged, so
/// they say where the violation is and which rule failed, but never quote a
/// value from the config.
pub fn validate(yaml: &str) -> Result<(), Vec<String>> {
    let doc: Value = serde_yaml_ng::from_str(yaml).map_err(|e| {
        vec![match e.location() {
            Some(at) => format!("not YAML (line {}, column {})", at.line(), at.column()),
            None => "not YAML".to_owned(),
        }]
    })?;
    let errors: Vec<String> = SCHEMA.iter_errors(&doc).map(|e| describe(&e)).collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
