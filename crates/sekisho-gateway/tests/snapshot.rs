mod common;

use std::time::Duration;

use sekisho_core::formats::{AzureEndpoint, HostApi, HostApiPath, HostApis, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_core::pricing::{CacheDefaults, ModelPrices};
use sekisho_gateway::{
    GatewaySnapshot, HookTargets, ModelEntry, PricedModel, ProviderEntry, RenderOptions,
    build_snapshot, render, validate,
};
use sekisho_store::{
    CredentialKind, EnvironmentSpec, NewModel, NewProvider, ProviderCredential, ProviderOptions,
    ProviderUpdate, Store,
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

fn entry(env: &str, model_name: &str, provider_name: &str) -> ModelEntry {
    ModelEntry {
        env: EnvName::parse(env).unwrap(),
        model: model(model_name),
        provider: provider(provider_name),
    }
}

async fn put_env(store: &Store, name: &str, models: &[&str]) {
    store
        .upsert_environment(
            EnvironmentSpec {
                name: EnvName::parse(name).unwrap(),
                description: String::new(),
                notice: String::new(),
                access_groups: Vec::new(),
                models: models.iter().map(|m| model(m)).collect(),
                owner: None,
            },
            sekisho_testkit::people::person(store, "admin@example.com", &[]).await,
        )
        .await
        .unwrap();
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
        // No environment uses it, so the gateway never gets its key.
        (
            "unused",
            ProviderKind::OpenAiCompatible,
            Some("http://unused:8000/v1"),
            "unused-secret",
        ),
    ] {
        store
            .create_provider(
                &sb,
                NewProvider {
                    // `mock` and `unused` don't resolve.
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
    put_env(&store, "ml-team", &["claude-sonnet-5-5", "mock-model"]).await;
    put_env(
        &store,
        "data-team",
        &["mock-model", "claude-sonnet-5-5", "claude-haiku-5-5"],
    )
    .await;
    put_env(&store, "empty", &[]).await;

    let snapshot = build_snapshot(&store, &sb, &hooks()).await.unwrap();
    let want = GatewaySnapshot {
        // Each provider an environment uses, once, sorted by name.
        providers: vec![
            ProviderEntry {
                name: provider("anthropic"),
                kind: ProviderKind::Anthropic,
                base_url: None,
                options: ProviderOptions::None,
                credential: ProviderCredential::Key("sk-ant-secret".into()),
                apis: HostApis::chat_only(),
                catalog_key: None,
            },
            ProviderEntry {
                name: provider("mock"),
                kind: ProviderKind::OpenAiCompatible,
                base_url: Some("http://mock:8000/v1".into()),
                options: ProviderOptions::None,
                credential: ProviderCredential::Key("mock-secret".into()),
                apis: HostApis::chat_only(),
                catalog_key: None,
            },
        ],
        // By environment name, then the environment's own order.
        models: vec![
            entry("data-team", "mock-model", "mock"),
            entry("data-team", "claude-sonnet-5-5", "anthropic"),
            entry("data-team", "claude-haiku-5-5", "anthropic"),
            entry("ml-team", "claude-sonnet-5-5", "anthropic"),
            entry("ml-team", "mock-model", "mock"),
        ],
        // Every priced model, sorted by provider then model; unpriced ones use agentgateway's catalog.
        priced_models: vec![
            PricedModel {
                provider: provider("anthropic"),
                kind: ProviderKind::Anthropic,
                catalog_key: "anthropic".into(),
                cache: CacheDefaults::Anthropic,
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
                catalog_key: "mock".into(),
                cache: CacheDefaults::InputPrice,
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
                catalog_key: "mock".into(),
                cache: CacheDefaults::InputPrice,
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
    let yaml = render(&snapshot, &RenderOptions::default());
    assert_eq!(validate(&yaml), Ok(()));
    assert!(!yaml.contains("unused-secret"));
}

/// A Bedrock provider carries the stored credential and region, and serves
/// its models beside another provider's in one environment.
#[tokio::test]
async fn a_bedrock_provider_carries_its_credential_and_region() {
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
                base_url: None,
                options: ProviderOptions::Bedrock {
                    region: "us-east-1".into(),
                },
                credential: aws.clone(),
                allow_private_network: false,
                apis: None,
                catalog_key: None,
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
    put_env(
        &store,
        "cloud",
        &[
            "openai.gpt-oss-120b-1:0",
            "claude-sonnet-5-5",
            "openai.gpt-oss-20b-1:0",
        ],
    )
    .await;

    let snapshot = build_snapshot(&store, &sb, &hooks()).await.unwrap();
    assert_eq!(
        snapshot.providers,
        [
            ProviderEntry {
                name: provider("anthropic"),
                kind: ProviderKind::Anthropic,
                base_url: None,
                options: ProviderOptions::None,
                credential: ProviderCredential::Key("sk-ant".into()),
                apis: HostApis::chat_only(),
                catalog_key: None,
            },
            ProviderEntry {
                name: provider("bedrock"),
                kind: ProviderKind::Bedrock,
                base_url: None,
                options: ProviderOptions::Bedrock {
                    region: "us-east-1".into()
                },
                credential: aws,
                apis: HostApis::chat_only(),
                catalog_key: None,
            },
        ]
    );
    assert_eq!(
        snapshot.models,
        [
            entry("cloud", "openai.gpt-oss-120b-1:0", "bedrock"),
            entry("cloud", "claude-sonnet-5-5", "anthropic"),
            entry("cloud", "openai.gpt-oss-20b-1:0", "bedrock"),
        ]
    );
    let yaml = render(&snapshot, &RenderOptions::default());
    assert_eq!(validate(&yaml), Ok(()));
    assert_eq!(yaml.matches("bedrock-secret").count(), 1, "{yaml}");
    assert!(yaml.contains("bedrock-session"));
}

/// A host's APIs and catalog key reach its provider entry, and the inline
/// catalog files its priced models under that key; a host without one keeps
/// its name.
#[tokio::test]
async fn host_apis_and_catalog_keys_reach_the_snapshot() {
    let (store, sb) = common::store().await;
    let apis = HostApis::new(vec![
        HostApiPath {
            api: HostApi::ChatCompletions,
            path: None,
        },
        HostApiPath {
            api: HostApi::Messages,
            path: Some("/anthropic/v1/messages".into()),
        },
        HostApiPath {
            api: HostApi::Responses,
            path: None,
        },
    ])
    .unwrap();
    for (name, catalog_key, apis) in [
        ("deepseek-host", Some("deepseek"), apis.clone()),
        ("scaleway", None, HostApis::chat_only()),
    ] {
        store
            .create_provider(
                &sb,
                NewProvider {
                    allow_private_network: true,
                    apis: Some(apis),
                    catalog_key: catalog_key.map(Into::into),
                    ..NewProvider::keyed(
                        provider(name),
                        ProviderKind::OpenAiCompatible,
                        Some("http://mock:8000/v1".into()),
                        "k",
                    )
                },
            )
            .await
            .unwrap();
    }
    for (name, provider_name, prices) in [
        ("deepseek-v4-pro", "deepseek-host", None),
        ("deepseek-priced", "deepseek-host", Some(1)),
        ("qwen3-235b", "scaleway", Some(2)),
    ] {
        store
            .create_model(NewModel {
                name: model(name),
                provider: provider(provider_name),
                input_price: prices.map(MicroUsd),
                output_price: prices.map(MicroUsd),
                cache_read_price: None,
                cache_write_price: None,
            })
            .await
            .unwrap();
    }
    put_env(
        &store,
        "team",
        &["deepseek-v4-pro", "deepseek-priced", "qwen3-235b"],
    )
    .await;

    let snapshot = build_snapshot(&store, &sb, &hooks()).await.unwrap();
    let hosts: Vec<_> = snapshot
        .providers
        .iter()
        .map(|p| (p.name.as_str(), p.apis.clone(), p.catalog_key.as_deref()))
        .collect();
    assert_eq!(
        hosts,
        [
            ("deepseek-host", apis, Some("deepseek")),
            ("scaleway", HostApis::chat_only(), None),
        ]
    );
    let keys: Vec<(&str, &str)> = snapshot
        .priced_models
        .iter()
        .map(|m| (m.model.as_str(), m.catalog_key.as_str()))
        .collect();
    assert_eq!(
        keys,
        [("deepseek-priced", "deepseek"), ("qwen3-235b", "scaleway")]
    );
    let yaml = render(&snapshot, &RenderOptions::default());
    assert_eq!(validate(&yaml), Ok(()));
    assert!(yaml.contains("providerOverride: deepseek\n"), "{yaml}");
    assert!(yaml.contains("path: /anthropic/v1/messages\n"), "{yaml}");
}

/// A credential is rendered with the base URL and settings it was saved
/// with. Here an update lands while `build_snapshot` waits between its list
/// of providers and its reads of each provider used (held by a lock on
/// `environments`, which it reads in between; not `models`, which
/// `update_provider` reads for the provider's own models): an
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
    put_env(&store, "cloud", &["gpt-deployment"]).await;

    let mut blocker = store.pool().begin().await.unwrap();
    sqlx::query("lock table environments in access exclusive mode")
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
             where wait_event_type = 'Lock' and query ilike '%from environments%'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        if waiting > 0 {
            break; // providers and models are listed; the environments read waits
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "build_snapshot never reached the environments"
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
    let azure = &snapshot.providers[0];
    assert_eq!(
        (azure.credential.kind(), azure.base_url.as_deref()),
        (CredentialKind::CloudIdentity, None),
        "{azure:?}"
    );
}
