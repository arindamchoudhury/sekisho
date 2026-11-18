//! The sync engine (plan 3, Task 6): hardened git against a real smart-HTTP
//! server, image tags resolved to digests, agent cards pinned, every
//! outside fetch address-checked.

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use common::{TestApp, app};
use sekisho_catalog::model::{EntrySpec, McpRuntime};
use sekisho_catalog::names::{EntryKind, EntryName, SourceKind, SourceName};
use sekisho_catalog::source::{Folder, GitRef, SourceUrl};
use sekisho_server::catalog::git::{Git, GitSource};
use sekisho_server::catalog::sync::{start_scheduler, sync_source};
use sekisho_store::{EntryKey, NewSource, SyncOutcomeKind};
use sekisho_testkit::git_http::{self, Repo};
use sekisho_testkit::{agents, registry};
use serde_json::{Value, json};

/// Serves `router` on 127.0.0.1 and answers its base URL.
async fn serve(router: Router) -> (String, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), addr)
}

/// A git server on 127.0.0.1 that counts the requests it gets.
struct GitServer {
    root: tempfile::TempDir,
    base: String,
    hits: Arc<AtomicUsize>,
}

impl GitServer {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let router = git_http::serve(root.path().to_path_buf()).layer(
            axum::middleware::map_request(move |r: axum::extract::Request| {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    r
                }
            }),
        );
        let (base, _) = serve(router).await;
        Self { root, base, hits }
    }

    fn repo(&self, name: &str) -> Repo {
        Repo::create(self.root.path(), name)
    }

    fn url(&self, name: &str) -> String {
        format!("{}/git/{name}.git", self.base)
    }
}

fn source_name(s: &str) -> SourceName {
    SourceName::parse(s).unwrap()
}

async fn add_source(app: &TestApp, name: &str, kind: SourceKind, url: &str, private: bool) {
    let new = NewSource {
        name: source_name(name),
        kind,
        url: SourceUrl::parse(url, true).unwrap(),
        git_ref: None,
        folder: Folder::default(),
        username: None,
        token: None,
        allow_private_network: private,
    };
    app.store.add_source(&app.sb, new, app.owner).await.unwrap();
}

fn skill_md(name: &str, text: &str) -> Vec<u8> {
    format!("---\nname: {name}\ndescription: {text}\n---\n{text}\n").into_bytes()
}

fn key(kind: EntryKind, source: &str, name: &str) -> EntryKey {
    EntryKey {
        kind,
        source: source_name(source),
        name: EntryName::parse(name).unwrap(),
    }
}

fn names(list: &[(EntryKind, EntryName)]) -> Vec<String> {
    let mut v: Vec<String> = list.iter().map(|(_, n)| n.to_string()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn a_skills_source_syncs_and_applies_itself() {
    let app = app().await;
    let git = GitServer::start().await;
    let commit = git.repo("team").commit(
        &[("pdf/SKILL.md", &skill_md("pdf", "Reads PDFs."))],
        "first",
    );
    add_source(&app, "team", SourceKind::Skills, &git.url("team"), true).await;

    let report = sync_source(&app.state, &source_name("team"), true)
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Ok, "{report:?}");
    assert_eq!(report.commit.as_deref(), Some(commit.as_str()));
    assert!(report.applied);
    assert_eq!(names(&report.new), ["pdf"]);
    let source = app.store.get_source(&source_name("team")).await.unwrap();
    assert_eq!(source.applied_commit.as_deref(), Some(commit.as_str()));
    assert!(!source.syncing, "the claim is released");
    let file = app
        .store
        .entry_file(&key(EntryKind::Skill, "team", "pdf"), "SKILL.md")
        .await
        .unwrap();
    assert_eq!(file, skill_md("pdf", "Reads PDFs."));
}

#[tokio::test]
async fn a_new_commit_is_found_and_waits() {
    let app = app().await;
    let git = GitServer::start().await;
    let repo = git.repo("team");
    let first = repo.commit(&[("pdf/SKILL.md", &skill_md("pdf", "v1"))], "first");
    add_source(&app, "team", SourceKind::Skills, &git.url("team"), true).await;
    sync_source(&app.state, &source_name("team"), true)
        .await
        .unwrap();

    let second = repo.commit(&[("pdf/SKILL.md", &skill_md("pdf", "v2"))], "second");
    let report = sync_source(&app.state, &source_name("team"), true)
        .await
        .unwrap();
    assert_eq!(report.commit.as_deref(), Some(second.as_str()));
    assert!(!report.applied);
    assert_eq!(names(&report.updated), ["pdf"]);
    let source = app.store.get_source(&source_name("team")).await.unwrap();
    assert!(source.pending);
    assert_eq!(source.applied_commit.as_deref(), Some(first.as_str()));
}

#[tokio::test]
async fn an_unmoved_ref_is_unchanged() {
    let app = app().await;
    let git = GitServer::start().await;
    git.repo("team")
        .commit(&[("pdf/SKILL.md", &skill_md("pdf", "v1"))], "first");
    add_source(&app, "team", SourceKind::Skills, &git.url("team"), true).await;
    sync_source(&app.state, &source_name("team"), true)
        .await
        .unwrap();
    let before = git.hits.load(Ordering::SeqCst);
    let report = sync_source(&app.state, &source_name("team"), true)
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Unchanged);
    // ls-remote's two requests (discovery, then the ref list), no fetch.
    let used = git.hits.load(Ordering::SeqCst) - before;
    assert!(used <= 2, "{used} requests: nothing was fetched");
}

#[tokio::test]
async fn a_ref_and_a_folder_choose_what_is_read() {
    let app = app().await;
    let git = GitServer::start().await;
    let repo = git.repo("team");
    repo.commit(&[("skills/a/SKILL.md", &skill_md("a", "On main."))], "main");
    repo.branch("release");
    repo.commit(
        &[("skills/b/SKILL.md", &skill_md("b", "On main only."))],
        "main again",
    );
    let new = NewSource {
        name: source_name("rel"),
        kind: SourceKind::Skills,
        url: SourceUrl::parse(&git.url("team"), true).unwrap(),
        git_ref: Some(GitRef::parse("release").unwrap()),
        folder: Folder::parse("skills").unwrap(),
        username: None,
        token: None,
        allow_private_network: true,
    };
    app.store.add_source(&app.sb, new, app.owner).await.unwrap();
    let report = sync_source(&app.state, &source_name("rel"), true)
        .await
        .unwrap();
    assert_eq!(names(&report.new), ["a"], "{report:?}");
    assert_eq!(
        app.store
            .get_entry(&key(EntryKind::Skill, "rel", "a"))
            .await
            .unwrap()
            .path,
        "a"
    );

    // A ref that doesn't exist fails the sync.
    let mut missing = app.store.get_source(&source_name("rel")).await.unwrap();
    missing.git_ref = Some(GitRef::parse("nope").unwrap());
    let patch = sekisho_store::SourcePatch {
        git_ref: Some(missing.git_ref),
        ..Default::default()
    };
    app.store
        .update_source(&app.sb, &source_name("rel"), patch)
        .await
        .unwrap();
    let report = sync_source(&app.state, &source_name("rel"), true)
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Failed);
    assert!(report.error.unwrap().contains("nope"));
}

/// A catalog repository with a containerized server and an A2A agent.
fn catalog_files(registry: &str, agents: &str) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "servers.yaml",
            format!(
                "name: tool\nruntime: containerized\ncontainerizedConfig:\n  image: {registry}/team/tool:1.0\n  port: 8080\n  path: /mcp\n"
            )
            .into_bytes(),
        ),
        (
            "agents.yaml",
            format!(
                "apiVersion: sekisho/v1alpha1\nkind: A2AAgent\nname: planner\nurl: {agents}/a2a/planner\n"
            )
            .into_bytes(),
        ),
    ]
}

fn commit_all(repo: &Repo, files: &[(&'static str, Vec<u8>)], message: &str) -> String {
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    repo.commit(&refs, message)
}

fn card(name: &str) -> Value {
    json!({"name": name, "description": "Plans.", "supportedInterfaces": [
        {"url": "https://agents.example/rpc", "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
    ]})
}

struct Lookups {
    registry: String,
    agents: String,
    images: Arc<Mutex<HashMap<String, String>>>,
    cards: Arc<Mutex<HashMap<String, Value>>>,
}

async fn lookups(require_token: bool) -> Lookups {
    let images = Arc::new(Mutex::new(HashMap::new()));
    let cards = Arc::new(Mutex::new(HashMap::new()));
    let (_, reg) = serve(registry::router(images.clone(), require_token)).await;
    let (agents_base, _) = serve(agents::router(cards.clone())).await;
    Lookups {
        registry: reg.to_string(),
        agents: agents_base,
        images,
        cards,
    }
}

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn image_of(spec: &EntrySpec) -> String {
    match spec {
        EntrySpec::McpServer(s) => match &s.runtime {
            McpRuntime::Container { image, .. } => image.to_string(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn image_tags_resolve_to_digests() {
    for require_token in [false, true] {
        let app = app().await;
        let git = GitServer::start().await;
        let l = lookups(require_token).await;
        l.images
            .lock()
            .unwrap()
            .insert("team/tool:1.0".into(), digest('a'));
        l.cards.lock().unwrap().insert(
            "planner/.well-known/agent-card.json".into(),
            card("Planner"),
        );
        commit_all(
            &git.repo("cat"),
            &catalog_files(&l.registry, &l.agents),
            "first",
        );
        add_source(&app, "cat", SourceKind::Catalog, &git.url("cat"), true).await;

        let report = sync_source(&app.state, &source_name("cat"), true)
            .await
            .unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let tool = app
            .store
            .get_entry(&key(EntryKind::McpServer, "cat", "tool"))
            .await
            .unwrap();
        let image = image_of(&tool.applied.unwrap().spec);
        assert_eq!(
            image,
            format!("{}/team/tool:1.0@{}", l.registry, digest('a')),
            "token: {require_token}"
        );
    }
}

#[tokio::test]
async fn a_moved_tag_is_an_update() {
    let app = app().await;
    let git = GitServer::start().await;
    let l = lookups(false).await;
    l.images
        .lock()
        .unwrap()
        .insert("team/tool:1.0".into(), digest('a'));
    l.cards.lock().unwrap().insert(
        "planner/.well-known/agent-card.json".into(),
        card("Planner"),
    );
    commit_all(
        &git.repo("cat"),
        &catalog_files(&l.registry, &l.agents),
        "first",
    );
    add_source(&app, "cat", SourceKind::Catalog, &git.url("cat"), true).await;
    sync_source(&app.state, &source_name("cat"), true)
        .await
        .unwrap();

    // The commit is the same; the tag now names another image.
    l.images
        .lock()
        .unwrap()
        .insert("team/tool:1.0".into(), digest('b'));
    let report = sync_source(&app.state, &source_name("cat"), true)
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Ok);
    assert_eq!(names(&report.updated), ["tool"]);
    let tool = app
        .store
        .get_entry(&key(EntryKind::McpServer, "cat", "tool"))
        .await
        .unwrap();
    assert!(image_of(&tool.applied.unwrap().spec).ends_with(&digest('a')));
    assert!(image_of(&tool.pending.unwrap().spec).ends_with(&digest('b')));
}

#[tokio::test]
async fn an_unresolvable_image_refuses_its_entry() {
    let app = app().await;
    let git = GitServer::start().await;
    let l = lookups(false).await;
    l.cards.lock().unwrap().insert(
        "planner/.well-known/agent-card.json".into(),
        card("Planner"),
    );
    commit_all(
        &git.repo("cat"),
        &catalog_files(&l.registry, &l.agents),
        "first",
    );
    add_source(&app, "cat", SourceKind::Catalog, &git.url("cat"), true).await;
    let report = sync_source(&app.state, &source_name("cat"), true)
        .await
        .unwrap();
    assert_eq!(names(&report.new), ["planner"]);
    let refusal = report
        .refused
        .iter()
        .find(|r| r.name.as_deref() == Some("tool"))
        .unwrap();
    assert!(refusal.reason.contains("team/tool:1.0"), "{refusal:?}");
}

fn agent_card_of(spec: &EntrySpec) -> Value {
    match spec {
        EntrySpec::A2aAgent(a) => a.card.as_ref().expect("a pinned card").json.clone(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn agent_cards_are_pinned() {
    let app = app().await;
    let git = GitServer::start().await;
    let l = lookups(false).await;
    l.images
        .lock()
        .unwrap()
        .insert("team/tool:1.0".into(), digest('a'));
    let path = "planner/.well-known/agent-card.json";
    commit_all(
        &git.repo("cat"),
        &catalog_files(&l.registry, &l.agents),
        "first",
    );
    add_source(&app, "cat", SourceKind::Catalog, &git.url("cat"), true).await;

    // Unreachable at its first sync: refused.
    let report = sync_source(&app.state, &source_name("cat"), true)
        .await
        .unwrap();
    let refusal = report
        .refused
        .iter()
        .find(|r| r.name.as_deref() == Some("planner"))
        .unwrap();
    assert!(refusal.reason.contains("404"), "{refusal:?}");

    l.cards.lock().unwrap().insert(path.into(), card("Planner"));
    sync_source(&app.state, &source_name("cat"), true)
        .await
        .unwrap();
    app.store
        .apply_source(&source_name("cat"), time::OffsetDateTime::now_utc())
        .await
        .unwrap();
    let planner = app
        .store
        .get_entry(&key(EntryKind::A2aAgent, "cat", "planner"))
        .await
        .unwrap();
    assert_eq!(
        agent_card_of(&planner.applied.unwrap().spec)["name"],
        "Planner"
    );

    // A changed card is an update that waits.
    l.cards
        .lock()
        .unwrap()
        .insert(path.into(), card("Planner Two"));
    sync_source(&app.state, &source_name("cat"), true)
        .await
        .unwrap();
    let planner = app
        .store
        .get_entry(&key(EntryKind::A2aAgent, "cat", "planner"))
        .await
        .unwrap();
    assert_eq!(
        agent_card_of(&planner.applied.unwrap().spec)["name"],
        "Planner"
    );
    assert_eq!(
        agent_card_of(&planner.pending.unwrap().spec)["name"],
        "Planner Two"
    );
}

#[tokio::test]
async fn the_v02_card_path_is_tried_after_a_404() {
    let app = app().await;
    let git = GitServer::start().await;
    let l = lookups(false).await;
    l.images
        .lock()
        .unwrap()
        .insert("team/tool:1.0".into(), digest('a'));
    l.cards.lock().unwrap().insert(
        "planner/.well-known/agent.json".into(),
        json!({"name": "Old Planner", "url": "https://agents.example/a2a", "protocolVersion": "0.2.6"}),
    );
    commit_all(
        &git.repo("cat"),
        &catalog_files(&l.registry, &l.agents),
        "first",
    );
    add_source(&app, "cat", SourceKind::Catalog, &git.url("cat"), true).await;
    let report = sync_source(&app.state, &source_name("cat"), true)
        .await
        .unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let planner = app
        .store
        .get_entry(&key(EntryKind::A2aAgent, "cat", "planner"))
        .await
        .unwrap();
    assert_eq!(
        agent_card_of(&planner.applied.unwrap().spec)["name"],
        "Old Planner"
    );
}

#[tokio::test]
async fn git_goes_only_to_the_checked_address() {
    let app = app().await;
    let git = GitServer::start().await;
    git.repo("team")
        .commit(&[("pdf/SKILL.md", &skill_md("pdf", "v1"))], "first");
    add_source(&app, "team", SourceKind::Skills, &git.url("team"), false).await;
    let report = sync_source(&app.state, &source_name("team"), true)
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Failed);
    assert!(
        report
            .error
            .unwrap()
            .contains("needs allow_private_network")
    );
    assert_eq!(git.hits.load(Ordering::SeqCst), 0, "git never ran");

    add_source(
        &app,
        "metadata",
        SourceKind::Skills,
        "https://169.254.169.254/r.git",
        true,
    )
    .await;
    let report = sync_source(&app.state, &source_name("metadata"), true)
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Failed);
    assert!(report.error.unwrap().contains("metadata"));
}

#[tokio::test]
async fn git_pins_the_checked_address() {
    // A host no resolver knows: git reaches the server only through the pin.
    let git = GitServer::start().await;
    let commit = git.repo("team").commit(&[("a.txt", b"pinned")], "first");
    let port = git.base.rsplit(':').next().unwrap();
    let url =
        SourceUrl::parse(&format!("http://pinned.invalid:{port}/git/team.git"), true).unwrap();
    let pins: Vec<SocketAddr> = vec![format!("127.0.0.1:{port}").parse().unwrap()];
    let src = GitSource {
        url: &url,
        git_ref: None,
        username: "x-access-token",
        token: None,
        pins: &pins,
    };
    let runner = Git::new("git".into(), true);
    assert_eq!(runner.ls_remote(&src).await.unwrap(), commit);
    let dest = tempfile::tempdir().unwrap();
    assert_eq!(runner.checkout(&src, dest.path()).await.unwrap(), commit);
    assert_eq!(std::fs::read(dest.path().join("a.txt")).unwrap(), b"pinned");
}

#[tokio::test]
async fn outside_fetches_never_follow_redirects_or_reach_metadata() {
    let app = app().await;
    let git = GitServer::start().await;
    let target_hits = Arc::new(AtomicUsize::new(0));
    let counted = target_hits.clone();
    let (target, _) = serve(Router::new().fallback(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { axum::Json(card("Elsewhere")) }
    }))
    .await;
    let location = format!("{target}/card.json");
    let (redirector, _) = serve(Router::new().fallback(move || {
        let location = location.clone();
        async move { (StatusCode::FOUND, [(header::LOCATION, location)]).into_response() }
    }))
    .await;
    let agents = format!(
        "- apiVersion: sekisho/v1alpha1\n  kind: A2AAgent\n  name: moved\n  url: {redirector}/a2a\n\
         - apiVersion: sekisho/v1alpha1\n  kind: A2AAgent\n  name: metadata\n  url: http://169.254.169.254/a2a\n"
    );
    git.repo("cat")
        .commit(&[("agents.yaml", agents.as_bytes())], "first");
    add_source(&app, "cat", SourceKind::Catalog, &git.url("cat"), true).await;
    let report = sync_source(&app.state, &source_name("cat"), true)
        .await
        .unwrap();
    let reason = |n: &str| {
        report
            .refused
            .iter()
            .find(|r| r.name.as_deref() == Some(n))
            .unwrap_or_else(|| panic!("{n} not refused: {report:?}"))
            .reason
            .clone()
    };
    assert!(reason("moved").contains("302"), "{}", reason("moved"));
    assert!(
        reason("metadata").contains("metadata"),
        "{}",
        reason("metadata")
    );
    assert_eq!(
        target_hits.load(Ordering::SeqCst),
        0,
        "the redirect was never followed"
    );
}

/// Log lines written while a test runs.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_token_never_appears_in_errors_or_argv() {
    let logs = Captured::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let app = app().await;
    let token = "s3cr3t-token";
    let (base, _) = serve(Router::new().fallback(|| async {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"git\"")],
            "denied",
        )
    }))
    .await;
    let new = NewSource {
        name: source_name("private"),
        kind: SourceKind::Skills,
        url: SourceUrl::parse(&format!("{base}/git/private.git"), true).unwrap(),
        git_ref: None,
        folder: Folder::default(),
        username: Some("robot".into()),
        token: Some(token.into()),
        allow_private_network: true,
    };
    app.store.add_source(&app.sb, new, app.owner).await.unwrap();
    let report = sync_source(&app.state, &source_name("private"), true)
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Failed);
    let error = report.error.unwrap();
    assert!(!error.contains(token), "{error}");
    let stored = app.store.get_source(&source_name("private")).await.unwrap();
    assert!(!stored.latest_sync.unwrap().error.unwrap().contains(token));
    let logged = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(!logged.contains(token), "{logged}");
}

#[tokio::test]
async fn a_huge_repository_is_stopped() {
    let git = GitServer::start().await;
    // Two MiB that don't compress.
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let noise: Vec<u8> = (0..2 << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    git.repo("big").commit(&[("noise.bin", &noise)], "big");
    let url = SourceUrl::parse(&git.url("big"), true).unwrap();
    let pins: Vec<SocketAddr> = vec![git.base.trim_start_matches("http://").parse().unwrap()];
    let src = GitSource {
        url: &url,
        git_ref: None,
        username: "x-access-token",
        token: None,
        pins: &pins,
    };
    let dest = tempfile::tempdir().unwrap();
    let err = Git::new("git".into(), true)
        .with_limit(1 << 20)
        .checkout(&src, dest.path())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("1 MiB"), "{err}");
}

#[tokio::test]
async fn the_scheduler_syncs_due_sources() {
    let app = app().await;
    let git = GitServer::start().await;
    git.repo("team")
        .commit(&[("pdf/SKILL.md", &skill_md("pdf", "v1"))], "first");
    add_source(&app, "team", SourceKind::Skills, &git.url("team"), true).await;
    let scheduler = start_scheduler(app.state.clone(), Duration::from_secs(1));
    let mut synced = false;
    for _ in 0..100 {
        let source = app.store.get_source(&source_name("team")).await.unwrap();
        if source.applied_commit.is_some() {
            synced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    scheduler.abort();
    assert!(synced, "the scheduler synced the due source");
}

#[tokio::test]
async fn symlinks_and_submodules_in_git_are_refused() {
    let app = app().await;
    let git = GitServer::start().await;
    let repo = git.repo("links");
    repo.commit(
        &[
            ("pdf/SKILL.md", &skill_md("pdf", "Reads PDFs.")),
            ("notes/SKILL.md", &skill_md("notes", "Notes.")),
            ("sub/SKILL.md", &skill_md("sub", "Has a submodule.")),
        ],
        "skills",
    );
    // A symlink checked out with core.symlinks=false is a plain file
    // holding the target: Sekisho must still see it as a symlink.
    repo.commit_link("pdf/passwd", "/etc/passwd", false);
    repo.commit_link("sub/vendored", &"a".repeat(40), true);
    add_source(&app, "links", SourceKind::Skills, &git.url("links"), true).await;

    let report = sync_source(&app.state, &source_name("links"), true)
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Ok, "{report:?}");
    assert_eq!(names(&report.new), ["notes"], "{report:?}");
    let refused: Vec<(String, String)> = report
        .refused
        .iter()
        .map(|r| (r.name.clone().unwrap_or_default(), r.reason.clone()))
        .collect();
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert!(
        refused
            .iter()
            .any(|(n, r)| n == "pdf" && r.contains("pdf/passwd") && r.contains("symlink")),
        "{refused:?}"
    );
    assert!(
        refused
            .iter()
            .any(|(n, r)| n == "sub" && r.contains("sub/vendored") && r.contains("submodule")),
        "{refused:?}"
    );
}
