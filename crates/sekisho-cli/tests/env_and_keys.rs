//! `sekisho env list` and `sekisho keys` against Sekisho's real server on a
//! loopback port, signed in through a session in a memory store.

mod common;

use std::path::Path;

use common::{Harness, server};
use sekisho_cli::commands::{Context, KeyAction, env_list, keys_act, keys_create, keys_list};
use sekisho_cli::config::{Dirs, ServerUrl, Vars};
use sekisho_cli::credentials::{MemoryStore, Store, Tokens};
use sekisho_cli::output::Exit;
use sekisho_core::formats::ProviderKind;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_store::{EnvironmentSpec, NewModel, NewProvider};
use sekisho_testkit::people::{person, session_token};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// A person signed in to the harness: the folders the CLI uses and its
/// store, holding their session.
struct Signed {
    store: MemoryStore,
    config: tempfile::TempDir,
    folder: tempfile::TempDir,
    vars: Vars,
}

impl Signed {
    fn context(&self) -> Context<'_> {
        Context {
            vars: self.vars.clone(),
            dirs: Dirs::at(self.config.path().to_owned()),
            store: &self.store,
            folder: self.folder.path().to_owned(),
            home: self.folder.path().join("home"),
            sekisho: "sekisho".into(),
            now: OffsetDateTime::now_utc(),
        }
    }
}

async fn sign_in(h: &Harness, email: &str, groups: &[&str]) -> (Uuid, Signed) {
    let user = person(&h.store, email, groups).await;
    let (access, _) = session_token(&h.store, &h.sb, &h.base, user).await;
    let store = MemoryStore::default();
    store
        .save(
            ServerUrl::parse(&h.base).unwrap().key(),
            &Tokens {
                access,
                access_expires_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
                refresh: "never-used".into(),
            },
        )
        .unwrap();
    let config = tempfile::tempdir().unwrap();
    let vars = Vars::from_pairs(&[
        ("SEKISHO_URL", &h.base),
        ("SEKISHO_CONFIG_DIR", config.path().to_str().unwrap()),
    ]);
    (
        user,
        Signed {
            store,
            config,
            folder: tempfile::tempdir().unwrap(),
            vars,
        },
    )
}

/// One model, and environments: `pay` and `ops` for `eng`, `hr` for
/// `sales`.
async fn environments(h: &Harness, actor: Uuid) {
    h.store
        .create_provider(
            &h.sb,
            NewProvider {
                allow_private_network: true,
                ..NewProvider::keyed(
                    ProviderName::parse("mock").unwrap(),
                    ProviderKind::OpenAiCompatible,
                    Some("http://mock:8000/v1".into()),
                    "k",
                )
            },
        )
        .await
        .unwrap();
    h.store
        .create_model(NewModel {
            name: ModelName::parse("mock-model").unwrap(),
            provider: ProviderName::parse("mock").unwrap(),
            input_price: Some(MicroUsd(1_000_000)),
            output_price: Some(MicroUsd(2_000_000)),
            cache_read_price: None,
            cache_write_price: None,
        })
        .await
        .unwrap();
    for (name, description, group, models) in [
        ("pay", "Payments", "github/eng", vec!["mock-model"]),
        ("ops", "", "github/eng", vec![]),
        ("hr", "People", "github/sales", vec!["mock-model"]),
    ] {
        let mut spec = EnvironmentSpec::new(EnvName::parse(name).unwrap());
        spec.description = description.into();
        spec.access_groups = vec![group.into()];
        spec.models = models
            .into_iter()
            .map(|m| ModelName::parse(m).unwrap())
            .collect();
        spec.owner = Some(actor);
        h.store.upsert_environment(spec, actor).await.unwrap();
    }
}

fn write_lock(folder: &Path, lock: Value) {
    std::fs::create_dir_all(folder.join(".sekisho")).unwrap();
    std::fs::write(
        folder.join(".sekisho/lock.json"),
        serde_json::to_vec_pretty(&lock).unwrap(),
    )
    .unwrap();
}

/// Every file under `dir`, as text.
fn every_file(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(every_file(&path));
        } else {
            out.push(String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned());
        }
    }
    out
}

#[tokio::test]
async fn env_list_shows_what_i_may_use() {
    let h = server().await;
    let (ada, signed) = sign_in(&h, "ada@example.com", &["eng"]).await;
    environments(&h, ada).await;

    let json_text = env_list(&signed.context(), true).await.unwrap();
    let listed: Value = serde_json::from_str(&json_text).unwrap();
    assert_eq!(
        listed,
        json!([
            {"name": "ops", "description": "", "notice": "", "models": [], "installed": false, "model_tools": []},
            {"name": "pay", "description": "Payments", "notice": "", "models": ["mock-model"], "installed": false, "model_tools": []},
        ])
    );

    // Installed in this folder, with the tools that take their models from it.
    write_lock(
        signed.folder.path(),
        json!({
            "version": 1,
            "environments": {"pay": {"revision": "r"}},
            "models": {"codex": "pay", "claude-code": "pay"},
        }),
    );
    let listed: Value =
        serde_json::from_str(&env_list(&signed.context(), true).await.unwrap()).unwrap();
    assert_eq!(listed[1]["name"], "pay");
    assert_eq!(listed[1]["installed"], true);
    assert_eq!(listed[1]["model_tools"], json!(["claude-code", "codex"]));
    assert_eq!(listed[0]["installed"], false);

    let table = env_list(&signed.context(), false).await.unwrap();
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), 3, "{table}");
    assert!(
        lines[0].contains("NAME") && lines[0].contains("MODELS"),
        "{table}"
    );
    assert!(lines[1].starts_with("  ops"), "{table}");
    assert!(lines[2].starts_with("* pay"), "{table}");
    assert!(lines[2].contains("mock-model"), "{table}");
    assert!(lines[2].contains("claude-code, codex"), "{table}");
    assert!(lines[2].contains("Payments"), "{table}");
    assert!(!table.contains("hr"), "{table}");

    // A lock that isn't Sekisho's: an error naming the file.
    std::fs::write(signed.folder.path().join(".sekisho/lock.json"), b"{nope").unwrap();
    let e = env_list(&signed.context(), false).await.unwrap_err();
    assert!(e.message.contains("lock.json"), "{e:?}");

    // Environment keys can't list environments: a person must sign in.
    std::fs::remove_file(signed.folder.path().join(".sekisho/lock.json")).unwrap();
    let mut keyed = signed.context();
    keyed.vars = Vars::from_pairs(&[
        ("SEKISHO_URL", &h.base),
        ("SEKISHO_KEY", "sek_abcdefghijkl_secret"),
    ]);
    let e = env_list(&keyed, false).await.unwrap_err();
    assert_eq!(e.exit, Exit::NotSignedIn, "{e:?}");
}

#[tokio::test]
async fn a_key_is_shown_once_and_never_stored() {
    let h = server().await;
    let (ada, signed) = sign_in(&h, "ada@example.com", &["eng"]).await;
    environments(&h, ada).await;
    let before = h.store.list_user_keys(&ada).await.unwrap().len();

    let printed = keys_create(&signed.context(), "pay", "ci", None)
        .await
        .unwrap();
    let token = printed.trim_end_matches('\n');
    assert!(token.starts_with("sek_"), "{printed:?}");
    assert_eq!(printed.lines().count(), 1, "only the token on stdout");
    assert_eq!(
        h.store.list_user_keys(&ada).await.unwrap().len(),
        before + 1
    );

    // Nowhere on this machine, and never answered again.
    for text in every_file(signed.config.path())
        .into_iter()
        .chain(every_file(signed.folder.path()))
    {
        assert!(!text.contains(token), "a file holds the token");
    }
    let origin = ServerUrl::parse(&h.base).unwrap();
    let stored = signed.store.load(origin.key()).unwrap().unwrap();
    assert!(stored.access != token && stored.refresh != token);
    for json in [false, true] {
        let listed = keys_list(&signed.context(), None, json).await.unwrap();
        assert!(listed.contains("ci"), "{listed}");
        assert!(!listed.contains(token), "{listed}");
        let secret = token.rsplit('_').next().unwrap();
        assert!(!listed.contains(secret), "{listed}");
    }
    let listed: Value =
        serde_json::from_str(&keys_list(&signed.context(), None, true).await.unwrap()).unwrap();
    assert_eq!(listed[0]["name"], "ci");
    assert_eq!(listed[0]["environment"], "pay");
    assert_eq!(listed[0]["state"], "active");

    // The key works, then is revoked by name.
    keys_act(&signed.context(), KeyAction::Suspend, "ci", None)
        .await
        .unwrap();
    let listed: Value =
        serde_json::from_str(&keys_list(&signed.context(), None, true).await.unwrap()).unwrap();
    assert_eq!(listed[0]["state"], "suspended");
    keys_act(&signed.context(), KeyAction::Resume, "ci", None)
        .await
        .unwrap();
    let id = listed[0]["id"].as_str().unwrap().to_owned();
    keys_act(&signed.context(), KeyAction::Revoke, &id, None)
        .await
        .unwrap();
    let listed: Value =
        serde_json::from_str(&keys_list(&signed.context(), None, true).await.unwrap()).unwrap();
    assert_eq!(listed[0]["state"], "revoked");
    // A revoked key's name no longer names a key to act on.
    let e = keys_act(&signed.context(), KeyAction::Suspend, "ci", None)
        .await
        .unwrap_err();
    assert_eq!(e.exit, Exit::Error, "{e:?}");

    // An environment I can't use, and one that doesn't exist: the same refusal.
    let hr = keys_create(&signed.context(), "hr", "ci", None)
        .await
        .unwrap_err();
    let nope = keys_create(&signed.context(), "nope", "ci", None)
        .await
        .unwrap_err();
    assert_eq!(
        hr.message.replace("hr", "X"),
        nope.message.replace("nope", "X")
    );
}

#[tokio::test]
async fn a_key_name_must_be_unique() {
    let h = server().await;
    let (ada, signed) = sign_in(&h, "ada@example.com", &["eng"]).await;
    environments(&h, ada).await;
    keys_create(&signed.context(), "pay", "laptop", None)
        .await
        .unwrap();
    let again = keys_create(&signed.context(), "pay", "laptop", None)
        .await
        .unwrap_err();
    assert!(again.message.contains("laptop"), "{again:?}");
    // The same name in another environment is another key.
    keys_create(&signed.context(), "ops", "laptop", None)
        .await
        .unwrap();
    let listed = keys_list(&signed.context(), Some("ops"), true)
        .await
        .unwrap();
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed[0]["environment"], "ops");

    // A name two environments share needs --env.
    let e = keys_act(&signed.context(), KeyAction::Suspend, "laptop", None)
        .await
        .unwrap_err();
    assert_eq!(e.exit, Exit::Usage, "{e:?}");
    assert!(e.hint.as_deref().unwrap_or("").contains("--env"), "{e:?}");
    keys_act(&signed.context(), KeyAction::Suspend, "laptop", Some("ops"))
        .await
        .unwrap();
    let listed: Value =
        serde_json::from_str(&keys_list(&signed.context(), None, true).await.unwrap()).unwrap();
    let states: Vec<(String, String)> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|k| {
            (
                k["environment"].as_str().unwrap().to_owned(),
                k["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        states.contains(&("ops".into(), "suspended".into())),
        "{states:?}"
    );
    assert!(
        states.contains(&("pay".into(), "active".into())),
        "{states:?}"
    );
    // No such key.
    let e = keys_act(&signed.context(), KeyAction::Revoke, "desktop", None)
        .await
        .unwrap_err();
    assert_eq!(e.exit, Exit::Error, "{e:?}");
}

#[tokio::test]
async fn expiry_accepts_days_or_a_date() {
    let h = server().await;
    let (ada, signed) = sign_in(&h, "ada@example.com", &["eng"]).await;
    environments(&h, ada).await;
    let ctx = signed.context();

    keys_create(&ctx, "pay", "month", Some("30d"))
        .await
        .unwrap();
    keys_create(&ctx, "pay", "dated", Some("2099-03-01T12:00:00Z"))
        .await
        .unwrap();
    keys_create(&ctx, "pay", "day", Some("2099-04-01"))
        .await
        .unwrap();
    let listed: Value = serde_json::from_str(&keys_list(&ctx, None, true).await.unwrap()).unwrap();
    let expiry = |name: &str| -> OffsetDateTime {
        let key = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == name)
            .unwrap();
        OffsetDateTime::parse(key["expires_at"].as_str().unwrap(), &Rfc3339).unwrap()
    };
    let in_30 = ctx.now + time::Duration::days(30);
    assert!((expiry("month") - in_30).abs() < time::Duration::minutes(1));
    assert_eq!(
        expiry("dated"),
        OffsetDateTime::parse("2099-03-01T12:00:00Z", &Rfc3339).unwrap()
    );
    assert_eq!(
        expiry("day"),
        OffsetDateTime::parse("2099-04-01T00:00:00Z", &Rfc3339).unwrap()
    );

    for bad in [
        "0d",
        "-3d",
        "30",
        "30 days",
        "tomorrow",
        "2099-13-01",
        "2000-01-01",
    ] {
        let e = keys_create(&ctx, "pay", "bad", Some(bad))
            .await
            .unwrap_err();
        assert_eq!(e.exit, Exit::Usage, "{bad}: {e:?}");
    }
    let names: Vec<String> =
        serde_json::from_str::<Value>(&keys_list(&ctx, None, true).await.unwrap())
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["name"].as_str().unwrap().to_owned())
            .collect();
    assert!(!names.contains(&"bad".to_owned()), "{names:?}");
}
