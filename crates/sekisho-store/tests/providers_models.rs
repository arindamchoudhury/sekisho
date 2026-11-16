mod common;

use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Duration;

use sekisho_core::formats::{
    AzureEndpoint, AzureResourceType, HostApi, HostApiPath, HostApis, ProviderKind,
};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{ModelName, ProviderName};
use sekisho_core::pricing::{CacheDefaults, ModelPrices};
use sekisho_store::{
    ChangeKind, CredentialKind, FieldError, ModelPricing, NewModel, NewProvider,
    ProviderCredential, ProviderOptions, ProviderUpdate, Store, StoreError, StoreOptions,
};
use tokio_stream::StreamExt;
use uuid::Uuid;

fn pname(name: &str) -> ProviderName {
    ProviderName::parse(name).unwrap()
}

fn new_provider(name: &str, kind: ProviderKind, api_key: &str) -> NewProvider {
    NewProvider::keyed(pname(name), kind, None, api_key)
}

/// An OpenAI-compatible provider on the test mock. `mock` doesn't resolve,
/// so it needs `allow_private_network`.
fn mock_provider(name: &str, api_key: &str) -> NewProvider {
    NewProvider {
        allow_private_network: true,
        ..NewProvider::keyed(
            pname(name),
            ProviderKind::OpenAiCompatible,
            Some("http://mock:8000/v1".into()),
            api_key,
        )
    }
}

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const CLIENT: &str = "66666666-7777-8888-9999-000000000000";
const AZURE_OPENAI: &str = "https://my-resource.openai.azure.com";
const AZURE_FOUNDRY: &str = "https://my-foundry.services.ai.azure.com/api/projects/team";

fn key(k: &str) -> ProviderCredential {
    ProviderCredential::Key(k.into())
}

fn aws_keys(id: &str, secret: &str, token: Option<&str>) -> ProviderCredential {
    ProviderCredential::AwsAccessKeys {
        access_key_id: id.into(),
        secret_access_key: secret.into(),
        session_token: token.map(Into::into),
    }
}

fn entra(tenant: &str, client: &str, secret: &str) -> ProviderCredential {
    ProviderCredential::AzureClientSecret {
        tenant_id: tenant.into(),
        client_id: client.into(),
        client_secret: secret.into(),
    }
}

fn bedrock(name: &str, region: &str, credential: ProviderCredential) -> NewProvider {
    NewProvider {
        name: pname(name),
        kind: ProviderKind::Bedrock,
        base_url: None,
        options: ProviderOptions::Bedrock {
            region: region.into(),
        },
        credential,
        allow_private_network: false,
        apis: None,
        catalog_key: None,
    }
}

fn azure(name: &str, endpoint: &str, credential: ProviderCredential) -> NewProvider {
    NewProvider {
        name: pname(name),
        kind: ProviderKind::Azure,
        base_url: None,
        options: ProviderOptions::Azure {
            endpoint: AzureEndpoint::parse(endpoint).unwrap(),
        },
        credential,
        allow_private_network: false,
        apis: None,
        catalog_key: None,
    }
}

/// The field names of a refused write, in order.
async fn refused(
    store: &Store,
    sb: &sekisho_core::secret_box::SecretBox,
    new: NewProvider,
) -> Vec<String> {
    fields(store.create_provider(sb, new).await.unwrap_err())
}

/// The message of a refused write's only error.
fn message(err: StoreError) -> String {
    match err {
        StoreError::Validation(f) if f.len() == 1 => f[0].message.clone(),
        other => panic!("expected one field error, got {other:?}"),
    }
}

fn new_model(name: &str, provider: &str, prices: Option<(i64, i64)>) -> NewModel {
    NewModel {
        name: ModelName::parse(name).unwrap(),
        provider: ProviderName::parse(provider).unwrap(),
        input_price: prices.map(|p| MicroUsd(p.0)),
        output_price: prices.map(|p| MicroUsd(p.1)),
        cache_read_price: None,
        cache_write_price: None,
    }
}

fn fields(err: StoreError) -> Vec<String> {
    match err {
        StoreError::Validation(f) => f
            .into_iter()
            .map(|FieldError { field, .. }| field)
            .collect(),
        other => panic!("expected Validation, got {other:?}"),
    }
}

async fn next_change(
    rx: &mut (impl tokio_stream::Stream<Item = ChangeKind> + Unpin),
) -> ChangeKind {
    tokio::time::timeout(Duration::from_secs(2), rx.next())
        .await
        .expect("a notification within 2 s")
        .expect("the stream is open")
}

#[tokio::test]
async fn provider_key_is_encrypted_and_readable() {
    let (store, sb) = common::store().await;
    let p = store
        .create_provider(
            &sb,
            new_provider("anthropic", ProviderKind::Anthropic, "sk-ant-test"),
        )
        .await
        .unwrap();
    let raw: Vec<u8> = sqlx::query_scalar("select credential_ciphertext from providers")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(!raw.windows(11).any(|w| w == b"sk-ant-test"));
    assert_eq!(
        store.provider_credential(&sb, p.id).await.unwrap(),
        key("sk-ant-test")
    );
}

#[tokio::test]
async fn provider_round_trip_and_lookups() {
    let (store, sb) = common::store().await;
    let mock = store
        .create_provider(&sb, mock_provider("mock", "k"))
        .await
        .unwrap();
    let anthropic = store
        .create_provider(&sb, new_provider("anthropic", ProviderKind::Anthropic, "k"))
        .await
        .unwrap();
    assert_eq!(mock.name.as_str(), "mock");
    assert_eq!(mock.kind, ProviderKind::OpenAiCompatible);
    assert_eq!(mock.apis, Some(HostApis::chat_only()));
    assert_eq!(mock.catalog_key, None);
    assert_eq!(mock.base_url.as_deref(), Some("http://mock:8000/v1"));
    assert_eq!(mock.options, ProviderOptions::None);
    assert_eq!(mock.credential, CredentialKind::Key);
    assert!(mock.allow_private_network);
    assert_eq!(
        (anthropic.apis.as_ref(), anthropic.catalog_key.as_deref()),
        (None, None)
    );
    assert!(!anthropic.allow_private_network);
    assert_eq!(store.get_provider(&mock.name).await.unwrap(), mock);
    assert_eq!(store.list_providers().await.unwrap(), vec![anthropic, mock]); // sorted by name

    let unknown = ProviderName::parse("nope").unwrap();
    assert!(matches!(
        store.get_provider(&unknown).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.delete_provider(&unknown).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.provider_credential(&sb, Uuid::nil()).await,
        Err(StoreError::NotFound)
    ));
    let unknown_model = ModelName::parse("nope").unwrap();
    assert!(matches!(
        store.delete_model(&unknown_model).await,
        Err(StoreError::NotFound)
    ));
}

fn update(api_key: Option<&str>, base_url: Option<Option<&str>>) -> ProviderUpdate {
    ProviderUpdate {
        credential: api_key.map(key),
        base_url: base_url.map(|url| url.map(Into::into)),
        ..ProviderUpdate::default()
    }
}

/// A provider's key rotates (and its URL changes) in place, without touching
/// the models and environments that use it.
#[tokio::test]
async fn provider_key_and_url_can_be_changed() {
    let (store, sb) = common::store().await;
    let p = store
        .create_provider(
            &sb,
            new_provider("anthropic", ProviderKind::Anthropic, "old-key"),
        )
        .await
        .unwrap();
    store
        .create_model(new_model("claude-sonnet-5-5", "anthropic", None))
        .await
        .unwrap();
    let mut rx = store.listen().await.unwrap();

    let rotated = store
        .update_provider(&sb, &p.name, update(Some("new-key"), None))
        .await
        .unwrap();
    assert_eq!(rotated, p); // nothing listed changed
    assert_eq!(
        store.provider_credential(&sb, p.id).await.unwrap(),
        key("new-key")
    );
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);

    // Moving the key to another server needs the key again.
    let proxied = store
        .update_provider(
            &sb,
            &p.name,
            update(Some("proxy-key"), Some(Some("https://203.0.113.10/v1"))),
        )
        .await
        .unwrap();
    assert_eq!(proxied.base_url.as_deref(), Some("https://203.0.113.10/v1"));
    assert_eq!(
        store.provider_credential(&sb, p.id).await.unwrap(),
        key("proxy-key")
    );
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);
    let cleared = store
        .update_provider(&sb, &p.name, update(Some("direct-key"), Some(None)))
        .await
        .unwrap();
    assert_eq!(cleared.base_url, None);
    assert_eq!(store.list_models().await.unwrap().len(), 1);
}

#[tokio::test]
async fn provider_updates_are_validated() {
    let (store, sb) = common::store().await;
    let mock = store
        .create_provider(&sb, mock_provider("mock", "k"))
        .await
        .unwrap();
    let mut rx = store.listen().await.unwrap();
    // The URL changes carry a key, so only the URL rule refuses them.
    for (refused, field) in [
        (update(Some(""), None), "api_key"),
        (update(Some("k2"), Some(None)), "base_url"), // a compatible provider needs one
        (update(Some("k2"), Some(Some("ftp://mock"))), "base_url"),
    ] {
        let err = store
            .update_provider(&sb, &mock.name, refused)
            .await
            .unwrap_err();
        assert_eq!(fields(err), [field]);
    }
    let unknown = ProviderName::parse("nope").unwrap();
    assert!(matches!(
        store
            .update_provider(&sb, &unknown, update(Some("k2"), None))
            .await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(store.get_provider(&mock.name).await.unwrap(), mock);
    assert_eq!(
        store.provider_credential(&sb, mock.id).await.unwrap(),
        key("k")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.next())
            .await
            .is_err(),
        "refused updates notify nothing"
    );
}

#[tokio::test]
async fn duplicate_provider_is_conflict() {
    let (store, sb) = common::store().await;
    store
        .create_provider(&sb, new_provider("openai", ProviderKind::OpenAi, "k"))
        .await
        .unwrap();
    let err = store
        .create_provider(&sb, new_provider("openai", ProviderKind::Gemini, "k2"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(ref m) if m.contains("openai")),
        "{err:?}"
    );
}

#[tokio::test]
async fn inputs_are_validated() {
    let (store, sb) = common::store().await;
    let no_url = new_provider("compat", ProviderKind::OpenAiCompatible, "");
    assert_eq!(
        fields(store.create_provider(&sb, no_url).await.unwrap_err()),
        ["base_url", "api_key"]
    );
    let mut bad_url = new_provider("compat", ProviderKind::OpenAiCompatible, "k");
    bad_url.base_url = Some("ftp://mock".into());
    assert_eq!(
        fields(store.create_provider(&sb, bad_url).await.unwrap_err()),
        ["base_url"]
    );
    assert!(store.list_providers().await.unwrap().is_empty());

    store
        .create_provider(&sb, new_provider("anthropic", ProviderKind::Anthropic, "k"))
        .await
        .unwrap();
    let mut one_price = new_model("m", "anthropic", None);
    one_price.input_price = Some(MicroUsd(3_000_000));
    assert_eq!(
        fields(store.create_model(one_price).await.unwrap_err()),
        ["output_price"]
    );
    let negative = new_model("m", "anthropic", Some((-1, 15)));
    assert_eq!(
        fields(store.create_model(negative).await.unwrap_err()),
        ["input_price"]
    );
    assert!(store.list_models().await.unwrap().is_empty());
}

/// agentgateway's catalog has no prices for an OpenAI-compatible provider's
/// models, so without Sekisho prices their spend would never be charged.
#[tokio::test]
async fn openai_compatible_models_need_prices() {
    let (store, sb) = common::store().await;
    store
        .create_provider(&sb, mock_provider("mock", "k"))
        .await
        .unwrap();
    assert_eq!(
        fields(
            store
                .create_model(new_model("local", "mock", None))
                .await
                .unwrap_err()
        ),
        ["input_price", "output_price"]
    );
    assert!(store.list_models().await.unwrap().is_empty());
    let priced = store
        .create_model(new_model("local", "mock", Some((0, 0))))
        .await
        .unwrap();
    assert_eq!(priced.input_price, Some(MicroUsd(0))); // free is a price
}

#[tokio::test]
async fn model_round_trip() {
    let (store, sb) = common::store().await;
    store
        .create_provider(&sb, new_provider("anthropic", ProviderKind::Anthropic, "k"))
        .await
        .unwrap();
    let priced = store
        .create_model(new_model(
            "claude-sonnet-5-5",
            "anthropic",
            Some((3_000_000, 15_000_000)),
        ))
        .await
        .unwrap();
    let catalog = store
        .create_model(new_model("claude-haiku-5-5", "anthropic", None))
        .await
        .unwrap();
    assert_eq!(priced.provider.as_str(), "anthropic");
    assert_eq!(priced.provider_kind, ProviderKind::Anthropic);
    assert_eq!(
        (priced.input_price, priced.output_price),
        (Some(MicroUsd(3_000_000)), Some(MicroUsd(15_000_000)))
    );
    assert_eq!((catalog.input_price, catalog.output_price), (None, None));
    assert_eq!(store.list_models().await.unwrap(), vec![catalog, priced]); // sorted by name

    let err = store
        .create_model(new_model("claude-sonnet-5-5", "anthropic", None))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(ref m) if m.contains("claude-sonnet-5-5")),
        "{err:?}"
    );
}

#[tokio::test]
async fn model_requires_existing_provider() {
    let (store, _sb) = common::store().await;
    let err = store
        .create_model(new_model("gpt-5", "openai", None))
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["provider"]);
}

#[tokio::test]
async fn delete_provider_in_use_is_refused() {
    let (store, sb) = common::store().await;
    let p = store
        .create_provider(&sb, new_provider("anthropic", ProviderKind::Anthropic, "k"))
        .await
        .unwrap();
    let m = store
        .create_model(new_model("claude-sonnet-5-5", "anthropic", None))
        .await
        .unwrap();
    let err = store.delete_provider(&p.name).await.unwrap_err();
    assert!(
        matches!(err, StoreError::InUse(ref m) if m == "models: claude-sonnet-5-5"),
        "{err:?}"
    );

    store.delete_model(&m.name).await.unwrap();
    store.delete_provider(&p.name).await.unwrap();
    assert!(matches!(
        store.get_provider(&p.name).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn changes_are_notified() {
    let (store, sb) = common::store().await;
    let mut rx = store.listen().await.unwrap();
    store
        .create_provider(&sb, new_provider("openai", ProviderKind::OpenAi, "k"))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), rx.next())
            .await
            .unwrap(),
        Some(ChangeKind::Providers)
    );
}

#[tokio::test]
async fn every_write_notifies_its_kind_and_failures_notify_nothing() {
    let (store, sb) = common::store().await;
    let mut rx = store.listen().await.unwrap();
    let p = store
        .create_provider(&sb, new_provider("openai", ProviderKind::OpenAi, "k"))
        .await
        .unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);
    let m = store
        .create_model(new_model("gpt-5", "openai", None))
        .await
        .unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Models);

    // Refused writes roll back, and their notifications with them.
    assert!(
        store
            .create_provider(&sb, new_provider("openai", ProviderKind::OpenAi, "k"))
            .await
            .is_err()
    );
    assert!(store.delete_provider(&p.name).await.is_err());
    assert!(
        store
            .create_model(new_model("gpt-5", "openai", None))
            .await
            .is_err()
    );

    store.delete_model(&m.name).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Models);
    store.delete_provider(&p.name).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.next())
            .await
            .is_err(),
        "no further notifications"
    );
}

#[tokio::test]
async fn listener_reports_everything_after_reconnect() {
    let (store, _sb) = common::store().await;
    let mut rx = store.listen().await.unwrap();
    // Notifications sent while the listener is cut off are lost, so after it
    // reconnects every kind is reported once for consumers to reload.
    let killed: Vec<bool> = sqlx::query_scalar(
        "select pg_terminate_backend(pid) from pg_stat_activity \
         where query like 'LISTEN %' and pid <> pg_backend_pid()",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(killed, [true]);
    let mut seen = HashSet::new();
    while seen.len() < ChangeKind::ALL.len() {
        let kind = tokio::time::timeout(Duration::from_secs(10), rx.next())
            .await
            .expect("every kind within 10 s of the reconnect")
            .unwrap();
        seen.insert(kind);
    }
}

#[tokio::test]
async fn listener_stops_when_the_pool_closes() {
    let (store, _sb) = common::store().await;
    let mut rx = store.listen().await.unwrap();
    // close() waits for every connection, including the listener's.
    tokio::time::timeout(Duration::from_secs(5), store.pool().close())
        .await
        .expect("the listener gives its connection back");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), rx.next())
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn migrate_twice_is_harmless() {
    let (store, _sb) = common::store().await;
    store.migrate().await.unwrap();
}

#[tokio::test]
async fn bedrock_and_azure_round_trip() {
    let (store, sb) = common::store().await;
    let credentials = [
        (
            "bedrock-claude",
            aws_keys("AKIAEXAMPLE0000001", "aws-secret", Some("aws-session")),
        ),
        ("bedrock-oss", key("bedrock-api-key")),
        ("azure-gpt", ProviderCredential::CloudIdentity),
        ("azure-claude", entra(TENANT, CLIENT, "sp-secret")),
    ];
    let mut created = Vec::new();
    for new in [
        bedrock("bedrock-claude", "us-east-1", credentials[0].1.clone()),
        bedrock("bedrock-oss", "eu-central-1", credentials[1].1.clone()),
        azure("azure-gpt", AZURE_OPENAI, credentials[2].1.clone()),
        azure("azure-claude", AZURE_FOUNDRY, credentials[3].1.clone()),
    ] {
        created.push(store.create_provider(&sb, new).await.unwrap());
    }

    let listed = store.list_providers().await.unwrap();
    let summary: Vec<_> = listed
        .iter()
        .map(|p| (p.name.as_str(), p.kind, p.credential))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "azure-claude",
                ProviderKind::Azure,
                CredentialKind::AzureClientSecret
            ),
            (
                "azure-gpt",
                ProviderKind::Azure,
                CredentialKind::CloudIdentity
            ),
            (
                "bedrock-claude",
                ProviderKind::Bedrock,
                CredentialKind::AwsAccessKeys
            ),
            ("bedrock-oss", ProviderKind::Bedrock, CredentialKind::Key),
        ]
    );
    let options: Vec<_> = listed.iter().map(|p| p.options.clone()).collect();
    assert_eq!(
        options,
        [
            ProviderOptions::Azure {
                endpoint: AzureEndpoint {
                    resource_name: "my-foundry".into(),
                    resource_type: AzureResourceType::Foundry,
                    project_name: Some("team".into()),
                }
            },
            ProviderOptions::Azure {
                endpoint: AzureEndpoint {
                    resource_name: "my-resource".into(),
                    resource_type: AzureResourceType::OpenAi,
                    project_name: None,
                }
            },
            ProviderOptions::Bedrock {
                region: "us-east-1".into()
            },
            ProviderOptions::Bedrock {
                region: "eu-central-1".into()
            },
        ]
    );
    assert!(
        listed
            .iter()
            .all(|p| p.base_url.is_none() && !p.allow_private_network)
    );
    created.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    assert_eq!(listed, created);

    for (name, credential) in credentials {
        let p = store.get_provider(&pname(name)).await.unwrap();
        assert_eq!(
            store.provider_credential(&sb, p.id).await.unwrap(),
            credential,
            "{name}"
        );
    }
    let unsealed: Vec<String> = sqlx::query_scalar(
        "select name from providers where credential_ciphertext is null order by name",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(unsealed, ["azure-gpt"]); // the cloud identity stores no secret
    let sealed: Vec<Vec<u8>> = sqlx::query_scalar(
        "select credential_ciphertext from providers where credential_ciphertext is not null",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    for secret in [
        &b"aws-secret"[..],
        b"aws-session",
        b"bedrock-api-key",
        b"sp-secret",
    ] {
        assert!(
            !sealed
                .iter()
                .any(|s| s.windows(secret.len()).any(|w| w == secret)),
            "{}",
            String::from_utf8_lossy(secret)
        );
    }
}

#[tokio::test]
async fn credentials_must_fit_the_kind() {
    let (store, sb) = common::store().await;
    let aws = || aws_keys("AKIAEXAMPLE0000001", "aws-secret", None);
    for (new, field) in [
        (
            NewProvider {
                credential: aws(),
                ..new_provider("p", ProviderKind::Anthropic, "k")
            },
            "credential",
        ),
        (azure("p", AZURE_OPENAI, aws()), "credential"),
        (
            bedrock("p", "us-east-1", entra(TENANT, CLIENT, "s")),
            "credential",
        ),
        (
            NewProvider {
                credential: ProviderCredential::CloudIdentity,
                ..new_provider("p", ProviderKind::OpenAi, "k")
            },
            "credential",
        ),
    ] {
        let what = format!("{new:?}");
        assert_eq!(refused(&store, &sb, new).await, [field], "{what}");
    }
    assert!(store.list_providers().await.unwrap().is_empty());
}

#[tokio::test]
async fn cloud_settings_are_validated() {
    let (store, sb) = common::store().await;
    let aws = || aws_keys("AKIAEXAMPLE0000001", "aws-secret", None);
    let in_region = |region: &str| bedrock("bedrock", region, aws());

    let no_region = NewProvider {
        options: ProviderOptions::None,
        ..in_region("us-east-1")
    };
    assert_eq!(refused(&store, &sb, no_region).await, ["region"]);
    // agentgateway builds bedrock-runtime.<region>.amazonaws.com from it.
    for bad in [
        "US-EAST-1",
        "us-east-1.evil.com",
        "../x",
        "us east 1",
        "",
        "us-east",
        "us-1",
        "us-east-123",
        "u-east-1",
        "use-east-1",
        "us--east-1",
        "us-east-1 ",
        "us-east-1/x",
    ] {
        assert_eq!(
            refused(&store, &sb, in_region(bad)).await,
            ["region"],
            "{bad:?}"
        );
    }
    for (i, good) in [
        "us-gov-west-1",
        "ap-southeast-3",
        "us-east-1",
        "eu-central-1",
    ]
    .into_iter()
    .enumerate()
    {
        let new = NewProvider {
            name: pname(&format!("bedrock-{i}")),
            ..in_region(good)
        };
        store.create_provider(&sb, new).await.unwrap();
    }

    let at = |endpoint: AzureEndpoint| NewProvider {
        options: ProviderOptions::Azure { endpoint },
        ..azure("azure", AZURE_OPENAI, key("k"))
    };
    let no_endpoint = NewProvider {
        options: ProviderOptions::None,
        ..azure("azure", AZURE_OPENAI, key("k"))
    };
    assert_eq!(refused(&store, &sb, no_endpoint).await, ["endpoint"]);
    // Endpoints built by hand must still be ones AzureEndpoint::parse accepts.
    for bad in [
        AzureEndpoint {
            resource_name: "a.b".into(),
            resource_type: AzureResourceType::OpenAi,
            project_name: None,
        },
        AzureEndpoint {
            resource_name: "evil.com/x".into(),
            resource_type: AzureResourceType::Foundry,
            project_name: None,
        },
        AzureEndpoint {
            resource_name: "my-resource".into(),
            resource_type: AzureResourceType::OpenAi,
            project_name: Some("team".into()),
        },
        AzureEndpoint {
            resource_name: "my-foundry".into(),
            resource_type: AzureResourceType::Foundry,
            project_name: Some("a/b".into()),
        },
    ] {
        let what = format!("{bad:?}");
        assert_eq!(refused(&store, &sb, at(bad)).await, ["endpoint"], "{what}");
    }

    // Settings belong to their kind.
    let anthropic = || new_provider("anthropic", ProviderKind::Anthropic, "k");
    let with_region = NewProvider {
        options: ProviderOptions::Bedrock {
            region: "us-east-1".into(),
        },
        ..anthropic()
    };
    assert_eq!(refused(&store, &sb, with_region).await, ["region"]);
    let with_endpoint = NewProvider {
        options: ProviderOptions::Azure {
            endpoint: AzureEndpoint::parse(AZURE_OPENAI).unwrap(),
        },
        ..anthropic()
    };
    assert_eq!(refused(&store, &sb, with_endpoint).await, ["endpoint"]);
    let bedrock_at_azure = NewProvider {
        options: ProviderOptions::Azure {
            endpoint: AzureEndpoint::parse(AZURE_OPENAI).unwrap(),
        },
        ..in_region("us-east-1")
    };
    assert_eq!(refused(&store, &sb, bedrock_at_azure).await, ["region"]);

    // Credential fields.
    let with = |credential| bedrock("bedrock", "us-east-1", credential);
    for (credential, field) in [
        (aws_keys("akia lower", "s", None), "aws_access_key_id"),
        (aws_keys("AKIA", "s", None), "aws_access_key_id"),
        (
            aws_keys("AKIAEXAMPLE000000/", "s", None),
            "aws_access_key_id",
        ),
        (
            aws_keys("AKIAEXAMPLE0000001", "", None),
            "aws_secret_access_key",
        ),
        (
            aws_keys("AKIAEXAMPLE0000001", "s", Some("")),
            "aws_session_token",
        ),
        (key(""), "api_key"),
    ] {
        assert_eq!(refused(&store, &sb, with(credential)).await, [field]);
    }
    let sp = |tenant: &str, client: &str, secret: &str| {
        azure("azure", AZURE_OPENAI, entra(tenant, client, secret))
    };
    for (new, field) in [
        (sp("not a tenant!", CLIENT, "s"), "azure_tenant_id"),
        (sp("contoso", CLIENT, "s"), "azure_tenant_id"),
        (
            sp("contoso.onmicrosoft.com/x", CLIENT, "s"),
            "azure_tenant_id",
        ),
        (sp("", CLIENT, "s"), "azure_tenant_id"),
        (sp(TENANT, "nope", "s"), "azure_client_id"),
        (
            sp(TENANT, "66666666-7777-8888-9999-00000000000g", "s"),
            "azure_client_id",
        ),
        (sp(TENANT, CLIENT, ""), "azure_client_secret"),
    ] {
        assert_eq!(refused(&store, &sb, new).await, [field]);
    }
    for (i, tenant) in [TENANT, "contoso.onmicrosoft.com", "Contoso-Ltd.example.com"]
        .into_iter()
        .enumerate()
    {
        let new = NewProvider {
            name: pname(&format!("azure-{i}")),
            ..sp(tenant, CLIENT, "s")
        };
        store.create_provider(&sb, new).await.unwrap();
    }
}

#[tokio::test]
async fn switching_credentials_drops_the_old_secret() {
    let (store, sb) = common::store().await;
    let p = store
        .create_provider(
            &sb,
            bedrock(
                "bedrock",
                "us-east-1",
                aws_keys("AKIAEXAMPLE0000001", "aws-secret", Some("aws-session")),
            ),
        )
        .await
        .unwrap();
    let mut rx = store.listen().await.unwrap();
    let stored = |id: Uuid| {
        let pool = store.pool().clone();
        async move {
            sqlx::query_as::<_, (String, Option<Vec<u8>>)>(
                "select credential_kind, credential_ciphertext from providers where id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    let cloud = store
        .update_provider(
            &sb,
            &p.name,
            ProviderUpdate {
                credential: Some(ProviderCredential::CloudIdentity),
                ..ProviderUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cloud.credential, CredentialKind::CloudIdentity);
    assert_eq!(stored(p.id).await, ("cloud_identity".into(), None));
    assert_eq!(
        store.provider_credential(&sb, p.id).await.unwrap(),
        ProviderCredential::CloudIdentity
    );
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);

    let keyed = store
        .update_provider(&sb, &p.name, update(Some("k2"), None))
        .await
        .unwrap();
    assert_eq!(keyed.credential, CredentialKind::Key);
    assert_eq!(stored(p.id).await.0, "key");
    assert_eq!(
        store.provider_credential(&sb, p.id).await.unwrap(),
        key("k2")
    );
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);

    // A refused update changes nothing.
    let az = store
        .create_provider(&sb, azure("azure", AZURE_OPENAI, key("az-key")))
        .await
        .unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);
    let err = store
        .update_provider(
            &sb,
            &az.name,
            ProviderUpdate {
                credential: Some(aws_keys("AKIAEXAMPLE0000001", "aws-secret", None)),
                ..ProviderUpdate::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["credential"]);
    assert_eq!(store.get_provider(&az.name).await.unwrap(), az);
    assert_eq!(
        store.provider_credential(&sb, az.id).await.unwrap(),
        key("az-key")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.next())
            .await
            .is_err(),
        "refused updates notify nothing"
    );
}

/// Without the e2e feature, the gateway's cloud identity, AWS keys and Entra
/// service principals only go to AWS or Azure hosts. Every provider here
/// allows private networks, so only the cloud-host rule decides.
#[cfg(not(feature = "e2e"))]
#[tokio::test]
async fn cloud_credentials_only_go_to_cloud_hosts() {
    let (store, sb) = common::store().await;
    let mut n = 0;
    let mut at = |mut new: NewProvider, url: &str| {
        n += 1;
        new.name = pname(&format!("p{n}"));
        new.base_url = Some(url.into());
        new.allow_private_network = true;
        new
    };

    for credential in [
        ProviderCredential::CloudIdentity,
        aws_keys("AKIAEXAMPLE0000001", "aws-secret", None),
    ] {
        let b = || bedrock("b", "us-east-1", credential.clone());
        for url in [
            "https://proxy.example.com",
            "http://bedrock-runtime.us-east-1.amazonaws.com",
            "https://amazonaws.com",
            "https://amazonaws.com.evil.com",
            "https://evil.com/x.amazonaws.com",
            "https://evil.com?x.amazonaws.com",
            "https://evil.com#.amazonaws.com",
            "https://x.amazonaws.com@evil.com",
            "https://bedrock.openai.azure.com",
        ] {
            let what = format!("{credential:?} at {url}");
            assert_eq!(
                refused(&store, &sb, at(b(), url)).await,
                ["base_url"],
                "{what}"
            );
        }
        for url in [
            "https://vpce-0a1b-x.bedrock-runtime.us-east-1.vpce.amazonaws.com",
            "https://bedrock-runtime.us-east-1.api.aws",
            "https://Bedrock-Runtime.us-east-1.AmazonAWS.com:443/",
        ] {
            store
                .create_provider(&sb, at(b(), url))
                .await
                .unwrap_or_else(|e| panic!("{url}: {e}"));
        }
    }

    for credential in [
        ProviderCredential::CloudIdentity,
        entra(TENANT, CLIENT, "sp-secret"),
    ] {
        let a = || azure("a", AZURE_OPENAI, credential.clone());
        for url in [
            "https://proxy.example.com",
            "http://my-resource.openai.azure.com",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            "https://openai.azure.com.evil.com",
        ] {
            let what = format!("{credential:?} at {url}");
            assert_eq!(
                refused(&store, &sb, at(a(), url)).await,
                ["base_url"],
                "{what}"
            );
        }
        for url in [
            "https://my-resource.openai.azure.com",
            "https://my-foundry.services.ai.azure.com",
            "https://my-resource.cognitiveservices.azure.com/",
        ] {
            store
                .create_provider(&sb, at(a(), url))
                .await
                .unwrap_or_else(|e| panic!("{url}: {e}"));
        }
    }

    // An API key goes wherever a keyed provider's base URL points.
    let keyed = NewProvider {
        base_url: Some("http://203.0.113.10:8000".into()),
        ..bedrock("bedrock-key", "us-east-1", key("k"))
    };
    store.create_provider(&sb, keyed).await.unwrap();

    // Updates follow the same rule.
    let cloud = store
        .create_provider(
            &sb,
            bedrock(
                "bedrock-cloud",
                "us-east-1",
                ProviderCredential::CloudIdentity,
            ),
        )
        .await
        .unwrap();
    let err = store
        .update_provider(
            &sb,
            &cloud.name,
            ProviderUpdate {
                base_url: Some(Some("https://proxy.example.com".into())),
                allow_private_network: Some(true),
                ..ProviderUpdate::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["base_url"]);
}

#[cfg(feature = "e2e")]
#[tokio::test]
async fn an_e2e_build_lets_cloud_credentials_reach_a_mock() {
    let (store, sb) = common::store().await;
    for (i, credential) in [
        ProviderCredential::CloudIdentity,
        aws_keys("AKIAE2EACCESSKEY01", "e2e-aws-secret", None),
    ]
    .into_iter()
    .enumerate()
    {
        let new = NewProvider {
            base_url: Some("http://mock-model:8000".into()),
            allow_private_network: true,
            ..bedrock(&format!("bedrock-{i}"), "us-east-1", credential)
        };
        store.create_provider(&sb, new).await.unwrap();
    }
}

#[tokio::test]
async fn moving_a_stored_secret_needs_it_again() {
    let (store, sb) = common::store().await;
    const AGAIN: &str = "changing where requests go needs the credential again";
    let public = "https://203.0.113.10/v1";

    let p = store
        .create_provider(
            &sb,
            new_provider("anthropic", ProviderKind::Anthropic, "k1"),
        )
        .await
        .unwrap();
    let err = store
        .update_provider(&sb, &p.name, update(None, Some(Some(public))))
        .await
        .unwrap_err();
    assert_eq!(message(err), AGAIN);
    let moved = store
        .update_provider(&sb, &p.name, update(Some("k2"), Some(Some(public))))
        .await
        .unwrap();
    assert_eq!(moved.base_url.as_deref(), Some(public));
    // Rotation alone, or setting the URL it already has, moves nothing.
    store
        .update_provider(&sb, &p.name, update(Some("k3"), None))
        .await
        .unwrap();
    store
        .update_provider(&sb, &p.name, update(None, Some(Some(public))))
        .await
        .unwrap();
    assert_eq!(
        store.provider_credential(&sb, p.id).await.unwrap(),
        key("k3")
    );

    let region = |r: &str| ProviderUpdate {
        options: Some(ProviderOptions::Bedrock { region: r.into() }),
        ..ProviderUpdate::default()
    };
    let aws = || aws_keys("AKIAEXAMPLE0000001", "aws-secret", None);
    let b = store
        .create_provider(&sb, bedrock("bedrock-aws", "us-east-1", aws()))
        .await
        .unwrap();
    let err = store
        .update_provider(&sb, &b.name, region("eu-west-1"))
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["credential"]);
    let moved = store
        .update_provider(
            &sb,
            &b.name,
            ProviderUpdate {
                credential: Some(aws()),
                ..region("eu-west-1")
            },
        )
        .await
        .unwrap();
    assert_eq!(
        moved.options,
        ProviderOptions::Bedrock {
            region: "eu-west-1".into()
        }
    );

    let az = store
        .create_provider(
            &sb,
            azure("azure-sp", AZURE_OPENAI, entra(TENANT, CLIENT, "sp-secret")),
        )
        .await
        .unwrap();
    let elsewhere = ProviderUpdate {
        options: Some(ProviderOptions::Azure {
            endpoint: AzureEndpoint::parse("https://other-resource.openai.azure.com").unwrap(),
        }),
        ..ProviderUpdate::default()
    };
    let err = store
        .update_provider(&sb, &az.name, elsewhere)
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["credential"]);
    // The same endpoint, written differently, is not a move.
    let same = ProviderUpdate {
        options: Some(ProviderOptions::Azure {
            endpoint: AzureEndpoint::parse("https://my-resource.openai.azure.com/openai/v1/")
                .unwrap(),
        }),
        ..ProviderUpdate::default()
    };
    store.update_provider(&sb, &az.name, same).await.unwrap();

    // The gateway's cloud identity is no stored secret.
    let cloud = store
        .create_provider(
            &sb,
            bedrock(
                "bedrock-cloud",
                "us-east-1",
                ProviderCredential::CloudIdentity,
            ),
        )
        .await
        .unwrap();
    let moved = store
        .update_provider(&sb, &cloud.name, region("eu-west-1"))
        .await
        .unwrap();
    assert_eq!(
        moved.options,
        ProviderOptions::Bedrock {
            region: "eu-west-1".into()
        }
    );
}

/// A base URL's host is resolved when the provider is saved; no case here
/// depends on DNS answering.
#[tokio::test]
async fn provider_addresses_are_checked() {
    let (store, sb) = common::store().await;
    let at = |name: &str, url: &str, allow: bool| NewProvider {
        allow_private_network: allow,
        ..NewProvider::keyed(
            pname(name),
            ProviderKind::OpenAiCompatible,
            Some(url.into()),
            "k",
        )
    };

    for (i, url) in [
        "http://127.0.0.1:8000",
        "http://10.1.2.3",
        "http://[::1]:8000",
        "http://no-such-host.invalid",
        "http://172.16.0.1",
        "http://192.168.1.1/v1",
        "http://100.64.0.1",
        "http://[fc00::1]",
        "http://[::ffff:10.1.2.3]",
        "http://10.1.2.3./v1",
    ]
    .into_iter()
    .enumerate()
    {
        let err = store
            .create_provider(&sb, at(&format!("private-{i}"), url, false))
            .await
            .unwrap_err();
        assert_eq!(fields(err), ["base_url"], "{url}");
        store
            .create_provider(&sb, at(&format!("private-{i}"), url, true))
            .await
            .unwrap_or_else(|e| panic!("{url}: {e}"));
    }
    let err = store
        .create_provider(&sb, at("ten", "http://10.1.2.3", false))
        .await
        .unwrap_err();
    assert_eq!(
        message(err),
        "10.1.2.3 is a private network address, which needs allow_private_network"
    );
    store
        .create_provider(&sb, at("public", "http://203.0.113.10", false))
        .await
        .unwrap();

    for (i, url) in [
        "http://169.254.169.254/",
        "http://[fd00:ec2::254]",
        "http://0.0.0.0",
        "http://[::ffff:169.254.169.254]",
        "http://100.100.100.200",
        "http://168.63.129.16",
        "http://169.254.1.1",
        "http://[fe80::1]",
        "http://[::]",
        "http://203.0.113.10@169.254.169.254/",
    ]
    .into_iter()
    .enumerate()
    {
        let err = store
            .create_provider(&sb, at(&format!("never-{i}"), url, true))
            .await
            .unwrap_err();
        assert_eq!(fields(err), ["base_url"], "{url}");
    }
    // A second colon would reach agentgateway as a host override it can't
    // read, which stops every later config from loading.
    let err = store
        .create_provider(&sb, at("two-colons", "http://ollama:11434:11434/v1", true))
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["base_url"]);
    let err = store
        .create_provider(&sb, at("metadata", "http://169.254.169.254/", true))
        .await
        .unwrap_err();
    assert_eq!(
        message(err),
        "169.254.169.254 is a cloud metadata address, which can't be used"
    );

    // Turning the setting off checks the stored URL again.
    let p = store
        .create_provider(&sb, at("lan", "http://10.1.2.3", true))
        .await
        .unwrap();
    let err = store
        .update_provider(
            &sb,
            &p.name,
            ProviderUpdate {
                allow_private_network: Some(false),
                ..ProviderUpdate::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["base_url"]);
    assert!(
        store
            .get_provider(&p.name)
            .await
            .unwrap()
            .allow_private_network
    );
}

/// One Bedrock or Azure provider serves every tool, so neither has a route
/// format: Claude and GPT behind one Bedrock provider, and one Foundry
/// project for both.
#[tokio::test]
async fn one_bedrock_or_azure_provider_needs_no_format() {
    let (store, sb) = common::store().await;
    let bedrock = store
        .create_provider(
            &sb,
            bedrock(
                "bedrock",
                "us-east-1",
                aws_keys("AKIAEXAMPLE0000001", "s", None),
            ),
        )
        .await
        .unwrap();
    let foundry = store
        .create_provider(
            &sb,
            azure("foundry", AZURE_FOUNDRY, entra(TENANT, CLIENT, "s")),
        )
        .await
        .unwrap();
    // GPT on Foundry with a key, and on Azure OpenAI with the gateway's
    // identity: no route decides the credential any more.
    store
        .create_provider(&sb, azure("foundry-key", AZURE_FOUNDRY, key("k")))
        .await
        .unwrap();
    store
        .create_provider(
            &sb,
            azure(
                "azure-cloud",
                AZURE_OPENAI,
                ProviderCredential::CloudIdentity,
            ),
        )
        .await
        .unwrap();
    for (model, provider) in [
        ("us.anthropic.claude-sonnet-4-6-v1:0", "bedrock"),
        ("openai.gpt-oss-120b-1:0", "bedrock"),
        ("claude-sonnet-x", "foundry"),
        ("gpt-5", "foundry"),
        ("gpt-5-mini", "foundry-key"),
        ("gpt-5-nano", "azure-cloud"),
    ] {
        store
            .create_model(new_model(model, provider, None))
            .await
            .unwrap();
    }
    let listed = store.list_providers().await.unwrap();
    assert_eq!(listed.len(), 4);
    assert!(
        listed
            .iter()
            .all(|p| p.apis.is_none() && p.catalog_key.is_none()),
        "{listed:?}"
    );
    assert_eq!(store.get_provider(&bedrock.name).await.unwrap(), bedrock);
    assert_eq!(store.get_provider(&foundry.name).await.unwrap(), foundry);
}

/// An OpenAI-compatible host says which APIs it serves, each with its path
/// when it isn't the standard one, and the key agentgateway's catalog files
/// its prices under.
#[tokio::test]
async fn an_open_weight_host_declares_its_apis() {
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
    ])
    .unwrap();
    let created = store
        .create_provider(
            &sb,
            NewProvider {
                apis: Some(apis.clone()),
                catalog_key: Some("deepseek".into()),
                ..mock_provider("deepseek", "k")
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (created.apis.as_ref(), created.catalog_key.as_deref()),
        (Some(&apis), Some("deepseek"))
    );
    assert_eq!(store.get_provider(&created.name).await.unwrap(), created);
    // No list of APIs without Chat Completions gets this far: the type
    // refuses it (the API answers 422 on `apis`).
    assert!(HostApis::new(vec![]).is_err());
    assert!(
        HostApis::new(vec![HostApiPath {
            api: HostApi::Messages,
            path: None,
        }])
        .is_err()
    );
    for (new, field) in [
        (
            NewProvider {
                apis: None,
                ..mock_provider("p", "k")
            },
            "apis",
        ),
        (
            NewProvider {
                apis: Some(HostApis::chat_only()),
                ..bedrock("p", "us-east-1", key("k"))
            },
            "apis",
        ),
        (
            NewProvider {
                catalog_key: Some("Bad Key".into()),
                ..mock_provider("p", "k")
            },
            "catalog_key",
        ),
        (
            NewProvider {
                catalog_key: Some("anthropic".into()),
                ..new_provider("p", ProviderKind::Anthropic, "k")
            },
            "catalog_key",
        ),
    ] {
        let what = format!("{new:?}");
        let err = store.create_provider(&sb, new).await.unwrap_err();
        assert!(!format!("{err:?}").contains("Bad Key"), "{err:?}");
        assert_eq!(fields(err), [field], "{what}");
    }

    // Editing the APIs, or the catalog key, doesn't move the provider to
    // another host, so it needs no credential.
    let updated = store
        .update_provider(
            &sb,
            &created.name,
            ProviderUpdate {
                apis: Some(HostApis::chat_only()),
                catalog_key: Some(None),
                ..ProviderUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (updated.apis, updated.catalog_key),
        (Some(HostApis::chat_only()), None)
    );
    let bedrock = store
        .create_provider(&sb, bedrock("bedrock", "us-east-1", key("k")))
        .await
        .unwrap();
    let err = store
        .update_provider(
            &sb,
            &bedrock.name,
            ProviderUpdate {
                apis: Some(HostApis::chat_only()),
                ..ProviderUpdate::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["apis"]);
}

/// agentgateway v1.7.0 refuses a base URL with user info, a query or a
/// fragment when it loads the file, which would hold back every
/// environment's config.
#[tokio::test]
async fn base_urls_with_user_info_query_or_fragment_are_refused() {
    const NO_EXTRAS: &str = "use a URL without user name, password, query or fragment";
    let (store, sb) = common::store().await;
    let bad = [
        "https://u:p@h.example/v1",
        "https://u@h.example/v1",
        "https://h.example/v1?x=1",
        "https://h.example/v1#f",
    ];
    for url in bad {
        let err = store
            .create_provider(
                &sb,
                NewProvider {
                    base_url: Some(url.into()),
                    ..mock_provider("p", "k")
                },
            )
            .await
            .unwrap_err();
        let StoreError::Validation(errors) = err else {
            panic!("{err:?}")
        };
        let got: Vec<(&str, &str)> = errors
            .iter()
            .map(|e| (e.field.as_str(), e.message.as_str()))
            .collect();
        assert_eq!(got, [("base_url", NO_EXTRAS)], "{url}");
    }
    let p = store
        .create_provider(&sb, mock_provider("p", "k"))
        .await
        .unwrap();
    for url in bad {
        let err = store
            .update_provider(&sb, &p.name, update(Some("k2"), Some(Some(url))))
            .await
            .unwrap_err();
        assert_eq!(message(err), NO_EXTRAS, "{url}");
    }
    // An `@` in the path is not user info.
    store
        .update_provider(
            &sb,
            &p.name,
            update(Some("k2"), Some(Some("http://mock:8000/v1/@team"))),
        )
        .await
        .unwrap();
}

/// A host's model that agentgateway's catalog prices under the host's
/// catalog key needs no Sekisho prices; any other still does.
#[tokio::test]
async fn a_preset_hosts_model_known_to_the_catalog_needs_no_price() {
    let (store, sb) = common::store().await;
    store
        .create_provider(
            &sb,
            NewProvider {
                catalog_key: Some("togetherai".into()),
                ..mock_provider("together", "k")
            },
        )
        .await
        .unwrap();
    let model = store
        .create_model(new_model("zai-org/GLM-5.1", "together", None))
        .await
        .unwrap();
    assert_eq!(model.prices(), None);
    let err = store
        .create_model(new_model("unknown-model", "together", None))
        .await
        .unwrap_err();
    let StoreError::Validation(errors) = err else {
        panic!("{err:?}")
    };
    let named: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
    assert_eq!(named, ["input_price", "output_price"]);
    assert!(
        errors[0].message.contains("togetherai"),
        "{}",
        errors[0].message
    );
    store
        .create_model(new_model("unknown-model", "together", Some((1, 2))))
        .await
        .unwrap();
}

/// A catalog key change that would leave an unpriced model without a price
/// is refused, naming the model.
#[tokio::test]
async fn changing_the_catalog_key_keeps_every_model_priced() {
    let (store, sb) = common::store().await;
    let together = store
        .create_provider(
            &sb,
            NewProvider {
                catalog_key: Some("togetherai".into()),
                ..mock_provider("together", "k")
            },
        )
        .await
        .unwrap();
    store
        .create_model(new_model("zai-org/GLM-5.1", "together", None))
        .await
        .unwrap();
    let change = |key: Option<&str>| ProviderUpdate {
        catalog_key: Some(key.map(Into::into)),
        ..ProviderUpdate::default()
    };
    for key in [None, Some("mistral")] {
        let err = store
            .update_provider(&sb, &together.name, change(key))
            .await
            .unwrap_err();
        let StoreError::Validation(errors) = err else {
            panic!("{err:?}")
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].field, "catalog_key");
        assert!(
            errors[0].message.contains("zai-org/GLM-5.1"),
            "{}",
            errors[0].message
        );
        // It names what an admin can do: a model's prices are set only when
        // it is added.
        assert!(
            errors[0].message.contains("keep the catalog key")
                && errors[0].message.contains("add them again with prices"),
            "{}",
            errors[0].message
        );
    }
    store
        .update_provider(&sb, &together.name, change(Some("togetherai")))
        .await
        .unwrap();
    // Once the model has its own prices, the key can go.
    store
        .delete_model(&ModelName::parse("zai-org/GLM-5.1").unwrap())
        .await
        .unwrap();
    store
        .create_model(new_model("zai-org/GLM-5.1", "together", Some((1, 2))))
        .await
        .unwrap();
    let updated = store
        .update_provider(&sb, &together.name, change(None))
        .await
        .unwrap();
    assert_eq!(updated.catalog_key, None);
}

/// agentgateway sends an Azure deployment whose name starts with `claude`
/// to Foundry's Messages endpoint, which takes Claude API keys only in a
/// header agentgateway can't send. So Claude on Azure needs Foundry and an
/// Entra credential, checked when the model is added and when the provider
/// changes.
#[tokio::test]
async fn claude_on_azure_needs_foundry_and_entra() {
    let (store, sb) = common::store().await;
    let sp = || entra(TENANT, CLIENT, "s");
    for new in [
        azure("azure-openai", AZURE_OPENAI, sp()),
        azure("foundry-key", AZURE_FOUNDRY, key("k")),
        azure("foundry-sp", AZURE_FOUNDRY, sp()),
        azure("azure-gpt", AZURE_OPENAI, key("k")),
    ] {
        store.create_provider(&sb, new).await.unwrap();
    }
    let refusal = async |model: &str, provider: &str| {
        let err = store
            .create_model(new_model(model, provider, None))
            .await
            .unwrap_err();
        let StoreError::Validation(errors) = err else {
            panic!("{err:?}")
        };
        errors
    };
    let foundry = refusal("claude-sonnet", "azure-openai").await;
    assert_eq!(foundry.len(), 1, "{foundry:?}");
    assert_eq!(foundry[0].field, "name");
    assert!(foundry[0].message.contains("Foundry"), "{foundry:?}");
    let entra_needed = refusal("claude-sonnet", "foundry-key").await;
    assert_eq!(entra_needed.len(), 1, "{entra_needed:?}");
    assert_eq!(entra_needed[0].field, "name");
    assert!(
        entra_needed[0].message.contains("Entra"),
        "{entra_needed:?}"
    );
    for name in ["claude-sonnet", "Claude-Opus"] {
        store
            .create_model(new_model(name, "foundry-sp", None))
            .await
            .unwrap();
    }
    store
        .create_model(new_model("gpt-5", "azure-gpt", None))
        .await
        .unwrap();

    // The provider can't change under its Claude models.
    let name = pname("foundry-sp");
    for (update, field) in [
        (
            ProviderUpdate {
                credential: Some(key("k")),
                ..ProviderUpdate::default()
            },
            "credential",
        ),
        (
            ProviderUpdate {
                credential: Some(sp()),
                options: Some(ProviderOptions::Azure {
                    endpoint: AzureEndpoint::parse(AZURE_OPENAI).unwrap(),
                }),
                ..ProviderUpdate::default()
            },
            "endpoint",
        ),
    ] {
        let err = store.update_provider(&sb, &name, update).await.unwrap_err();
        let StoreError::Validation(errors) = err else {
            panic!("{err:?}")
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].field, field);
        for model in ["Claude-Opus", "claude-sonnet"] {
            assert!(errors[0].message.contains(model), "{errors:?}");
        }
    }
    // A GPT-only Azure provider still switches to a key.
    store
        .update_provider(
            &sb,
            &pname("azure-gpt"),
            ProviderUpdate {
                credential: Some(key("k2")),
                ..ProviderUpdate::default()
            },
        )
        .await
        .unwrap();
}

/// Plan 1b's rows lose their route format and keep everything else: an
/// OpenAI-compatible provider gets Chat Completions and no catalog key.
#[tokio::test]
async fn migration_0008_keeps_existing_providers() {
    let (store, sb) = common::store().await;
    sqlx::query("create database pre1d")
        .execute(store.pool())
        .await
        .unwrap();
    let url = format!(
        "postgres://postgres:sekisho@127.0.0.1:{}/pre1d",
        store.pool().connect_options().get_port()
    );
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let mut before = sqlx::migrate!("./migrations");
    before.migrations = Cow::Owned(
        before
            .migrations
            .iter()
            .filter(|m| m.version <= 7)
            .cloned()
            .collect(),
    );
    before.run(&pool).await.unwrap();
    let sealed = sb.seal(b"mock-key");
    for (name, kind, format, base_url, options, credential, ciphertext) in [
        (
            "bedrock",
            "bedrock",
            "anthropic",
            None,
            r#"{"region": "us-east-1"}"#,
            "cloud_identity",
            None,
        ),
        (
            "azure",
            "azure",
            "openai",
            None,
            r#"{"endpoint": "https://my-resource.openai.azure.com"}"#,
            "cloud_identity",
            None,
        ),
        (
            "mock",
            "openai_compatible",
            "openai",
            Some("http://mock:8000/v1"),
            "{}",
            "key",
            Some(sealed.clone()),
        ),
    ] {
        sqlx::query(
            "insert into providers (name, kind, api_format, base_url, options, \
             credential_kind, credential_ciphertext, allow_private_network) \
             values ($1, $2, $3, $4, $5::jsonb, $6, $7, true)",
        )
        .bind(name)
        .bind(kind)
        .bind(format)
        .bind(base_url)
        .bind(options)
        .bind(credential)
        .bind(ciphertext)
        .execute(&pool)
        .await
        .unwrap();
    }
    for (model, provider, prices) in [
        ("us.anthropic.claude-x", "bedrock", None),
        ("gpt-x", "azure", None),
        ("qwen", "mock", Some((1_i64, 2_i64))),
    ] {
        sqlx::query(
            "insert into models (name, provider_id, input_price_micro_usd, \
             output_price_micro_usd) select $1, id, $3, $4 from providers where name = $2",
        )
        .bind(model)
        .bind(provider)
        .bind(prices.map(|p| p.0))
        .bind(prices.map(|p| p.1))
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;

    let migrated = Store::connect(&url, StoreOptions::default()).await.unwrap();
    migrated.migrate().await.unwrap();
    let listed = migrated.list_providers().await.unwrap();
    let summary: Vec<_> = listed
        .iter()
        .map(|p| {
            (
                p.name.as_str(),
                p.kind,
                p.apis.clone(),
                p.catalog_key.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("azure", ProviderKind::Azure, None, None),
            ("bedrock", ProviderKind::Bedrock, None, None),
            (
                "mock",
                ProviderKind::OpenAiCompatible,
                Some(HostApis::chat_only()),
                None
            ),
        ]
    );
    assert_eq!(
        migrated
            .provider_credential(&sb, listed[2].id)
            .await
            .unwrap(),
        key("mock-key")
    );
    for model in ["us.anthropic.claude-x", "gpt-x", "qwen"] {
        let pricing = migrated
            .model_pricing(&ModelName::parse(model).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            pricing.prices.is_some(),
            model == "qwen",
            "{model}: {pricing:?}"
        );
    }
    let format_columns: i64 = sqlx::query_scalar(
        "select count(*) from information_schema.columns \
         where table_name = 'providers' and column_name = 'api_format'",
    )
    .fetch_one(migrated.pool())
    .await
    .unwrap();
    assert_eq!(format_columns, 0);
}

#[test]
fn debug_hides_cloud_secrets() {
    let hidden = [
        "s3cr3t-value",
        "t0ken-value",
        "sp-s3cr3t",
        "key-value",
        "AKIAEXAMPLE0000009",
        TENANT,
        CLIENT,
    ];
    for credential in [
        aws_keys("AKIAEXAMPLE0000009", "s3cr3t-value", Some("t0ken-value")),
        entra(TENANT, CLIENT, "sp-s3cr3t"),
        key("key-value"),
    ] {
        let shape = format!("{:?}", credential.kind());
        for text in [
            format!("{credential:?}"),
            format!("{:?}", bedrock("p", "us-east-1", credential.clone())),
            format!(
                "{:?}",
                ProviderUpdate {
                    credential: Some(credential.clone()),
                    ..ProviderUpdate::default()
                }
            ),
        ] {
            assert!(text.contains(&shape), "{text}");
            for secret in hidden {
                assert!(!text.contains(secret), "{text}");
            }
        }
    }
}

#[tokio::test]
async fn cache_prices_round_trip() {
    let (store, sb) = common::store().await;
    store
        .create_provider(&sb, new_provider("anthropic", ProviderKind::Anthropic, "k"))
        .await
        .unwrap();
    let priced = store
        .create_model(NewModel {
            cache_read_price: Some(MicroUsd(300_000)),
            cache_write_price: Some(MicroUsd(3_750_000)),
            ..new_model("claude-x", "anthropic", Some((3_000_000, 15_000_000)))
        })
        .await
        .unwrap();
    assert_eq!(
        (priced.cache_read_price, priced.cache_write_price),
        (Some(MicroUsd(300_000)), Some(MicroUsd(3_750_000)))
    );
    assert_eq!(
        priced.prices(),
        Some(ModelPrices {
            input: MicroUsd(3_000_000),
            output: MicroUsd(15_000_000),
            cache_read: Some(MicroUsd(300_000)),
            cache_write: Some(MicroUsd(3_750_000)),
        })
    );
    let input_and_output = store
        .create_model(new_model("claude-y", "anthropic", Some((1, 2))))
        .await
        .unwrap();
    assert_eq!(
        input_and_output.prices(),
        Some(ModelPrices {
            input: MicroUsd(1),
            output: MicroUsd(2),
            cache_read: None,
            cache_write: None,
        })
    );
    let catalog = store
        .create_model(new_model("claude-z", "anthropic", None))
        .await
        .unwrap();
    assert_eq!(catalog.prices(), None);
    assert_eq!(
        store.list_models().await.unwrap(),
        vec![priced, input_and_output, catalog]
    );
}

#[tokio::test]
async fn cache_prices_need_input_and_output() {
    let (store, sb) = common::store().await;
    store
        .create_provider(&sb, new_provider("anthropic", ProviderKind::Anthropic, "k"))
        .await
        .unwrap();
    for (model, want) in [
        (
            NewModel {
                cache_read_price: Some(MicroUsd(300_000)),
                ..new_model("m", "anthropic", None)
            },
            vec!["cache_read_price"],
        ),
        (
            NewModel {
                cache_read_price: Some(MicroUsd(1)),
                cache_write_price: Some(MicroUsd(1)),
                ..new_model("m", "anthropic", None)
            },
            vec!["cache_read_price", "cache_write_price"],
        ),
        (
            NewModel {
                cache_write_price: Some(MicroUsd(-1)),
                ..new_model("m", "anthropic", Some((1, 2)))
            },
            vec!["cache_write_price"],
        ),
    ] {
        let err = store.create_model(model).await.unwrap_err();
        assert_eq!(fields(err), want);
    }
    assert!(store.list_models().await.unwrap().is_empty());
}

#[tokio::test]
async fn model_pricing_reads_one_model() {
    let (store, sb) = common::store().await;
    store
        .create_provider(&sb, bedrock("bedrock-oss", "us-east-1", key("k")))
        .await
        .unwrap();
    store
        .create_model(NewModel {
            cache_read_price: Some(MicroUsd(3)),
            ..new_model("gpt-oss", "bedrock-oss", Some((1, 2)))
        })
        .await
        .unwrap();
    store
        .create_model(new_model("gpt-oss-catalog", "bedrock-oss", None))
        .await
        .unwrap();
    let name = |n: &str| ModelName::parse(n).unwrap();
    assert_eq!(
        store.model_pricing(&name("gpt-oss")).await.unwrap(),
        Some(ModelPricing {
            provider: pname("bedrock-oss"),
            kind: ProviderKind::Bedrock,
            catalog_key: Some("aws.bedrock".into()),
            cache: CacheDefaults::OpenAi,
            prices: Some(ModelPrices {
                input: MicroUsd(1),
                output: MicroUsd(2),
                cache_read: Some(MicroUsd(3)),
                cache_write: None,
            }),
        })
    );
    assert_eq!(
        store
            .model_pricing(&name("gpt-oss-catalog"))
            .await
            .unwrap()
            .unwrap()
            .prices,
        None
    );
    assert_eq!(store.model_pricing(&name("nope")).await.unwrap(), None);
}

/// Default cache prices follow the model: Claude on Bedrock keeps
/// Anthropic's ratios, another Bedrock model takes OpenAI's, and an
/// OpenAI-compatible host without a catalog key has none.
#[tokio::test]
async fn model_pricing_names_the_cache_defaults_and_catalog_key() {
    let (store, sb) = common::store().await;
    store
        .create_provider(&sb, bedrock("bedrock", "us-east-1", key("k")))
        .await
        .unwrap();
    store
        .create_provider(&sb, mock_provider("qwen-host", "k"))
        .await
        .unwrap();
    for (model, provider, prices) in [
        ("anthropic.claude-x", "bedrock", None),
        ("amazon.nova-pro", "bedrock", None),
        ("qwen3", "qwen-host", Some((1, 2))),
    ] {
        store
            .create_model(new_model(model, provider, prices))
            .await
            .unwrap();
    }
    let pricing = async |n: &str| {
        store
            .model_pricing(&ModelName::parse(n).unwrap())
            .await
            .unwrap()
            .unwrap()
    };

    let claude = pricing("anthropic.claude-x").await;
    assert_eq!(claude.cache, CacheDefaults::Anthropic);
    assert_eq!(claude.catalog_key.as_deref(), Some("aws.bedrock"));
    assert_eq!(
        pricing("amazon.nova-pro").await.cache,
        CacheDefaults::OpenAi
    );
    let qwen = pricing("qwen3").await;
    assert_eq!(qwen.cache, CacheDefaults::InputPrice);
    assert_eq!(qwen.catalog_key, None);
    assert_eq!(qwen.kind, ProviderKind::OpenAiCompatible);
}

/// agentgateway reads every `baseUrl` with the `url` crate when it loads the
/// file, and refuses the whole file if one doesn't parse, which would stop
/// every environment's config from updating. Sekisho reads it the same way,
/// so such a URL is refused when it is saved.
#[tokio::test]
async fn base_urls_agentgateway_cannot_read_are_refused() {
    let (store, sb) = common::store().await;
    for (i, url) in [
        "http://10.0.0.256:11434/v1",
        "http://foo|bar:8000/v1",
        "http://xn--a.com/v1",
        "http://ollama:11434:11434/v1",
        "http://ollama:99999/v1",
    ]
    .into_iter()
    .enumerate()
    {
        let new = NewProvider {
            base_url: Some(url.into()),
            ..mock_provider(&format!("p{i}"), "k")
        };
        assert_eq!(refused(&store, &sb, new).await, ["base_url"], "{url}");
    }
    for (i, url) in [
        "http://[::1]:11434/v1",
        "http://Ollama.LAN:11434/v1/",
        "http://10.0.0.5:11434",
    ]
    .into_iter()
    .enumerate()
    {
        let new = NewProvider {
            base_url: Some(url.into()),
            ..mock_provider(&format!("ok{i}"), "k")
        };
        store.create_provider(&sb, new).await.unwrap();
    }
}

/// agentgateway, like a browser, ends a host at a backslash and decodes a
/// percent-encoded host, so these all go to the cloud metadata address,
/// which is never allowed, private network or not.
#[tokio::test]
async fn a_metadata_address_cannot_hide_in_a_base_url() {
    let (store, sb) = common::store().await;
    for (i, url) in [
        r"http://169.254.169.254\x.com/v1",
        r"http://mock\@169.254.169.254/v1",
        "http://%31%36%39.254.169.254/v1",
        "http://0xa9.254.169.254/v1",
    ]
    .into_iter()
    .enumerate()
    {
        let new = NewProvider {
            base_url: Some(url.into()),
            ..mock_provider(&format!("p{i}"), "k")
        };
        assert_eq!(refused(&store, &sb, new).await, ["base_url"], "{url}");
    }
}
