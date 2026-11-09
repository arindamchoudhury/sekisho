mod common;

use std::collections::HashSet;
use std::time::Duration;

use sekisho_core::formats::ProviderKind;
use sekisho_core::keys::{EnvKeyId, KeyState, hash_secret, parse_token};
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_core::secret_box::SecretBox;
use sekisho_store::{
    ChangeKind, EnvironmentSpec, FieldError, NewModel, NewProvider, Store, StoreError,
};
use time::OffsetDateTime;
use tokio_stream::StreamExt;

fn env_name(name: &str) -> EnvName {
    EnvName::parse(name).unwrap()
}

fn spec(name: &str, models: &[&str]) -> EnvironmentSpec {
    EnvironmentSpec {
        name: env_name(name),
        description: String::new(),
        notice: String::new(),
        access_groups: Vec::new(),
        models: models
            .iter()
            .map(|m| ModelName::parse(m).unwrap())
            .collect(),
    }
}

/// Adds a provider of `kind` with one model per name in `models`.
async fn provider_with_models(
    store: &Store,
    sb: &SecretBox,
    name: &str,
    kind: ProviderKind,
    models: &[&str],
) {
    store
        .create_provider(
            sb,
            NewProvider {
                name: ProviderName::parse(name).unwrap(),
                kind,
                base_url: (kind == ProviderKind::OpenAiCompatible)
                    .then(|| "http://mock:8000/v1".into()),
                api_key: "k".into(),
            },
        )
        .await
        .unwrap();
    for model in models {
        store
            .create_model(NewModel {
                name: ModelName::parse(model).unwrap(),
                provider: ProviderName::parse(name).unwrap(),
                input_price: None,
                output_price: None,
            })
            .await
            .unwrap();
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

async fn found_state(store: &Store, id: &EnvKeyId) -> KeyState {
    let (key, _) = store.find_key(id).await.unwrap().expect("key exists");
    key.state(OffsetDateTime::now_utc())
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
async fn upsert_and_read_environment() {
    let (store, sb) = common::store().await;
    provider_with_models(
        &store,
        &sb,
        "anthropic",
        ProviderKind::Anthropic,
        &["claude-sonnet-5-5", "claude-haiku-5-5"],
    )
    .await;
    let mut want = spec("data-team", &["claude-sonnet-5-5", "claude-haiku-5-5"]); // not sorted: order is kept
    want.description = "Data team".into();
    want.notice = "Do not paste customer data.".into();
    want.access_groups = vec!["data".into(), "ml".into()];

    let created = store.upsert_environment(want.clone()).await.unwrap();
    assert_eq!(created.spec, want);
    let got = store.get_environment(&want.name).await.unwrap();
    assert_eq!(got, created);
    assert_eq!(
        store.list_environments().await.unwrap(),
        vec![created.clone()]
    );

    // A second upsert replaces the spec and keeps the identity.
    let mut changed = spec("data-team", &["claude-haiku-5-5"]);
    changed.description = "Data and ML".into();
    let updated = store.upsert_environment(changed.clone()).await.unwrap();
    assert_eq!(
        (updated.id, updated.created_at),
        (created.id, created.created_at)
    );
    assert!(updated.updated_at >= created.updated_at);
    assert_eq!(
        store.get_environment(&changed.name).await.unwrap().spec,
        changed
    );

    let empty = store.upsert_environment(spec("empty", &[])).await.unwrap();
    let names: Vec<_> = store
        .list_environments()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.spec.name)
        .collect();
    assert_eq!(names, [env_name("data-team"), env_name("empty")]);
    assert!(empty.spec.models.is_empty());
}

#[tokio::test]
async fn environment_input_is_validated() {
    let (store, sb) = common::store().await;
    provider_with_models(
        &store,
        &sb,
        "anthropic",
        ProviderKind::Anthropic,
        &["claude-sonnet-5-5"],
    )
    .await;
    let err = store
        .upsert_environment(spec(
            "data-team",
            &["claude-sonnet-5-5", "gpt-5", "claude-sonnet-5-5"],
        ))
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["models[1]", "models[2]"]); // unknown, then listed twice
    let mut groups = spec("data-team", &[]);
    groups.access_groups = vec!["".into(), "data".into(), "data".into()];
    assert_eq!(
        fields(store.upsert_environment(groups).await.unwrap_err()),
        ["access_groups[0]", "access_groups[2]"]
    );
    assert!(matches!(
        store.get_environment(&env_name("data-team")).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.delete_environment(&env_name("data-team")).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn two_providers_same_format_rejected() {
    let (store, sb) = common::store().await;
    provider_with_models(
        &store,
        &sb,
        "anthropic-a",
        ProviderKind::Anthropic,
        &["m-a"],
    )
    .await;
    provider_with_models(
        &store,
        &sb,
        "anthropic-b",
        ProviderKind::Anthropic,
        &["m-b"],
    )
    .await;
    let err = store
        .upsert_environment(spec("data-team", &["m-a", "m-b"]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Validation(ref f) if f[0].field == "models" && f[0].message.contains("anthropic-a") && f[0].message.contains("anthropic-b")),
        "{err:?}"
    );

    // OpenAI and an OpenAI-compatible endpoint share the openai format too.
    provider_with_models(&store, &sb, "openai", ProviderKind::OpenAi, &["gpt-5"]).await;
    provider_with_models(
        &store,
        &sb,
        "mock",
        ProviderKind::OpenAiCompatible,
        &["mock-model"],
    )
    .await;
    assert_eq!(
        fields(
            store
                .upsert_environment(spec("data-team", &["gpt-5", "mock-model"]))
                .await
                .unwrap_err()
        ),
        ["models"]
    );

    // One provider per format is fine, across formats.
    store
        .upsert_environment(spec("data-team", &["m-a", "gpt-5"]))
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_model_in_use_is_refused() {
    let (store, sb) = common::store().await;
    provider_with_models(
        &store,
        &sb,
        "anthropic",
        ProviderKind::Anthropic,
        &["claude-sonnet-5-5"],
    )
    .await;
    store
        .upsert_environment(spec("data-team", &["claude-sonnet-5-5"]))
        .await
        .unwrap();
    let model = ModelName::parse("claude-sonnet-5-5").unwrap();
    let err = store.delete_model(&model).await.unwrap_err();
    assert!(
        matches!(err, StoreError::InUse(ref m) if m == "environments: data-team"),
        "{err:?}"
    );

    store
        .upsert_environment(spec("data-team", &[]))
        .await
        .unwrap();
    store.delete_model(&model).await.unwrap();
}

#[tokio::test]
async fn key_lifecycle() {
    let (store, _sb) = common::store().await;
    let env = store
        .upsert_environment(spec("data-team", &[]))
        .await
        .unwrap()
        .spec
        .name;
    let (key, token) = store
        .create_key(&env, "alice@example.com", "ci", None)
        .await
        .unwrap();
    assert!(token.starts_with("sek_"));
    assert_eq!(
        (
            key.environment.clone(),
            key.owner.as_str(),
            key.name.as_str()
        ),
        (env.clone(), "alice@example.com", "ci")
    );
    assert_eq!(found_state(&store, &key.id).await, KeyState::Active);
    store.set_suspended(&key.id, true).await.unwrap();
    assert_eq!(found_state(&store, &key.id).await, KeyState::Suspended);
    store.set_suspended(&key.id, false).await.unwrap();
    assert_eq!(found_state(&store, &key.id).await, KeyState::Active);
    store.revoke_key(&key.id).await.unwrap();
    assert_eq!(found_state(&store, &key.id).await, KeyState::Revoked);
    assert!(store.set_suspended(&key.id, false).await.is_err()); // revoked is final
    assert!(
        matches!(store.set_suspended(&key.id, true).await, Err(StoreError::Conflict(ref m)) if m == "revoked")
    );

    // Revoking again keeps the first revocation time.
    let first = store.find_key(&key.id).await.unwrap().unwrap().0.revoked_at;
    store.revoke_key(&key.id).await.unwrap();
    assert_eq!(
        store.find_key(&key.id).await.unwrap().unwrap().0.revoked_at,
        first
    );
}

#[tokio::test]
async fn token_secret_not_stored() {
    let (store, _sb) = common::store().await;
    let env = store
        .upsert_environment(spec("data-team", &[]))
        .await
        .unwrap()
        .spec
        .name;
    let (key, token) = store
        .create_key(&env, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let (id, secret) = parse_token(&token).unwrap();
    assert_eq!(id, key.id);
    let (found, sha) = store.find_key(&key.id).await.unwrap().unwrap();
    assert_eq!(found, key);
    assert_eq!(sha, hash_secret(&secret));
    let rows: Vec<String> = sqlx::query_scalar("select row_to_json(k)::text from env_keys k")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        !rows[0].contains(&secret) && !rows[0].contains(&token),
        "{}",
        rows[0]
    );
}

#[tokio::test]
async fn key_inputs_and_lookups() {
    let (store, _sb) = common::store().await;
    let env = store
        .upsert_environment(spec("data-team", &[]))
        .await
        .unwrap()
        .spec
        .name;
    let unknown_env = env_name("nope");
    assert!(matches!(
        store
            .create_key(&unknown_env, "alice@example.com", "ci", None)
            .await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.list_keys(&unknown_env).await,
        Err(StoreError::NotFound)
    ));

    let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
    let err = store
        .create_key(&env, "", "a\u{7}b", Some(past))
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["owner", "name", "expires_at"]);
    assert_eq!(
        fields(
            store
                .create_key(&env, "alice@example.com", "", None)
                .await
                .unwrap_err()
        ),
        ["name"]
    );

    let soon = OffsetDateTime::now_utc() + time::Duration::hours(1);
    let (ci, _) = store
        .create_key(&env, "alice@example.com", "ci", Some(soon))
        .await
        .unwrap();
    let (deploy, _) = store
        .create_key(&env, "bob@example.com", "deploy", None)
        .await
        .unwrap();
    let err = store
        .create_key(&env, "carol@example.com", "ci", None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(ref m) if m.contains("ci")),
        "{err:?}"
    );
    assert_eq!(
        store.list_keys(&env).await.unwrap(),
        vec![ci.clone(), deploy]
    );
    assert_eq!(
        ci.state(soon + time::Duration::seconds(1)),
        KeyState::Expired
    );

    let missing = EnvKeyId::parse("aaaaaaaaaaaa").unwrap();
    assert_eq!(store.find_key(&missing).await.unwrap(), None);
    assert!(matches!(
        store.set_suspended(&missing, true).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.revoke_key(&missing).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn delete_environment_cascades() {
    let (store, _sb) = common::store().await;
    let env = store
        .upsert_environment(spec("data-team", &[]))
        .await
        .unwrap();
    let other = store
        .upsert_environment(spec("ml-team", &[]))
        .await
        .unwrap()
        .spec
        .name;
    let (key, _) = store
        .create_key(&env.spec.name, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let (kept, _) = store
        .create_key(&other, "alice@example.com", "ci", None)
        .await
        .unwrap();
    // Budgets arrive in Task 5; insert one per scope directly.
    sqlx::query(
        "insert into budgets (name, scope, environment_id, key_id, person, limit_micro_usd, window_kind, calendar) values \
         ('env', 'environment', $1, null, null, 1, 'month', true), \
         ('key', 'key', null, $2, null, 1, 'month', true), \
         ('person-here', 'person', $1, null, 'alice@example.com', 1, 'month', true), \
         ('person-everywhere', 'person', null, null, 'alice@example.com', 1, 'month', true)",
    )
    .bind(env.id)
    .bind(key.id.as_str())
    .execute(store.pool())
    .await
    .unwrap();

    let mut rx = store.listen().await.unwrap();
    store.delete_environment(&env.spec.name).await.unwrap();
    let mut seen = HashSet::new();
    for _ in 0..3 {
        seen.insert(next_change(&mut rx).await);
    }
    assert_eq!(
        seen,
        HashSet::from([
            ChangeKind::Environments,
            ChangeKind::Keys,
            ChangeKind::Budgets
        ])
    );

    assert!(matches!(
        store.get_environment(&env.spec.name).await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(store.find_key(&key.id).await.unwrap(), None);
    assert!(store.find_key(&kept.id).await.unwrap().is_some());
    let budgets: Vec<String> = sqlx::query_scalar("select name from budgets order by name")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert_eq!(budgets, ["person-everywhere"]);
}

#[tokio::test]
async fn writes_notify_and_failures_do_not() {
    let (store, _sb) = common::store().await;
    let mut rx = store.listen().await.unwrap();
    let env = store
        .upsert_environment(spec("data-team", &[]))
        .await
        .unwrap()
        .spec
        .name;
    assert_eq!(next_change(&mut rx).await, ChangeKind::Environments);
    let (key, _) = store
        .create_key(&env, "alice@example.com", "ci", None)
        .await
        .unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Keys);
    store.set_suspended(&key.id, true).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Keys);
    store.revoke_key(&key.id).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Keys);

    assert!(
        store
            .upsert_environment(spec("data-team", &["gpt-5"]))
            .await
            .is_err()
    );
    assert!(
        store
            .create_key(&env, "alice@example.com", "ci", None)
            .await
            .is_err()
    );
    assert!(store.set_suspended(&key.id, false).await.is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.next())
            .await
            .is_err(),
        "no notification from refused writes"
    );
}

#[tokio::test]
async fn auth_snapshot_has_every_key_and_environment() {
    let (store, _sb) = common::store().await;
    let data = store
        .upsert_environment(spec("data-team", &[]))
        .await
        .unwrap()
        .spec
        .name;
    let empty = store
        .upsert_environment(spec("empty", &[]))
        .await
        .unwrap()
        .spec
        .name;
    let (live, token) = store
        .create_key(&data, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let (gone, _) = store
        .create_key(&data, "bob@example.com", "old", None)
        .await
        .unwrap();
    store.revoke_key(&gone.id).await.unwrap();

    let snapshot = store.auth_snapshot().await.unwrap();
    assert_eq!(snapshot.environments, vec![data, empty]);
    let (_, secret) = parse_token(&token).unwrap();
    let ids: Vec<&str> = snapshot.keys.iter().map(|(k, _)| k.id.as_str()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&live.id.as_str()) && ids.contains(&gone.id.as_str()));
    let (_, sha) = snapshot.keys.iter().find(|(k, _)| k.id == live.id).unwrap();
    assert_eq!(*sha, hash_secret(&secret));
    assert!(
        snapshot
            .keys
            .iter()
            .any(|(k, _)| k.id == gone.id && k.revoked_at.is_some())
    );
}
