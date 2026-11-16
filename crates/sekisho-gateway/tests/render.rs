use sekisho_core::formats::{AzureEndpoint, HostApi, HostApiPath, HostApis, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_core::pricing::{CacheDefaults, ModelPrices};
use sekisho_gateway::{
    GatewaySnapshot, HookTargets, MODEL_ROUTE_RULE, ModelEntry, PricedModel, ProviderEntry,
    RenderOptions, render, usd, validate,
};
use sekisho_store::{ProviderCredential, ProviderOptions};
use serde_json::{Value, json};

mod fixtures {
    use super::*;

    pub fn hooks() -> HookTargets {
        HookTargets {
            authz_http: "sekisho:9080".into(),
            grpc: "sekisho:9081".into(),
            charge_secret: "test-charge-secret".into(),
        }
    }

    /// A provider with the key `<name>-key`. Bedrock and Azure get their
    /// settings (us-east-1; an Azure OpenAI resource).
    pub fn provider(name: &str, kind: ProviderKind, base_url: Option<&str>) -> ProviderEntry {
        ProviderEntry {
            name: ProviderName::parse(name).unwrap(),
            kind,
            base_url: base_url.map(String::from),
            options: match kind {
                ProviderKind::Bedrock => bedrock_options("us-east-1"),
                ProviderKind::Azure => azure_options("https://my-resource.openai.azure.com"),
                _ => ProviderOptions::None,
            },
            credential: ProviderCredential::Key(format!("{name}-key")),
            apis: HostApis::chat_only(),
            catalog_key: None,
        }
    }

    pub fn entry(env: &str, model: &str, provider: &str) -> ModelEntry {
        ModelEntry {
            env: EnvName::parse(env).unwrap(),
            model: ModelName::parse(model).unwrap(),
            provider: ProviderName::parse(provider).unwrap(),
        }
    }

    pub fn bedrock_options(region: &str) -> ProviderOptions {
        ProviderOptions::Bedrock {
            region: region.into(),
        }
    }

    pub fn azure_options(endpoint: &str) -> ProviderOptions {
        ProviderOptions::Azure {
            endpoint: AzureEndpoint::parse(endpoint).unwrap(),
        }
    }

    pub fn aws_keys() -> ProviderCredential {
        ProviderCredential::AwsAccessKeys {
            access_key_id: "AKIAEXAMPLE0000001".into(),
            secret_access_key: "bedrock-secret".into(),
            session_token: Some("bedrock-session".into()),
        }
    }

    pub fn entra() -> ProviderCredential {
        ProviderCredential::AzureClientSecret {
            tenant_id: "11111111-2222-3333-4444-555555555555".into(),
            client_id: "66666666-7777-8888-9999-000000000000".into(),
            client_secret: "sp-secret".into(),
        }
    }

    pub fn priced(
        provider: &str,
        kind: ProviderKind,
        model: &str,
        input: i64,
        output: i64,
    ) -> PricedModel {
        PricedModel {
            provider: ProviderName::parse(provider).unwrap(),
            kind,
            catalog_key: kind.catalog_key().unwrap_or(provider).to_owned(),
            cache: CacheDefaults::for_model(kind, model),
            model: ModelName::parse(model).unwrap(),
            prices: ModelPrices {
                input: MicroUsd(input),
                output: MicroUsd(output),
                cache_read: None,
                cache_write: None,
            },
        }
    }

    /// Two environments, three model entries, two providers: data-team has
    /// mock-model (OpenAI-compatible `mock`, priced 3/15 USD per 1M) then
    /// claude-sonnet-5-5 (`anthropic`); ml-team has mock-model too.
    pub fn basic() -> GatewaySnapshot {
        GatewaySnapshot {
            providers: vec![
                provider("anthropic", ProviderKind::Anthropic, None),
                provider(
                    "mock",
                    ProviderKind::OpenAiCompatible,
                    Some("http://mock:8000/v1"),
                ),
            ],
            models: vec![
                entry("data-team", "mock-model", "mock"),
                entry("data-team", "claude-sonnet-5-5", "anthropic"),
                entry("ml-team", "mock-model", "mock"),
            ],
            priced_models: vec![priced(
                "mock",
                ProviderKind::OpenAiCompatible,
                "mock-model",
                3_000_000,
                15_000_000,
            )],
            hooks: hooks(),
        }
    }

    /// A host that serves all three APIs, DeepSeek-style Messages path.
    pub fn every_api() -> HostApis {
        HostApis::new(vec![
            HostApiPath {
                api: HostApi::Responses,
                path: None,
            },
            HostApiPath {
                api: HostApi::Messages,
                path: Some("/anthropic/v1/messages".into()),
            },
            HostApiPath {
                api: HostApi::ChatCompletions,
                path: None,
            },
        ])
        .unwrap()
    }

    /// One provider of every kind, in three environments, given out of
    /// order to check that render sorts. Bedrock signs with AWS keys, Azure
    /// with the gateway's cloud identity.
    pub fn every_kind() -> GatewaySnapshot {
        GatewaySnapshot {
            providers: vec![
                ProviderEntry {
                    apis: every_api(),
                    ..provider(
                        "vllm",
                        ProviderKind::OpenAiCompatible,
                        Some("https://vllm.example.com/v1"),
                    )
                },
                provider("openai", ProviderKind::OpenAi, None),
                provider("gemini", ProviderKind::Gemini, None),
                provider("anthropic", ProviderKind::Anthropic, None),
                ProviderEntry {
                    credential: aws_keys(),
                    ..provider("bedrock", ProviderKind::Bedrock, None)
                },
                ProviderEntry {
                    credential: ProviderCredential::CloudIdentity,
                    ..provider("azure", ProviderKind::Azure, None)
                },
            ],
            models: vec![
                entry("ml-team", "qwen3", "vllm"),
                entry("data-team", "claude-sonnet-5-5", "anthropic"),
                entry("data-team", "gpt-5", "openai"),
                entry("data-team", "gemini-3-pro", "gemini"),
                entry("data-team", "gpt-5-mini", "openai"),
                entry(
                    "cloud-team",
                    "us.anthropic.claude-sonnet-4-6-v1:0",
                    "bedrock",
                ),
                entry("cloud-team", "gpt-deployment", "azure"),
            ],
            priced_models: vec![
                priced(
                    "vllm",
                    ProviderKind::OpenAiCompatible,
                    "qwen3",
                    100_000,
                    400_000,
                ),
                priced(
                    "gemini",
                    ProviderKind::Gemini,
                    "gemini-3-pro",
                    2_000_000,
                    12_000_000,
                ),
                priced(
                    "anthropic",
                    ProviderKind::Anthropic,
                    "claude-sonnet-5-5",
                    3_000_000,
                    15_000_000,
                ),
                priced(
                    "openai",
                    ProviderKind::OpenAi,
                    "gpt-5",
                    1_250_000,
                    10_000_000,
                ),
                priced(
                    "bedrock",
                    ProviderKind::Bedrock,
                    "us.anthropic.claude-sonnet-4-6-v1:0",
                    3_000_000,
                    15_000_000,
                ),
                priced(
                    "azure",
                    ProviderKind::Azure,
                    "gpt-deployment",
                    1_250_000,
                    10_000_000,
                ),
            ],
            hooks: hooks(),
        }
    }

    /// `provider` alone, with one model in `team`.
    pub fn only(provider: ProviderEntry) -> GatewaySnapshot {
        GatewaySnapshot {
            models: vec![entry("team", "m", provider.name.as_str())],
            providers: vec![provider],
            priced_models: vec![],
            hooks: hooks(),
        }
    }
}

fn parse(yaml: &str) -> Value {
    serde_yaml_ng::from_str(yaml).unwrap()
}

/// The `llm:` block's policies, which every model route shares.
fn policies(doc: &Value) -> &Value {
    &doc["llm"]["policies"]
}

fn providers(doc: &Value) -> &Vec<Value> {
    doc["llm"]["providers"].as_array().unwrap()
}

fn provider_named(doc: &Value, name: &str) -> Value {
    providers(doc)
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("no provider {name}"))
        .clone()
}

fn model_entries(doc: &Value) -> &Vec<Value> {
    doc["llm"]["models"].as_array().unwrap()
}

/// `snapshot` rendered with default options, validated and parsed.
fn rendered(snapshot: &GatewaySnapshot) -> Value {
    let y = render(snapshot, &RenderOptions::default());
    assert_eq!(validate(&y), Ok(()), "{y}");
    parse(&y)
}

fn with_bare_route() -> RenderOptions {
    RenderOptions {
        bare_route: true,
        ..Default::default()
    }
}

#[test]
fn basic_matches_golden_and_schema() {
    let yaml = render(&fixtures::basic(), &RenderOptions::default());
    // Regenerate after a deliberate change, then review the diff by hand.
    if std::env::var_os("SEKISHO_UPDATE_GOLDEN").is_some() {
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/basic.yaml"),
            &yaml,
        )
        .unwrap();
    }
    assert_eq!(yaml, include_str!("golden/basic.yaml"));
    assert_eq!(validate(&yaml), Ok(())); // jsonschema Draft 2020-12 over schema/agentgateway-v1.7.0.config.schema.json
    let doc = parse(&yaml);
    assert_eq!(doc["gateways"], json!({ "sekisho": { "port": 3000 } }));
    assert_eq!(doc["llm"]["gateways"], "sekisho");
    assert_eq!(doc["llm"]["pathPrefix"], "/env");
    assert!(doc.get("binds").is_none() && doc.get("routes").is_none());
}

/// A rotated key changes one place, however many environments use the
/// provider.
#[test]
fn each_provider_is_written_once() {
    let p = ProviderEntry {
        credential: ProviderCredential::Key("k-unique-1".into()),
        ..fixtures::provider(
            "p",
            ProviderKind::OpenAiCompatible,
            Some("http://h:8000/v1"),
        )
    };
    let mut models = Vec::new();
    for env in ["team-a", "team-b", "team-c"] {
        for model in ["m-1", "m-2"] {
            models.push(fixtures::entry(env, model, "p"));
        }
    }
    let snapshot = GatewaySnapshot {
        providers: vec![p],
        models,
        priced_models: vec![],
        hooks: fixtures::hooks(),
    };
    let y = render(&snapshot, &RenderOptions::default());
    assert_eq!(validate(&y), Ok(()));
    assert_eq!(y.matches("k-unique-1").count(), 1, "{y}");
    let doc = parse(&y);
    assert_eq!(providers(&doc).len(), 1);
    assert_eq!(model_entries(&doc).len(), 6);
    for entry in model_entries(&doc) {
        assert_eq!(entry["provider"], json!({ "reference": "p" }), "{entry}");
    }
}

#[test]
fn model_entries_match_their_environment() {
    let snapshot = GatewaySnapshot {
        providers: vec![fixtures::provider("p", ProviderKind::Anthropic, None)],
        // Environments out of name order; each keeps its own model order.
        models: vec![
            fixtures::entry("zeta", "m-2", "p"),
            fixtures::entry("zeta", "m-1", "p"),
            fixtures::entry("alpha", "m-1", "p"),
            fixtures::entry("alpha", "m-3", "p"),
        ],
        priced_models: vec![],
        hooks: fixtures::hooks(),
    };
    let doc = rendered(&snapshot);
    let want: Vec<Value> = [
        ("alpha", "m-1"),
        ("alpha", "m-3"),
        ("zeta", "m-2"),
        ("zeta", "m-1"),
    ]
    .into_iter()
    .map(|(env, model)| {
        json!({
            "id": format!("{env}/{model}"),
            "name": model,
            "provider": { "reference": "p" },
            "matches": [{ "headers": [{ "name": "x-sekisho-env", "value": { "exact": env } }] }],
            "requestHeaders": { "remove": ["x-sekisho-env"] },
        })
    })
    .collect();
    assert_eq!(model_entries(&doc), &want);
}

/// agentgateway overwrites both headers from the path and the request body,
/// so a client's own `x-sekisho-env` or `x-sekisho-model` never chooses an
/// environment or a model.
#[test]
fn client_headers_never_choose_the_environment() {
    let doc = rendered(&fixtures::basic());
    let p = policies(&doc);
    let env_from_path = "request.path.split(\"/\")[2]";
    assert_eq!(
        p["extAuthz"]["protocol"]["http"]["addRequestHeaders"],
        json!({ "x-sekisho-env": env_from_path, "x-sekisho-model": "llm.requestModel" })
    );
    let request = &p["transformations"]["request"];
    assert_eq!(
        request["set"],
        json!({
            "x-sekisho-env": env_from_path,
            ":path": "request.pathAndQuery.regexReplace(\"^/env/[a-z0-9-]+/(anthropic|openai|gemini)/\", \"/env/\")",
        })
    );
    assert_eq!(request["metadata"], json!({ "sekisho_env": env_from_path }));
    let all = p.to_string();
    assert!(!all.contains("request.headers"), "{all}");
}

/// A model entry removes `x-sekisho-env` before the provider, and a
/// descriptor whose entry can't be evaluated is dropped silently, so the
/// environment comes from the transformation's metadata.
#[test]
fn charges_read_the_environment_from_metadata() {
    let doc = rendered(&fixtures::basic());
    let descriptors = policies(&doc)["remoteRateLimit"]["descriptors"]
        .as_array()
        .unwrap();
    assert_eq!(descriptors.len(), 6);
    for d in descriptors {
        let value = |key: &str| {
            d["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["key"] == key)
                .unwrap()["value"]
                .clone()
        };
        assert_eq!(value("env"), "metadata.sekisho_env", "{d}");
        assert_eq!(value("key"), "extauthz.key", "{d}");
        assert_eq!(value("person"), "extauthz.person", "{d}");
    }
}

/// Only POSTs to the model endpoints pass, with no query but Claude Code's
/// `beta=true` and Gemini's `alt=sse`: a Sekisho token in Gemini's `?key=`
/// never reaches a provider.
#[test]
fn only_two_queries_pass_the_rule() {
    let doc = rendered(&fixtures::basic());
    assert_eq!(
        policies(&doc)["authorization"],
        json!({ "rules": [{ "require": MODEL_ROUTE_RULE }] })
    );
    assert_eq!(
        MODEL_ROUTE_RULE,
        concat!(
            "request.method == 'POST'",
            " && request.path.matches('^/env/[a-z0-9-]{1,63}/(anthropic/v1/messages(/count_tokens)?",
            "|openai/v1/(chat/completions|responses)",
            "|gemini/v1(alpha|beta)?/models/[A-Za-z0-9._-]+:(generateContent|streamGenerateContent|countTokens))$')",
            " && request.pathAndQuery.matches('^[^?]*([?]beta=true|[?]alt=sse)?$')",
        )
    );
    assert!(MODEL_ROUTE_RULE.contains("request.method == 'POST'"));
    assert!(MODEL_ROUTE_RULE.contains("([?]beta=true|[?]alt=sse)?$"));
}

/// Every header a client might carry its Sekisho token in, `Authorization`
/// included; the provider's credential is added after the transformation.
#[test]
fn client_credentials_are_removed_before_the_provider() {
    let doc = rendered(&fixtures::every_kind());
    // And a client's own `x-sekisho-model`: the gateway sets it only on the
    // hook's request, so any copy that would reach the provider is the
    // client's. (`x-sekisho-env` is set here and removed by each model
    // entry, after the router matched on it.)
    assert_eq!(
        policies(&doc)["transformations"]["request"]["remove"],
        json!([
            "authorization",
            "x-api-key",
            "api-key",
            "x-goog-api-key",
            "x-sekisho-model"
        ])
    );
}

#[test]
fn each_kind_renders_its_provider_block() {
    let doc = rendered(&fixtures::every_kind());
    let names: Vec<&str> = providers(&doc)
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["anthropic", "azure", "bedrock", "gemini", "openai", "vllm"]
    );
    for (name, provider, params) in [
        ("anthropic", json!("anthropic"), json!({ "tokenize": true })),
        ("openai", json!("openAI"), json!({ "tokenize": true })),
        ("gemini", json!("gemini"), json!({ "tokenize": true })),
        (
            "vllm",
            json!({ "custom": {
                "providerOverride": "vllm",
                "formats": [
                    { "type": "completions" },
                    { "type": "messages", "path": "/anthropic/v1/messages" },
                    { "type": "responses" },
                ],
            } }),
            json!({ "baseUrl": "https://vllm.example.com:443/v1", "tokenize": true }),
        ),
        (
            "bedrock",
            json!("bedrock"),
            json!({ "awsRegion": "us-east-1", "tokenize": true }),
        ),
        (
            "azure",
            json!("azure"),
            json!({ "azureResourceName": "my-resource", "azureResourceType": "openAI", "tokenize": true }),
        ),
    ] {
        let block = provider_named(&doc, name);
        assert_eq!(block["provider"], provider, "{name}");
        assert_eq!(block["params"], params, "{name}");
    }
    for p in providers(&doc) {
        assert_eq!(p["params"]["tokenize"], true, "{p}");
    }

    // An Azure AI Foundry project.
    let foundry = fixtures::only(ProviderEntry {
        options: fixtures::azure_options(
            "https://my-foundry.services.ai.azure.com/api/projects/team-project",
        ),
        credential: fixtures::entra(),
        ..fixtures::provider("foundry", ProviderKind::Azure, None)
    });
    assert_eq!(
        provider_named(&rendered(&foundry), "foundry")["params"],
        json!({
            "azureResourceName": "my-foundry", "azureResourceType": "foundry",
            "azureProjectName": "team-project", "tokenize": true,
        })
    );

    // A host's catalog key names the catalog entry its prices sit under;
    // a host with Chat Completions only lists that.
    let scaleway = fixtures::only(ProviderEntry {
        catalog_key: Some("scaleway".into()),
        ..fixtures::provider(
            "scw",
            ProviderKind::OpenAiCompatible,
            Some("https://api.scaleway.ai/v1"),
        )
    });
    assert_eq!(
        provider_named(&rendered(&scaleway), "scw")["provider"],
        json!({ "custom": { "providerOverride": "scaleway", "formats": [{ "type": "completions" }] } })
    );
}

/// agentgateway appends each endpoint to the base URL's path, so built-in
/// Anthropic and OpenAI providers without one get `/v1`; Gemini's, Bedrock's
/// and Azure's endpoint paths are absolute.
#[test]
fn built_in_kinds_get_their_base_path() {
    for (kind, base_url, want) in [
        (
            ProviderKind::Anthropic,
            Some("https://proxy.example.com"),
            Some("https://proxy.example.com:443/v1"),
        ),
        (
            ProviderKind::Anthropic,
            Some("https://llm-proxy.example.com/anthropic/"),
            Some("https://llm-proxy.example.com:443/anthropic"),
        ),
        (
            ProviderKind::OpenAi,
            Some("http://h:8000/v1/"),
            Some("http://h:8000/v1"),
        ),
        (
            ProviderKind::OpenAi,
            Some("http://h:8000"),
            Some("http://h:8000/v1"),
        ),
        (
            ProviderKind::Gemini,
            Some("http://mock:8000"),
            Some("http://mock:8000"),
        ),
        (
            ProviderKind::Bedrock,
            Some("http://mock:8000"),
            Some("http://mock:8000"),
        ),
        (
            ProviderKind::Azure,
            Some("http://mock:8000"),
            Some("http://mock:8000"),
        ),
        (
            ProviderKind::OpenAiCompatible,
            Some("http://[::1]:8000"),
            Some("http://[::1]:8000"),
        ),
        (ProviderKind::Anthropic, None, None),
        (ProviderKind::Gemini, None, None),
    ] {
        let doc = rendered(&fixtures::only(fixtures::provider("p", kind, base_url)));
        assert_eq!(
            provider_named(&doc, "p")["params"].get("baseUrl"),
            want.map(|w| json!(w)).as_ref(),
            "{kind:?} {base_url:?}"
        );
    }
}

/// agentgateway v1.7.0 refuses a `baseUrl` with user info, a query or a
/// fragment when it loads the file, which would hold back every
/// environment's config. Rows saved before such URLs were refused render
/// without them.
#[test]
fn base_urls_render_without_user_info_query_or_fragment() {
    let snapshot = fixtures::only(fixtures::provider(
        "p",
        ProviderKind::OpenAiCompatible,
        Some("https://u:p@h.example/v1?x=1#f"),
    ));
    let y = render(&snapshot, &with_bare_route());
    assert_eq!(validate(&y), Ok(()));
    let doc = parse(&y);
    assert_eq!(
        provider_named(&doc, "p")["params"]["baseUrl"],
        "https://h.example:443/v1"
    );
    assert_eq!(
        doc["routes"][0]["backends"][0]["ai"]["hostOverride"],
        "h.example:443"
    );
    for leaked in ["u:p", "x=1", "#f"] {
        assert!(!y.contains(leaked), "{leaked} in {y}");
    }
}

/// The provider's credential as `defaults.auth`, which each model entry
/// that names the provider takes. Always explicit: with a base URL,
/// agentgateway v1.7.0 skips Bedrock's and Azure's default cloud auth, so a
/// request would go out unsigned.
#[test]
fn each_credential_renders_its_auth() {
    use fixtures::{aws_keys, entra, provider};
    let bedrock = |credential, base_url| ProviderEntry {
        credential,
        ..provider("p", ProviderKind::Bedrock, base_url)
    };
    let azure = |credential, base_url| ProviderEntry {
        credential,
        ..provider("p", ProviderKind::Azure, base_url)
    };
    for (entry, auth) in [
        (
            provider(
                "p",
                ProviderKind::OpenAiCompatible,
                Some("http://h:8000/v1"),
            ),
            json!({ "key": "p-key" }),
        ),
        (
            bedrock(ProviderCredential::Key("bedrock-api-key".into()), None),
            json!({ "key": "bedrock-api-key" }),
        ),
        (
            bedrock(aws_keys(), None),
            json!({ "aws": {
                "accessKeyId": "AKIAEXAMPLE0000001",
                "secretAccessKey": "bedrock-secret",
                "sessionToken": "bedrock-session",
            } }),
        ),
        (
            bedrock(
                ProviderCredential::AwsAccessKeys {
                    access_key_id: "AKIAEXAMPLE0000001".into(),
                    secret_access_key: "bedrock-secret".into(),
                    session_token: None,
                },
                None,
            ),
            json!({ "aws": {
                "accessKeyId": "AKIAEXAMPLE0000001",
                "secretAccessKey": "bedrock-secret",
            } }),
        ),
        (
            bedrock(ProviderCredential::CloudIdentity, None),
            json!({ "aws": {} }),
        ),
        (
            bedrock(ProviderCredential::CloudIdentity, Some("http://mock:8000")),
            json!({ "aws": {} }),
        ),
        (
            azure(ProviderCredential::Key("azure-api-key".into()), None),
            json!({ "key": "azure-api-key" }),
        ),
        (
            azure(entra(), None),
            json!({ "azure": { "explicitConfig": { "clientSecret": {
                "tenant_id": "11111111-2222-3333-4444-555555555555",
                "client_id": "66666666-7777-8888-9999-000000000000",
                "client_secret": "sp-secret",
            } } } }),
        ),
        (
            azure(ProviderCredential::CloudIdentity, None),
            json!({ "azure": { "implicit": {} } }),
        ),
        (
            azure(ProviderCredential::CloudIdentity, Some("http://mock:8000")),
            json!({ "azure": { "implicit": {} } }),
        ),
    ] {
        let shown = format!("{entry:?}");
        let doc = rendered(&fixtures::only(entry));
        assert_eq!(
            provider_named(&doc, "p")["defaults"],
            json!({ "auth": auth }),
            "{shown}"
        );
    }
}

/// A cloud credential never renders for a kind that doesn't take it (the
/// store refuses such providers): the config fails validation instead, so
/// it is never published.
#[test]
fn cloud_credentials_never_render_for_other_kinds() {
    use fixtures::{aws_keys, entra, provider};
    for (kind, credential) in [
        (ProviderKind::Anthropic, ProviderCredential::CloudIdentity),
        (ProviderKind::OpenAi, aws_keys()),
        (ProviderKind::Azure, aws_keys()),
        (ProviderKind::Bedrock, entra()),
    ] {
        let snapshot = fixtures::only(ProviderEntry {
            credential,
            ..provider("p", kind, None)
        });
        let y = render(&snapshot, &RenderOptions::default());
        assert!(validate(&y).is_err(), "{kind:?}: {y}");
        for secret in ["bedrock-secret", "bedrock-session", "sp-secret"] {
            assert!(!y.contains(secret), "{kind:?}: {y}");
        }
    }
}

/// agentgateway v1.7.0 refuses the whole file when a Bedrock provider has
/// no region or an Azure provider no resource (`types/local.rs`), which
/// would hold back every environment's config. The store always gives them
/// their setting; without it the config fails validation, so it is never
/// published.
#[test]
fn a_cloud_provider_without_its_setting_never_validates() {
    for kind in [ProviderKind::Bedrock, ProviderKind::Azure] {
        let snapshot = fixtures::only(ProviderEntry {
            options: ProviderOptions::None,
            ..fixtures::provider("p", kind, None)
        });
        let y = render(&snapshot, &RenderOptions::default());
        assert!(validate(&y).is_err(), "{kind:?}: {y}");
    }
}

/// The end-to-end latency test's route: the provider of the first model
/// entry of an OpenAI kind, without hooks or prompt counting.
#[test]
fn bare_route_skips_the_hooks() {
    let y = render(&fixtures::every_kind(), &with_bare_route());
    assert_eq!(validate(&y), Ok(()));
    let doc = parse(&y);
    let routes = doc["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 1);
    let bare = &routes[0];
    assert_eq!(bare["name"], "bare-openai");
    assert_eq!(bare["gateways"], "sekisho");
    // Model calls are POSTs; agentgateway sends the client's method upstream.
    assert_eq!(
        bare["matches"],
        json!([{ "path": { "pathPrefix": "/bare/openai" }, "method": "POST" }])
    );
    for hook in ["extAuthz", "remoteRateLimit", "extProc"] {
        assert!(bare["policies"].get(hook).is_none(), "{hook}");
    }
    assert_eq!(
        bare["policies"]["requestHeaderModifier"],
        json!({ "remove": ["x-api-key", "api-key", "x-goog-api-key"] })
    );
    // cloud-team (Bedrock, Azure) sorts first; data-team's first OpenAI
    // model is gpt-5, on `openai`.
    let backend = &bare["backends"][0];
    assert_eq!(
        backend["ai"],
        json!({ "name": "openai", "provider": { "openAI": {} } })
    );
    assert_eq!(
        backend["policies"]["ai"]["routes"],
        json!({ "/v1/chat/completions": "completions", "/v1/responses": "responses" })
    );
    assert_eq!(
        backend["policies"]["backendAuth"],
        json!({ "key": "openai-key" })
    );
    assert_eq!(
        backend["policies"]["authorization"]["rules"][0]["require"],
        "llm.requestModel in [\"gpt-5\", \"gpt-5-mini\"]"
    );

    // An OpenAI-compatible host keeps its base URL's path as the prefix.
    let doc = parse(&render(&fixtures::basic(), &with_bare_route()));
    let ai = &doc["routes"][0]["backends"][0]["ai"];
    assert_eq!(
        (&ai["name"], &ai["hostOverride"], &ai["pathPrefix"]),
        (&json!("mock"), &json!("mock:8000"), &json!("/v1"))
    );
    assert!(ai.get("tokenize").is_none());

    let plain = render(&fixtures::every_kind(), &RenderOptions::default());
    assert!(!plain.contains("/bare/") && parse(&plain).get("routes").is_none());
}

#[test]
fn an_environment_without_models_renders_no_entries() {
    let nothing = GatewaySnapshot {
        providers: vec![],
        models: vec![],
        priced_models: vec![],
        hooks: fixtures::hooks(),
    };
    let doc = rendered(&nothing);
    assert_eq!(doc["llm"]["providers"], json!([]));
    assert_eq!(doc["llm"]["models"], json!([]));
    // The hooks still guard the prefix, so an unknown environment is refused.
    assert!(policies(&doc)["extAuthz"].is_object());
    assert!(doc.get("config").is_none());
}

#[test]
fn hooks_fail_closed_by_default() {
    let y = render(&fixtures::basic(), &RenderOptions::default());
    assert!(y.contains("failureMode: deny") && y.matches("failureMode: failClosed").count() == 2);
}

#[test]
fn fail_open_option_switches_modes() {
    let y = render(
        &fixtures::basic(),
        &RenderOptions {
            fail_open_authz: true,
            ..Default::default()
        },
    );
    assert!(y.contains("failureMode: allow") && !y.contains("failureMode: deny"));
    assert_eq!(validate(&y), Ok(()));
}

#[test]
fn each_hook_fails_open_on_its_own() {
    let ratelimit = render(
        &fixtures::basic(),
        &RenderOptions {
            fail_open_ratelimit: true,
            ..Default::default()
        },
    );
    let doc = parse(&ratelimit);
    let p = policies(&doc);
    assert_eq!(
        (
            &p["extAuthz"]["failureMode"],
            &p["remoteRateLimit"]["failureMode"],
            &p["extProc"]["failureMode"]
        ),
        (
            &Value::from("deny"),
            &Value::from("failOpen"),
            &Value::from("failClosed")
        )
    );
    let extproc = render(
        &fixtures::basic(),
        &RenderOptions {
            fail_open_extproc: true,
            ..Default::default()
        },
    );
    let doc = parse(&extproc);
    let p = policies(&doc);
    assert_eq!(
        (
            &p["remoteRateLimit"]["failureMode"],
            &p["extProc"]["failureMode"]
        ),
        (&Value::from("failClosed"), &Value::from("failOpen"))
    );
    assert_eq!(validate(&ratelimit), Ok(()));
    assert_eq!(validate(&extproc), Ok(()));
}

#[test]
fn mutation_is_rejected_by_schema() {
    // proves the schema check bites
    let y = render(&fixtures::basic(), &RenderOptions::default());
    assert!(validate(&y.replace("failureMode: deny", "failureMode: maybe")).is_err());
    // A misspelled field is caught too: the schema's objects are closed.
    assert!(validate(&y.replace("processingOptions:", "processingOption:")).is_err());
    assert!(y.contains("  pathPrefix: /env\n"));
    assert!(validate(&y.replace("  pathPrefix: /env\n", "  pathPrefx: /env\n")).is_err());
    let errors = validate("binds: 3").unwrap_err();
    assert!(
        !errors.is_empty() && errors[0].contains("binds"),
        "{errors:?}"
    );
    assert!(validate("binds: [").is_err()); // not YAML at all
}

/// A rendered config holds decrypted provider keys, and validation errors go
/// to the logs, so they say where and which rule failed, never a value.
#[test]
fn validation_errors_never_echo_config_values() {
    let y = render(&fixtures::basic(), &RenderOptions::default());
    let key_line = "        key: mock-key\n";
    let port_line = "    port: 3000\n";
    assert!(y.contains(key_line) && y.contains(port_line), "{y}");
    let cases = [
        // The `llm:` block is a choice of shapes (anyOf), so anything wrong
        // inside it fails the block as a whole, and the message used to
        // print it whole.
        (y.replace(key_line, "        key: [mock-key]\n"), "/llm"),
        (y.replace("  - name: mock\n", "  - nam: mock\n"), "/llm"),
        // A wrong type, carrying a secret.
        (
            y.replace(port_line, "    port: sk-secret-in-port\n"),
            "/gateways/sekisho/port",
        ),
    ];
    for (broken, location) in cases {
        let errors = validate(&broken).unwrap_err();
        assert!(errors.iter().any(|e| e.contains(location)), "{errors:?}");
        for e in &errors {
            assert!(!e.contains("mock-key") && !e.contains("sk-secret"), "{e}");
        }
    }
    // A misspelled field is still named, since field names are not secrets.
    let typo = y.replace("processingOptions:", "processingOption:");
    let errors = validate(&typo).unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("processingOption")),
        "{errors:?}"
    );
}

#[test]
fn render_is_deterministic() {
    let a = render(&fixtures::every_kind(), &with_bare_route());
    let mut shuffled = fixtures::every_kind();
    shuffled.providers.reverse();
    shuffled.priced_models.reverse();
    // Environments in reverse name order, each keeping its own model order.
    shuffled
        .models
        .sort_by(|x, y| y.env.as_str().cmp(x.env.as_str()));
    assert_eq!(a, render(&shuffled, &with_bare_route()));
}

#[test]
fn usd_strings_are_exact() {
    for (micro, text) in [
        (0, "0"),
        (1, "0.000001"),
        (4_500, "0.0045"),
        (3_000_000, "3"),
        (1_250_000, "1.25"),
        (15_000_000, "15"),
    ] {
        assert_eq!(usd(MicroUsd(micro)), text);
    }
}

#[test]
fn debug_output_hides_provider_keys() {
    let shown = format!("{:?}", fixtures::basic());
    for secret in ["mock-key", "anthropic-key", "test-charge-secret"] {
        assert!(!shown.contains(secret), "{secret} in {shown}");
    }
    let mut every = fixtures::every_kind();
    every.providers[5].credential = fixtures::entra(); // azure
    let shown = format!("{every:?}");
    for secret in [
        "openai-key",
        "AKIAEXAMPLE0000001",
        "bedrock-secret",
        "bedrock-session",
        "66666666-7777-8888-9999-000000000000",
        "sp-secret",
    ] {
        assert!(!shown.contains(secret), "{secret} in {shown}");
    }
}

/// agentgateway evaluates descriptor entries after `backendAuth` has replaced
/// `Authorization` with the provider key, so spend is keyed on the key id
/// Sekisho's own extAuthz hook verified, exposed as `extauthz.key`.
#[test]
fn spend_is_charged_to_the_key_extauthz_verified() {
    let doc = rendered(&fixtures::basic());
    let p = policies(&doc);
    assert_eq!(
        p["extAuthz"]["protocol"]["http"]["metadata"],
        json!({
            "key": "response.headers[\"x-sekisho-key\"]",
            "person": "response.headers[\"x-sekisho-person\"]",
        })
    );
    assert_eq!(
        p["extAuthz"]["includeRequestHeaders"],
        json!(["authorization"])
    );
    // The charge secret lets the charging hook refuse descriptors that
    // didn't come from this install's agentgateway.
    assert_eq!(
        p["remoteRateLimit"]["descriptors"][0]["entries"],
        json!([
            { "key": "env", "value": "metadata.sekisho_env" },
            { "key": "key", "value": "extauthz.key" },
            { "key": "person", "value": "extauthz.person" },
            { "key": "secret", "value": "\"test-charge-secret\"" },
            { "key": "model", "value": "llm.requestModel" },
            { "key": "request", "value": "default(extproc.sekisho_request, \"\")" },
            { "key": "measure", "value": "\"input\"" }
        ])
    );
}

/// Spend is charged to the person extAuthz named (the key's owner, or a
/// session's user), on every descriptor.
#[test]
fn descriptors_name_the_person() {
    let doc = rendered(&fixtures::every_kind());
    let descriptors = policies(&doc)["remoteRateLimit"]["descriptors"]
        .as_array()
        .unwrap();
    assert!(!descriptors.is_empty());
    for descriptor in descriptors {
        let person: Vec<_> = descriptor["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["key"] == "person")
            .collect();
        assert_eq!(
            person,
            [&json!({ "key": "person", "value": "extauthz.person" })]
        );
    }
}

/// One descriptor per measure, each costed when the request ends: a cost
/// agentgateway can't evaluate (no output count after a stream ended early)
/// skips its descriptor, which is how Sekisho knows to estimate.
#[test]
fn descriptors_report_each_measure() {
    let doc = rendered(&fixtures::basic());
    let descriptors = policies(&doc)["remoteRateLimit"]["descriptors"]
        .as_array()
        .unwrap()
        .clone();
    let measure = |d: &Value| {
        let entries = d["entries"].as_array().unwrap();
        let entry = entries.iter().find(|e| e["key"] == "measure").unwrap();
        entry["value"].as_str().unwrap().to_owned()
    };
    assert_eq!(
        descriptors.iter().map(measure).collect::<Vec<_>>(),
        [
            "\"input\"",
            "\"cache_read\"",
            "\"cache_write\"",
            "\"output\"",
            "\"catalog\"",
            "\"prompt\""
        ]
    );
    // Without a cost, agentgateway sends its prompt count on the
    // request-side call, which is all Sekisho hears of a stream that ends
    // early.
    let costs: Vec<Option<&str>> = descriptors.iter().map(|d| d["cost"].as_str()).collect();
    assert_eq!(
        costs,
        [
            Some("llm.inputTokens"),
            Some("llm.cachedInputTokens"),
            Some("llm.cacheCreationInputTokens"),
            Some("llm.outputTokens"),
            Some("int(llm.cost.total * 1000000.0 + 0.5)"),
            None
        ]
    );
    assert_eq!(descriptors[5].get("cost"), None);
    for d in &descriptors {
        assert_eq!(d["type"], "tokens");
        let keys: Vec<&str> = d["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "env", "key", "person", "secret", "model", "request", "measure"
            ]
        );
        assert_eq!(
            d["entries"][5]["value"],
            "default(extproc.sekisho_request, \"\")"
        );
    }
    let p = policies(&doc);
    assert_eq!(p["remoteRateLimit"]["host"], "sekisho:9081");
    assert_eq!(p["remoteRateLimit"]["domain"], "sekisho");
    assert_eq!(
        p["extProc"]["processingOptions"],
        json!({ "requestBodyMode": "none", "responseBodyMode": "fullDuplexStreamed" })
    );
}

#[test]
fn inline_catalog_carries_cache_rates() {
    let mut snapshot = fixtures::basic();
    snapshot.priced_models = vec![
        fixtures::priced(
            "anthropic",
            ProviderKind::Anthropic,
            "claude-sonnet-5-5",
            3_000_000,
            15_000_000,
        ),
        PricedModel {
            prices: ModelPrices {
                cache_read: Some(MicroUsd(100_000)),
                ..fixtures::priced(
                    "openai",
                    ProviderKind::OpenAi,
                    "gpt-5",
                    3_000_000,
                    15_000_000,
                )
                .prices
            },
            ..fixtures::priced(
                "openai",
                ProviderKind::OpenAi,
                "gpt-5",
                3_000_000,
                15_000_000,
            )
        },
        fixtures::priced(
            "scaleway",
            ProviderKind::OpenAiCompatible,
            "qwen3-235b-a22b-instruct-2507",
            750_000,
            2_250_000,
        ),
        fixtures::priced(
            "bedrock",
            ProviderKind::Bedrock,
            "amazon.nova-pro-v1:0",
            800_000,
            3_200_000,
        ),
        fixtures::priced(
            "gemini",
            ProviderKind::Gemini,
            "gemini-3-pro",
            2_000_000,
            12_000_000,
        ),
        fixtures::priced(
            "azure",
            ProviderKind::Azure,
            "gpt-deployment",
            1_250_000,
            10_000_000,
        ),
    ];
    let doc = rendered(&snapshot);
    let catalog = &doc["config"]["modelCatalog"][0]["inline"]["providers"];
    // Under agentgateway's provider names: gcp.gemini for Gemini,
    // aws.bedrock for Bedrock, a host's name for OpenAI-compatible ones.
    let keys: Vec<&String> = catalog.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "anthropic",
            "aws.bedrock",
            "azure",
            "gcp.gemini",
            "openai",
            "scaleway"
        ]
    );
    assert_eq!(
        catalog["anthropic"]["models"]["claude-sonnet-5-5"]["rates"],
        json!({ "input": "3", "output": "15", "cacheRead": "0.3", "cacheWrite": "3.75" })
    );
    assert_eq!(
        catalog["openai"]["models"]["gpt-5"]["rates"],
        json!({ "input": "3", "output": "15", "cacheRead": "0.1", "cacheWrite": "3" })
    );
    // An OpenAI-compatible model's cache prices are its input price, filed
    // under the provider's name (its providerOverride).
    assert_eq!(
        catalog["scaleway"]["models"]["qwen3-235b-a22b-instruct-2507"]["rates"],
        json!({ "input": "0.75", "output": "2.25", "cacheRead": "0.75", "cacheWrite": "0.75" })
    );
    // A Bedrock model that isn't Claude takes OpenAI's ratios.
    assert_eq!(
        catalog["aws.bedrock"]["models"]["amazon.nova-pro-v1:0"]["rates"],
        json!({ "input": "0.8", "output": "3.2", "cacheRead": "0.4", "cacheWrite": "0.8" })
    );
    assert_eq!(
        catalog["azure"]["models"]["gpt-deployment"]["rates"],
        json!({ "input": "1.25", "output": "10", "cacheRead": "0.625", "cacheWrite": "1.25" })
    );
}

/// A provider saved before plan 1d whose base URL agentgateway can't read,
/// or that hides a cloud metadata address, is left out with its model
/// entries: agentgateway would refuse the whole file, and stop every
/// environment's config from updating. Others render as agentgateway reads
/// them.
#[test]
fn a_base_url_agentgateway_cannot_read_is_left_out() {
    let mut snapshot = fixtures::basic();
    for (name, url) in [
        ("typo", "http://10.0.0.256:11434/v1"),
        ("hidden", "http://169.254.169.254BACKSLASHx.com/v1"),
        ("cased", "http://Ollama.LAN:11434/v1/"),
    ] {
        let url = url.replace("BACKSLASH", "\\");
        snapshot.providers.push(fixtures::provider(
            name,
            ProviderKind::OpenAiCompatible,
            Some(&url),
        ));
        snapshot
            .models
            .push(fixtures::entry("data-team", &format!("{name}-model"), name));
    }
    let y = render(&snapshot, &with_bare_route());
    assert_eq!(validate(&y), Ok(()));
    let doc = parse(&y);
    let names: Vec<&str> = providers(&doc)
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["anthropic", "cased", "mock"], "{y}");
    let ids: Vec<&str> = model_entries(&doc)
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(
        !ids.iter()
            .any(|id| id.contains("typo") || id.contains("hidden")),
        "{ids:?}"
    );
    assert!(ids.contains(&"data-team/cased-model"), "{ids:?}");
    assert_eq!(
        provider_named(&doc, "cased")["params"]["baseUrl"],
        "http://ollama.lan:11434/v1"
    );
    for leaked in ["10.0.0.256", "169.254.169.254"] {
        assert!(!y.contains(leaked), "{leaked} in {y}");
    }
}
