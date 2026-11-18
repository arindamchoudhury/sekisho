//! Git mode (plan 3, Task 10): environments applied from a repository
//! served by `git_http`, changes proposed through the testkit's GitHub mock.

mod common;

use std::time::Duration;

use axum::Router;
use common::{TestApp, app};
use sekisho_catalog::names::SourceName;
use sekisho_server::ApiError;
use sekisho_server::catalog::sync::sync_source;
use sekisho_store::SyncReport;
use sekisho_testkit::git_http::{self, Repo};
use sekisho_testkit::github::{self, API_VERSION};
use serde_json::{Value, json};

const GITHUB_TOKEN: &str = "ghp-SECRET-0123456789";
const GIT_TOKEN: &str = "git-SECRET-0123456789";

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn env_yaml(name: &str, description: &str, models: &[&str]) -> Vec<u8> {
    let models: Vec<String> = models.iter().map(|m| format!("- {m}")).collect();
    format!(
        "api_version: sekisho/v1alpha1\nkind: Environment\nname: {name}\ndescription: {description}\naccess_groups:\n- github/eng\nmodels:\n{}\n",
        models.join("\n")
    )
    .into_bytes()
}

struct Git {
    app: TestApp,
    _root: tempfile::TempDir,
    repo: Repo,
    github: github::GitHub,
    gh_base: String,
}

/// An app with Git mode on, reading `envs.git`'s `environments/` folder,
/// which holds `team` and `ops`, both applied.
async fn git_mode() -> Git {
    let app = app().await;
    app.provider_and_model().await;
    let root = tempfile::tempdir().unwrap();
    let repo = Repo::create(root.path(), "envs");
    repo.commit(
        &[
            (
                "environments/team.yaml",
                &env_yaml("team", "v1", &["mock-model"]),
            ),
            (
                "environments/ops.yaml",
                &env_yaml("ops", "ops", &["mock-model"]),
            ),
            ("README.md", b"Environments.\n"),
        ],
        "first",
    );
    let git_base = serve(git_http::serve(root.path().to_path_buf())).await;
    let (gh_router, github) = github::router(root.path().to_path_buf());
    let gh_base = serve(gh_router).await;
    let r = app
        .admin(
            "PUT",
            "/api/v1/git-mode",
            Some(json!({
                "url": format!("{git_base}/git/envs.git"),
                "folder": "environments",
                "token": GIT_TOKEN,
                "allow_private_network": true,
                "github_api_url": gh_base,
                "github_token": GITHUB_TOKEN,
            })),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    let g = Git {
        app,
        _root: root,
        repo,
        github,
        gh_base,
    };
    // The first sync, started by the PUT.
    g.app
        .eventually(Duration::from_secs(30), "the first sync", || true)
        .await;
    g.wait_for_env("team").await;
    g
}

impl Git {
    async fn wait_for_env(&self, name: &str) {
        for _ in 0..300 {
            let r = self
                .app
                .admin("GET", &format!("/api/v1/environments/{name}"), None)
                .await;
            if r.status == 200 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{name} never appeared");
    }

    /// Syncs Git mode's source now, waiting out a sync already running.
    async fn sync(&self) -> SyncReport {
        loop {
            match sync_source(&self.app.state, &SourceName::git_mode(), true).await {
                Ok(report) => return report,
                Err(ApiError::Conflict(_)) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(e) => panic!("sync failed: {e:?}"),
            }
        }
    }

    async fn env(&self, name: &str) -> Value {
        let r = self
            .app
            .admin("GET", &format!("/api/v1/environments/{name}"), None)
            .await;
        assert_eq!(r.status, 200, "{}", r.text);
        r.body
    }
}

#[tokio::test]
async fn git_mode_applies_the_repository() {
    let g = git_mode().await;
    g.wait_for_env("ops").await;
    for name in ["team", "ops"] {
        let env = g.env(name).await;
        assert_eq!(env["owner"], json!(g.app.owner), "{name}");
        assert_eq!(env["models"], json!(["mock-model"]));
    }
    let mode = g.app.admin("GET", "/api/v1/git-mode", None).await;
    assert_eq!(mode.status, 200, "{}", mode.text);
    assert_eq!(mode.body["enabled"], true);
    assert_eq!(mode.body["source"]["name"], "environments");
    assert_eq!(mode.body["source"]["kind"], "environments");
    assert_eq!(mode.body["github_api_url"], format!("{}/", g.gh_base));
    assert_eq!(mode.body["has_github_token"], true);
    assert_eq!(mode.body["enabled_by"], json!(g.app.owner));
    let report = &mode.body["latest_sync"]["environments"];
    let mut applied: Vec<&str> = report["applied"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    applied.sort();
    assert_eq!(applied, ["ops", "team"], "{}", mode.text);
    assert_eq!(report["refused"], json!([]));
}

#[tokio::test]
async fn each_new_commit_is_applied() {
    let g = git_mode().await;
    g.repo.commit(
        &[(
            "environments/team.yaml",
            &env_yaml("team", "v2", &["mock-model"]),
        )],
        "v2",
    );
    let report = g.sync().await;
    assert!(report.applied, "{report:?}");
    assert_eq!(g.env("team").await["description"], "v2");
}

#[tokio::test]
async fn a_bad_file_keeps_its_environment() {
    let g = git_mode().await;
    g.repo.commit(
        &[
            (
                "environments/team.yaml",
                &env_yaml("team", "v2", &["mock-model", "nope"]),
            ),
            ("environments/broken.yaml", b"kind: Environment\nname: [\n"),
        ],
        "bad",
    );
    let report = g.sync().await;
    let refused = report.environments["refused"].as_array().unwrap();
    let team = refused
        .iter()
        .find(|r| r["name"] == "team")
        .unwrap_or_else(|| panic!("{refused:?}"));
    assert_eq!(team["path"], "environments/team.yaml");
    assert_eq!(team["fields"][0]["field"], "models[1]");
    assert!(
        refused
            .iter()
            .any(|r| r["path"] == "environments/broken.yaml"),
        "{refused:?}"
    );
    assert_eq!(report.environments["not_in_git"], json!([]));
    let env = g.env("team").await;
    assert_eq!(env["description"], "v1");
    assert_eq!(env["models"], json!(["mock-model"]));
}

#[tokio::test]
async fn a_sync_never_deletes_an_environment() {
    let g = git_mode().await;
    g.wait_for_env("ops").await;
    g.repo.remove(&["environments/ops.yaml"], "no ops");
    let report = g.sync().await;
    assert_eq!(report.environments["not_in_git"], json!(["ops"]));
    g.env("ops").await;
    // Gone from Git, it can be deleted here; one in Git can't.
    let r = g
        .app
        .admin("DELETE", "/api/v1/environments/ops", None)
        .await;
    assert_eq!(r.status, 204, "{}", r.text);
    let r = g
        .app
        .admin("DELETE", "/api/v1/environments/team", None)
        .await;
    assert_eq!(r.status, 409, "{}", r.text);
}

#[tokio::test]
async fn the_api_refuses_edits_in_git_mode() {
    let g = git_mode().await;
    for (method, path, body) in [
        (
            "PUT",
            "/api/v1/environments/team",
            json!({"access_groups": ["github/eng"]}),
        ),
        (
            "PUT",
            "/api/v1/environments/new",
            json!({"access_groups": ["github/eng"]}),
        ),
        (
            "POST",
            "/api/v1/catalog-sources/obot/vmcps/kit/environment",
            json!({"name": "kit"}),
        ),
    ] {
        let r = g.app.admin(method, path, Some(body)).await;
        assert_eq!(r.status, 409, "{method} {path}: {}", r.text);
        assert!(r.text.contains("proposals"), "{}", r.text);
    }
    let r = g
        .app
        .admin("DELETE", "/api/v1/environments/team", None)
        .await;
    assert_eq!(r.status, 409, "{}", r.text);
    let (ada, _) = g.app.sign_in("ada@example.com", &["eng"]).await;
    let r = g
        .app
        .admin(
            "PATCH",
            "/api/v1/environments/team",
            Some(json!({"owner": ada})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
}

#[tokio::test]
async fn a_proposal_opens_a_pull_request() {
    let g = git_mode().await;
    let r = g
        .app
        .admin(
            "POST",
            "/api/v1/environments/team/proposals",
            Some(json!({"environment": {
                "description": "proposed", "access_groups": ["github/eng"], "models": ["mock-model"]
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.body["number"], 1);
    assert!(
        r.body["url"].as_str().unwrap().ends_with("/pull/1"),
        "{}",
        r.text
    );

    let requests = g.github.requests();
    assert!(!requests.is_empty());
    for req in &requests {
        assert_eq!(req.api_version.as_deref(), Some(API_VERSION), "{req:?}");
        assert_eq!(
            req.authorization.as_deref(),
            Some(format!("Bearer {GITHUB_TOKEN}").as_str())
        );
        assert_eq!(req.accept.as_deref(), Some("application/vnd.github+json"));
    }
    let made = requests
        .iter()
        .find(|r| r.method == "POST" && r.path.ends_with("/git/refs"))
        .expect("a branch");
    assert!(
        made.body["ref"]
            .as_str()
            .unwrap()
            .starts_with("refs/heads/sekisho/team-"),
        "{:?}",
        made.body
    );
    let put = requests
        .iter()
        .find(|r| r.method == "PUT")
        .expect("a file write");
    assert_eq!(put.path, "/repos/git/envs/contents/environments/team.yaml");
    assert!(
        put.body["sha"].is_string(),
        "the file existed: {:?}",
        put.body
    );
    let pull = &g.github.pulls()[0];
    assert_eq!(pull.base, "main");
    assert!(pull.body.contains("admin@example.com"), "{}", pull.body);
    assert!(pull.body.contains("Proposed in Sekisho"), "{}", pull.body);
    // Nothing changes until it is merged.
    assert_eq!(g.env("team").await["description"], "v1");

    let merged = g
        .app
        .http
        .post(format!("{}/_github/git/envs/pulls/1/merge", g.gh_base))
        .send()
        .await
        .unwrap();
    assert_eq!(merged.status(), 200);
    g.sync().await;
    assert_eq!(g.env("team").await["description"], "proposed");

    // A new environment, proposed as YAML.
    let yaml = String::from_utf8(env_yaml("data", "data", &["mock-model"])).unwrap();
    let r = g
        .app
        .admin(
            "POST",
            "/api/v1/environments/data/proposals",
            Some(json!({ "environment": yaml })),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text);
    let put = g
        .github
        .requests()
        .into_iter()
        .rfind(|r| r.method == "PUT")
        .unwrap();
    assert_eq!(put.path, "/repos/git/envs/contents/environments/data.yaml");
    assert!(put.body.get("sha").is_none(), "{:?}", put.body);
}

#[tokio::test]
async fn a_proposal_is_checked_first() {
    let g = git_mode().await;
    let r = g
        .app
        .admin(
            "POST",
            "/api/v1/environments/team/proposals",
            Some(json!({"environment": {"access_groups": ["github/eng"], "models": ["nope"]}})),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.text);
    assert_eq!(r.body["fields"][0]["field"], "models[0]");
    let r = g
        .app
        .admin(
            "POST",
            "/api/v1/environments/team/proposals",
            Some(json!({"environment": {"name": "other"}})),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.text);
    let r = g
        .app
        .admin(
            "POST",
            "/api/v1/environments/team/proposals",
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text);
    assert!(g.github.requests().is_empty(), "{:?}", g.github.requests());
}

#[tokio::test]
async fn a_delete_proposal_removes_the_file() {
    let g = git_mode().await;
    g.wait_for_env("ops").await;
    let r = g
        .app
        .admin(
            "POST",
            "/api/v1/environments/ops/proposals",
            Some(json!({"delete": true})),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text);
    let del = g
        .github
        .requests()
        .into_iter()
        .find(|r| r.method == "DELETE")
        .expect("a delete");
    assert_eq!(del.path, "/repos/git/envs/contents/environments/ops.yaml");
    g.app
        .http
        .post(format!("{}/_github/git/envs/pulls/1/merge", g.gh_base))
        .send()
        .await
        .unwrap();
    let report = g.sync().await;
    assert_eq!(report.environments["not_in_git"], json!(["ops"]));
    g.env("ops").await;
    let r = g
        .app
        .admin(
            "POST",
            "/api/v1/environments/nope/proposals",
            Some(json!({"delete": true})),
        )
        .await;
    assert_eq!(r.status, 404, "{}", r.text);
}

#[tokio::test]
async fn turning_git_mode_off_keeps_the_environments() {
    let g = git_mode().await;
    let r = g.app.admin("DELETE", "/api/v1/git-mode", None).await;
    assert_eq!(r.status, 204, "{}", r.text);
    let mode = g.app.admin("GET", "/api/v1/git-mode", None).await;
    assert_eq!(mode.body, json!({"enabled": false}));
    g.env("team").await;
    let r = g
        .app
        .admin(
            "PUT",
            "/api/v1/environments/team",
            Some(json!({"access_groups": ["github/eng"], "models": ["mock-model"]})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    let r = g
        .app
        .admin(
            "POST",
            "/api/v1/environments/team/proposals",
            Some(json!({"delete": true})),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.text);
    assert_eq!(
        g.app.admin("DELETE", "/api/v1/git-mode", None).await.status,
        404
    );
    let sources = g.app.admin("GET", "/api/v1/catalog-sources", None).await;
    assert_eq!(sources.body, json!([]));
}

#[tokio::test]
async fn the_github_token_is_never_answered() {
    let g = git_mode().await;
    let replies = [
        g.app.admin("GET", "/api/v1/git-mode", None).await,
        g.app.admin("GET", "/api/v1/catalog-sources", None).await,
    ];
    for r in &replies {
        assert!(!r.text.contains(GITHUB_TOKEN), "{}", r.text);
        assert!(!r.text.contains(GIT_TOKEN), "{}", r.text);
    }
    // A host other than github.com needs the API's URL; a token is required.
    let r = g
        .app
        .admin(
            "PUT",
            "/api/v1/git-mode",
            Some(json!({"url": "https://git.example.com/a/b.git"})),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.text);
    let mut fields: Vec<&str> = r.body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["field"].as_str().unwrap())
        .collect();
    fields.sort();
    assert_eq!(fields, ["github_api_url", "github_token"]);
    assert!(!r.text.contains(GITHUB_TOKEN));
}
