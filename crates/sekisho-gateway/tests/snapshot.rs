mod common;

use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_gateway::{
    EnvRoutes, GatewaySnapshot, HookTargets, ModelRoute, PricedModel, RenderOptions,
    build_snapshot, render, validate,
};
use sekisho_store::{EnvironmentSpec, NewModel, NewProvider, ProviderCredential, ProviderOptions};

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
                        options: ProviderOptions::None,
                        credential: ProviderCredential::Key("sk-ant-secret".into()),
                        models: vec![model("claude-sonnet-5-5"), model("claude-haiku-5-5")], // environment order
                    },
                    ModelRoute {
                        format: ApiFormat::OpenAi,
                        provider: provider("mock"),
                        kind: ProviderKind::OpenAiCompatible,
                        base_url: Some("http://mock:8000/v1".into()),
                        options: ProviderOptions::None,
                        credential: ProviderCredential::Key("mock-secret".into()),
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

/// A Bedrock route carries the stored credential, read once per provider,
/// and the format the provider was added with, not its kind's first one.
#[tokio::test]
async fn bedrock_routes_carry_their_credential_and_format() {
    let (store, sb) = common::store().await;
    let aws = ProviderCredential::AwsAccessKeys {
        access_key_id: "AKIAEXAMPLE0000001".into(),
        secret_access_key: "bedrock-secret".into(),
        session_token: Some("bedrock-session".into()),
    };
    store
        .create_provider(
            &sb,
            NewProvider {
                name: provider("bedrock"),
                kind: ProviderKind::Bedrock,
                format: ApiFormat::OpenAi,
                base_url: None,
                options: ProviderOptions::Bedrock {
                    region: "us-east-1".into(),
                },
                credential: aws.clone(),
                allow_private_network: false,
            },
        )
        .await
        .unwrap();
    store
        .create_provider(
            &sb,
            NewProvider::keyed(
                provider("anthropic"),
                ProviderKind::Anthropic,
                None,
                "sk-ant",
            ),
        )
        .await
        .unwrap();
    for (name, provider_name) in [
        ("openai.gpt-oss-120b-1:0", "bedrock"),
        ("openai.gpt-oss-20b-1:0", "bedrock"),
        ("claude-sonnet-5-5", "anthropic"),
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
            models: vec![
                model("openai.gpt-oss-120b-1:0"),
                model("claude-sonnet-5-5"),
                model("openai.gpt-oss-20b-1:0"),
            ],
        })
        .await
        .unwrap();

    let snapshot = build_snapshot(&store, &sb, &hooks()).await.unwrap();
    assert_eq!(
        snapshot.environments[0].routes,
        [
            ModelRoute {
                format: ApiFormat::Anthropic,
                provider: provider("anthropic"),
                kind: ProviderKind::Anthropic,
                base_url: None,
                options: ProviderOptions::None,
                credential: ProviderCredential::Key("sk-ant".into()),
                models: vec![model("claude-sonnet-5-5")],
            },
            ModelRoute {
                format: ApiFormat::OpenAi,
                provider: provider("bedrock"),
                kind: ProviderKind::Bedrock,
                base_url: None,
                options: ProviderOptions::Bedrock {
                    region: "us-east-1".into()
                },
                credential: aws,
                models: vec![
                    model("openai.gpt-oss-120b-1:0"),
                    model("openai.gpt-oss-20b-1:0")
                ],
            },
        ]
    );
    let yaml = render(&snapshot, &RenderOptions::default());
    assert_eq!(validate(&yaml), Ok(()));
    assert!(yaml.contains("bedrock-secret") && yaml.contains("bedrock-session"));
}
