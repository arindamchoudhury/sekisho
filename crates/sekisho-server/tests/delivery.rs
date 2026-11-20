//! What the server delivers to machines (plan 4, Task 2): the delivery
//! document, its files by SHA-256, the caller's spend, install-wide package
//! registries and reference checks, for people and for environment keys.
//! Catalog entries are recorded straight into the store, as a sync would.

mod common;

use common::{Reply, TestApp, app};
use sekisho_catalog::model::{
    CheckedEntry, EntrySpec, PluginSpec, SkillSpec, ToolkitServer, ToolkitSpec,
};
use sekisho_catalog::names::{EntryKind, EntryName, EntryRef, SourceKind, SourceName};
use sekisho_catalog::source::{Folder, SourceUrl};
use sekisho_catalog::tree::TreeFile;
use sekisho_core::money::MicroUsd;
use sekisho_store::{NewSource, SyncFound, SyncOutcome, SyncRecord};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

const PDF_MD: &str = "---\nname: pdf\ndescription: Reads PDFs.\n---\nRead PDFs.\n";
const RUN_SH: &str = "#!/bin/sh\necho pdf\n";

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn file(path: &str, bytes: &str, executable: bool) -> TreeFile {
    TreeFile {
        path: path.into(),
        bytes: bytes.as_bytes().to_vec(),
        executable,
    }
}

fn skill(name: &str, files: Vec<TreeFile>) -> CheckedEntry {
    CheckedEntry {
        kind: EntryKind::Skill,
        name: EntryName::parse(name).unwrap(),
        path: name.into(),
        spec: EntrySpec::Skill(SkillSpec {
            description: format!("The {name} skill."),
            license: None,
            compatibility: None,
        }),
        files,
    }
}

fn plugin(name: &str) -> CheckedEntry {
    CheckedEntry {
        kind: EntryKind::Plugin,
        name: EntryName::parse(name).unwrap(),
        path: name.into(),
        spec: EntrySpec::Plugin(PluginSpec {
            description: "Formats code.".into(),
            version: Some("1.0.0".into()),
            mcp_servers: Vec::new(),
            apm_checked: false,
        }),
        files: vec![file(
            ".claude-plugin/plugin.json",
            &format!("{{\"name\": \"{name}\"}}\n"),
            false,
        )],
    }
}

fn toolkit(name: &str, skills: &[&str]) -> CheckedEntry {
    CheckedEntry {
        kind: EntryKind::Toolkit,
        name: EntryName::parse(name).unwrap(),
        path: format!("{name}.yaml"),
        spec: EntrySpec::Toolkit(ToolkitSpec {
            description: "Shared.".into(),
            mcp_servers: Vec::<ToolkitServer>::new(),
            a2a_agents: Vec::new(),
            skills: skills.iter().map(|s| EntryRef::parse(s).unwrap()).collect(),
            plugins: Vec::new(),
            prompts: Vec::new(),
        }),
        files: Vec::new(),
    }
}

fn pdf(body: &str) -> CheckedEntry {
    skill(
        "pdf",
        vec![
            file("SKILL.md", body, false),
            file("scripts/run.sh", RUN_SH, true),
        ],
    )
}

/// Records a sync of `source` with `entries`; the first applies itself.
async fn sync(app: &TestApp, source: &str, commit: &str, entries: Vec<CheckedEntry>) {
    let now = OffsetDateTime::now_utc();
    app.store
        .record_sync(
            &app.sb,
            &SourceName::parse(source).unwrap(),
            SyncRecord {
                settings_at: None,
                commit: Some(commit.into()),
                started_at: now,
                finished_at: now,
                outcome: SyncOutcome::Ok(SyncFound {
                    entries: entries
                        .into_iter()
                        .map(sekisho_store::FoundEntry::from_checked)
                        .collect(),
                    refused: Vec::new(),
                    warnings: Vec::new(),
                    vmcps: Vec::new(),
                    environments: Value::Null,
                }),
            },
            now + time::Duration::hours(1),
        )
        .await
        .unwrap();
}

async fn add_source(app: &TestApp, name: &str) {
    app.store
        .add_source(
            &app.sb,
            NewSource {
                name: SourceName::parse(name).unwrap(),
                kind: SourceKind::Catalog,
                url: SourceUrl::parse(&format!("https://git.example.com/{name}.git"), false)
                    .unwrap(),
                git_ref: None,
                folder: Folder::parse("").unwrap(),
                username: None,
                token: None,
                allow_private_network: false,
            },
            app.owner,
        )
        .await
        .unwrap();
}

/// Two models on one OpenAI-compatible provider.
async fn models(app: &TestApp) {
    app.provider_and_model().await;
    let m = app
        .admin(
            "POST",
            "/api/v1/models",
            Some(json!({
                "name": "small-model", "provider": "mock", "input_price": "1", "output_price": "2"
            })),
        )
        .await;
    assert_eq!(m.status, 201, "{}", m.text);
}

/// An environment for the group `eng`, owned by the test's admin.
async fn put_env(app: &TestApp, name: &str, body: Value) -> Reply {
    let mut body = body;
    body["access_groups"] = json!(["github/eng"]);
    body["owner"] = json!(app.owner);
    let r = app
        .admin("PUT", &format!("/api/v1/environments/{name}"), Some(body))
        .await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text);
    r
}

fn pay_body() -> Value {
    json!({
        "models": ["mock-model", "small-model"],
        "default_models": {"anthropic": "mock-model"},
        "small_model": "small-model",
        "skills": ["pdf@team"],
        "plugins": ["fmt"],
        "toolkits": ["kit"],
    })
}

/// The catalog and two environments: `pay` lists two models, the pdf
/// skill, the fmt plugin and the kit toolkit (notes and pdf); `ops` lists
/// the lint skill. The other source also has a pdf skill.
async fn world(app: &TestApp) {
    models(app).await;
    add_source(app, "team").await;
    sync(
        app,
        "team",
        "c1",
        vec![
            pdf(PDF_MD),
            skill("notes", vec![file("SKILL.md", "Take notes.\n", false)]),
            skill("lint", vec![file("SKILL.md", "Lint things.\n", false)]),
            plugin("fmt"),
            toolkit("kit", &["notes", "pdf"]),
        ],
    )
    .await;
    add_source(app, "other").await;
    sync(
        app,
        "other",
        "d1",
        vec![skill(
            "pdf",
            vec![file("SKILL.md", "Another pdf.\n", false)],
        )],
    )
    .await;
    put_env(app, "pay", pay_body()).await;
    put_env(
        app,
        "ops",
        json!({"models": ["mock-model"], "skills": ["lint"]}),
    )
    .await;
}

/// A person in `eng`, who may use both environments.
async fn developer(app: &TestApp) -> String {
    app.sign_in("dev@example.com", &["eng"]).await.1
}

async fn delivery(app: &TestApp, token: &str, env: &str) -> Reply {
    app.by(
        token,
        "GET",
        &format!("/api/v1/me/environments/{env}/delivery"),
        None,
    )
    .await
}

async fn revision(app: &TestApp, token: &str) -> String {
    let r = delivery(app, token, "pay").await;
    assert_eq!(r.status, 200, "{}", r.text);
    r.body["revision"].as_str().unwrap().to_owned()
}

fn entry<'a>(doc: &'a Value, kind: &str, name: &str) -> Option<&'a Value> {
    doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == kind && e["name"] == name)
}

async fn quarantine(app: &TestApp, path: &str) {
    let r = app
        .admin(
            "POST",
            &format!("/api/v1/catalog/{path}/quarantine"),
            Some(json!({"reason": "leaks secrets"})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
}

#[tokio::test]
async fn delivery_carries_models_defaults_and_entries() {
    let app = app().await;
    world(&app).await;
    let dev = developer(&app).await;
    let r = delivery(&app, &dev, "pay").await;
    assert_eq!(r.status, 200, "{}", r.text);
    let doc = &r.body;
    assert_eq!(doc["environment"], "pay");
    assert_eq!(doc["notice"], "");
    assert_eq!(doc["gateway_url"], "https://sekisho.test/env/pay");
    let every_openai_api = json!(["messages", "chat_completions", "responses"]);
    assert_eq!(
        doc["models"],
        json!([
            {"name": "mock-model", "apis": every_openai_api},
            {"name": "small-model", "apis": every_openai_api},
        ])
    );
    assert_eq!(
        doc["defaults"],
        json!({"anthropic": "mock-model", "openai": "mock-model", "gemini": null, "small": "small-model"})
    );
    assert_eq!(doc["mcp"], Value::Null);
    assert_eq!(doc["quarantined"], json!([]));
    assert_eq!(doc["registries"], json!({"npm": null, "python": null}));
    let revision = doc["revision"].as_str().unwrap();
    assert_eq!(revision.len(), 64);
    assert!(
        revision
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );

    // Each entry once: its own first, then what only the toolkit brings;
    // never the toolkit.
    let order: Vec<(String, String, String)> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap().to_owned(),
                e["source"].as_str().unwrap().to_owned(),
                e["name"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let owned = |k: &str, s: &str, n: &str| (k.to_owned(), s.to_owned(), n.to_owned());
    assert_eq!(
        order,
        [
            owned("skill", "team", "pdf"),
            owned("plugin", "team", "fmt"),
            owned("skill", "team", "notes"),
        ]
    );
    let pdf = entry(doc, "skill", "pdf").unwrap();
    assert_eq!(pdf["commit"], "c1");
    assert_eq!(pdf["digest"], self::pdf(PDF_MD).digest());
    assert!(
        OffsetDateTime::parse(
            pdf["applied_at"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339
        )
        .is_ok(),
        "{pdf}"
    );
    assert_eq!(pdf["tools"], Value::Null);
    assert_eq!(pdf["spec"]["kind"], "skill");
    assert_eq!(pdf["spec"]["description"], "The pdf skill.");
    assert_eq!(
        pdf["files"],
        json!([
            {"path": "SKILL.md", "sha256": sha256(PDF_MD.as_bytes()), "size": PDF_MD.len(), "executable": false},
            {"path": "scripts/run.sh", "sha256": sha256(RUN_SH.as_bytes()), "size": RUN_SH.len(), "executable": true},
        ])
    );
    assert_eq!(
        entry(doc, "plugin", "fmt").unwrap()["spec"]["version"],
        "1.0.0"
    );

    // The environment's key reads the same document.
    let key = app.owners_key("pay", "ci").await.body["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let by_key = delivery(&app, &key, "pay").await;
    assert_eq!(by_key.status, 200, "{}", by_key.text);
    assert_eq!(by_key.body, r.body);
}

#[tokio::test]
async fn delivery_is_only_for_those_who_may_use_it() {
    let app = app().await;
    world(&app).await;
    let missing = delivery(&app, &app.token, "nope").await;
    assert_eq!(missing.status, 404, "{}", missing.text);

    let (_, outsider) = app.sign_in("sales@example.com", &["sales"]).await;
    let refused = delivery(&app, &outsider, "pay").await;
    assert_eq!(refused.status, 404);
    assert_eq!(refused.body, missing.body);
    let bad_name = delivery(&app, &outsider, "Not_A_Name").await;
    assert_eq!(bad_name.status, 404);
    assert_eq!(bad_name.body, missing.body);

    // Admins get no environment they aren't in the groups of.
    let (_, boss) = app.sign_in("boss@example.com", &["sekisho-admins"]).await;
    assert_eq!(
        app.by(&boss, "GET", "/api/v1/users", None).await.status,
        200
    );
    let as_admin = delivery(&app, &boss, "pay").await;
    assert_eq!(as_admin.status, 404);
    assert_eq!(as_admin.body, missing.body);

    let ops_key = app.owners_key("ops", "ci").await.body["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let other_env = delivery(&app, &ops_key, "pay").await;
    assert_eq!(other_env.status, 404);
    assert_eq!(other_env.body, missing.body);
    assert_eq!(delivery(&app, &ops_key, "ops").await.status, 200);

    let made = app.owners_key("pay", "laptop").await;
    let pay_key = made.body["token"].as_str().unwrap().to_owned();
    assert_eq!(delivery(&app, &pay_key, "pay").await.status, 200);

    // A key reaches only the routes machines read.
    let me = app.by(&pay_key, "GET", "/api/v1/me", None).await;
    assert_eq!(me.status, 401, "{}", me.text);
    assert_eq!(
        app.by(&pay_key, "GET", "/api/v1/environments", None)
            .await
            .status,
        401
    );

    // An inactive key, a wrong secret and no credential at all: 401.
    let id = made.body["id"].as_str().unwrap();
    let r = app
        .admin("POST", &format!("/api/v1/me/keys/{id}/revoke"), None)
        .await;
    assert_eq!(r.status, 204, "{}", r.text);
    let revoked = app
        .until_status(&pay_key, "/api/v1/me/environments/pay/delivery", 401)
        .await;
    assert_eq!(revoked.status, 401, "{}", revoked.text);
    let mut wrong = ops_key.clone();
    let last = wrong.pop().unwrap();
    wrong.push(if last == 'a' { 'b' } else { 'a' });
    assert_eq!(delivery(&app, &wrong, "ops").await.status, 401);
    let none = app
        .call("GET", "/api/v1/me/environments/pay/delivery", None, None)
        .await;
    assert_eq!(none.status, 401);
    let basic = app
        .http
        .get(format!("{}/api/v1/me/environments/pay/delivery", app.base))
        .header("authorization", "Basic dXNlcjpwYXNz")
        .send()
        .await
        .unwrap();
    assert_eq!(basic.status(), 401);
}

async fn blob(app: &TestApp, token: &str, env: &str, sha: &str) -> reqwest::Response {
    app.http
        .get(format!(
            "{}/api/v1/me/environments/{env}/blobs/{sha}",
            app.base
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn blobs_are_only_what_the_environment_delivers() {
    let app = app().await;
    world(&app).await;
    let dev = developer(&app).await;
    let pdf_sha = sha256(PDF_MD.as_bytes());

    let ok = blob(&app, &dev, "pay", &pdf_sha).await;
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.headers()["content-type"], "application/octet-stream");
    assert_eq!(
        ok.headers()["cache-control"],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(ok.bytes().await.unwrap(), PDF_MD.as_bytes());
    // A file only a toolkit brings.
    let notes = blob(&app, &dev, "pay", &sha256(b"Take notes.\n")).await;
    assert_eq!(notes.status(), 200);

    // lint is ops's, and the other source's pdf is listed by no one.
    let lint = sha256(b"Lint things.\n");
    assert_eq!(blob(&app, &dev, "pay", &lint).await.status(), 404);
    assert_eq!(blob(&app, &dev, "ops", &lint).await.status(), 200);
    assert_eq!(
        blob(&app, &dev, "pay", &sha256(b"Another pdf.\n"))
            .await
            .status(),
        404
    );
    assert_eq!(blob(&app, &dev, "pay", &"0".repeat(64)).await.status(), 404);
    assert_eq!(blob(&app, &dev, "pay", "abc").await.status(), 404);
    assert_eq!(
        blob(&app, &dev, "pay", &pdf_sha.to_uppercase())
            .await
            .status(),
        404
    );
    let (_, outsider) = app.sign_in("sales@example.com", &["sales"]).await;
    assert_eq!(blob(&app, &outsider, "pay", &pdf_sha).await.status(), 404);

    quarantine(&app, "skills/team/pdf").await;
    assert_eq!(blob(&app, &dev, "pay", &pdf_sha).await.status(), 404);
    // The run script was only pdf's too.
    assert_eq!(
        blob(&app, &dev, "pay", &sha256(RUN_SH.as_bytes()))
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn quarantined_entries_are_listed_for_removal() {
    let app = app().await;
    world(&app).await;
    let dev = developer(&app).await;
    quarantine(&app, "skills/team/pdf").await;
    let doc = delivery(&app, &dev, "pay").await.body;
    assert!(entry(&doc, "skill", "pdf").is_none(), "{doc}");
    assert!(entry(&doc, "skill", "notes").is_some(), "{doc}");
    assert_eq!(
        doc["quarantined"],
        json!([{"kind": "skill", "source": "team", "name": "pdf"}])
    );

    let lifted = app
        .admin("DELETE", "/api/v1/catalog/skills/team/pdf/quarantine", None)
        .await;
    assert_eq!(lifted.status, 200, "{}", lifted.text);
    let doc = delivery(&app, &dev, "pay").await.body;
    assert!(entry(&doc, "skill", "pdf").is_some(), "{doc}");
    assert_eq!(doc["quarantined"], json!([]));
}

#[tokio::test]
async fn the_revision_changes_when_anything_delivered_changes() {
    let app = app().await;
    world(&app).await;
    let dev = developer(&app).await;
    let first = revision(&app, &dev).await;

    // Nothing delivered changes: another environment, a key, a pending sync.
    assert_eq!(revision(&app, &dev).await, first);
    put_env(
        &app,
        "ops",
        json!({"models": ["small-model"], "skills": ["lint"]}),
    )
    .await;
    app.owners_key("pay", "spare").await;
    let changed = "---\nname: pdf\ndescription: Reads PDFs.\n---\nRead PDFs well.\n";
    sync(
        &app,
        "team",
        "c2",
        vec![
            pdf(changed),
            skill("notes", vec![file("SKILL.md", "Take notes.\n", false)]),
            skill("lint", vec![file("SKILL.md", "Lint things.\n", false)]),
            plugin("fmt"),
            toolkit("kit", &["notes", "pdf"]),
        ],
    )
    .await;
    assert_eq!(revision(&app, &dev).await, first);

    // The apply brings the new pdf.
    let applied = app
        .admin("POST", "/api/v1/catalog-sources/team/apply", None)
        .await;
    assert_eq!(applied.status, 200, "{}", applied.text);
    let after_apply = revision(&app, &dev).await;
    assert_ne!(after_apply, first);

    // A model added.
    let m = app
        .admin(
            "POST",
            "/api/v1/models",
            Some(json!({
                "name": "extra-model", "provider": "mock", "input_price": "1", "output_price": "2"
            })),
        )
        .await;
    assert_eq!(m.status, 201, "{}", m.text);
    assert_eq!(revision(&app, &dev).await, after_apply);
    let mut body = pay_body();
    body["models"] = json!(["mock-model", "small-model", "extra-model"]);
    put_env(&app, "pay", body).await;
    let after_default = revision(&app, &dev).await;
    assert_ne!(after_default, after_apply);

    quarantine(&app, "plugins/team/fmt").await;
    let after_quarantine = revision(&app, &dev).await;
    assert_ne!(after_quarantine, after_default);

    let r = app
        .admin(
            "PUT",
            "/api/v1/package-registries",
            Some(json!({"npm": "https://npm.example.com/", "python": null})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    let after_registry = revision(&app, &dev).await;
    assert_ne!(after_registry, after_quarantine);
    assert_eq!(revision(&app, &dev).await, after_registry);
}

#[tokio::test]
async fn package_registries_are_https_and_reach_the_delivery() {
    let app = app().await;
    world(&app).await;
    let dev = developer(&app).await;
    let none = app.admin("GET", "/api/v1/package-registries", None).await;
    assert_eq!(none.status, 200, "{}", none.text);
    assert_eq!(none.body, json!({"npm": null, "python": null}));

    for (npm, python, field) in [
        (
            "http://npm.example.com/",
            "https://pypi.example.com/simple",
            "npm",
        ),
        (
            "https://npm.example.com/",
            "https://me:pw@pypi.example.com/simple",
            "python",
        ),
        (
            "https://npm.example.com/?token=x",
            "https://pypi.example.com/simple",
            "npm",
        ),
        ("not a url", "https://pypi.example.com/simple", "npm"),
    ] {
        let r = app
            .admin(
                "PUT",
                "/api/v1/package-registries",
                Some(json!({"npm": npm, "python": python})),
            )
            .await;
        assert_eq!(r.status, 422, "{npm} {python}: {}", r.text);
        assert_eq!(r.body["fields"][0]["field"], field, "{}", r.text);
        // The refused value isn't echoed: it may hold a password.
        assert!(!r.text.contains("pw@"), "{}", r.text);
    }

    let set =
        json!({"npm": "https://npm.example.com/", "python": "https://pypi.example.com/simple"});
    let r = app
        .admin("PUT", "/api/v1/package-registries", Some(set.clone()))
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.body, set);
    assert_eq!(
        app.admin("GET", "/api/v1/package-registries", None)
            .await
            .body,
        set
    );
    assert_eq!(delivery(&app, &dev, "pay").await.body["registries"], set);
    assert_eq!(
        app.by(&dev, "PUT", "/api/v1/package-registries", Some(set.clone()))
            .await
            .status,
        403
    );
    assert_eq!(
        app.by(&dev, "GET", "/api/v1/package-registries", None)
            .await
            .status,
        403
    );

    let cleared = json!({"npm": null, "python": null});
    let r = app
        .admin("PUT", "/api/v1/package-registries", Some(cleared.clone()))
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(
        delivery(&app, &dev, "pay").await.body["registries"],
        cleared
    );
}

#[tokio::test]
async fn references_are_checked_for_anyone_signed_in() {
    let app = app().await;
    world(&app).await;
    let dev = developer(&app).await;
    let body = json!({"references": [
        {"kind": "skill", "reference": "lint"},
        {"kind": "skill", "reference": "nope"},
        {"kind": "skill", "reference": "pdf"},
        {"kind": "skill", "reference": "pdf@other"},
        {"kind": "plugin", "reference": "lint"},
        {"kind": "skill", "reference": "Not A Name"},
    ]});
    let r = app
        .by(&dev, "POST", "/api/v1/check/references", Some(body.clone()))
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    let answers = r.body.as_array().unwrap();
    assert_eq!(answers.len(), 6, "{}", r.text);
    assert_eq!(
        answers[0],
        json!({"kind": "skill", "reference": "lint", "source": "team", "name": "lint"})
    );
    assert_eq!(answers[1]["reference"], "nope");
    assert_eq!(answers[1]["error"], "no such entry");
    let ambiguous = answers[2]["error"].as_str().unwrap();
    assert!(
        ambiguous.contains("other") && ambiguous.contains("team"),
        "{ambiguous}"
    );
    assert_eq!(answers[2].get("source"), None);
    assert_eq!(answers[3]["source"], "other");
    assert_eq!(answers[3]["name"], "pdf");
    assert_eq!(answers[4]["error"], "no such entry");
    assert!(answers[5]["error"].as_str().is_some(), "{}", answers[5]);

    let key = app.owners_key("ops", "ci").await.body["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let by_key = app
        .by(&key, "POST", "/api/v1/check/references", Some(body.clone()))
        .await;
    assert_eq!(by_key.status, 200, "{}", by_key.text);
    assert_eq!(by_key.body, r.body);

    let anonymous = app
        .call("POST", "/api/v1/check/references", None, Some(body))
        .await;
    assert_eq!(anonymous.status, 401);
    let bad_kind = app
        .by(
            &dev,
            "POST",
            "/api/v1/check/references",
            Some(json!({"references": [{"kind": "model", "reference": "x"}]})),
        )
        .await;
    assert_eq!(bad_kind.status, 400, "{}", bad_kind.text);
}

async fn budget(app: &TestApp, body: Value) -> Uuid {
    let mut body = body;
    body["window"] = json!("month");
    let r = app.admin("POST", "/api/v1/budgets", Some(body)).await;
    assert_eq!(r.status, 201, "{}", r.text);
    r.body["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn people_see_the_spend_that_counts_them() {
    let app = app().await;
    world(&app).await;
    let (dev_id, dev) = app.sign_in("dev@example.com", &["eng"]).await;
    let (other_id, _) = app.sign_in("other@example.com", &["eng"]).await;
    let made = app
        .by(
            &dev,
            "POST",
            "/api/v1/me/keys",
            Some(json!({"environment": "pay", "name": "laptop"})),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.text);
    let key_id = made.body["id"].as_str().unwrap().to_owned();
    let key = made.body["token"].as_str().unwrap().to_owned();
    let (_, sales) = app.sign_in("sales@example.com", &["sales"]).await;
    put_env(&app, "solo", json!({"models": ["mock-model"]})).await;
    let r = app
        .admin(
            "PUT",
            "/api/v1/environments/solo",
            Some(json!({"models": ["mock-model"], "access_groups": ["github/sales"], "owner": app.owner})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    let _ = sales;

    let pay = budget(
        &app,
        json!({"name": "pay-team", "scope": "environment", "environment": "pay", "limit": "100"}),
    )
    .await;
    let mine = budget(
        &app,
        json!({"name": "dev-all", "scope": "person", "person": dev_id, "limit": "20"}),
    )
    .await;
    let mine_in_ops = budget(&app, json!({"name": "dev-ops", "scope": "person", "person": dev_id, "environment": "ops", "limit": "5"})).await;
    let laptop = budget(
        &app,
        json!({"name": "laptop", "scope": "key", "key": key_id, "limit": "2.5"}),
    )
    .await;
    budget(
        &app,
        json!({"name": "someone-else", "scope": "person", "person": other_id, "limit": "9"}),
    )
    .await;
    budget(
        &app,
        json!({"name": "solo-team", "scope": "environment", "environment": "solo", "limit": "9"}),
    )
    .await;
    app.store
        .charge(
            &[pay, mine, laptop],
            MicroUsd(1_250_000),
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    let names = |r: &Reply| -> Vec<String> {
        let mut v: Vec<String> = r
            .body
            .as_array()
            .unwrap_or_else(|| panic!("{}", r.text))
            .iter()
            .map(|b| b["name"].as_str().unwrap().to_owned())
            .collect();
        v.sort();
        v
    };
    let as_person = app.by(&dev, "GET", "/api/v1/me/spend", None).await;
    assert_eq!(as_person.status, 200, "{}", as_person.text);
    assert_eq!(
        names(&as_person),
        ["dev-all", "dev-ops", "laptop", "pay-team"]
    );
    let pay_team = as_person
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "pay-team")
        .unwrap();
    assert_eq!(pay_team["scope"], "environment");
    assert_eq!(pay_team["environment"], "pay");
    assert_eq!(pay_team["limit_usd"], "100");
    assert_eq!(pay_team["spent_usd"], "1.25");
    assert!(pay_team["resets_at"].as_str().is_some(), "{pay_team}");
    let _ = mine_in_ops;

    let as_key = app.by(&key, "GET", "/api/v1/me/spend", None).await;
    assert_eq!(as_key.status, 200, "{}", as_key.text);
    assert_eq!(names(&as_key), ["laptop", "pay-team"]);
    let laptop_body = as_key
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "laptop")
        .unwrap();
    assert_eq!(laptop_body["scope"], "key");
    assert_eq!(laptop_body["key"], key_id.as_str());
    assert_eq!(laptop_body["spent_usd"], "1.25");
    assert_eq!(laptop_body["limit_usd"], "2.5");

    assert_eq!(
        app.call("GET", "/api/v1/me/spend", None, None).await.status,
        401
    );
}
