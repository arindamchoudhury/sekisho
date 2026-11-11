mod common;

use std::collections::HashSet;
use std::time::Duration;

use sekisho_core::formats::{ApiFormat, AzureEndpoint, AzureResourceType, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{ModelName, ProviderName};
use sekisho_store::{
    ChangeKind, CredentialKind, FieldError, NewModel, NewProvider, ProviderCredential,
    ProviderOptions, ProviderUpdate, Store, StoreError,
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

fn bedrock(
    name: &str,
    format: ApiFormat,
    region: &str,
    credential: ProviderCredential,
) -> NewProvider {
    NewProvider {
        name: pname(name),
        kind: ProviderKind::Bedrock,
        format,
        base_url: None,
        options: ProviderOptions::Bedrock {
            region: region.into(),
        },
        credential,
        allow_private_network: false,
    }
}

fn azure(
    name: &str,
    format: ApiFormat,
    endpoint: &str,
    credential: ProviderCredential,
) -> NewProvider {
    NewProvider {
        name: pname(name),
        kind: ProviderKind::Azure,
        format,
        base_url: None,
        options: ProviderOptions::Azure {
            endpoint: AzureEndpoint::parse(endpoint).unwrap(),
        },
        credential,
        allow_private_network: false,
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
    assert_eq!(mock.format, ApiFormat::OpenAi);
    assert_eq!(mock.base_url.as_deref(), Some("http://mock:8000/v1"));
    assert_eq!(mock.options, ProviderOptions::None);
    assert_eq!(mock.credential, CredentialKind::Key);
    assert!(mock.allow_private_network);
    assert_eq!(anthropic.format, ApiFormat::Anthropic);
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
        bedrock(
            "bedrock-claude",
            ApiFormat::Anthropic,
            "us-east-1",
            credentials[0].1.clone(),
        ),
        bedrock(
            "bedrock-oss",
            ApiFormat::OpenAi,
            "eu-central-1",
            credentials[1].1.clone(),
        ),
        azure(
            "azure-gpt",
            ApiFormat::OpenAi,
            AZURE_OPENAI,
            credentials[2].1.clone(),
        ),
        azure(
            "azure-claude",
            ApiFormat::Anthropic,
            AZURE_FOUNDRY,
            credentials[3].1.clone(),
        ),
    ] {
        created.push(store.create_provider(&sb, new).await.unwrap());
    }

    let listed = store.list_providers().await.unwrap();
    let summary: Vec<_> = listed
        .iter()
        .map(|p| (p.name.as_str(), p.kind, p.format, p.credential))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "azure-claude",
                ProviderKind::Azure,
                ApiFormat::Anthropic,
                CredentialKind::AzureClientSecret
            ),
            (
                "azure-gpt",
                ProviderKind::Azure,
                ApiFormat::OpenAi,
                CredentialKind::CloudIdentity
            ),
            (
                "bedrock-claude",
                ProviderKind::Bedrock,
                ApiFormat::Anthropic,
                CredentialKind::AwsAccessKeys
            ),
            (
                "bedrock-oss",
                ProviderKind::Bedrock,
                ApiFormat::OpenAi,
                CredentialKind::Key
            ),
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
        (
            azure("p", ApiFormat::OpenAi, AZURE_OPENAI, aws()),
            "credential",
        ),
        (
            bedrock(
                "p",
                ApiFormat::Anthropic,
                "us-east-1",
                entra(TENANT, CLIENT, "s"),
            ),
            "credential",
        ),
        (
            NewProvider {
                credential: ProviderCredential::CloudIdentity,
                ..new_provider("p", ProviderKind::OpenAi, "k")
            },
            "credential",
        ),
        // Foundry takes Claude API keys only in x-api-key, which
        // agentgateway's Azure provider can't send.
        (
            azure("p", ApiFormat::Anthropic, AZURE_FOUNDRY, key("k")),
            "credential",
        ),
        // Claude on Azure is served by Foundry.
        (
            azure(
                "p",
                ApiFormat::Anthropic,
                "https://rs.openai.azure.com",
                ProviderCredential::CloudIdentity,
            ),
            "endpoint",
        ),
        (
            bedrock("p", ApiFormat::Gemini, "us-east-1", key("k")),
            "format",
        ),
        (
            NewProvider {
                format: ApiFormat::Anthropic,
                ..new_provider("p", ProviderKind::OpenAi, "k")
            },
            "format",
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
    let in_region = |region: &str| bedrock("bedrock", ApiFormat::Anthropic, region, aws());

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
        ..azure("azure", ApiFormat::OpenAi, AZURE_OPENAI, key("k"))
    };
    let no_endpoint = NewProvider {
        options: ProviderOptions::None,
        ..azure("azure", ApiFormat::OpenAi, AZURE_OPENAI, key("k"))
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
    let with = |credential| bedrock("bedrock", ApiFormat::Anthropic, "us-east-1", credential);
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
        azure(
            "azure",
            ApiFormat::OpenAi,
            AZURE_OPENAI,
            entra(tenant, client, secret),
        )
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
                ApiFormat::Anthropic,
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
        .create_provider(
            &sb,
            azure("azure", ApiFormat::OpenAi, AZURE_OPENAI, key("az-key")),
        )
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
        let b = || bedrock("b", ApiFormat::Anthropic, "us-east-1", credential.clone());
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
        let a = || azure("a", ApiFormat::OpenAi, AZURE_OPENAI, credential.clone());
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
        ..bedrock("bedrock-key", ApiFormat::OpenAi, "us-east-1", key("k"))
    };
    store.create_provider(&sb, keyed).await.unwrap();

    // Updates follow the same rule.
    let cloud = store
        .create_provider(
            &sb,
            bedrock(
                "bedrock-cloud",
                ApiFormat::Anthropic,
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
            ..bedrock(
                &format!("bedrock-{i}"),
                ApiFormat::Anthropic,
                "us-east-1",
                credential,
            )
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
        .create_provider(
            &sb,
            bedrock("bedrock-aws", ApiFormat::Anthropic, "us-east-1", aws()),
        )
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
            azure(
                "azure-sp",
                ApiFormat::OpenAi,
                AZURE_OPENAI,
                entra(TENANT, CLIENT, "sp-secret"),
            ),
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
                ApiFormat::Anthropic,
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
        "http://203.0.113.10@10.1.2.3/v1", // the host is after the @
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

#[tokio::test]
async fn claude_on_azure_needs_claude_deployments() {
    let (store, sb) = common::store().await;
    store
        .create_provider(
            &sb,
            azure(
                "azure-claude",
                ApiFormat::Anthropic,
                AZURE_FOUNDRY,
                ProviderCredential::CloudIdentity,
            ),
        )
        .await
        .unwrap();
    store
        .create_provider(
            &sb,
            azure("azure-gpt", ApiFormat::OpenAi, AZURE_OPENAI, key("k")),
        )
        .await
        .unwrap();
    // agentgateway sends a deployment to Foundry's /anthropic/v1/messages
    // only when its name starts with "claude".
    let err = store
        .create_model(new_model("gpt-x", "azure-claude", None))
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["name"]);
    for name in ["claude-sonnet-x", "Claude-Opus"] {
        store
            .create_model(new_model(name, "azure-claude", None))
            .await
            .unwrap();
    }
    store
        .create_model(new_model("gpt-x", "azure-gpt", None))
        .await
        .unwrap();
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
            format!(
                "{:?}",
                bedrock("p", ApiFormat::Anthropic, "us-east-1", credential.clone())
            ),
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
