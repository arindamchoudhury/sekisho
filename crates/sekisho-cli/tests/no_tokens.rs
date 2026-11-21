//! `sekisho use` and `uninstall` against Sekisho's real server on a
//! loopback port, and Global Constraints' "no long-lived secret in any
//! coding tool's file": every adapter's files, made as a signed-in person
//! and with `SEKISHO_KEY`, hold neither credential. Grows with each adapter
//! task.

mod common;

use std::path::Path;

use common::{Harness, server};
use sekisho_cli::adapters::Tool;
use sekisho_cli::commands::{Context, UninstallArgs, UseArgs, keys_create, uninstall, use_};
use sekisho_cli::config::{Dirs, ServerUrl, Vars};
use sekisho_cli::credentials::{MemoryStore, Store, Tokens};
use sekisho_core::formats::ProviderKind;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, ModelName, ProviderName};
use sekisho_store::{EnvironmentSpec, NewModel, NewProvider};
use sekisho_testkit::people::{person, session_token};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

/// A machine signed in to the harness: its config, project and home
/// folders, and its store holding the session.
struct Machine {
    store: MemoryStore,
    tmp: tempfile::TempDir,
    vars: Vars,
}

impl Machine {
    fn context(&self) -> Context<'_> {
        Context {
            vars: self.vars.clone(),
            dirs: Dirs::at(self.tmp.path().join("config")),
            store: &self.store,
            folder: self.tmp.path().join("project"),
            home: self.tmp.path().join("home"),
            sekisho: "sekisho".into(),
            now: OffsetDateTime::now_utc(),
        }
    }

    fn project(&self) -> std::path::PathBuf {
        self.tmp.path().join("project")
    }
}

async fn machine(h: &Harness, user: Uuid, key: Option<&str>) -> (Machine, String) {
    let (access, _) = session_token(&h.store, &h.sb, &h.base, user).await;
    let store = MemoryStore::default();
    store
        .save(
            ServerUrl::parse(&h.base).unwrap().key(),
            &Tokens {
                access: access.clone(),
                access_expires_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
                refresh: "refresh-never-used".into(),
            },
        )
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("project")).unwrap();
    let config = tmp.path().join("config");
    let mut pairs = vec![
        ("SEKISHO_URL", h.base.clone()),
        ("SEKISHO_CONFIG_DIR", config.to_string_lossy().into_owned()),
    ];
    if let Some(key) = key {
        pairs.push(("SEKISHO_KEY", key.to_owned()));
    }
    let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    (
        Machine {
            store,
            tmp,
            vars: Vars::from_pairs(&refs),
        },
        access,
    )
}

/// The mock model and environment `pay` for `eng`.
async fn pay(h: &Harness, actor: Uuid) {
    h.store
        .create_provider(
            &h.sb,
            NewProvider {
                allow_private_network: true,
                ..NewProvider::keyed(
                    ProviderName::parse("mock").unwrap(),
                    ProviderKind::OpenAiCompatible,
                    Some("http://mock:8000/v1".into()),
                    "provider-key-never-written",
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
    let mut spec = EnvironmentSpec::new(EnvName::parse("pay").unwrap());
    spec.access_groups = vec!["github/eng".into()];
    spec.models = vec![ModelName::parse("mock-model").unwrap()];
    spec.owner = Some(actor);
    h.store.upsert_environment(spec, actor).await.unwrap();
}

fn use_pay() -> UseArgs {
    UseArgs {
        env: "pay".into(),
        targets: vec![Tool::ClaudeCode],
        ..UseArgs::default()
    }
}

/// Every file under `dir`, as text.
fn every_file(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(every_file(&path));
        } else {
            out.push((
                path.display().to_string(),
                String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned(),
            ));
        }
    }
    out
}

#[tokio::test]
async fn use_installs_from_the_server_and_uninstall_removes_it() {
    let h = server().await;
    let ada = person(&h.store, "ada@example.com", &["eng"]).await;
    pay(&h, ada).await;
    let (m, _) = machine(&h, ada, None).await;
    let printed = use_(&m.context(), &use_pay(), &|_| false).await.unwrap();
    assert!(
        printed.contains("+ claude-code: .claude/settings.local.json apiKeyHelper"),
        "{printed}"
    );
    assert!(
        printed.contains("Claude Code takes its models from \"pay\"."),
        "{printed}"
    );
    let settings: Value = serde_json::from_slice(
        &std::fs::read(m.project().join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["apiKeyHelper"], "sekisho token");
    assert_eq!(settings["model"], "mock-model");
    assert_eq!(
        settings["env"]["ANTHROPIC_BASE_URL"],
        format!("{}/env/pay/anthropic", h.base)
    );
    let again = use_(&m.context(), &use_pay(), &|_| false).await.unwrap();
    assert!(again.contains("Nothing to change"), "{again}");

    let removed = uninstall(&m.context(), &UninstallArgs::default())
        .await
        .unwrap();
    assert!(
        removed.contains("- claude-code: .claude/settings.local.json model"),
        "{removed}"
    );
    assert!(
        every_file(&m.project()).is_empty(),
        "{:?}",
        every_file(&m.project())
    );

    // An environment that isn't installed is an error.
    let err = uninstall(
        &m.context(),
        &UninstallArgs {
            envs: vec!["pay".into()],
            ..UninstallArgs::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("isn't installed"), "{err:?}");
}

#[tokio::test]
async fn no_tool_file_holds_a_token() {
    let h = server().await;
    let ada = person(&h.store, "ada@example.com", &["eng"]).await;
    pay(&h, ada).await;

    // Signed in.
    let (m, access) = machine(&h, ada, None).await;
    use_(&m.context(), &use_pay(), &|_| false).await.unwrap();
    let global = UseArgs {
        global: true,
        ..use_pay()
    };
    use_(&m.context(), &global, &|_| false).await.unwrap();
    let files = every_file(m.tmp.path());
    assert!(files.len() > 2, "{files:?}");
    for (path, text) in &files {
        assert!(!text.contains(&access), "{path} holds the session token");
        assert!(
            !text.contains("refresh-never-used"),
            "{path} holds the refresh token"
        );
        assert!(
            !text.contains("provider-key-never-written"),
            "{path} holds the provider key"
        );
    }

    // With SEKISHO_KEY.
    let key = keys_create(&m.context(), "pay", "ci", None)
        .await
        .unwrap()
        .trim_end()
        .to_owned();
    let (k, _) = machine(&h, ada, Some(&key)).await;
    k.store
        .delete(ServerUrl::parse(&h.base).unwrap().key())
        .unwrap();
    use_(&k.context(), &use_pay(), &|_| false).await.unwrap();
    use_(&k.context(), &global, &|_| false).await.unwrap();
    for (path, text) in every_file(k.tmp.path()) {
        assert!(!text.contains(&key), "{path} holds the environment key");
    }
}
