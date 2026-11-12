mod common;

use std::time::Duration;

use sekisho_core::formats::{ApiFormat, AzureEndpoint, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_core::pricing::ModelPrices;
use sekisho_gateway::{
    EnvRoutes, GatewaySnapshot, HookTargets, ModelRoute, PricedModel, RenderOptions,
    build_snapshot, render, validate,
};
use sekisho_store::{
    CredentialKind, EnvironmentSpec, NewModel, NewProvider, ProviderCredential, ProviderOptions,
    ProviderUpdate,
};
use tokio::time::Instant;

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
                cache_read_price: None,
                cache_write_price: None,
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
            .upsert_environment(
                EnvironmentSpec {
                    name: EnvName::parse(env).unwrap(),
                    description: String::new(),
                    notice: String::new(),
                    access_groups: Vec::new(),
                    models: models.into_iter().map(model).collect(),
                    owner: None,
                },
                sekisho_testkit::people::person(&store, "admin@example.com", &[]).await,
            )
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
                format: ApiFormat::Anthropic,
                model: model("claude-sonnet-5-5"),
                prices: ModelPrices {
                    input: MicroUsd(3_000_000),
                    output: MicroUsd(15_000_000),
                    cache_read: None,
                    cache_write: None,
                },
            },
            PricedModel {
                provider: provider("mock"),
                kind: ProviderKind::OpenAiCompatible,
                format: ApiFormat::OpenAi,
                model: model("mock-model"),
                prices: ModelPrices {
                    input: MicroUsd(3_000_000),
                    output: MicroUsd(15_000_000),
                    cache_read: None,
                    cache_write: None,
                },
            },
            PricedModel {
                provider: provider("mock"),
                kind: ProviderKind::OpenAiCompatible,
                format: ApiFormat::OpenAi,
                model: model("unused-model"),
                prices: ModelPrices {
                    input: MicroUsd(1),
                    output: MicroUsd(2),
                    cache_read: None,
                    cache_write: None,
                },
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
                cache_read_price: None,
                cache_write_price: None,
            })
            .await
            .unwrap();
    }
    store
        .upsert_environment(
            EnvironmentSpec {
                name: EnvName::parse("cloud").unwrap(),
                description: String::new(),
                notice: String::new(),
                access_groups: Vec::new(),
                models: vec![
                    model("openai.gpt-oss-120b-1:0"),
                    model("claude-sonnet-5-5"),
                    model("openai.gpt-oss-20b-1:0"),
                ],
                owner: None,
            },
            sekisho_testkit::people::person(&store, "admin@example.com", &[]).await,
        )
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

/// A credential is rendered with the base URL and settings it was saved
/// with. Here an update lands while `build_snapshot` waits between its list
/// of providers and the rest of its reads (held by a lock on `models`): an
/// Azure provider with a key and a base URL off Azure switches to the
/// gateway's cloud identity and drops the base URL. Pairing the new identity
/// with the old URL would send the gateway's Entra token to that host.
#[tokio::test]
async fn a_credential_renders_with_the_settings_saved_with_it() {
    let (store, sb) = common::store().await;
    store
        .create_provider(
            &sb,
            NewProvider {
                format: ApiFormat::OpenAi,
                base_url: Some("https://203.0.113.10".into()),
                options: ProviderOptions::Azure {
                    endpoint: AzureEndpoint::parse("https://my-resource.openai.azure.com").unwrap(),
                },
                ..NewProvider::keyed(provider("azure"), ProviderKind::Azure, None, "azure-key")
            },
        )
        .await
        .unwrap();
    store
        .create_model(NewModel {
            name: model("gpt-deployment"),
            provider: provider("azure"),
            input_price: Some(MicroUsd(3_000_000)),
            output_price: Some(MicroUsd(15_000_000)),
            cache_read_price: None,
            cache_write_price: None,
        })
        .await
        .unwrap();
    store
        .upsert_environment(
            EnvironmentSpec {
                name: EnvName::parse("cloud").unwrap(),
                description: String::new(),
                notice: String::new(),
                access_groups: Vec::new(),
                models: vec![model("gpt-deployment")],
                owner: None,
            },
            sekisho_testkit::people::person(&store, "admin@example.com", &[]).await,
        )
        .await
        .unwrap();

    let mut blocker = store.pool().begin().await.unwrap();
    sqlx::query("lock table models in access exclusive mode")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let build = {
        let (store, sb) = (store.clone(), sb.clone());
        tokio::spawn(async move { build_snapshot(&store, &sb, &hooks()).await })
    };
    let started = Instant::now();
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity \
             where wait_event_type = 'Lock' and query ilike '%from models%'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        if waiting > 0 {
            break; // the providers are listed; the models read waits
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "build_snapshot never reached the models"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    store
        .update_provider(
            &sb,
            &provider("azure"),
            ProviderUpdate {
                credential: Some(ProviderCredential::CloudIdentity),
                base_url: Some(None),
                ..ProviderUpdate::default()
            },
        )
        .await
        .unwrap();
    blocker.rollback().await.unwrap();

    let snapshot = build.await.unwrap().unwrap();
    let route = &snapshot.environments[0].routes[0];
    assert_eq!(
        (route.credential.kind(), route.base_url.as_deref()),
        (CredentialKind::CloudIdentity, None),
        "{route:?}"
    );
}
