mod common;

use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_gateway::{
    EnvRoutes, GatewaySnapshot, HookTargets, ModelRoute, PricedModel, RenderOptions,
    build_snapshot, render, validate,
};
use sekisho_store::{EnvironmentSpec, NewModel, NewProvider, ProviderOptions};

fn hooks() -> HookTargets {
    HookTargets {
        authz_http: "sekisho:9080".into(),
        grpc: "sekisho:9081".into(),
        charge_secret: "test-charge-secret".into(),
    }
}

fn model(name: &str) -> ModelName {
    ModelName::parse(name).unwrap()
}

fn provider(name: &str) -> ProviderName {
    ProviderName::parse(name).unwrap()
}

#[tokio::test]
async fn snapshot_reflects_the_store() {
    let (store, sb) = common::store().await;
    for (name, kind, base_url, key) in [
        ("anthropic", ProviderKind::Anthropic, None, "sk-ant-secret"),
        (
            "mock",
            ProviderKind::OpenAiCompatible,
            Some("http://mock:8000/v1"),
            "mock-secret",
        ),
    ] {
        store
            .create_provider(
                &sb,
                NewProvider {
                    // `mock` doesn't resolve.
                    allow_private_network: base_url.is_some(),
                    ..NewProvider::keyed(provider(name), kind, base_url.map(String::from), key)
                },
            )
            .await
            .unwrap();
    }
    for (name, provider_name, prices) in [
        (
            "claude-sonnet-5-5",
            "anthropic",
            Some((3_000_000, 15_000_000)),
        ),
        ("claude-haiku-5-5", "anthropic", None),
        ("mock-model", "mock", Some((3_000_000, 15_000_000))),
        ("unused-model", "mock", Some((1, 2))),
    ] {
        store
            .create_model(NewModel {
                name: model(name),
                provider: provider(provider_name),
                input_price: prices.map(|p| MicroUsd(p.0)),
                output_price: prices.map(|p| MicroUsd(p.1)),
            })
            .await
            .unwrap();
    }
    for (env, models) in [
        (
            "data-team",
            vec!["mock-model", "claude-sonnet-5-5", "claude-haiku-5-5"],
        ),
        ("empty", vec![]),
    ] {
        store
            .upsert_environment(EnvironmentSpec {
                name: EnvName::parse(env).unwrap(),
                description: String::new(),
                notice: String::new(),
                access_groups: Vec::new(),
                models: models.into_iter().map(model).collect(),
            })
            .await
            .unwrap();
    }

    let snapshot = build_snapshot(&store, &sb, &hooks()).await.unwrap();
    let want = GatewaySnapshot {
        environments: vec![
            EnvRoutes {
                env: EnvName::parse("data-team").unwrap(),
                routes: vec![
                    ModelRoute {
                        format: ApiFormat::Anthropic,
                        provider: provider("anthropic"),
                        kind: ProviderKind::Anthropic,
                        base_url: None,
                        api_key: "sk-ant-secret".into(),
                        models: vec![model("claude-sonnet-5-5"), model("claude-haiku-5-5")], // environment order
                    },
                    ModelRoute {
                        format: ApiFormat::OpenAi,
                        provider: provider("mock"),
                        kind: ProviderKind::OpenAiCompatible,
                        base_url: Some("http://mock:8000/v1".into()),
                        api_key: "mock-secret".into(),
                        models: vec![model("mock-model")],
                    },
                ],
            },
            EnvRoutes {
                env: EnvName::parse("empty").unwrap(),
                routes: vec![],
            },
        ],
        // Every priced model, sorted by provider then model; unpriced ones use agentgateway's catalog.
        priced_models: vec![
            PricedModel {
                provider: provider("anthropic"),
                kind: ProviderKind::Anthropic,
                model: model("claude-sonnet-5-5"),
                input: MicroUsd(3_000_000),
                output: MicroUsd(15_000_000),
            },
            PricedModel {
                provider: provider("mock"),
                kind: ProviderKind::OpenAiCompatible,
                model: model("mock-model"),
                input: MicroUsd(3_000_000),
                output: MicroUsd(15_000_000),
            },
            PricedModel {
                provider: provider("mock"),
                kind: ProviderKind::OpenAiCompatible,
                model: model("unused-model"),
                input: MicroUsd(1),
                output: MicroUsd(2),
            },
        ],
        hooks: hooks(),
    };
    assert_eq!(snapshot, want);
}

/// Until Bedrock and Azure backends are rendered (plan 1b, Task 3), their
/// models get no route, so they can't stop every other environment's config
/// from publishing.
#[tokio::test]
async fn bedrock_and_azure_wait_for_their_backends() {
    let (store, sb) = common::store().await;
    store
        .create_provider(
            &sb,
            NewProvider {
                options: ProviderOptions::Bedrock {
                    region: "us-east-1".into(),
                },
                ..NewProvider::keyed(
                    provider("bedrock"),
                    ProviderKind::Bedrock,
                    None,
                    "bedrock-key",
                )
            },
        )
        .await
        .unwrap();
    store
        .create_provider(
            &sb,
            NewProvider::keyed(provider("openai"), ProviderKind::OpenAi, None, "sk-openai"),
        )
        .await
        .unwrap();
    for (name, provider_name) in [
        ("us.anthropic.claude-mock-v1:0", "bedrock"),
        ("gpt-5", "openai"),
    ] {
        store
            .create_model(NewModel {
                name: model(name),
                provider: provider(provider_name),
                input_price: Some(MicroUsd(3_000_000)),
                output_price: Some(MicroUsd(15_000_000)),
            })
            .await
            .unwrap();
    }
    store
        .upsert_environment(EnvironmentSpec {
            name: EnvName::parse("cloud").unwrap(),
            description: String::new(),
            notice: String::new(),
            access_groups: Vec::new(),
            models: vec![model("us.anthropic.claude-mock-v1:0"), model("gpt-5")],
        })
        .await
        .unwrap();

    let snapshot = build_snapshot(&store, &sb, &hooks()).await.unwrap();
    let routes: Vec<_> = snapshot.environments[0]
        .routes
        .iter()
        .map(|r| (r.format, r.provider.as_str()))
        .collect();
    assert_eq!(routes, [(ApiFormat::OpenAi, "openai")]);
    let yaml = render(&snapshot, &RenderOptions::default());
    assert_eq!(validate(&yaml), Ok(()));
    assert!(!yaml.contains("bedrock-key"));
}
