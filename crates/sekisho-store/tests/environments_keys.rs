mod common;

use std::collections::HashSet;
use std::time::Duration;

use sekisho_catalog::environment::McpServerUse;
use sekisho_catalog::labels::Labels;
use sekisho_catalog::model::{
    A2aAgentSpec, CheckedEntry, EntrySpec, McpRuntime, McpServerSpec, PluginSpec, PromptSpec,
    SkillSpec, ToolkitServer, ToolkitSpec,
};
use sekisho_catalog::names::{EntryKind, EntryName, EntryRef, SourceKind, SourceName};
use sekisho_catalog::source::{Folder, SourceUrl};
use sekisho_core::formats::ProviderKind;
use sekisho_core::keys::{EnvKeyId, KeyState, hash_secret, parse_token};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_core::secret_box::SecretBox;
use sekisho_store::{
    ChangeKind, EntryKey, EnvModel, EnvironmentSpec, FieldError, FoundEntry, NewModel, NewProvider,
    NewSource, ProviderOptions, Store, StoreError, SyncFound, SyncOutcome, SyncRecord,
};
use time::OffsetDateTime;
use tokio_stream::StreamExt;

fn env_name(name: &str) -> EnvName {
    EnvName::parse(name).unwrap()
}

fn spec(name: &str, models: &[&str]) -> EnvironmentSpec {
    EnvironmentSpec {
        access_groups: vec!["github/eng".into()],
        models: models
            .iter()
            .map(|m| ModelName::parse(m).unwrap())
            .collect(),
        ..EnvironmentSpec::new(env_name(name))
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
                // `mock` doesn't resolve.
                allow_private_network: kind == ProviderKind::OpenAiCompatible,
                ..NewProvider::keyed(
                    ProviderName::parse(name).unwrap(),
                    kind,
                    (kind == ProviderKind::OpenAiCompatible).then(|| "http://mock:8000/v1".into()),
                    "k",
                )
            },
        )
        .await
        .unwrap();
    // OpenAI-compatible models need prices; others use the catalog's.
    let price = (kind == ProviderKind::OpenAiCompatible).then_some(MicroUsd(1_000_000));
    for model in models {
        store
            .create_model(NewModel {
                name: ModelName::parse(model).unwrap(),
                provider: ProviderName::parse(name).unwrap(),
                input_price: price,
                output_price: price,
                cache_read_price: None,
                cache_write_price: None,
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
    want.owner = Some(common::admin(&store).await);

    let created = store
        .upsert_environment(want.clone(), common::admin(&store).await)
        .await
        .unwrap();
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
    changed.owner = want.owner;
    let updated = store
        .upsert_environment(changed.clone(), common::admin(&store).await)
        .await
        .unwrap();
    assert_eq!(
        (updated.id, updated.created_at),
        (created.id, created.created_at)
    );
    assert!(updated.updated_at >= created.updated_at);
    assert_eq!(
        store.get_environment(&changed.name).await.unwrap().spec,
        changed
    );

    let empty = store
        .upsert_environment(spec("empty", &[]), common::admin(&store).await)
        .await
        .unwrap();
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
        .upsert_environment(
            spec(
                "data-team",
                &["claude-sonnet-5-5", "gpt-5", "claude-sonnet-5-5"],
            ),
            common::admin(&store).await,
        )
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["models[1]", "models[2]"]); // unknown, then listed twice
    let mut groups = spec("data-team", &[]);
    groups.access_groups = vec![
        "".into(),
        "data".into(),
        "data".into(),
        // An identity provider's group needs its connector (D124).
        "acme:eng".into(),
        "github/acme:eng".into(),
    ];
    assert_eq!(
        fields(
            store
                .upsert_environment(groups, common::admin(&store).await)
                .await
                .unwrap_err()
        ),
        ["access_groups[0]", "access_groups[2]", "access_groups[3]"]
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

/// Each environment chooses a provider by model (plan 1d), so its models
/// may come from any providers: two of one kind, OpenAI beside an
/// OpenAI-compatible host, or two Bedrock providers.
#[tokio::test]
async fn an_environment_mixes_providers_of_one_format() {
    let (store, sb) = common::store().await;
    for (name, kind, models) in [
        (
            "scaleway",
            ProviderKind::OpenAiCompatible,
            &["qwen3-235b"][..],
        ),
        ("mock", ProviderKind::OpenAiCompatible, &["mock-model"][..]),
        ("openai", ProviderKind::OpenAi, &["gpt-5"][..]),
        ("anthropic-a", ProviderKind::Anthropic, &["m-a"][..]),
        ("anthropic-b", ProviderKind::Anthropic, &["m-b"][..]),
    ] {
        provider_with_models(&store, &sb, name, kind, models).await;
    }
    for name in ["bedrock-a", "bedrock-b"] {
        store
            .create_provider(
                &sb,
                NewProvider {
                    options: ProviderOptions::Bedrock {
                        region: "us-east-1".into(),
                    },
                    ..NewProvider::keyed(
                        ProviderName::parse(name).unwrap(),
                        ProviderKind::Bedrock,
                        None,
                        "bedrock-api-key",
                    )
                },
            )
            .await
            .unwrap();
        store
            .create_model(NewModel {
                name: ModelName::parse(&format!("{name}-model")).unwrap(),
                provider: ProviderName::parse(name).unwrap(),
                input_price: None,
                output_price: None,
                cache_read_price: None,
                cache_write_price: None,
            })
            .await
            .unwrap();
    }
    let models = [
        "qwen3-235b",
        "mock-model",
        "gpt-5",
        "m-a",
        "m-b",
        "bedrock-a-model",
        "bedrock-b-model",
    ];
    let saved = store
        .upsert_environment(spec("data-team", &models), common::admin(&store).await)
        .await
        .unwrap();
    let names: Vec<&str> = saved.spec.models.iter().map(|m| m.as_str()).collect();
    assert_eq!(names, models);
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
        .upsert_environment(
            spec("data-team", &["claude-sonnet-5-5"]),
            common::admin(&store).await,
        )
        .await
        .unwrap();
    let model = ModelName::parse("claude-sonnet-5-5").unwrap();
    let err = store.delete_model(&model).await.unwrap_err();
    assert!(
        matches!(err, StoreError::InUse(ref m) if m == "environments: data-team"),
        "{err:?}"
    );

    store
        .upsert_environment(spec("data-team", &[]), common::admin(&store).await)
        .await
        .unwrap();
    store.delete_model(&model).await.unwrap();
}

#[tokio::test]
async fn key_lifecycle() {
    let (store, _sb) = common::store().await;
    let env = store
        .upsert_environment(spec("data-team", &[]), common::admin(&store).await)
        .await
        .unwrap()
        .spec
        .name;
    let (key, token) = store
        .create_key(
            &env,
            common::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
        .await
        .unwrap();
    assert!(token.starts_with("sek_"));
    let alice = common::person(&store, "alice@example.com", &["eng"]).await;
    assert_eq!(
        (key.environment.clone(), key.owner, key.name.as_str()),
        (env.clone(), alice, "ci")
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
        .upsert_environment(spec("data-team", &[]), common::admin(&store).await)
        .await
        .unwrap()
        .spec
        .name;
    let (key, token) = store
        .create_key(
            &env,
            common::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
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
        .upsert_environment(spec("data-team", &[]), common::admin(&store).await)
        .await
        .unwrap()
        .spec
        .name;
    let unknown_env = env_name("nope");
    assert!(matches!(
        store
            .create_key(
                &unknown_env,
                common::person(&store, "alice@example.com", &["eng"]).await,
                "ci",
                None
            )
            .await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.list_keys(&unknown_env).await,
        Err(StoreError::NotFound)
    ));

    let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
    let err = store
        .create_key(&env, common::admin(&store).await, "a\u{7}b", Some(past))
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["name", "expires_at"]);
    assert_eq!(
        fields(
            store
                .create_key(
                    &env,
                    common::person(&store, "alice@example.com", &["eng"]).await,
                    "",
                    None
                )
                .await
                .unwrap_err()
        ),
        ["name"]
    );

    let soon = OffsetDateTime::now_utc() + time::Duration::hours(1);
    let (ci, _) = store
        .create_key(
            &env,
            common::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            Some(soon),
        )
        .await
        .unwrap();
    let (deploy, _) = store
        .create_key(
            &env,
            common::person(&store, "bob@example.com", &["eng"]).await,
            "deploy",
            None,
        )
        .await
        .unwrap();
    let err = store
        .create_key(
            &env,
            common::person(&store, "carol@example.com", &["eng"]).await,
            "ci",
            None,
        )
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
        .upsert_environment(spec("data-team", &[]), common::admin(&store).await)
        .await
        .unwrap();
    let other = store
        .upsert_environment(spec("ml-team", &[]), common::admin(&store).await)
        .await
        .unwrap()
        .spec
        .name;
    let (key, _) = store
        .create_key(
            &env.spec.name,
            common::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
        .await
        .unwrap();
    let (kept, _) = store
        .create_key(
            &other,
            common::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
        .await
        .unwrap();
    // Budgets arrive in Task 5; insert one per scope directly.
    sqlx::query(
        "insert into budgets (name, scope, environment_id, key_id, person_id, limit_micro_usd, window_kind, calendar) values \
         ('env', 'environment', $1, null, null, 1, 'month', true), \
         ('key', 'key', null, $2, null, 1, 'month', true), \
         ('person-here', 'person', $1, null, $3, 1, 'month', true), \
         ('person-everywhere', 'person', null, null, $3, 1, 'month', true)",
    )
    .bind(env.id)
    .bind(key.id.as_str())
    .bind(key.owner)
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
    common::admin(&store).await;
    common::person(&store, "alice@example.com", &["eng"]).await;
    let mut rx = store.listen().await.unwrap();
    let env = store
        .upsert_environment(spec("data-team", &[]), common::admin(&store).await)
        .await
        .unwrap()
        .spec
        .name;
    assert_eq!(next_change(&mut rx).await, ChangeKind::Environments);
    let (key, _) = store
        .create_key(
            &env,
            common::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
        .await
        .unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Keys);
    store.set_suspended(&key.id, true).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Keys);
    store.revoke_key(&key.id).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Keys);

    assert!(
        store
            .upsert_environment(spec("data-team", &["gpt-5"]), common::admin(&store).await)
            .await
            .is_err()
    );
    assert!(
        store
            .create_key(
                &env,
                common::person(&store, "alice@example.com", &["eng"]).await,
                "ci",
                None
            )
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
        .upsert_environment(spec("data-team", &[]), common::admin(&store).await)
        .await
        .unwrap()
        .spec
        .name;
    let empty = store
        .upsert_environment(spec("empty", &[]), common::admin(&store).await)
        .await
        .unwrap()
        .spec
        .name;
    let (live, token) = store
        .create_key(
            &data,
            common::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
        .await
        .unwrap();
    let (gone, _) = store
        .create_key(
            &data,
            common::person(&store, "bob@example.com", &["eng"]).await,
            "old",
            None,
        )
        .await
        .unwrap();
    store.revoke_key(&gone.id).await.unwrap();

    let snapshot = store
        .auth_snapshot(OffsetDateTime::now_utc())
        .await
        .unwrap();
    let names: Vec<EnvName> = snapshot
        .environments
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(names, vec![data, empty]);
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

#[tokio::test]
async fn the_auth_snapshot_lists_each_environments_models() {
    let (store, sb) = common::store().await;
    provider_with_models(
        &store,
        &sb,
        "openai",
        ProviderKind::OpenAi,
        &["gpt-5", "gpt-5-mini"],
    )
    .await;
    provider_with_models(
        &store,
        &sb,
        "anthropic",
        ProviderKind::Anthropic,
        &["claude-sonnet-4-5"],
    )
    .await;
    provider_with_models(
        &store,
        &sb,
        "gemini",
        ProviderKind::Gemini,
        &["gemini-2.5-pro"],
    )
    .await;
    let admin = common::admin(&store).await;
    for (name, models) in [
        (
            "data-team",
            &["gpt-5-mini", "claude-sonnet-4-5", "gemini-2.5-pro"][..],
        ),
        ("empty", &[][..]),
        ("ml-team", &["gpt-5", "gpt-5-mini"][..]),
    ] {
        store
            .upsert_environment(spec(name, models), admin)
            .await
            .unwrap();
    }
    let snapshot = store
        .auth_snapshot(OffsetDateTime::now_utc())
        .await
        .unwrap();
    let models: Vec<(String, Vec<(String, ProviderKind)>)> = snapshot
        .environments
        .iter()
        .map(|e| {
            let models = e
                .models
                .iter()
                .map(|m: &EnvModel| (m.name.as_str().to_owned(), m.kind))
                .collect();
            (e.name.as_str().to_owned(), models)
        })
        .collect();
    let owned = |pairs: &[(&str, ProviderKind)]| {
        pairs
            .iter()
            .map(|(n, k)| ((*n).to_owned(), *k))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        models,
        vec![
            (
                "data-team".to_owned(),
                owned(&[
                    ("gpt-5-mini", ProviderKind::OpenAi),
                    ("claude-sonnet-4-5", ProviderKind::Anthropic),
                    ("gemini-2.5-pro", ProviderKind::Gemini),
                ])
            ),
            ("empty".to_owned(), Vec::new()),
            (
                "ml-team".to_owned(),
                owned(&[
                    ("gpt-5", ProviderKind::OpenAi),
                    ("gpt-5-mini", ProviderKind::OpenAi)
                ])
            ),
        ]
    );
}

/// agentgateway takes the model in a Gemini path from after the first
/// `/models/`, so in `/env/models/gemini/v1beta/models/<model>:…` it would
/// read `gemini/v1beta/models/<model>`. The name is reserved.
#[tokio::test]
async fn an_environment_cannot_be_named_models() {
    let (store, _sb) = common::store().await;
    let err = store
        .upsert_environment(spec("models", &[]), common::admin(&store).await)
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["name"]);
    // Names that only contain it are fine.
    store
        .upsert_environment(spec("models-team", &[]), common::admin(&store).await)
        .await
        .unwrap();
}

// ------------------------------------------------------------ catalog entries (plan 3, Task 8)

fn entry(kind: EntryKind, name: &str) -> CheckedEntry {
    let text = format!("The {name} entry.");
    let spec = match kind {
        EntryKind::McpServer => EntrySpec::McpServer(McpServerSpec {
            description: text,
            runtime: McpRuntime::Remote {
                url: format!("https://mcp.example.com/{name}"),
            },
            config: Vec::new(),
            tool_preview: Vec::new(),
            unsupported_tools: Vec::new(),
            static_oauth: false,
        }),
        EntryKind::A2aAgent => EntrySpec::A2aAgent(A2aAgentSpec {
            description: text,
            url: format!("https://agents.example.com/{name}"),
            card_url: None,
            card: None,
        }),
        EntryKind::Skill => EntrySpec::Skill(SkillSpec {
            description: text,
            license: None,
            compatibility: None,
        }),
        EntryKind::Plugin => EntrySpec::Plugin(PluginSpec {
            description: text,
            version: None,
            mcp_servers: Vec::new(),
            apm_checked: false,
        }),
        EntryKind::Prompt => EntrySpec::Prompt(PromptSpec { description: text }),
        EntryKind::Toolkit => panic!("use kit()"),
    };
    CheckedEntry {
        kind,
        name: EntryName::parse(name).unwrap(),
        path: name.into(),
        spec,
        files: Vec::new(),
    }
}

/// A toolkit of MCP servers (with their tools) and skills.
fn kit(name: &str, servers: &[(&str, Option<&[&str]>)], skills: &[&str]) -> CheckedEntry {
    CheckedEntry {
        kind: EntryKind::Toolkit,
        name: EntryName::parse(name).unwrap(),
        path: format!("{name}.yaml"),
        spec: EntrySpec::Toolkit(ToolkitSpec {
            description: String::new(),
            mcp_servers: servers
                .iter()
                .map(|(s, tools)| ToolkitServer {
                    server: EntryRef::parse(s).unwrap(),
                    tools: tools.map(|t| t.iter().map(|x| (*x).to_owned()).collect()),
                })
                .collect(),
            a2a_agents: Vec::new(),
            skills: skills.iter().map(|s| EntryRef::parse(s).unwrap()).collect(),
            plugins: Vec::new(),
            prompts: Vec::new(),
        }),
        files: Vec::new(),
    }
}

fn record(commit: &str, entries: Vec<CheckedEntry>) -> SyncRecord {
    let now = OffsetDateTime::now_utc();
    SyncRecord {
        commit: Some(commit.into()),
        started_at: now,
        finished_at: now,
        outcome: SyncOutcome::Ok(SyncFound {
            entries: entries.into_iter().map(FoundEntry::from_checked).collect(),
            ..SyncFound::default()
        }),
    }
}

/// Adds the source `name` holding `entries`; its first sync applies itself.
async fn catalog(store: &Store, sb: &SecretBox, name: &str, entries: Vec<CheckedEntry>) {
    let admin = common::admin(store).await;
    store
        .add_source(
            sb,
            NewSource {
                name: SourceName::parse(name).unwrap(),
                kind: SourceKind::Catalog,
                url: SourceUrl::parse(&format!("https://git.example.com/{name}.git"), false)
                    .unwrap(),
                git_ref: None,
                folder: Folder::default(),
                username: None,
                token: None,
                allow_private_network: false,
            },
            admin,
        )
        .await
        .unwrap();
    resync(store, sb, name, "c1", entries).await;
}

async fn resync(
    store: &Store,
    sb: &SecretBox,
    name: &str,
    commit: &str,
    entries: Vec<CheckedEntry>,
) {
    let report = store
        .record_sync(
            sb,
            &SourceName::parse(name).unwrap(),
            record(commit, entries),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await
        .unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
}

fn entry_key(kind: EntryKind, source: &str, name: &str) -> EntryKey {
    EntryKey {
        kind,
        source: SourceName::parse(source).unwrap(),
        name: EntryName::parse(name).unwrap(),
    }
}

fn refs(names: &[&str]) -> Vec<EntryRef> {
    names.iter().map(|n| EntryRef::parse(n).unwrap()).collect()
}

fn server_use(name: &str, tools: Option<&[&str]>) -> McpServerUse {
    McpServerUse {
        server: EntryRef::parse(name).unwrap(),
        tools: tools.map(|t| t.iter().map(|x| (*x).to_owned()).collect()),
    }
}

/// `(kind, written, source/name, tools)` of an environment's entries.
fn listed(
    env: &sekisho_store::Environment,
) -> Vec<(EntryKind, String, String, Option<Vec<String>>)> {
    env.entries
        .iter()
        .map(|e| {
            (
                e.kind,
                e.written.clone(),
                format!("{}/{}", e.key.source, e.key.name),
                e.tools.clone(),
            )
        })
        .collect()
}

fn messages(err: StoreError) -> Vec<(String, String)> {
    match err {
        StoreError::Validation(f) => f
            .into_iter()
            .map(|FieldError { field, message }| (field, message))
            .collect(),
        other => panic!("expected Validation, got {other:?}"),
    }
}

/// What an environment delivers: `source/name` with the tools and how.
async fn delivered(
    store: &Store,
    env: &sekisho_store::Environment,
) -> Vec<(String, Option<Vec<String>>, bool, Vec<String>)> {
    store
        .environment_catalog(env.id)
        .await
        .unwrap()
        .into_iter()
        .map(|d| {
            (
                format!("{}/{}", d.key.source, d.key.name),
                d.tools,
                d.listed,
                d.via.iter().map(|k| k.name.to_string()).collect(),
            )
        })
        .collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn environments_list_catalog_entries() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    catalog(
        &store,
        &sb,
        "a",
        vec![
            entry(EntryKind::McpServer, "docs"),
            entry(EntryKind::A2aAgent, "planner"),
            entry(EntryKind::Skill, "pdf"),
            entry(EntryKind::Skill, "notes"),
            entry(EntryKind::Plugin, "fmt"),
            entry(EntryKind::Prompt, "review"),
            kit("kit", &[("docs", None)], &[]),
        ],
    )
    .await;
    let mut s = spec("team", &[]);
    s.mcp_servers = vec![server_use("docs", Some(&["search", "read"]))];
    s.a2a_agents = refs(&["planner"]);
    s.skills = refs(&["notes", "pdf@a"]);
    s.plugins = refs(&["fmt"]);
    s.prompts = refs(&["review"]);
    s.toolkits = refs(&["kit"]);
    store.upsert_environment(s.clone(), admin).await.unwrap();

    let env = store.get_environment(&env_name("team")).await.unwrap();
    assert_eq!(env.spec.mcp_servers, s.mcp_servers);
    assert_eq!(env.spec.skills, s.skills);
    assert_eq!(env.spec.toolkits, s.toolkits);
    let tools = Some(strings(&["search", "read"]));
    assert_eq!(
        listed(&env),
        [
            (EntryKind::McpServer, "docs".into(), "a/docs".into(), tools),
            (
                EntryKind::A2aAgent,
                "planner".into(),
                "a/planner".into(),
                None
            ),
            (EntryKind::Skill, "notes".into(), "a/notes".into(), None),
            (EntryKind::Skill, "pdf@a".into(), "a/pdf".into(), None),
            (EntryKind::Plugin, "fmt".into(), "a/fmt".into(), None),
            (EntryKind::Prompt, "review".into(), "a/review".into(), None),
            (EntryKind::Toolkit, "kit".into(), "a/kit".into(), None),
        ]
    );
    assert!(env.entries.iter().all(|e| !e.quarantined && !e.removed));
    let pdf = store
        .get_entry(&entry_key(EntryKind::Skill, "a", "pdf"))
        .await
        .unwrap();
    assert_eq!(pdf.environments, [env_name("team")]);

    // Unknown references are refused, each at its place.
    let mut bad = spec("other", &[]);
    bad.skills = refs(&["pdf", "nope"]);
    bad.mcp_servers = vec![server_use("missing", None)];
    let errs = store.upsert_environment(bad, admin).await.unwrap_err();
    assert_eq!(
        messages(errs),
        [
            (
                "mcp_servers[0]".to_string(),
                "no MCP server named missing".to_string()
            ),
            ("skills[1]".to_string(), "no skill named nope".to_string()),
        ]
    );
}

#[tokio::test]
async fn a_bare_name_in_two_sources_is_refused() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    catalog(&store, &sb, "a", vec![entry(EntryKind::Skill, "pdf")]).await;
    catalog(&store, &sb, "b", vec![entry(EntryKind::Skill, "pdf")]).await;
    let mut s = spec("team", &[]);
    s.skills = refs(&["pdf"]);
    let errs = store
        .upsert_environment(s.clone(), admin)
        .await
        .unwrap_err();
    assert_eq!(
        messages(errs),
        [(
            "skills[0]".to_string(),
            "pdf is in sources a, b: write pdf@a or pdf@b".to_string()
        )]
    );
    s.skills = refs(&["pdf@b"]);
    let env = store.upsert_environment(s, admin).await.unwrap();
    assert_eq!(env.entries[0].key.source.as_str(), "b");
    // One entry named twice.
    let mut twice = spec("twice", &[]);
    twice.skills = refs(&["pdf@a", "pdf@b", "pdf@a"]);
    let errs = store.upsert_environment(twice, admin).await.unwrap_err();
    assert!(
        messages(errs)
            .iter()
            .any(|(f, m)| f == "skills[2]" && m.contains("listed twice")),
    );
}

#[tokio::test]
async fn a_saved_reference_stays_bound() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    catalog(&store, &sb, "a", vec![entry(EntryKind::Skill, "pdf")]).await;
    let mut s = spec("team", &[]);
    s.skills = refs(&["pdf"]);
    store.upsert_environment(s.clone(), admin).await.unwrap();
    catalog(&store, &sb, "b", vec![entry(EntryKind::Skill, "pdf")]).await;

    let env = store.upsert_environment(s.clone(), admin).await.unwrap();
    assert_eq!(listed(&env)[0].2, "a/pdf");
    // Written another way, it is resolved again.
    let mut other = s.clone();
    other.skills = refs(&["pdf@b"]);
    let env = store.upsert_environment(other, admin).await.unwrap();
    assert_eq!(listed(&env)[0].2, "b/pdf");
    // And back to the bare name, which is ambiguous now.
    assert!(store.upsert_environment(s, admin).await.is_err());
}

#[tokio::test]
async fn toolkit_entries_reach_the_environment_once() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    catalog(
        &store,
        &sb,
        "a",
        vec![
            entry(EntryKind::McpServer, "github"),
            entry(EntryKind::Skill, "pdf"),
            kit("one", &[("github", None)], &["pdf"]),
            kit("two", &[("github", None)], &[]),
        ],
    )
    .await;
    let mut s = spec("team", &[]);
    s.mcp_servers = vec![server_use("github", None)];
    s.toolkits = refs(&["one", "two"]);
    let env = store.upsert_environment(s, admin).await.unwrap();
    assert_eq!(
        delivered(&store, &env).await,
        [
            ("a/github".to_string(), None, true, strings(&["one", "two"])),
            ("a/pdf".to_string(), None, false, strings(&["one"])),
        ]
    );
    let all = store.environment_catalog(env.id).await.unwrap();
    assert!(all.iter().all(|d| d.revision.is_some() && !d.quarantined));
}

#[tokio::test]
async fn allow_lists_from_toolkits_are_unioned() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    catalog(
        &store,
        &sb,
        "a",
        vec![
            entry(EntryKind::McpServer, "github"),
            kit("one", &[("github", Some(&["a", "c"]))], &[]),
            kit("two", &[("github", Some(&["b", "a"]))], &[]),
            kit("all", &[("github", None)], &[]),
        ],
    )
    .await;
    let mut s = spec("team", &[]);
    s.toolkits = refs(&["one", "two"]);
    let env = store.upsert_environment(s.clone(), admin).await.unwrap();
    assert_eq!(
        delivered(&store, &env).await[0].1,
        Some(strings(&["a", "b", "c"]))
    );

    s.mcp_servers = vec![server_use("github", Some(&["d"]))];
    let env = store.upsert_environment(s.clone(), admin).await.unwrap();
    assert_eq!(
        delivered(&store, &env).await[0].1,
        Some(strings(&["a", "b", "c", "d"]))
    );

    s.toolkits = refs(&["one", "all"]);
    let env = store.upsert_environment(s, admin).await.unwrap();
    assert_eq!(delivered(&store, &env).await[0].1, None);
}

#[tokio::test]
async fn a_quarantined_toolkit_drops_only_what_it_alone_brings() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    catalog(
        &store,
        &sb,
        "a",
        vec![
            entry(EntryKind::McpServer, "github"),
            entry(EntryKind::Skill, "pdf"),
            entry(EntryKind::Skill, "notes"),
            entry(EntryKind::Skill, "calc"),
            kit("one", &[("github", None)], &["pdf", "notes", "calc"]),
            kit("two", &[("github", None)], &[]),
        ],
    )
    .await;
    let mut s = spec("team", &[]);
    s.skills = refs(&["calc"]);
    s.toolkits = refs(&["one", "two"]);
    let env = store.upsert_environment(s, admin).await.unwrap();
    store
        .quarantine(
            &entry_key(EntryKind::Toolkit, "a", "one"),
            "Bad.",
            admin,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert_eq!(
        delivered(&store, &env).await,
        [
            ("a/calc".to_string(), None, true, Vec::new()),
            ("a/github".to_string(), None, false, strings(&["two"])),
        ]
    );
    let env = store.get_environment(&env_name("team")).await.unwrap();
    let one = env.entries.iter().find(|e| e.written == "one").unwrap();
    assert!(one.quarantined);
}

#[tokio::test]
async fn quarantined_and_removed_entries_cannot_be_added_but_stay() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    catalog(
        &store,
        &sb,
        "a",
        vec![
            entry(EntryKind::Skill, "pdf"),
            entry(EntryKind::Skill, "notes"),
        ],
    )
    .await;
    let mut s = spec("team", &[]);
    s.skills = refs(&["pdf", "notes"]);
    store.upsert_environment(s.clone(), admin).await.unwrap();

    store
        .quarantine(
            &entry_key(EntryKind::Skill, "a", "pdf"),
            "Bad.",
            admin,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    resync(&store, &sb, "a", "c2", vec![entry(EntryKind::Skill, "pdf")]).await;
    store
        .apply_source(&SourceName::parse("a").unwrap(), OffsetDateTime::now_utc())
        .await
        .unwrap();

    // Already listed: they stay, flagged.
    let env = store.upsert_environment(s.clone(), admin).await.unwrap();
    let flags: Vec<(bool, bool)> = env
        .entries
        .iter()
        .map(|e| (e.quarantined, e.removed))
        .collect();
    assert_eq!(flags, [(true, false), (false, true)]);
    let d = store.environment_catalog(env.id).await.unwrap();
    assert_eq!(
        d.iter()
            .map(|d| (d.quarantined, d.removed))
            .collect::<Vec<_>>(),
        [(true, false), (false, true)]
    );

    // New ones can't add them.
    let mut new = spec("new", &[]);
    new.skills = refs(&["pdf", "notes"]);
    let errs = messages(store.upsert_environment(new, admin).await.unwrap_err());
    assert_eq!(
        errs,
        [
            ("skills[0]".to_string(), "pdf is quarantined".to_string()),
            (
                "skills[1]".to_string(),
                "notes was removed from its source".to_string()
            ),
        ]
    );
}

#[tokio::test]
async fn defaults_must_be_listed_and_reach_their_format() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    provider_with_models(
        &store,
        &sb,
        "anthropic",
        ProviderKind::Anthropic,
        &["claude"],
    )
    .await;
    provider_with_models(&store, &sb, "openai", ProviderKind::OpenAi, &["gpt"]).await;
    provider_with_models(&store, &sb, "gemini", ProviderKind::Gemini, &["gem"]).await;
    let model = |m: &str| Some(ModelName::parse(m).unwrap());

    let mut s = spec("team", &["claude", "gpt", "gem"]);
    // Codex's Responses can't reach Claude on Anthropic; only Gemini
    // providers serve the Gemini API.
    s.default_models.openai = model("claude");
    s.default_models.gemini = model("gpt");
    let errs = messages(
        store
            .upsert_environment(s.clone(), admin)
            .await
            .unwrap_err(),
    );
    let fields: Vec<&str> = errs.iter().map(|(f, _)| f.as_str()).collect();
    assert_eq!(fields, ["default_models.openai", "default_models.gemini"]);

    // Not listed.
    let mut s = spec("team", &["claude"]);
    s.default_models.anthropic = model("gpt");
    s.small_model = model("gem");
    let errs = messages(store.upsert_environment(s, admin).await.unwrap_err());
    let fields: Vec<&str> = errs.iter().map(|(f, _)| f.as_str()).collect();
    assert_eq!(fields, ["default_models.anthropic", "small_model"]);

    // Messages reaches every kind; a model named as a default can't be
    // deleted while it is listed.
    let mut s = spec("team", &["claude", "gpt", "gem"]);
    s.default_models.anthropic = model("gpt");
    s.default_models.openai = model("gpt");
    s.default_models.gemini = model("gem");
    s.small_model = model("claude");
    let env = store.upsert_environment(s.clone(), admin).await.unwrap();
    assert_eq!(env.spec.default_models, s.default_models);
    assert_eq!(env.spec.small_model, s.small_model);
    assert!(matches!(
        store.delete_model(&ModelName::parse("gpt").unwrap()).await,
        Err(StoreError::InUse(_))
    ));
}

#[tokio::test]
async fn effective_defaults_fall_back_to_the_first_reachable_model() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    provider_with_models(
        &store,
        &sb,
        "anthropic",
        ProviderKind::Anthropic,
        &["claude"],
    )
    .await;
    provider_with_models(&store, &sb, "openai", ProviderKind::OpenAi, &["gpt"]).await;
    let model = |m: &str| Some(ModelName::parse(m).unwrap());

    let env = store
        .upsert_environment(spec("team", &["claude", "gpt"]), admin)
        .await
        .unwrap();
    let d = &env.effective_defaults;
    assert_eq!(
        (&d.anthropic, &d.openai, &d.gemini, &d.small),
        (&model("claude"), &model("gpt"), &None, &None)
    );

    let mut s = spec("team", &["claude", "gpt"]);
    s.default_models.anthropic = model("gpt");
    s.small_model = model("gpt");
    store.upsert_environment(s, admin).await.unwrap();
    let env = store.get_environment(&env_name("team")).await.unwrap();
    let d = &env.effective_defaults;
    assert_eq!(
        (&d.anthropic, &d.openai, &d.small),
        (&model("gpt"), &model("gpt"), &model("gpt"))
    );
    let listed = store.list_environments().await.unwrap();
    assert_eq!(listed[0].effective_defaults, env.effective_defaults);
}

#[tokio::test]
async fn labels_are_stored_and_checked() {
    let (store, _) = common::store().await;
    let admin = common::admin(&store).await;
    let labels = Labels::new(
        [("cost-centre", "4711"), ("team", "Data Platform")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    )
    .unwrap();
    let mut s = spec("team", &[]);
    s.labels = labels.clone();
    store.upsert_environment(s, admin).await.unwrap();
    let env = store.get_environment(&env_name("team")).await.unwrap();
    assert_eq!(env.spec.labels, labels);
    // The type refuses bad labels before they reach the store.
    let bad = Labels::new(
        [("sekisho-env".to_owned(), "x".to_owned())]
            .into_iter()
            .collect(),
    );
    assert!(bad.is_err());
}

#[tokio::test]
async fn a_source_with_listed_entries_cannot_be_deleted() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    catalog(&store, &sb, "a", vec![entry(EntryKind::Skill, "pdf")]).await;
    let mut s = spec("team", &[]);
    s.skills = refs(&["pdf"]);
    store.upsert_environment(s, admin).await.unwrap();
    match store.delete_source(&SourceName::parse("a").unwrap()).await {
        Err(StoreError::InUse(m)) => assert!(m.contains("environment team"), "{m}"),
        other => panic!("expected InUse, got {other:?}"),
    }
    store.delete_environment(&env_name("team")).await.unwrap();
    store
        .delete_source(&SourceName::parse("a").unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn the_owner_changes_on_its_own() {
    let (store, _) = common::store().await;
    let admin = common::admin(&store).await;
    let ada = common::person(&store, "ada@example.com", &["eng"]).await;
    store
        .upsert_environment(spec("team", &[]), admin)
        .await
        .unwrap();
    let env = store
        .set_environment_owner(&env_name("team"), ada)
        .await
        .unwrap();
    assert_eq!(env.owner, Some(ada));
    assert_eq!(
        fields(
            store
                .set_environment_owner(&env_name("team"), uuid::Uuid::nil())
                .await
                .unwrap_err()
        ),
        ["owner"]
    );
    assert!(matches!(
        store.set_environment_owner(&env_name("nope"), ada).await,
        Err(StoreError::NotFound)
    ));
}
