use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_gateway::{
    EnvRoutes, GatewaySnapshot, HookTargets, ModelRoute, PricedModel, RenderOptions, render, usd,
    validate,
};
use serde_json::Value;

mod fixtures {
    use super::*;

    pub fn hooks() -> HookTargets {
        HookTargets {
            authz_http: "sekisho:9080".into(),
            grpc: "sekisho:9081".into(),
        }
    }

    pub fn route(
        format: ApiFormat,
        provider: &str,
        kind: ProviderKind,
        base_url: Option<&str>,
        models: &[&str],
    ) -> ModelRoute {
        ModelRoute {
            format,
            provider: ProviderName::parse(provider).unwrap(),
            kind,
            base_url: base_url.map(String::from),
            api_key: format!("{provider}-key"),
            models: models
                .iter()
                .map(|m| ModelName::parse(m).unwrap())
                .collect(),
        }
    }

    pub fn env(name: &str, routes: Vec<ModelRoute>) -> EnvRoutes {
        EnvRoutes {
            env: EnvName::parse(name).unwrap(),
            routes,
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
            model: ModelName::parse(model).unwrap(),
            input: MicroUsd(input),
            output: MicroUsd(output),
        }
    }

    /// data-team: OpenAiCompatible "mock" provider, model mock-model priced 3/15 USD per 1M.
    pub fn basic() -> GatewaySnapshot {
        GatewaySnapshot {
            environments: vec![env(
                "data-team",
                vec![route(
                    ApiFormat::OpenAi,
                    "mock",
                    ProviderKind::OpenAiCompatible,
                    Some("http://mock:8000/v1"),
                    &["mock-model"],
                )],
            )],
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

    pub fn with_empty_env() -> GatewaySnapshot {
        let mut s = basic();
        s.environments.push(env("empty", vec![]));
        s
    }

    /// Every provider kind, given out of order to check that render sorts.
    pub fn every_kind() -> GatewaySnapshot {
        GatewaySnapshot {
            environments: vec![
                env(
                    "ml-team",
                    vec![route(
                        ApiFormat::OpenAi,
                        "vllm",
                        ProviderKind::OpenAiCompatible,
                        Some("https://vllm.example.com"),
                        &["qwen3"],
                    )],
                ),
                env(
                    "data-team",
                    vec![
                        route(
                            ApiFormat::OpenAi,
                            "openai",
                            ProviderKind::OpenAi,
                            None,
                            &["gpt-5", "gpt-5-mini"],
                        ),
                        route(
                            ApiFormat::Gemini,
                            "gemini",
                            ProviderKind::Gemini,
                            None,
                            &["gemini-3-pro"],
                        ),
                        route(
                            ApiFormat::Anthropic,
                            "anthropic",
                            ProviderKind::Anthropic,
                            None,
                            &["claude-sonnet-5-5"],
                        ),
                    ],
                ),
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
            ],
            hooks: hooks(),
        }
    }
}

fn parse(yaml: &str) -> Value {
    serde_yaml_ng::from_str(yaml).unwrap()
}

fn routes(doc: &Value) -> &Vec<Value> {
    doc["binds"][0]["listeners"][0]["routes"]
        .as_array()
        .unwrap()
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
    let route = &routes(&doc)[0]["policies"];
    assert_eq!(
        (
            &route["extAuthz"]["failureMode"],
            &route["remoteRateLimit"]["failureMode"],
            &route["extProc"]["failureMode"]
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
    let route = &routes(&doc)[0]["policies"];
    assert_eq!(
        (
            &route["remoteRateLimit"]["failureMode"],
            &route["extProc"]["failureMode"]
        ),
        (&Value::from("failClosed"), &Value::from("failOpen"))
    );
    assert_eq!(validate(&ratelimit), Ok(()));
    assert_eq!(validate(&extproc), Ok(()));
}

#[test]
fn env_without_models_renders_no_routes() {
    let y = render(&fixtures::with_empty_env(), &RenderOptions::default());
    assert!(!y.contains("/env/empty/"));
    assert_eq!(validate(&y), Ok(()));
    let nothing = GatewaySnapshot {
        environments: vec![fixtures::env("empty", vec![])],
        priced_models: vec![],
        hooks: fixtures::hooks(),
    };
    assert_eq!(
        validate(&render(&nothing, &RenderOptions::default())),
        Ok(())
    );
}

#[test]
fn mutation_is_rejected_by_schema() {
    // proves the schema check bites
    let y = render(&fixtures::basic(), &RenderOptions::default())
        .replace("failureMode: deny", "failureMode: maybe");
    assert!(validate(&y).is_err());
    // A misspelled field is caught too: the schema's objects are closed.
    let typo = render(&fixtures::basic(), &RenderOptions::default())
        .replace("processingOptions:", "processingOption:");
    assert!(validate(&typo).is_err());
    let errors = validate("binds: 3").unwrap_err();
    assert!(
        !errors.is_empty() && errors[0].contains("binds"),
        "{errors:?}"
    );
    assert!(validate("binds: [").is_err()); // not YAML at all
}

#[test]
fn every_kind_renders_sorted_and_valid() {
    let y = render(&fixtures::every_kind(), &RenderOptions::default());
    assert_eq!(validate(&y), Ok(()));
    let doc = parse(&y);
    let names: Vec<&str> = routes(&doc)
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "data-team-anthropic",
            "data-team-gemini",
            "data-team-openai",
            "ml-team-openai"
        ]
    );
    let backend = |i: usize| routes(&doc)[i]["backends"][0].clone();
    assert_eq!(
        backend(0)["ai"]["provider"],
        serde_json::json!({ "anthropic": {} })
    );
    assert_eq!(
        backend(1)["ai"]["provider"],
        serde_json::json!({ "gemini": {} })
    );
    assert_eq!(
        backend(2)["ai"]["provider"],
        serde_json::json!({ "openAI": {} })
    );
    assert_eq!(
        backend(2)["policies"]["authorization"]["rules"][0]["require"],
        "llm.requestModel in [\"gpt-5\", \"gpt-5-mini\"]"
    );
    assert_eq!(backend(2)["policies"]["backendAuth"]["key"], "openai-key");
    // An https base URL connects with TLS on port 443; the path is kept as a prefix only when it has one.
    let vllm = backend(3);
    assert_eq!(vllm["ai"]["hostOverride"], "vllm.example.com:443");
    assert!(vllm["ai"].get("pathPrefix").is_none());
    assert_eq!(vllm["policies"]["backendTLS"], serde_json::json!({}));
    assert_eq!(vllm["ai"]["provider"]["custom"]["providerOverride"], "vllm");

    // Catalog entries sit under agentgateway's provider names: gcp.gemini for Gemini, the
    // provider's own name for OpenAI-compatible ones.
    let catalog = &doc["config"]["modelCatalog"][0]["inline"]["providers"];
    let keys: Vec<&String> = catalog.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["anthropic", "gcp.gemini", "openai", "vllm"]);
    assert_eq!(
        catalog["vllm"]["models"]["qwen3"]["rates"],
        serde_json::json!({ "input": "0.1", "output": "0.4" })
    );
    assert_eq!(
        catalog["openai"]["models"]["gpt-5"]["rates"],
        serde_json::json!({ "input": "1.25", "output": "10" })
    );
}

/// agentgateway evaluates descriptor entries after `backendAuth` has replaced
/// `Authorization` with the provider key, so spend is keyed on the key id
/// Sekisho's own extAuthz hook verified, exposed as `extauthz.key`.
#[test]
fn spend_is_charged_to_the_key_extauthz_verified() {
    let doc = parse(&render(&fixtures::basic(), &RenderOptions::default()));
    let policies = &routes(&doc)[0]["policies"];
    assert_eq!(
        policies["extAuthz"]["protocol"]["http"]["metadata"],
        serde_json::json!({ "key": "response.headers[\"x-sekisho-key\"]" })
    );
    assert_eq!(
        policies["remoteRateLimit"]["descriptors"][0]["entries"],
        serde_json::json!([
            { "key": "env", "value": "\"data-team\"" },
            { "key": "key", "value": "extauthz.key" }
        ])
    );
}

#[test]
fn client_credentials_are_stripped_on_every_route() {
    let doc = parse(&render(
        &fixtures::every_kind(),
        &RenderOptions {
            bare_route: true,
            ..Default::default()
        },
    ));
    for route in routes(&doc) {
        let removed: Vec<&str> = route["policies"]["requestHeaderModifier"]["remove"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h.as_str().unwrap())
            .collect();
        assert_eq!(
            removed,
            ["x-api-key", "api-key", "x-goog-api-key"],
            "{}",
            route["name"]
        );
    }
}

#[test]
fn bare_route_skips_the_hooks() {
    let y = render(
        &fixtures::every_kind(),
        &RenderOptions {
            bare_route: true,
            ..Default::default()
        },
    );
    assert_eq!(validate(&y), Ok(()));
    let doc = parse(&y);
    let bare: Vec<&Value> = routes(&doc)
        .iter()
        .filter(|r| r["name"] == "bare-openai")
        .collect();
    assert_eq!(bare.len(), 1);
    assert_eq!(bare[0]["matches"][0]["path"]["pathPrefix"], "/bare/openai");
    for hook in ["extAuthz", "remoteRateLimit", "extProc"] {
        assert!(bare[0]["policies"].get(hook).is_none(), "{hook}");
    }
    // The first environment's openai backend, unchanged.
    let data_openai = routes(&doc)
        .iter()
        .find(|r| r["name"] == "data-team-openai")
        .unwrap();
    assert_eq!(bare[0]["backends"], data_openai["backends"]);
    assert!(!render(&fixtures::every_kind(), &RenderOptions::default()).contains("/bare/"));
}

#[test]
fn render_is_deterministic() {
    let a = render(&fixtures::every_kind(), &RenderOptions::default());
    let mut reversed = fixtures::every_kind();
    reversed.environments.reverse();
    reversed.priced_models.reverse();
    for env in &mut reversed.environments {
        env.routes.reverse();
    }
    assert_eq!(a, render(&reversed, &RenderOptions::default()));
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
    assert!(!shown.contains("mock-key"), "{shown}");
}
