mod common;

use std::collections::HashSet;
use std::time::Duration;

use sekisho_core::formats::ProviderKind;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{ModelName, ProviderName};
use sekisho_store::{ChangeKind, FieldError, NewModel, NewProvider, ProviderUpdate, StoreError};
use tokio_stream::StreamExt;
use uuid::Uuid;

fn new_provider(name: &str, kind: ProviderKind, api_key: &str) -> NewProvider {
    NewProvider {
        name: ProviderName::parse(name).unwrap(),
        kind,
        base_url: None,
        api_key: api_key.to_owned(),
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
    let raw: Vec<u8> = sqlx::query_scalar("select api_key_ciphertext from providers")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(!raw.windows(11).any(|w| w == b"sk-ant-test"));
    assert_eq!(
        store.provider_api_key(&sb, p.id).await.unwrap(),
        "sk-ant-test"
    );
}

#[tokio::test]
async fn provider_round_trip_and_lookups() {
    let (store, sb) = common::store().await;
    let mut compat = new_provider("mock", ProviderKind::OpenAiCompatible, "k");
    compat.base_url = Some("http://mock:8000/v1".into());
    let mock = store.create_provider(&sb, compat).await.unwrap();
    let anthropic = store
        .create_provider(&sb, new_provider("anthropic", ProviderKind::Anthropic, "k"))
        .await
        .unwrap();
    assert_eq!(mock.name.as_str(), "mock");
    assert_eq!(mock.kind, ProviderKind::OpenAiCompatible);
    assert_eq!(mock.base_url.as_deref(), Some("http://mock:8000/v1"));
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
        store.provider_api_key(&sb, Uuid::nil()).await,
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
        api_key: api_key.map(Into::into),
        base_url: base_url.map(|url| url.map(Into::into)),
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
    assert_eq!(store.provider_api_key(&sb, p.id).await.unwrap(), "new-key");
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);

    let proxied = store
        .update_provider(
            &sb,
            &p.name,
            update(None, Some(Some("https://proxy.example/v1"))),
        )
        .await
        .unwrap();
    assert_eq!(
        proxied.base_url.as_deref(),
        Some("https://proxy.example/v1")
    );
    assert_eq!(store.provider_api_key(&sb, p.id).await.unwrap(), "new-key"); // kept
    assert_eq!(next_change(&mut rx).await, ChangeKind::Providers);
    let cleared = store
        .update_provider(&sb, &p.name, update(None, Some(None)))
        .await
        .unwrap();
    assert_eq!(cleared.base_url, None);
    assert_eq!(store.list_models().await.unwrap().len(), 1);
}

#[tokio::test]
async fn provider_updates_are_validated() {
    let (store, sb) = common::store().await;
    let mut compat = new_provider("mock", ProviderKind::OpenAiCompatible, "k");
    compat.base_url = Some("http://mock:8000/v1".into());
    let mock = store.create_provider(&sb, compat).await.unwrap();
    let mut rx = store.listen().await.unwrap();
    for (refused, field) in [
        (update(Some(""), None), "api_key"),
        (update(None, Some(None)), "base_url"), // a compatible provider needs one
        (update(None, Some(Some("ftp://mock"))), "base_url"),
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
    assert_eq!(store.provider_api_key(&sb, mock.id).await.unwrap(), "k");
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
    let mut compat = new_provider("mock", ProviderKind::OpenAiCompatible, "k");
    compat.base_url = Some("http://mock:8000/v1".into());
    store.create_provider(&sb, compat).await.unwrap();
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
