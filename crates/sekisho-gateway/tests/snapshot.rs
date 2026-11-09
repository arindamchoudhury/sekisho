mod common;

use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_gateway::{
    EnvRoutes, GatewaySnapshot, HookTargets, ModelRoute, PricedModel, build_snapshot,
};
use sekisho_store::{EnvironmentSpec, NewModel, NewProvider};

fn hooks() -> HookTargets {
    HookTargets {
        authz_http: "sekisho:9080".into(),
        grpc: "sekisho:9081".into(),
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
                    name: provider(name),
                    kind,
                    base_url: base_url.map(String::from),
                    api_key: key.into(),
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
