//! The catalog API (plan 3, Task 7): sources, syncs, apply, Obot import,
//! entries and their files, owners, quarantine and registry credentials,
//! against a real smart-HTTP Git server on 127.0.0.1.

mod common;

use std::time::Duration;

use axum::Router;
use common::{Reply, TestApp, app};
use sekisho_catalog::names::{SourceKind, SourceName};
use sekisho_catalog::source::{Folder, SourceUrl};
use sekisho_store::NewSource;
use sekisho_testkit::git_http::{self, Repo};
use serde_json::{Value, json};

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct GitServer {
    root: tempfile::TempDir,
    base: String,
}

impl GitServer {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = serve(git_http::serve(root.path().to_path_buf())).await;
        Self { root, base }
    }

    fn repo(&self, name: &str) -> Repo {
        Repo::create(self.root.path(), name)
    }

    fn url(&self, name: &str) -> String {
        format!("{}/git/{name}.git", self.base)
    }
}

fn skill_md(name: &str, text: &str) -> Vec<u8> {
    format!("---\nname: {name}\ndescription: {text}\n---\n{text}\n").into_bytes()
}

/// Adds a skills source through the API; its first sync starts at once.
async fn add_skills(app: &TestApp, name: &str, url: &str) -> Reply {
    app.admin(
        "POST",
        "/api/v1/catalog-sources",
        Some(json!({
            "name": name, "kind": "skills", "url": url, "allow_private_network": true
        })),
    )
    .await
}

/// The latest sync's report once `done` holds for it.
async fn latest_sync(app: &TestApp, source: &str, done: impl Fn(&Value) -> bool) -> Value {
    let path = format!("/api/v1/catalog-sources/{source}/syncs/latest");
    for _ in 0..300 {
        let reply = app.admin("GET", &path, None).await;
        if reply.status == 200 && done(&reply.body) {
            return reply.body;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no matching sync of {source}");
}

/// A source with its pdf skill synced and applied.
async fn synced_team(app: &TestApp, git: &GitServer, files: &[(&str, &[u8])]) -> String {
    let commit = git.repo("team").commit(files, "first");
    assert_eq!(add_skills(app, "team", &git.url("team")).await.status, 201);
    let report = latest_sync(app, "team", |_| true).await;
    assert_eq!(report["outcome"], "ok", "{report}");
    commit
}

fn field_names(reply: &Reply) -> Vec<String> {
    let mut v: Vec<String> = reply.body["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("no fields in {}", reply.text))
        .iter()
        .map(|f| f["field"].as_str().unwrap().to_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn an_admin_adds_a_source_and_sees_its_entries() {
    let app = app().await;
    let git = GitServer::start().await;
    let commit = git.repo("team").commit(
        &[("pdf/SKILL.md", &skill_md("pdf", "Reads PDFs."))],
        "first",
    );
    let added = add_skills(&app, "team", &git.url("team")).await;
    assert_eq!(added.status, 201, "{}", added.text);
    assert_eq!(added.body["name"], "team");
    assert_eq!(added.body["kind"], "skills");
    assert_eq!(added.body["url"], git.url("team"));
    assert_eq!(added.body["ref"], Value::Null);
    assert_eq!(added.body["folder"], "");
    assert_eq!(added.body["has_token"], false);
    assert_eq!(added.body["added_by"], app.owner.to_string());

    let report = latest_sync(&app, "team", |_| true).await;
    assert_eq!(report["outcome"], "ok", "{report}");
    assert_eq!(report["commit"], commit.as_str());
    assert_eq!(report["applied"], true);
    assert_eq!(report["new"], json!([{"kind": "skill", "name": "pdf"}]));

    let skills = app.admin("GET", "/api/v1/catalog/skills", None).await;
    assert_eq!(skills.status, 200, "{}", skills.text);
    let list = skills.body.as_array().unwrap();
    assert_eq!(list.len(), 1, "{}", skills.text);
    assert_eq!(list[0]["kind"], "skill");
    assert_eq!(list[0]["source"], "team");
    assert_eq!(list[0]["name"], "pdf");
    assert_eq!(list[0]["owner"], app.owner.to_string());
    assert_eq!(list[0]["applied"]["commit"], commit.as_str());
    assert_eq!(list[0]["pending"], Value::Null);
    assert_eq!(list[0]["quarantine"], Value::Null);
    assert_eq!(list[0]["removed_at"], Value::Null);

    let source = app.admin("GET", "/api/v1/catalog-sources/team", None).await;
    assert_eq!(source.body["applied_commit"], commit.as_str());
    assert_eq!(source.body["latest_sync"]["outcome"], "ok");
    let all = app.admin("GET", "/api/v1/catalog-sources", None).await;
    assert_eq!(all.body.as_array().unwrap().len(), 1);
    assert_eq!(
        app.admin("GET", "/api/v1/catalog/a2a-agents", None)
            .await
            .body,
        json!([])
    );
    assert_eq!(
        app.admin("GET", "/api/v1/catalog/models", None)
            .await
            .status,
        404
    );
    assert_eq!(
        app.admin("GET", "/api/v1/catalog-sources/nope/syncs/latest", None)
            .await
            .status,
        404
    );

    let deleted = app
        .admin("DELETE", "/api/v1/catalog-sources/team", None)
        .await;
    assert_eq!(deleted.status, 204, "{}", deleted.text);
    assert_eq!(
        app.admin("GET", "/api/v1/catalog/skills", None).await.body,
        json!([])
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/catalog-sources/team", None)
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn people_who_are_not_admins_are_forbidden() {
    let app = app().await;
    let (_, token) = app.sign_in("dev@example.com", &["eng"]).await;
    for (method, path, body) in [
        ("GET", "/api/v1/catalog-sources", None),
        (
            "POST",
            "/api/v1/catalog-sources",
            Some(json!({"name": "x", "kind": "skills", "url": "https://h/x.git"})),
        ),
        ("GET", "/api/v1/catalog-sources/x", None),
        ("POST", "/api/v1/catalog-sources/x/sync", None),
        ("POST", "/api/v1/catalog-sources/x/apply", None),
        (
            "POST",
            "/api/v1/catalog-sources/import-obot",
            Some(json!({"urls": []})),
        ),
        ("GET", "/api/v1/catalog/skills", None),
        ("GET", "/api/v1/catalog/skills/x/y", None),
        ("GET", "/api/v1/catalog/skills/x/y/files/SKILL.md", None),
        (
            "POST",
            "/api/v1/catalog/skills/x/y/quarantine",
            Some(json!({"reason": "r"})),
        ),
        ("GET", "/api/v1/registry-credentials", None),
        (
            "PUT",
            "/api/v1/registry-credentials/ghcr.io",
            Some(json!({"username": "u", "password": "p"})),
        ),
    ] {
        let reply = app.by(&token, method, path, body).await;
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.text);
    }
    let reply = app.call("GET", "/api/v1/catalog-sources", None, None).await;
    assert_eq!(reply.status, 401);
}

#[tokio::test]
async fn sources_never_answer_their_token() {
    let app = app().await;
    let token = "tok-SECRET-0123456789";
    let added = app
        .admin(
            "POST",
            "/api/v1/catalog-sources",
            Some(json!({
                "name": "private", "kind": "skills",
                // Nothing listens on port 9: the sync fails.
                "url": "http://127.0.0.1:9/team.git",
                "username": "bot", "token": token, "allow_private_network": true
            })),
        )
        .await;
    assert_eq!(added.status, 201, "{}", added.text);
    assert_eq!(added.body["has_token"], true);
    assert_eq!(added.body["username"], "bot");
    let report = latest_sync(&app, "private", |_| true).await;
    assert_eq!(report["outcome"], "failed", "{report}");
    let patched = app
        .admin(
            "PATCH",
            "/api/v1/catalog-sources/private",
            Some(json!({"folder": "skills"})),
        )
        .await;
    assert_eq!(patched.status, 200, "{}", patched.text);
    assert_eq!(patched.body["folder"], "skills");
    assert_eq!(patched.body["has_token"], true);
    let replies = [
        added,
        app.admin("GET", "/api/v1/catalog-sources/private", None)
            .await,
        app.admin("GET", "/api/v1/catalog-sources", None).await,
        patched,
        app.admin("GET", "/api/v1/catalog-sources/private/syncs/latest", None)
            .await,
    ];
    for reply in replies {
        assert!(!reply.text.contains(token), "{}", reply.text);
        assert!(!reply.text.contains("\"token\""), "{}", reply.text);
    }

    // A new URL needs the token again; clearing it lets the URL move.
    let moved = app
        .admin(
            "PATCH",
            "/api/v1/catalog-sources/private",
            Some(json!({"url": "http://127.0.0.1:9/other.git"})),
        )
        .await;
    assert_eq!(moved.status, 422, "{}", moved.text);
    assert_eq!(field_names(&moved), ["token"]);
    let cleared = app
        .admin(
            "PATCH",
            "/api/v1/catalog-sources/private",
            Some(json!({"url": "http://127.0.0.1:9/other.git", "token": null, "ref": "main"})),
        )
        .await;
    assert_eq!(cleared.status, 200, "{}", cleared.text);
    assert_eq!(cleared.body["has_token"], false);
    assert_eq!(cleared.body["ref"], "main");
    let unknown = app
        .admin(
            "PATCH",
            "/api/v1/catalog-sources/private",
            Some(json!({"kind": "plugins"})),
        )
        .await;
    assert_eq!(unknown.status, 400, "{}", unknown.text);
}

#[tokio::test]
async fn invalid_sources_are_refused_per_field() {
    let app = app().await;
    let reply = app
        .admin(
            "POST",
            "/api/v1/catalog-sources",
            Some(json!({
                "name": "environments", "kind": "nope", "url": "ssh://h/x.git",
                "ref": "--x", "folder": "../a"
            })),
        )
        .await;
    assert_eq!(reply.status, 422, "{}", reply.text);
    assert_eq!(
        field_names(&reply),
        ["folder", "kind", "name", "ref", "url"]
    );
    assert!(!reply.text.contains("ssh://"), "{}", reply.text);

    // Git mode's kind isn't added here.
    let reply = app
        .admin(
            "POST",
            "/api/v1/catalog-sources",
            Some(json!({"name": "envs", "kind": "environments", "url": "https://h/e.git"})),
        )
        .await;
    assert_eq!(reply.status, 422, "{}", reply.text);
    assert_eq!(field_names(&reply), ["kind"]);

    let reply = app
        .admin(
            "POST",
            "/api/v1/catalog-sources",
            Some(json!({"name": "a", "kind": "skills"})),
        )
        .await;
    assert_eq!(reply.status, 422, "{}", reply.text);
    assert_eq!(field_names(&reply), ["url"]);

    let reply = app
        .admin(
            "POST",
            "/api/v1/catalog-sources",
            Some(json!({"name": "a", "kind": "skills", "url": "https://h/a.git", "token": "a b"})),
        )
        .await;
    assert_eq!(reply.status, 422, "{}", reply.text);
    assert_eq!(field_names(&reply), ["token"]);

    // The same name twice.
    let body = json!({"name": "dup", "kind": "skills", "url": "http://127.0.0.1:9/d.git"});
    let first = app
        .admin("POST", "/api/v1/catalog-sources", Some(body.clone()))
        .await;
    assert_eq!(first.status, 201, "{}", first.text);
    let again = app
        .admin("POST", "/api/v1/catalog-sources", Some(body))
        .await;
    assert_eq!(again.status, 409, "{}", again.text);
}

#[tokio::test]
async fn apply_moves_an_update_to_environments() {
    let app = app().await;
    let git = GitServer::start().await;
    let first = synced_team(&app, &git, &[("pdf/SKILL.md", &skill_md("pdf", "v1"))]).await;
    let nothing = app
        .admin("POST", "/api/v1/catalog-sources/team/apply", None)
        .await;
    assert_eq!(nothing.status, 409, "{}", nothing.text);
    assert_eq!(nothing.body["error"], "conflict");

    let second = git
        .repo("team")
        .commit(&[("pdf/SKILL.md", &skill_md("pdf", "v2"))], "second");
    let started = app
        .admin("POST", "/api/v1/catalog-sources/team/sync", None)
        .await;
    assert_eq!(started.status, 202, "{}", started.text);
    let report = latest_sync(&app, "team", |r| r["commit"] == second.as_str()).await;
    assert_eq!(report["outcome"], "ok", "{report}");
    assert_eq!(report["applied"], false);
    assert_eq!(report["updated"], json!([{"kind": "skill", "name": "pdf"}]));

    let entry = app
        .admin("GET", "/api/v1/catalog/skills/team/pdf", None)
        .await;
    assert_eq!(entry.status, 200, "{}", entry.text);
    assert_eq!(entry.body["applied"]["commit"], first.as_str());
    assert_eq!(entry.body["pending"]["commit"], second.as_str());
    let source = app.admin("GET", "/api/v1/catalog-sources/team", None).await;
    assert_eq!(source.body["pending"], true);

    let applied = app
        .admin("POST", "/api/v1/catalog-sources/team/apply", None)
        .await;
    assert_eq!(applied.status, 200, "{}", applied.text);
    assert_eq!(
        applied.body,
        json!({"updated": [{"kind": "skill", "name": "pdf"}], "removed": []})
    );
    let entry = app
        .admin("GET", "/api/v1/catalog/skills/team/pdf", None)
        .await;
    assert_eq!(entry.body["applied"]["commit"], second.as_str());
    assert_eq!(entry.body["pending"], Value::Null);
    let again = app
        .admin("POST", "/api/v1/catalog-sources/team/apply", None)
        .await;
    assert_eq!(again.status, 409, "{}", again.text);
    assert_eq!(
        app.admin("POST", "/api/v1/catalog-sources/nope/apply", None)
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn a_running_sync_conflicts() {
    let app = app().await;
    let name = SourceName::parse("held").unwrap();
    app.store
        .add_source(
            &app.sb,
            NewSource {
                name: name.clone(),
                kind: SourceKind::Skills,
                url: SourceUrl::parse("http://127.0.0.1:9/held.git", true).unwrap(),
                git_ref: None,
                folder: Folder::default(),
                username: None,
                token: None,
                allow_private_network: true,
            },
            app.owner,
        )
        .await
        .unwrap();
    // Another replica holds it.
    assert!(
        app.store
            .claim_source(
                &name,
                time::OffsetDateTime::now_utc(),
                time::Duration::minutes(10)
            )
            .await
            .unwrap()
    );
    let reply = app
        .admin("POST", "/api/v1/catalog-sources/held/sync", None)
        .await;
    assert_eq!(reply.status, 409, "{}", reply.text);
    assert_eq!(reply.body["error"], "conflict");
    let source = app.admin("GET", "/api/v1/catalog-sources/held", None).await;
    assert_eq!(source.body["syncing"], true);
    assert_eq!(
        app.admin("POST", "/api/v1/catalog-sources/nope/sync", None)
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn obot_urls_become_catalog_sources() {
    let app = app().await;
    let reply = app
        .admin(
            "POST",
            "/api/v1/catalog-sources/import-obot",
            Some(json!({"urls": [
                "https://github.com/obot-platform/mcp-catalog",
                "https://github.com/obot-platform/mcp-catalog/release",
                "https://git.example.com/a/b.git/dev",
            ]})),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.text);
    let sources = reply.body["sources"].as_array().unwrap();
    let seen: Vec<(Value, Value, Value, Value)> = sources
        .iter()
        .map(|s| {
            (
                s["name"].clone(),
                s["kind"].clone(),
                s["url"].clone(),
                s["ref"].clone(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (
                json!("obot-mcp-catalog"),
                json!("catalog"),
                json!("https://github.com/obot-platform/mcp-catalog.git"),
                Value::Null
            ),
            (
                json!("obot-mcp-catalog-2"),
                json!("catalog"),
                json!("https://github.com/obot-platform/mcp-catalog.git"),
                json!("release")
            ),
            (
                json!("obot-b"),
                json!("catalog"),
                json!("https://git.example.com/a/b.git"),
                json!("dev")
            ),
        ]
    );
    for s in sources {
        assert_eq!(s["allow_private_network"], false);
    }

    // A name taken by an earlier import gets the next number.
    let again = app
        .admin(
            "POST",
            "/api/v1/catalog-sources/import-obot",
            Some(json!({"urls": ["https://gitlab.com/x/b.git"], "token": "tok-1234"})),
        )
        .await;
    assert_eq!(again.status, 201, "{}", again.text);
    assert_eq!(again.body["sources"][0]["name"], "obot-b-2");
    assert_eq!(again.body["sources"][0]["has_token"], true);

    // One bad URL refuses them all, naming its position.
    let bad = app
        .admin(
            "POST",
            "/api/v1/catalog-sources/import-obot",
            Some(json!({"urls": ["https://github.com/o/fine", "ssh://git@github.com/o/r.git"]})),
        )
        .await;
    assert_eq!(bad.status, 422, "{}", bad.text);
    assert_eq!(field_names(&bad), ["urls[1]"]);
    let empty = app
        .admin(
            "POST",
            "/api/v1/catalog-sources/import-obot",
            Some(json!({"urls": []})),
        )
        .await;
    assert_eq!(empty.status, 422, "{}", empty.text);
    assert_eq!(field_names(&empty), ["urls"]);
    let all = app.admin("GET", "/api/v1/catalog-sources", None).await;
    assert_eq!(all.body.as_array().unwrap().len(), 4, "{}", all.text);
}

#[tokio::test]
async fn entries_show_files_and_serve_them() {
    let app = app().await;
    let git = GitServer::start().await;
    let guide = b"# Guide\n".as_slice();
    synced_team(
        &app,
        &git,
        &[
            ("pdf/SKILL.md", &skill_md("pdf", "Reads PDFs.")),
            ("pdf/docs/guide.md", guide),
        ],
    )
    .await;
    let entry = app
        .admin("GET", "/api/v1/catalog/skills/team/pdf", None)
        .await;
    assert_eq!(entry.status, 200, "{}", entry.text);
    assert_eq!(entry.body["path"], "pdf");
    assert_eq!(entry.body["applied"]["spec"]["kind"], "skill");
    assert_eq!(entry.body["applied"]["spec"]["description"], "Reads PDFs.");
    let files = entry.body["applied"]["files"].as_array().unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["SKILL.md", "docs/guide.md"]);
    assert_eq!(files[1]["size"], guide.len());
    let sha = files[1]["sha256"].as_str().unwrap();
    assert_eq!(sha.len(), 64);
    assert!(entry.body["applied"]["digest"].is_string());
    assert!(
        entry.body["quarantine_history"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let file = app
        .http
        .get(format!(
            "{}/api/v1/catalog/skills/team/pdf/files/docs/guide.md",
            app.base
        ))
        .bearer_auth(&app.token)
        .send()
        .await
        .unwrap();
    assert_eq!(file.status(), 200);
    assert_eq!(file.headers()["content-type"], "application/octet-stream");
    assert_eq!(file.headers()["x-content-type-options"], "nosniff");
    assert_eq!(file.bytes().await.unwrap().as_ref(), guide);
    for missing in [
        "/api/v1/catalog/skills/team/pdf/files/nope.md",
        "/api/v1/catalog/skills/team/nope/files/SKILL.md",
        "/api/v1/catalog/skills/team/nope",
        "/api/v1/catalog/skills/nope/pdf",
        "/api/v1/catalog/skills/team/Bad_Name",
        "/api/v1/catalog/prompts/team/pdf",
    ] {
        assert_eq!(
            app.admin("GET", missing, None).await.status,
            404,
            "{missing}"
        );
    }
}

#[tokio::test]
async fn quarantine_and_lift() {
    let app = app().await;
    let git = GitServer::start().await;
    synced_team(&app, &git, &[("pdf/SKILL.md", &skill_md("pdf", "v1"))]).await;
    let path = "/api/v1/catalog/skills/team/pdf/quarantine";

    let lift = app.admin("DELETE", path, None).await;
    assert_eq!(lift.status, 409, "{}", lift.text);
    let blank = app.admin("POST", path, Some(json!({"reason": "  "}))).await;
    assert_eq!(blank.status, 422, "{}", blank.text);
    assert_eq!(field_names(&blank), ["reason"]);

    let put = app
        .admin("POST", path, Some(json!({"reason": "CVE-2026-1234"})))
        .await;
    assert_eq!(put.status, 200, "{}", put.text);
    let q = &put.body["quarantine"];
    assert_eq!(q["reason"], "CVE-2026-1234");
    assert_eq!(q["by"], app.owner.to_string());
    assert!(q["at"].is_string());
    let again = app
        .admin("POST", path, Some(json!({"reason": "again"})))
        .await;
    assert_eq!(again.status, 409, "{}", again.text);
    let list = app.admin("GET", "/api/v1/catalog/skills", None).await;
    assert_eq!(list.body[0]["quarantine"]["reason"], "CVE-2026-1234");

    let lifted = app.admin("DELETE", path, None).await;
    assert_eq!(lifted.status, 200, "{}", lifted.text);
    assert_eq!(lifted.body["quarantine"], Value::Null);
    let history = lifted.body["quarantine_history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["reason"], "CVE-2026-1234");
    assert_eq!(history[0]["lifted_by"], app.owner.to_string());
    assert_eq!(app.admin("DELETE", path, None).await.status, 409);
    assert_eq!(
        app.admin(
            "POST",
            "/api/v1/catalog/skills/team/nope/quarantine",
            Some(json!({"reason": "r"}))
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn owners_change_and_offboarding_hands_entries_over() {
    let app = app().await;
    let git = GitServer::start().await;
    synced_team(&app, &git, &[("pdf/SKILL.md", &skill_md("pdf", "v1"))]).await;
    let (ada, _) = app.sign_in("ada@example.com", &["eng"]).await;
    let (bob, _) = app.sign_in("bob@example.com", &["eng"]).await;
    let entry = "/api/v1/catalog/skills/team/pdf";

    let patched = app
        .admin("PATCH", entry, Some(json!({"owner": ada.to_string()})))
        .await;
    assert_eq!(patched.status, 200, "{}", patched.text);
    assert_eq!(patched.body["owner"], ada.to_string());
    let nobody = app
        .admin(
            "PATCH",
            entry,
            Some(json!({"owner": uuid::Uuid::nil().to_string()})),
        )
        .await;
    assert_eq!(nobody.status, 422, "{}", nobody.text);
    assert_eq!(field_names(&nobody), ["owner"]);

    let refused = app
        .admin(
            "POST",
            &format!("/api/v1/users/{ada}/offboard"),
            Some(json!({})),
        )
        .await;
    assert_eq!(refused.status, 422, "{}", refused.text);
    assert!(refused.text.contains("skills/team/pdf"), "{}", refused.text);
    let done = app
        .admin(
            "POST",
            &format!("/api/v1/users/{ada}/offboard"),
            Some(json!({"new_owner": bob.to_string()})),
        )
        .await;
    assert_eq!(done.status, 200, "{}", done.text);
    assert_eq!(done.body["handed_over_entries"], json!(["skills/team/pdf"]));
    assert_eq!(
        app.admin("GET", entry, None).await.body["owner"],
        bob.to_string()
    );
}

#[tokio::test]
async fn registry_credentials_never_answer_the_password() {
    let app = app().await;
    let password = "pw-SECRET-0123456789";
    let put = app
        .admin(
            "PUT",
            "/api/v1/registry-credentials/ghcr.io",
            Some(json!({"username": "bot", "password": password})),
        )
        .await;
    assert_eq!(put.status, 200, "{}", put.text);
    assert_eq!(put.body, json!({"host": "ghcr.io", "username": "bot"}));
    let list = app.admin("GET", "/api/v1/registry-credentials", None).await;
    assert_eq!(list.body, json!([{"host": "ghcr.io", "username": "bot"}]));
    for reply in [&put, &list] {
        assert!(!reply.text.contains(password), "{}", reply.text);
    }
    let bad = app
        .admin(
            "PUT",
            "/api/v1/registry-credentials/not%20a%20host",
            Some(json!({"username": "", "password": "p"})),
        )
        .await;
    assert_eq!(bad.status, 422, "{}", bad.text);
    assert_eq!(field_names(&bad), ["host", "username"]);

    let deleted = app
        .admin("DELETE", "/api/v1/registry-credentials/ghcr.io", None)
        .await;
    assert_eq!(deleted.status, 204, "{}", deleted.text);
    assert_eq!(
        app.admin("GET", "/api/v1/registry-credentials", None)
            .await
            .body,
        json!([])
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/registry-credentials/ghcr.io", None)
            .await
            .status,
        404
    );
}

/// A skills source `team` with `pdf`, then a catalog source `kits` with an
/// MCP server `docs` and the toolkit `finops` listing both.
async fn team_and_kits(app: &TestApp, git: &GitServer) {
    synced_team(app, git, &[("pdf/SKILL.md", &skill_md("pdf", "v1"))]).await;
    let catalog = "- apiVersion: sekisho/v1alpha1\n  kind: MCPServer\n  name: docs\n  remote:\n    url: https://mcp.example.com/docs\n- apiVersion: sekisho/v1alpha1\n  kind: Toolkit\n  name: finops\n  description: Costs.\n  mcp_servers:\n    - server: docs\n      tools: [search]\n  skills: [pdf]\n";
    git.repo("kits")
        .commit(&[("catalog.yaml", catalog.as_bytes())], "kits");
    let added = app
        .admin(
            "POST",
            "/api/v1/catalog-sources",
            Some(json!({
                "name": "kits", "kind": "catalog", "url": git.url("kits"),
                "allow_private_network": true
            })),
        )
        .await;
    assert_eq!(added.status, 201, "{}", added.text);
    let report = latest_sync(app, "kits", |_| true).await;
    assert_eq!(report["outcome"], "ok", "{report}");
    assert!(report["refused"].as_array().unwrap().is_empty(), "{report}");
}

#[tokio::test]
async fn toolkits_show_members_and_entries_their_toolkits() {
    let app = app().await;
    let git = GitServer::start().await;
    team_and_kits(&app, &git).await;

    let list = app.admin("GET", "/api/v1/catalog/toolkits", None).await;
    assert_eq!(list.status, 200, "{}", list.text);
    assert_eq!(list.body.as_array().unwrap().len(), 1, "{}", list.text);
    assert_eq!(list.body[0]["kind"], "toolkit");
    assert_eq!(list.body[0]["name"], "finops");

    let kit = app
        .admin("GET", "/api/v1/catalog/toolkits/kits/finops", None)
        .await;
    assert_eq!(kit.status, 200, "{}", kit.text);
    assert_eq!(kit.body["applied"]["spec"]["kind"], "toolkit");
    assert_eq!(
        kit.body["applied"]["members"],
        json!([
            {"kind": "mcp_server", "reference": "docs", "source": "kits", "name": "docs",
             "tools": ["search"], "quarantined": false, "removed": false},
            {"kind": "skill", "reference": "pdf", "source": "team", "name": "pdf",
             "tools": null, "quarantined": false, "removed": false},
        ])
    );
    assert_eq!(kit.body["toolkits"], json!([]));

    let pdf = app
        .admin("GET", "/api/v1/catalog/skills/team/pdf", None)
        .await;
    assert_eq!(pdf.body["toolkits"], json!(["kits/finops"]));
    assert!(pdf.body["applied"].get("members").is_none(), "{}", pdf.text);
    let skills = app.admin("GET", "/api/v1/catalog/skills", None).await;
    assert_eq!(skills.body[0]["toolkits"], json!(["kits/finops"]));

    // The toolkit keeps team's skill: team can't be deleted.
    let deleted = app
        .admin("DELETE", "/api/v1/catalog-sources/team", None)
        .await;
    assert_eq!(deleted.status, 409, "{}", deleted.text);
    assert_eq!(deleted.body["error"], "in_use");
    assert!(
        deleted.text.contains("toolkits/kits/finops"),
        "{}",
        deleted.text
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/catalog-sources/kits", None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/catalog-sources/team", None)
            .await
            .status,
        204
    );
}

#[tokio::test]
async fn a_toolkit_can_be_quarantined() {
    let app = app().await;
    let git = GitServer::start().await;
    team_and_kits(&app, &git).await;
    let path = "/api/v1/catalog/toolkits/kits/finops/quarantine";
    let put = app
        .admin("POST", path, Some(json!({"reason": "Leaks costs."})))
        .await;
    assert_eq!(put.status, 200, "{}", put.text);
    assert_eq!(put.body["quarantine"]["reason"], "Leaks costs.");
    assert_eq!(
        app.admin("POST", path, Some(json!({"reason": "again"})))
            .await
            .status,
        409
    );

    // A quarantined member shows in the toolkit's answer.
    let member = app
        .admin(
            "POST",
            "/api/v1/catalog/skills/team/pdf/quarantine",
            Some(json!({"reason": "Broken."})),
        )
        .await;
    assert_eq!(member.status, 200, "{}", member.text);
    let kit = app
        .admin("GET", "/api/v1/catalog/toolkits/kits/finops", None)
        .await;
    assert_eq!(kit.body["applied"]["members"][1]["quarantined"], true);
    assert_eq!(kit.body["applied"]["members"][0]["quarantined"], false);

    let lifted = app.admin("DELETE", path, None).await;
    assert_eq!(lifted.status, 200, "{}", lifted.text);
    assert_eq!(lifted.body["quarantine"], Value::Null);
}

#[tokio::test]
async fn environments_list_entries_and_keep_their_sources() {
    let app = app().await;
    let git = GitServer::start().await;
    team_and_kits(&app, &git).await;
    let put = app
        .admin(
            "PUT",
            "/api/v1/environments/data",
            Some(json!({
                "access_groups": ["github/eng"],
                "skills": ["pdf"],
                "toolkits": ["finops"],
            })),
        )
        .await;
    assert_eq!(put.status, 200, "{}", put.text);
    assert_eq!(
        put.body["catalog_status"],
        json!([
            {"kind": "skill", "reference": "pdf", "source": "team", "name": "pdf",
             "quarantined": false, "removed": false},
            {"kind": "toolkit", "reference": "finops", "source": "kits", "name": "finops",
             "quarantined": false, "removed": false},
        ])
    );
    assert_eq!(
        put.body["delivered"],
        json!([
            {"kind": "skill", "source": "team", "name": "pdf", "tools": null, "listed": true,
             "via": ["kits/finops"], "quarantined": false, "removed": false},
            {"kind": "mcp_server", "source": "kits", "name": "docs", "tools": ["search"],
             "listed": false, "via": ["kits/finops"], "quarantined": false, "removed": false},
        ])
    );
    let pdf = app
        .admin("GET", "/api/v1/catalog/skills/team/pdf", None)
        .await;
    assert_eq!(pdf.body["environments"], json!(["data"]));
    let skills = app.admin("GET", "/api/v1/catalog/skills", None).await;
    assert_eq!(skills.body[0]["environments"], json!(["data"]));

    let deleted = app
        .admin("DELETE", "/api/v1/catalog-sources/kits", None)
        .await;
    assert_eq!(deleted.status, 409, "{}", deleted.text);
    assert_eq!(deleted.body["error"], "in_use");
    assert!(
        deleted.text.contains("environment data"),
        "{}",
        deleted.text
    );

    // A reference to nothing is refused at its place.
    let bad = app
        .admin(
            "PUT",
            "/api/v1/environments/data",
            Some(json!({"access_groups": ["github/eng"], "skills": ["pdf", "nope"]})),
        )
        .await;
    assert_eq!(bad.status, 422, "{}", bad.text);
    assert_eq!(
        bad.body["fields"],
        json!([{"field": "skills[1]", "message": "no skill named nope"}])
    );
}
