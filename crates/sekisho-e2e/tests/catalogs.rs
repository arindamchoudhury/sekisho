//! Plan 3's acceptance tests: catalogs synced from Git served by the mock,
//! images pinned at its registry, agent cards pinned from it, quarantine,
//! toolkits, Obot's virtual servers and Git mode with the mock's GitHub.
//! Ignored by default; run with the stack up:
//!   cargo test -p sekisho-e2e -- --ignored --test-threads=1

use std::time::Duration;

use reqwest::StatusCode;
use sekisho_e2e::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::{Instant, sleep};

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn skill_md(name: &str, text: &str) -> String {
    format!("---\nname: {name}\ndescription: {text}\n---\n{text}\n")
}

/// An entry of `source` among the entries of `kind`.
async fn entry_of(kind: &str, source: &str, name: &str) -> Value {
    let (s, body) = admin(
        "GET",
        &format!("/api/v1/catalog/{kind}/{source}/{name}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{kind}/{source}/{name}: {body}");
    body
}

/// The names of `source`'s entries of `kind`.
async fn names_of(kind: &str, source: &str) -> Vec<String> {
    let (_, list) = admin("GET", &format!("/api/v1/catalog/{kind}"), None).await;
    let mut names: Vec<String> = list
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["source"] == source)
        .map(|e| e["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

async fn put_environment(name: &str, body: Value) -> (StatusCode, Value) {
    admin("PUT", &format!("/api/v1/environments/{name}"), Some(body)).await
}

async fn environment(name: &str) -> Value {
    let (s, body) = admin("GET", &format!("/api/v1/environments/{name}"), None).await;
    assert_eq!(s, StatusCode::OK, "{name}: {body}");
    body
}

#[tokio::test]
#[ignore]
async fn obot_catalogs_and_skills_appear() {
    let repo = unique("obot");
    let source = unique("obot");
    let digest = format!("sha256:{}", sha256_hex(repo.as_bytes()));
    set_image("acme/server", &repo, &digest).await;
    let catalog = format!(
        "- entryKey: fetch
  name: Fetch
  runtime: npx
  npxConfig:
    package: \"@acme/fetch@1.2.3\"
- entryKey: time
  name: Time
  runtime: uvx
  uvxConfig:
    package: mcp-time==1.0.0
- entryKey: local
  name: Local
  runtime: containerized
  containerizedConfig:
    image: mock-model:8000/acme/server:{repo}
    port: 8080
    path: /mcp
- entryKey: docs
  name: Docs
  runtime: remote
  remoteConfig:
    fixedURL: https://mcp.example.com/docs
- entryKey: api
  name: API
  runtime: openapi
- type: vmcp
  entryKey: team
  displayName: Team
  components:
    - id: c1
      name: Fetch
      mcpServerCatalogEntryKey: fetch
"
    );
    git_commit(&repo, &[("catalog.yaml", &catalog)]).await;
    let report = add_source(&source, "catalog", &repo).await;
    assert_eq!(report["outcome"], "ok", "{report}");
    assert_eq!(report["applied"], true);
    assert_eq!(
        names_of("mcp-servers", &source).await,
        ["docs", "fetch", "local", "time"]
    );
    let refused = report["refused"].as_array().unwrap();
    assert!(
        refused
            .iter()
            .any(|r| r["name"] == "api" && r["reason"].as_str().unwrap().contains("openapi")),
        "{report}"
    );
    assert_eq!(report["vmcps"][0]["key"], "team");
    let local = entry_of("mcp-servers", &source, "local").await;
    assert!(
        local["applied"]["spec"].to_string().contains(&digest),
        "the image is pinned to the registry's digest: {local}"
    );

    // A skills repository.
    let skills_repo = unique("skills");
    let skills = unique("skills");
    git_commit(
        &skills_repo,
        &[("pdf/SKILL.md", &skill_md("pdf", "Reads PDFs."))],
    )
    .await;
    let report = add_source(&skills, "skills", &skills_repo).await;
    assert_eq!(report["outcome"], "ok", "{report}");
    assert_eq!(names_of("skills", &skills).await, ["pdf"]);
    let pdf = entry_of("skills", &skills, "pdf").await;
    let (s, _) = admin(
        "GET",
        &format!("/api/v1/catalog/skills/{skills}/pdf/files/SKILL.md"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{pdf}");
}

#[tokio::test]
#[ignore]
async fn an_update_reaches_environments_only_when_applied() {
    seed().await;
    let (repo, source, env) = (unique("upd"), unique("upd"), unique("upd"));
    let first = git_commit(&repo, &[("pdf/SKILL.md", &skill_md("pdf", "v1"))]).await;
    add_source(&source, "skills", &repo).await;
    let (s, body) = put_environment(
        &env,
        json!({"access_groups": [GROUP], "models": [MODEL], "skills": [format!("pdf@{source}")]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    let second = git_commit(&repo, &[("pdf/SKILL.md", &skill_md("pdf", "v2"))]).await;
    let report = sync_now(&source).await;
    assert_eq!(report["commit"], second.as_str(), "{report}");
    assert_eq!(report["applied"], false);
    let pdf = entry_of("skills", &source, "pdf").await;
    assert_eq!(pdf["applied"]["commit"], first.as_str());
    assert_eq!(pdf["pending"]["commit"], second.as_str());
    let delivered = environment(&env).await["delivered"].clone();
    assert_eq!(delivered[0]["name"], "pdf", "{delivered}");

    let (s, applied) = admin(
        "POST",
        &format!("/api/v1/catalog-sources/{source}/apply"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{applied}");
    assert_eq!(
        applied["updated"],
        json!([{"kind": "skill", "name": "pdf"}])
    );
    let pdf = entry_of("skills", &source, "pdf").await;
    assert_eq!(pdf["applied"]["commit"], second.as_str());
    assert_eq!(pdf["pending"], Value::Null);
    assert_eq!(pdf["environments"], json!([env]));
}

/// An APM bundle's files and its lock, hashing every file but those named
/// in `tamper`, which are changed after hashing.
fn apm_bundle(dir: &str, tamper: &[&str]) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = vec![
        (
            "plugin.json".into(),
            r#"{"name": "apm-tools", "description": "Packed by APM.", "version": "0.3.0"}"#.into(),
        ),
        ("commands/ship.md".into(), "Ship it.\n".into()),
        ("skills/notes/SKILL.md".into(), skill_md("notes", "Notes.")),
    ];
    let mut lock =
        String::from("lockfile_version: '1'\npack:\n  format: plugin\n  bundle_files:\n");
    for (path, text) in &files {
        lock.push_str(&format!("    {path:?}: {}\n", sha256_hex(text.as_bytes())));
    }
    for (path, text) in &mut files {
        if tamper.contains(&path.as_str()) {
            text.push_str("tampered\n");
        }
    }
    files.push(("apm.lock.yaml".into(), lock));
    files
        .into_iter()
        .map(|(p, t)| (format!("{dir}/{p}"), t))
        .collect()
}

#[tokio::test]
#[ignore]
async fn plugins_and_apm_bundles_sync() {
    let (repo, source) = (unique("plug"), unique("plug"));
    let marketplace = r#"{
  "name": "acme-tools",
  "owner": {"name": "Acme"},
  "plugins": [
    {"name": "formatter", "source": "./plugins/formatter"},
    {"name": "apm-tools", "source": "./plugins/apm-tools"}
  ]
}"#;
    let mut files: Vec<(String, String)> = vec![
        (".claude-plugin/marketplace.json".into(), marketplace.into()),
        (
            "plugins/formatter/.claude-plugin/plugin.json".into(),
            r#"{"name": "formatter", "description": "Formats."}"#.into(),
        ),
        (
            "plugins/formatter/commands/fmt.md".into(),
            "Format.\n".into(),
        ),
    ];
    files.extend(apm_bundle("plugins/apm-tools", &[]));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    git_commit(&repo, &refs).await;
    let report = add_source(&source, "plugins", &repo).await;
    assert_eq!(report["outcome"], "ok", "{report}");
    assert!(report["refused"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(
        names_of("plugins", &source).await,
        ["apm-tools", "formatter"]
    );
    let bundle = entry_of("plugins", &source, "apm-tools").await;
    assert_eq!(bundle["applied"]["spec"]["apm_checked"], true, "{bundle}");
    let digest = bundle["applied"]["digest"].clone();

    // A tampered bundle is refused; the applied revision stays.
    let tampered = apm_bundle("plugins/apm-tools", &["commands/ship.md"]);
    let refs: Vec<(&str, &str)> = tampered
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    git_commit(&repo, &refs).await;
    let report = sync_now(&source).await;
    let refused = report["refused"].as_array().unwrap();
    assert!(
        refused
            .iter()
            .any(|r| r["name"] == "apm-tools"
                && r["reason"].as_str().unwrap().contains("hash mismatch")),
        "{report}"
    );
    assert!(report["missing"].as_array().unwrap().is_empty(), "{report}");
    let bundle = entry_of("plugins", &source, "apm-tools").await;
    assert_eq!(bundle["applied"]["digest"], digest);
    assert_eq!(bundle["pending"], Value::Null);
}

#[tokio::test]
#[ignore]
async fn agent_cards_are_pinned_and_updates_wait() {
    let (repo, source, agent) = (unique("a2a"), unique("a2a"), unique("planner"));
    let url = format!("{MOCK_INTERNAL}/a2a/{agent}");
    let card = |description: &str| json!({"name": "Planner", "description": description, "url": url, "version": "1.0.0"});
    set_card(&agent, card("v1")).await;
    let catalog = format!(
        "apiVersion: sekisho/v1alpha1\nkind: A2AAgent\nname: planner\ndescription: Plans.\nurl: {url}\n"
    );
    git_commit(&repo, &[("agents.yaml", &catalog)]).await;
    let report = add_source(&source, "catalog", &repo).await;
    assert_eq!(report["outcome"], "ok", "{report}");
    let planner = entry_of("a2a-agents", &source, "planner").await;
    assert_eq!(
        planner["applied"]["spec"]["card"]["json"]["description"], "v1",
        "{planner}"
    );

    // The card changes; the same commit syncs again.
    set_card(&agent, card("v2")).await;
    let report = sync_now(&source).await;
    assert_eq!(
        report["updated"],
        json!([{"kind": "a2a_agent", "name": "planner"}]),
        "{report}"
    );
    let planner = entry_of("a2a-agents", &source, "planner").await;
    assert_eq!(
        planner["applied"]["spec"]["card"]["json"]["description"],
        "v1"
    );
    assert_eq!(
        planner["pending"]["spec"]["card"]["json"]["description"],
        "v2"
    );
    let (s, _) = admin(
        "POST",
        &format!("/api/v1/catalog-sources/{source}/apply"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let planner = entry_of("a2a-agents", &source, "planner").await;
    assert_eq!(
        planner["applied"]["spec"]["card"]["json"]["description"],
        "v2"
    );
}

#[tokio::test]
#[ignore]
async fn quarantine_marks_every_environment() {
    seed().await;
    let (repo, source) = (unique("q"), unique("q"));
    git_commit(&repo, &[("pdf/SKILL.md", &skill_md("pdf", "Reads PDFs."))]).await;
    add_source(&source, "skills", &repo).await;
    let reference = format!("pdf@{source}");
    let envs = [unique("qa"), unique("qb")];
    for env in &envs {
        let (s, body) = put_environment(
            env,
            json!({"access_groups": [GROUP], "models": [MODEL], "skills": [reference]}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    let path = format!("/api/v1/catalog/skills/{source}/pdf/quarantine");
    let (s, body) = admin("POST", &path, Some(json!({"reason": "Leaks data."}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    for env in &envs {
        let e = environment(env).await;
        assert_eq!(e["catalog_status"][0]["quarantined"], true, "{e}");
        assert_eq!(e["delivered"][0]["quarantined"], true, "{e}");
    }
    let third = unique("qc");
    let (s, body) = put_environment(
        &third,
        json!({"access_groups": [GROUP], "models": [MODEL], "skills": [reference]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("is quarantined"), "{body}");

    let (s, _) = admin("DELETE", &path, None).await;
    assert_eq!(s, StatusCode::OK);
    for env in &envs {
        assert_eq!(
            environment(env).await["catalog_status"][0]["quarantined"],
            false
        );
    }
    let (s, body) = put_environment(
        &third,
        json!({"access_groups": [GROUP], "models": [MODEL], "skills": [reference]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test]
#[ignore]
async fn a_toolkit_reaches_the_environments_that_list_it() {
    seed().await;
    let (skills_repo, skills) = (unique("tks"), unique("tks"));
    git_commit(
        &skills_repo,
        &[
            ("pdf/SKILL.md", &skill_md("pdf", "Reads PDFs.")),
            ("notes/SKILL.md", &skill_md("notes", "Notes.")),
        ],
    )
    .await;
    add_source(&skills, "skills", &skills_repo).await;
    let (prompts_repo, prompts) = (unique("tkp"), unique("tkp"));
    git_commit(&prompts_repo, &[("review.md", "Review this.\n")]).await;
    add_source(&prompts, "prompts", &prompts_repo).await;

    let (repo, source) = (unique("tk"), unique("tk"));
    let catalog = |skills_list: &str| {
        format!(
            "- apiVersion: sekisho/v1alpha1
  kind: MCPServer
  name: docs
  remote:
    url: https://mcp.example.com/docs
- apiVersion: sekisho/v1alpha1
  kind: Toolkit
  name: finops
  description: Costs.
  mcp_servers:
    - server: docs@{source}
      tools: [search]
  skills: [{skills_list}]
  prompts: [review@{prompts}]
"
        )
    };
    git_commit(
        &repo,
        &[("catalog.yaml", &catalog(&format!("pdf@{skills}")))],
    )
    .await;
    let report = add_source(&source, "catalog", &repo).await;
    assert!(report["refused"].as_array().unwrap().is_empty(), "{report}");

    let kit = format!("finops@{source}");
    let (a, b) = (unique("tka"), unique("tkb"));
    let (s, body) = put_environment(
        &a,
        json!({"access_groups": [GROUP], "models": [MODEL], "toolkits": [kit],
               "mcp_servers": [format!("docs@{source}")]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = put_environment(
        &b,
        json!({"access_groups": [GROUP], "models": [MODEL], "toolkits": [kit]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let delivered = |e: &Value| -> Vec<String> {
        e["delivered"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let ea = environment(&a).await;
    assert_eq!(delivered(&ea), ["docs", "pdf", "review"], "{ea}");
    // Listed and in the toolkit: once, with every tool (the environment's
    // own grant allows all).
    assert_eq!(ea["delivered"][0]["listed"], true);
    assert_eq!(ea["delivered"][0]["tools"], Value::Null);
    let eb = environment(&b).await;
    assert_eq!(delivered(&eb), ["docs", "pdf", "review"], "{eb}");
    assert_eq!(eb["delivered"][0]["tools"], json!(["search"]));

    // A member added upstream waits for apply.
    git_commit(
        &repo,
        &[(
            "catalog.yaml",
            &catalog(&format!("pdf@{skills}, notes@{skills}")),
        )],
    )
    .await;
    let report = sync_now(&source).await;
    assert_eq!(
        report["updated"],
        json!([{"kind": "toolkit", "name": "finops"}]),
        "{report}"
    );
    assert_eq!(delivered(&environment(&a).await).len(), 3);
    let (s, _) = admin(
        "POST",
        &format!("/api/v1/catalog-sources/{source}/apply"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    for env in [&a, &b] {
        let names = delivered(&environment(env).await);
        assert!(names.contains(&"notes".to_owned()), "{env}: {names:?}");
    }
}

#[tokio::test]
#[ignore]
async fn obot_virtual_servers_become_environments() {
    seed().await;
    let (repo, source) = (unique("vm"), unique("vm"));
    let catalog = "- entryKey: a
  name: A
  runtime: remote
  remoteConfig:
    fixedURL: https://mcp.example.com/a
- entryKey: b
  name: B
  runtime: remote
  remoteConfig:
    fixedURL: https://mcp.example.com/b
- type: vmcp
  entryKey: team-tools
  displayName: Team Tools
  description: Shared tools.
  components:
    - id: c1
      name: A
      mcpServerCatalogEntryKey: a
    - id: c2
      name: B
      mcpServerCatalogEntryKey: b
      toolOverrides:
        - name: x
          enabled: true
";
    git_commit(&repo, &[("catalog.yaml", catalog)]).await;
    add_source(&source, "catalog", &repo).await;
    let (s, offers) = admin(
        "GET",
        &format!("/api/v1/catalog-sources/{source}/vmcps"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{offers}");
    assert_eq!(offers[0]["key"], "team-tools");
    let env = unique("vm");
    let (s, made) = admin(
        "POST",
        &format!("/api/v1/catalog-sources/{source}/vmcps/team-tools/environment"),
        Some(json!({"name": env, "access_groups": [GROUP], "models": [MODEL]})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    assert_eq!(
        made["environment"]["mcp_servers"],
        json!([format!("a@{source}"), {"server": format!("b@{source}"), "tools": ["x"]}])
    );
    assert_eq!(made["environment"]["description"], "Shared tools.");
    let (s, _) = admin(
        "POST",
        &format!("/api/v1/catalog-sources/{source}/vmcps/team-tools/environment"),
        Some(json!({"name": env})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
}

/// Turns Git mode off when dropped, so a failure can't leave the stack in
/// Git mode for every later test.
struct GitModeOff;

impl Drop for GitModeOff {
    fn drop(&mut self) {
        std::thread::spawn(|| {
            tokio::runtime::Runtime::new().unwrap().block_on(admin(
                "DELETE",
                "/api/v1/git-mode",
                None,
            ));
        })
        .join()
        .unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn git_mode_round_trip() {
    seed().await;
    // Off first: a run that was stopped mid-test may have left it on.
    let (s, _) = admin("DELETE", "/api/v1/git-mode", None).await;
    assert!(
        s == StatusCode::NO_CONTENT || s == StatusCode::NOT_FOUND,
        "{s}"
    );
    let _off = GitModeOff;

    let repo = unique("envs");
    let env = unique("git");
    let file = |description: &str| {
        format!(
            "api_version: sekisho/v1alpha1\nkind: Environment\nname: {env}\ndescription: {description}\naccess_groups:\n- {GROUP}\nmodels:\n- {MODEL}\n"
        )
    };
    git_commit(
        &repo,
        &[(&format!("environments/{env}.yaml"), &file("from Git"))],
    )
    .await;
    let (s, body) = admin(
        "PUT",
        "/api/v1/git-mode",
        Some(json!({
            "url": git_url(&repo),
            "folder": "environments",
            "allow_private_network": true,
            "github_api_url": MOCK_INTERNAL,
            "github_token": "e2e-github-token",
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(!body.to_string().contains("e2e-github-token"));

    // The first sync starts at once.
    let start = Instant::now();
    loop {
        let (s, e) = admin("GET", &format!("/api/v1/environments/{env}"), None).await;
        if s == StatusCode::OK {
            assert_eq!(e["description"], "from Git");
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{env} never appeared"
        );
        sleep(Duration::from_millis(200)).await;
    }
    // The API refuses edits while Git mode is on.
    let (s, _) = put_environment(&env, json!({"access_groups": [GROUP]})).await;
    assert_eq!(s, StatusCode::CONFLICT);

    let (s, pull) = admin(
        "POST",
        &format!("/api/v1/environments/{env}/proposals"),
        Some(json!({"environment": {
            "description": "proposed", "access_groups": [GROUP], "models": [MODEL]
        }})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{pull}");
    let number = pull["number"].as_u64().unwrap();
    let merged = http()
        .post(format!("{MOCK}/_github/git/{repo}/pulls/{number}/merge"))
        .send()
        .await
        .unwrap();
    assert!(merged.status().is_success(), "{}", merged.status());

    // Applied within two sync intervals (a minute each, looked at every
    // 30 s), with no apply.
    let start = Instant::now();
    loop {
        if environment(&env).await["description"] == "proposed" {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(150),
            "the merged change never arrived"
        );
        sleep(Duration::from_secs(2)).await;
    }
    let (s, mode) = admin("GET", "/api/v1/git-mode", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        mode["latest_sync"]["environments"]["applied"]
            .as_array()
            .unwrap()
            .contains(&json!(env)),
        "{mode}"
    );

    let (s, _) = admin("DELETE", "/api/v1/git-mode", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    environment(&env).await;
}
