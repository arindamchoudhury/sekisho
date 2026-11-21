//! The install engine (plan 4, Task 4), driven through the test adapter,
//! which writes skills and prompts as whole files, MCP servers as JSON keys,
//! one JSON-with-comments key per environment and the model settings as
//! TOML keys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sekisho_catalog::delivery::{
    Defaults, DeliveredEntry, DeliveredFile, DeliveredModel, Delivery, EntryAddress, Registries,
};
use sekisho_catalog::model::{
    EntrySpec, McpRuntime, McpServerSpec, PromptSpec, SkillSpec, sha256_hex,
};
use sekisho_catalog::names::{EntryKind, EntryName, SourceName};
use sekisho_cli::adapters::{Adapter, ContentKind, ScopeKind, TestAdapter, Tool};
use sekisho_cli::install::{self, InstallError, Options, Report, Scope, Sign, ToolHomes};
use sekisho_core::formats::ClientApi;
use sekisho_core::names::{EnvName, ModelName};
use tempfile::TempDir;
use time::OffsetDateTime;

// ---------------------------------------------------------------- fixtures

struct World {
    _tmp: TempDir,
    project: PathBuf,
    config: PathBuf,
    homes: ToolHomes,
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl World {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let config = tmp.path().join("config");
        std::fs::create_dir_all(&project).unwrap();
        let homes = ToolHomes::new(
            Tool::ALL
                .iter()
                .map(|t| (*t, tmp.path().join("home").join(t.as_str())))
                .collect(),
        );
        Self {
            _tmp: tmp,
            project,
            config,
            homes,
            blobs: Mutex::new(HashMap::new()),
        }
    }

    fn project_scope(&self) -> Scope {
        Scope::project(&self.project, &self.config, self.homes.clone())
    }

    fn global_scope(&self) -> Scope {
        Scope::global(&self.config, self.homes.clone())
    }

    /// Keeps `bytes` where `fetch` finds them; their file entry.
    fn blob(&self, path: &str, bytes: &[u8]) -> DeliveredFile {
        let sha = sha256_hex(bytes);
        self.blobs
            .lock()
            .unwrap()
            .insert(sha.clone(), bytes.to_vec());
        DeliveredFile {
            path: path.into(),
            sha256: sha,
            size: bytes.len() as u64,
            executable: false,
        }
    }

    fn skill(&self, name: &str, body: &str) -> DeliveredEntry {
        self.skill_from("team", name, body)
    }

    fn skill_from(&self, source: &str, name: &str, body: &str) -> DeliveredEntry {
        let text = format!("---\nname: {name}\ndescription: The {name} skill.\n---\n{body}\n");
        entry(
            EntryKind::Skill,
            source,
            name,
            EntrySpec::Skill(SkillSpec {
                description: format!("The {name} skill."),
                license: None,
                compatibility: None,
            }),
            vec![self.blob("SKILL.md", text.as_bytes())],
        )
    }

    fn prompt(&self, source: &str, name: &str, body: &str) -> DeliveredEntry {
        entry(
            EntryKind::Prompt,
            source,
            name,
            EntrySpec::Prompt(PromptSpec {
                description: format!("The {name} prompt."),
            }),
            vec![self.blob(&format!("{name}.md"), body.as_bytes())],
        )
    }

    fn fetch(&self) -> impl Fn(&str) -> Result<Vec<u8>, InstallError> + '_ {
        |sha: &str| {
            self.blobs
                .lock()
                .unwrap()
                .get(sha)
                .cloned()
                .ok_or_else(|| InstallError::Io(format!("no blob {sha}")))
        }
    }

    fn install(&self, d: &Delivery, options: Options) -> Result<Report, InstallError> {
        self.install_with(d, &[&TestAdapter::new(Tool::ClaudeCode)], options)
    }

    fn install_with(
        &self,
        d: &Delivery,
        adapters: &[&dyn Adapter],
        options: Options,
    ) -> Result<Report, InstallError> {
        install::install(d, adapters, &self.project_scope(), &options, &self.fetch())
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.project.join(rel)
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    fn lock(&self) -> serde_json::Value {
        serde_json::from_str(&self.read(".sekisho/lock.json")).unwrap()
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

fn entry(
    kind: EntryKind,
    source: &str,
    name: &str,
    spec: EntrySpec,
    files: Vec<DeliveredFile>,
) -> DeliveredEntry {
    DeliveredEntry {
        kind,
        source: SourceName::parse(source).unwrap(),
        name: EntryName::parse(name).unwrap(),
        commit: "c1".into(),
        digest: sha256_hex(format!("{source}/{name}/{files:?}").as_bytes()),
        applied_at: OffsetDateTime::UNIX_EPOCH,
        tools: None,
        spec,
        files,
    }
}

fn server(source: &str, name: &str, url: &str) -> DeliveredEntry {
    entry(
        EntryKind::McpServer,
        source,
        name,
        EntrySpec::McpServer(McpServerSpec {
            description: format!("The {name} server."),
            runtime: McpRuntime::Remote { url: url.into() },
            config: Vec::new(),
            tool_preview: Vec::new(),
            unsupported_tools: Vec::new(),
            static_oauth: false,
        }),
        Vec::new(),
    )
}

fn env(name: &str) -> EnvName {
    EnvName::parse(name).unwrap()
}

fn delivery(name: &str, model: &str, entries: Vec<DeliveredEntry>) -> Delivery {
    let model = ModelName::parse(model).unwrap();
    Delivery {
        environment: env(name),
        notice: String::new(),
        revision: String::new(),
        gateway_url: format!("https://gw.example.com/env/{name}"),
        models: vec![DeliveredModel {
            name: model.clone(),
            apis: vec![ClientApi::Messages],
        }],
        defaults: Defaults {
            anthropic: Some(model),
            ..Defaults::default()
        },
        mcp: None,
        entries,
        quarantined: Vec::new(),
        registries: Registries::default(),
    }
    .seal()
}

fn yes() -> Options {
    Options {
        yes: true,
        ..Options::default()
    }
}

fn signs(report: &Report, sign: Sign) -> Vec<String> {
    let mut v: Vec<String> = report
        .items
        .iter()
        .filter(|i| i.sign == sign)
        .map(|i| i.what.clone())
        .collect();
    v.sort();
    v
}

/// Every file under `dir`, relative and `/`-separated, sorted.
fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for e in read {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

const SKILL: &str = ".claude-code/skills/deploy/SKILL.md";
const MCP: &str = ".claude-code/mcp.json";
const SETTINGS: &str = ".claude-code/settings.jsonc";
const CONFIG: &str = ".claude-code/config.toml";

// ------------------------------------------------------------------ tests

#[test]
fn a_first_install_writes_files_keys_and_the_lock() {
    let w = World::new();
    let d = delivery(
        "a",
        "sonnet",
        vec![
            w.skill("deploy", "Deploy it."),
            server("team", "db", "https://mcp.example.com/db"),
        ],
    );
    let report = w.install(&d, Options::default()).unwrap();
    assert!(w.read(SKILL).contains("Deploy it."));
    let mcp: serde_json::Value = serde_json::from_str(&w.read(MCP)).unwrap();
    assert_eq!(mcp["mcpServers"]["db"]["url"], "https://mcp.example.com/db");
    assert!(w.read(SETTINGS).contains(&d.revision));
    let config: toml_edit::DocumentMut = w.read(CONFIG).parse().unwrap();
    assert_eq!(config["model"].as_str(), Some("sonnet"));
    assert!(signs(&report, Sign::Added).len() >= 4, "{report:?}");
    assert_eq!(report.models, [(Tool::ClaudeCode, env("a"))]);

    let lock = w.lock();
    assert_eq!(lock["version"], 1);
    assert_eq!(lock["environments"]["a"]["revision"], d.revision.as_str());
    assert_eq!(lock["models"]["claude-code"], "a");
    // Task 3's reader still reads it.
    let installed = sekisho_cli::install::lock::installed(&w.path(".sekisho/lock.json")).unwrap();
    assert_eq!(installed.environments, ["a"]);
    // The run spec, for `sekisho run` (Task 7).
    assert_eq!(
        lock["environments"]["a"]["tools"]["claude-code"]["run"]["program"],
        "claude-code"
    );
}

#[test]
fn paths_cannot_leave_the_root() {
    use sekisho_cli::adapters::RelPath;
    for bad in [
        "",
        "../x",
        "/etc/x",
        "a/../../x",
        "a/./b",
        "a\\b",
        "a//b",
        "x\0y",
    ] {
        assert!(RelPath::parse(bad).is_err(), "{bad:?}");
    }
    for good in [".claude/skills/a/SKILL.md", "a/b.c"] {
        assert_eq!(RelPath::parse(good).unwrap().as_str(), good);
    }
    #[cfg(windows)]
    for bad in ["C:x", "a/CON", "a/nul.txt", "a/b.", "a/b "] {
        assert!(RelPath::parse(bad).is_err(), "{bad:?}");
    }
    // A skill whose file path leaves its folder is refused before any write.
    let w = World::new();
    let mut evil = w.skill("evil", "x");
    evil.files[0].path = "../../escape.md".into();
    let err = w
        .install(&delivery("a", "sonnet", vec![evil]), Options::default())
        .unwrap_err();
    assert!(matches!(err, InstallError::Refused(_)), "{err:?}");
    assert_eq!(err.exit().code(), 3);
    assert!(tree(&w.project).is_empty(), "{:?}", tree(&w.project));
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_written_through() {
    let w = World::new();
    let elsewhere = TempDir::new().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), w.path(".claude-code")).unwrap();
    let err = w
        .install(
            &delivery("a", "sonnet", vec![w.skill("deploy", "x")]),
            yes(),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::Refused(_)), "{err:?}");
    assert!(tree(elsewhere.path()).is_empty());
    std::fs::remove_file(w.path(".claude-code")).unwrap();

    // A symlinked file target.
    let target = elsewhere.path().join("target.md");
    std::fs::write(&target, "mine").unwrap();
    std::fs::create_dir_all(w.path(".claude-code/skills/deploy")).unwrap();
    std::os::unix::fs::symlink(&target, w.path(SKILL)).unwrap();
    let err = w
        .install(
            &delivery("a", "sonnet", vec![w.skill("deploy", "x")]),
            yes(),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::Refused(_)), "{err:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
}

#[test]
fn a_changed_file_stops_the_install() {
    let w = World::new();
    let d = delivery("a", "sonnet", vec![w.skill("deploy", "v1")]);
    w.install(&d, Options::default()).unwrap();
    let before = w.read(".sekisho/lock.json");

    let mut d2 = delivery(
        "a",
        "sonnet",
        vec![w.skill("deploy", "v2"), w.skill("other", "o")],
    );
    let sha = d2.entries[0].files[0].sha256.clone();
    w.blobs.lock().unwrap().insert(sha, b"tampered".to_vec());
    d2 = d2.seal();
    let err = w.install(&d2, Options::default()).unwrap_err();
    assert!(matches!(err, InstallError::Refused(_)), "{err:?}");
    assert!(err.to_string().contains("SHA-256"), "{err}");
    assert!(w.read(SKILL).contains("v1"));
    assert!(!w.exists(".claude-code/skills/other/SKILL.md"));
    assert_eq!(w.read(".sekisho/lock.json"), before);
}

#[test]
fn hidden_characters_stop_the_install() {
    let w = World::new();
    let err = w
        .install(
            &delivery("a", "sonnet", vec![w.skill("deploy", "run \u{202E}txt.sh")]),
            Options::default(),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::Refused(_)), "{err:?}");
    assert!(!w.exists(SKILL));

    let report = w
        .install(
            &delivery("a", "sonnet", vec![w.skill("deploy", "zero\u{200B}width")]),
            Options::default(),
        )
        .unwrap();
    assert!(w.exists(SKILL));
    assert!(
        report.warnings.iter().any(|x| x.contains("U+200B")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn the_users_files_and_keys_are_asked_about() {
    let w = World::new();
    w.write(
        MCP,
        r#"{"mcpServers": {"db": {"url": "https://mine.example.com"}}}"#,
    );
    let d = delivery(
        "a",
        "sonnet",
        vec![server("team", "db", "https://mcp.example.com/db")],
    );
    let err = w.install(&d, Options::default()).unwrap_err();
    match &err {
        InstallError::NeedsConsent(items) => {
            assert!(
                items.iter().any(|i| i.contains("mcpServers.db")),
                "{items:?}"
            )
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(err.exit().code(), 3);
    assert!(w.read(MCP).contains("mine.example.com"));
    assert!(!w.exists(".sekisho/lock.json"));

    w.install(&d, yes()).unwrap();
    assert!(w.read(MCP).contains("mcp.example.com/db"));

    // The user's own skill file, identical: adopted without asking.
    let w = World::new();
    let skill = w.skill("deploy", "Deploy it.");
    let text = String::from_utf8(w.blobs.lock().unwrap()[&skill.files[0].sha256].clone()).unwrap();
    w.write(SKILL, &text);
    w.install(&delivery("a", "sonnet", vec![skill]), Options::default())
        .unwrap();
    // Different: asked about.
    let w2 = World::new();
    w2.write(SKILL, "my own deploy skill");
    let err = w2
        .install(
            &delivery("a", "sonnet", vec![w2.skill("deploy", "x")]),
            Options::default(),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::NeedsConsent(_)), "{err:?}");
    assert_eq!(w2.read(SKILL), "my own deploy skill");
}

#[test]
fn comments_and_other_keys_survive_a_toml_merge() {
    let w = World::new();
    w.write(
        CONFIG,
        "# my settings\ntheme = \"dark\" # keep\n\n[profile]\nname = \"me\"\n",
    );
    w.write(SETTINGS, "{\n  // mine\n  \"editor\": \"vim\",\n}\n");
    let d = delivery("a", "sonnet", vec![]);
    w.install(&d, Options::default()).unwrap();
    let config = w.read(CONFIG);
    assert!(config.contains("# my settings"), "{config}");
    assert!(config.contains("theme = \"dark\" # keep"), "{config}");
    assert!(config.contains("[profile]"), "{config}");
    assert!(config.contains("model = \"sonnet\""), "{config}");
    let settings = w.read(SETTINGS);
    assert!(settings.contains("// mine"), "{settings}");
    assert!(settings.contains("\"editor\": \"vim\""), "{settings}");
    assert!(settings.contains(&d.revision), "{settings}");

    // Uninstall takes Sekisho's keys out and leaves the rest.
    install::uninstall(&w.project_scope(), None, None, &Options::default()).unwrap();
    let config = w.read(CONFIG);
    assert!(!config.contains("model"), "{config}");
    assert!(config.contains("theme = \"dark\" # keep"), "{config}");
    assert!(w.read(SETTINGS).contains("// mine"));
}

#[test]
fn entries_that_left_are_removed() {
    let w = World::new();
    w.install(
        &delivery(
            "a",
            "sonnet",
            vec![w.skill("deploy", "d"), w.skill("lint", "l")],
        ),
        Options::default(),
    )
    .unwrap();
    let report = w
        .install(
            &delivery("a", "sonnet", vec![w.skill("deploy", "d")]),
            Options::default(),
        )
        .unwrap();
    assert!(!w.exists(".claude-code/skills/lint/SKILL.md"));
    assert!(
        !w.exists(".claude-code/skills/lint"),
        "the empty folder goes too"
    );
    assert!(w.exists(SKILL));
    assert_eq!(
        signs(&report, Sign::Removed),
        [".claude-code/skills/lint/SKILL.md"]
    );
}

#[test]
fn quarantined_entries_are_removed() {
    let w = World::new();
    let lint = w.skill("lint", "l");
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "d"), lint.clone()]),
        Options::default(),
    )
    .unwrap();
    let mut d = delivery("a", "sonnet", vec![w.skill("deploy", "d")]);
    d.quarantined.push(EntryAddress {
        kind: lint.kind,
        source: lint.source.clone(),
        name: lint.name.clone(),
    });
    let report = w.install(&d.seal(), Options::default()).unwrap();
    assert!(!w.exists(".claude-code/skills/lint/SKILL.md"));
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("quarantined") && x.contains("lint")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn an_edited_file_is_kept_and_reported() {
    let w = World::new();
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "v1")]),
        Options::default(),
    )
    .unwrap();
    w.write(SKILL, "my edit");
    let report = w
        .install(
            &delivery("a", "sonnet", vec![w.skill("deploy", "v2")]),
            Options::default(),
        )
        .unwrap();
    assert_eq!(w.read(SKILL), "my edit");
    assert_eq!(signs(&report, Sign::Kept), [SKILL]);
    // Removing it keeps it too.
    let report = w
        .install(&delivery("a", "sonnet", vec![]), Options::default())
        .unwrap();
    assert_eq!(w.read(SKILL), "my edit");
    assert_eq!(signs(&report, Sign::Kept), [SKILL]);

    let w = World::new();
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "v1")]),
        Options::default(),
    )
    .unwrap();
    w.write(SKILL, "my edit");
    let forced = Options {
        force: true,
        ..Options::default()
    };
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "v2")]),
        forced,
    )
    .unwrap();
    assert!(w.read(SKILL).contains("v2"));
}

#[test]
fn a_file_two_tools_share_goes_with_the_last() {
    let w = World::new();
    let claude = TestAdapter::new(Tool::ClaudeCode).shared_mcp();
    let copilot = TestAdapter::new(Tool::CopilotCli).shared_mcp();
    let d = delivery(
        "a",
        "sonnet",
        vec![server("team", "db", "https://mcp.example.com/db")],
    );
    w.install_with(&d, &[&claude, &copilot], Options::default())
        .unwrap();
    assert!(w.read(".shared/mcp.json").contains("mcp.example.com/db"));
    // Claude Code alone stops getting it: the file stays for Copilot CLI.
    install::uninstall(
        &w.project_scope(),
        None,
        Some(&[Tool::ClaudeCode]),
        &Options::default(),
    )
    .unwrap();
    assert!(w.read(".shared/mcp.json").contains("mcp.example.com/db"));
    install::uninstall(
        &w.project_scope(),
        None,
        Some(&[Tool::CopilotCli]),
        &Options::default(),
    )
    .unwrap();
    assert!(!w.exists(".shared/mcp.json"), "removed with the last");
}

#[test]
fn dry_run_writes_nothing_and_shows_the_same_items() {
    let w = World::new();
    let d = delivery("a", "sonnet", vec![w.skill("deploy", "d")]);
    let dry = w
        .install(
            &d,
            Options {
                dry_run: true,
                ..Options::default()
            },
        )
        .unwrap();
    assert!(tree(&w.project).is_empty(), "{:?}", tree(&w.project));
    let real = w.install(&d, Options::default()).unwrap();
    assert_eq!(dry.items, real.items);

    // An update shows its diff.
    let dry = w
        .install(
            &delivery("a", "sonnet", vec![w.skill("deploy", "changed")]),
            Options {
                dry_run: true,
                ..Options::default()
            },
        )
        .unwrap();
    let item = dry.items.iter().find(|i| i.what == SKILL).unwrap();
    assert_eq!(item.sign, Sign::Updated);
    let diff = item.diff.as_deref().unwrap();
    assert!(diff.contains("-d") && diff.contains("+changed"), "{diff}");
    assert!(w.read(SKILL).contains("\nd\n"));
}

#[test]
fn a_second_environment_adds_its_content() {
    let w = World::new();
    w.install(
        &delivery(
            "a",
            "sonnet",
            vec![w.skill("deploy", "d"), server("team", "db", "https://db")],
        ),
        Options::default(),
    )
    .unwrap();
    let before = w.read(SKILL);
    w.install(
        &delivery(
            "b",
            "haiku",
            vec![w.skill("lint", "l"), server("ops", "logs", "https://logs")],
        ),
        Options::default(),
    )
    .unwrap();
    assert_eq!(w.read(SKILL), before);
    assert!(w.exists(".claude-code/skills/lint/SKILL.md"));
    let mcp: serde_json::Value = serde_json::from_str(&w.read(MCP)).unwrap();
    assert_eq!(mcp["mcpServers"]["db"]["url"], "https://db");
    assert_eq!(mcp["mcpServers"]["logs"]["url"], "https://logs");
    let lock = w.lock();
    let envs: Vec<&String> = lock["environments"].as_object().unwrap().keys().collect();
    assert_eq!(envs, ["a", "b"]);
    // b isn't the model environment: a's model stays.
    assert!(w.read(CONFIG).contains("model = \"sonnet\""));
}

#[test]
fn a_clash_between_environments_is_refused() {
    let w = World::new();
    w.install(
        &delivery(
            "a",
            "sonnet",
            vec![
                w.prompt("team", "deploy", "a's"),
                server("team", "db", "https://a"),
            ],
        ),
        Options::default(),
    )
    .unwrap();
    let lock = w.read(".sekisho/lock.json");
    let err = w
        .install(
            &delivery(
                "b",
                "sonnet",
                vec![w.prompt("ops", "deploy", "b's"), w.skill("lint", "l")],
            ),
            yes(),
        )
        .unwrap_err();
    match &err {
        InstallError::Clash(clashes) => {
            assert_eq!(clashes.len(), 1, "{clashes:?}");
            assert_eq!(clashes[0].kind, ContentKind::Prompts);
            assert_eq!(clashes[0].name, "deploy");
            assert_eq!(clashes[0].held_by, env("a"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(err.exit().code(), 3);
    assert!(
        err.to_string().contains("\"deploy\"") && err.to_string().contains("\"a\""),
        "{err}"
    );
    assert!(!w.exists(".claude-code/skills/lint/SKILL.md"));
    assert_eq!(w.read(".sekisho/lock.json"), lock);

    // A key: the same MCP server name from another entry.
    let err = w
        .install(
            &delivery("b", "sonnet", vec![server("ops", "db", "https://b")]),
            yes(),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::Clash(_)), "{err:?}");

    // b installed; then a re-use of a bringing b's prompt name stops, a stays.
    w.install(
        &delivery("b", "sonnet", vec![w.prompt("ops", "review", "b's")]),
        Options::default(),
    )
    .unwrap();
    let a_revision = w.lock()["environments"]["a"]["revision"].clone();
    let err = w
        .install(
            &delivery(
                "a",
                "sonnet",
                vec![
                    w.prompt("team", "review", "a's"),
                    server("team", "db", "https://a"),
                ],
            ),
            yes(),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::Clash(_)), "{err:?}");
    assert_eq!(w.lock()["environments"]["a"]["revision"], a_revision);
}

#[test]
fn the_same_entry_in_two_environments_is_shared() {
    let w = World::new();
    let deploy = w.skill("deploy", "d");
    w.install(
        &delivery("a", "sonnet", vec![deploy.clone()]),
        Options::default(),
    )
    .unwrap();
    let report = w
        .install(
            &delivery("b", "sonnet", vec![deploy.clone()]),
            Options::default(),
        )
        .unwrap();
    assert!(
        signs(&report, Sign::Added).iter().all(|s| s != SKILL),
        "{report:?}"
    );
    install::uninstall(
        &w.project_scope(),
        Some(&[env("a")]),
        None,
        &Options::default(),
    )
    .unwrap();
    assert!(w.exists(SKILL), "b still delivers it");
    install::uninstall(
        &w.project_scope(),
        Some(&[env("b")]),
        None,
        &Options::default(),
    )
    .unwrap();
    assert!(!w.exists(SKILL));
}

#[test]
fn the_same_entry_at_another_digest_is_updated() {
    let w = World::new();
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "x")]),
        Options::default(),
    )
    .unwrap();
    let mut newer = w.skill("deploy", "y");
    newer.commit = "c2".into();
    let report = w
        .install(&delivery("b", "sonnet", vec![newer]), Options::default())
        .unwrap();
    assert!(w.read(SKILL).contains("\ny\n"));
    assert_eq!(signs(&report, Sign::Updated), [SKILL]);
    // Counted for both: a's uninstall keeps it.
    install::uninstall(
        &w.project_scope(),
        Some(&[env("a")]),
        None,
        &Options::default(),
    )
    .unwrap();
    assert!(w.exists(SKILL));
}

#[test]
fn uninstalling_one_environment_keeps_the_others() {
    let w = World::new();
    w.install(
        &delivery(
            "a",
            "sonnet",
            vec![w.skill("deploy", "d"), server("team", "db", "https://db")],
        ),
        Options::default(),
    )
    .unwrap();
    w.install(
        &delivery(
            "b",
            "sonnet",
            vec![w.skill("lint", "l"), server("ops", "logs", "https://logs")],
        ),
        Options::default(),
    )
    .unwrap();
    let report = install::uninstall(
        &w.project_scope(),
        Some(&[env("b")]),
        None,
        &Options::default(),
    )
    .unwrap();
    assert!(!w.exists(".claude-code/skills/lint/SKILL.md"));
    assert!(w.exists(SKILL));
    let mcp: serde_json::Value = serde_json::from_str(&w.read(MCP)).unwrap();
    assert!(mcp["mcpServers"].get("logs").is_none(), "{mcp}");
    assert_eq!(mcp["mcpServers"]["db"]["url"], "https://db");
    let lock = w.lock();
    assert!(lock["environments"].get("b").is_none());
    assert!(lock["environments"].get("a").is_some());
    assert!(!signs(&report, Sign::Removed).is_empty());
}

#[test]
fn models_come_from_one_environment_per_tool() {
    let w = World::new();
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "d")]),
        Options::default(),
    )
    .unwrap();
    w.install(
        &delivery("b", "haiku", vec![w.skill("lint", "l")]),
        Options::default(),
    )
    .unwrap();
    assert!(w.read(CONFIG).contains("model = \"sonnet\""));
    // use b --models moves them.
    let report = w
        .install(
            &delivery("b", "haiku", vec![w.skill("lint", "l")]),
            Options {
                models: true,
                ..Options::default()
            },
        )
        .unwrap();
    assert!(w.read(CONFIG).contains("model = \"haiku\""));
    assert!(w.exists(SKILL), "a's other content stays");
    assert_eq!(report.models, [(Tool::ClaudeCode, env("b"))]);
    assert_eq!(w.lock()["models"]["claude-code"], "b");
    // Uninstalling the model environment hands them to the earliest left.
    let report = install::uninstall(
        &w.project_scope(),
        Some(&[env("b")]),
        None,
        &Options::default(),
    )
    .unwrap();
    assert!(w.read(CONFIG).contains("model = \"sonnet\""));
    assert_eq!(report.models, [(Tool::ClaudeCode, env("a"))]);
    // None left: the model keys go.
    install::uninstall(
        &w.project_scope(),
        Some(&[env("a")]),
        None,
        &Options::default(),
    )
    .unwrap();
    assert!(!w.exists(CONFIG) || !w.read(CONFIG).contains("model"));
}

#[test]
fn a_global_install_of_another_environment_is_reported() {
    let w = World::new();
    let global = w.global_scope();
    install::install(
        &delivery(
            "b",
            "sonnet",
            vec![w.skill("deploy", "d"), server("ops", "db", "https://db")],
        ),
        &[&TestAdapter::new(Tool::ClaudeCode)],
        &global,
        &Options::default(),
        &w.fetch(),
    )
    .unwrap();
    let home = w.homes.get(Tool::ClaudeCode).to_path_buf();
    assert!(home.join(".claude-code/skills/deploy/SKILL.md").exists());

    let report = w
        .install(
            &delivery("a", "sonnet", vec![w.skill("deploy", "d")]),
            Options::default(),
        )
        .unwrap();
    assert!(home.join(".claude-code/skills/deploy/SKILL.md").exists());
    let mut layered: Vec<(String, ContentKind, String, ScopeKind)> = report
        .layered
        .iter()
        .map(|l| (l.other_env.to_string(), l.kind, l.name.clone(), l.used))
        .collect();
    layered.sort_by(|x, y| x.2.cmp(&y.2));
    assert_eq!(
        layered,
        [
            (
                "b".into(),
                ContentKind::McpServers,
                "db".into(),
                ScopeKind::Global
            ),
            (
                "b".into(),
                ContentKind::Skills,
                "deploy".into(),
                ScopeKind::Global
            ),
        ]
    );
    // The same environment in both scopes: nothing to report.
    let report = w
        .install(
            &delivery("b", "sonnet", vec![w.skill("deploy", "d")]),
            Options::default(),
        )
        .unwrap();
    assert!(
        report.layered.iter().all(|l| l.other_env != env("b")),
        "{:?}",
        report.layered
    );
}

#[test]
fn uninstall_removes_everything_it_wrote_and_nothing_else() {
    let w = World::new();
    w.write("README.md", "mine");
    w.write(".claude-code/notes.md", "mine too");
    w.write(MCP, r#"{"theirs": 1}"#);
    w.install(
        &delivery(
            "a",
            "sonnet",
            vec![w.skill("deploy", "d"), server("team", "db", "https://db")],
        ),
        Options::default(),
    )
    .unwrap();
    install::uninstall(&w.project_scope(), None, None, &Options::default()).unwrap();
    assert_eq!(
        tree(&w.project),
        [
            ".claude-code/mcp.json",
            ".claude-code/notes.md",
            "README.md"
        ]
    );
    let mcp: serde_json::Value = serde_json::from_str(&w.read(MCP)).unwrap();
    assert_eq!(mcp, serde_json::json!({"theirs": 1}));
}

#[test]
fn the_exclude_block_lists_what_was_written() {
    let w = World::new();
    std::fs::create_dir_all(w.path(".git/info")).unwrap();
    w.write(".git/info/exclude", "# git's own\n*.log\n");
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "d")]),
        Options::default(),
    )
    .unwrap();
    let exclude = w.read(".git/info/exclude");
    assert!(exclude.starts_with("# git's own\n*.log\n"), "{exclude}");
    assert!(exclude.contains("/.sekisho/\n"), "{exclude}");
    assert!(exclude.contains(&format!("/{SKILL}\n")), "{exclude}");
    install::uninstall(&w.project_scope(), None, None, &Options::default()).unwrap();
    assert_eq!(w.read(".git/info/exclude"), "# git's own\n*.log\n");

    // No .git: no block, and no .git made.
    let w = World::new();
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "d")]),
        Options::default(),
    )
    .unwrap();
    assert!(!w.exists(".git"));
}

#[test]
fn a_failed_write_leaves_the_previous_install() {
    let w = World::new();
    w.install(
        &delivery("a", "sonnet", vec![w.skill("deploy", "v1")]),
        Options::default(),
    )
    .unwrap();
    let lock = w.read(".sekisho/lock.json");
    // A folder where a new file must go: the write fails after deploy's.
    std::fs::create_dir_all(w.path(".claude-code/skills/zzz/SKILL.md")).unwrap();
    let err = w
        .install(
            &delivery(
                "a",
                "sonnet",
                vec![w.skill("deploy", "v2"), w.skill("zzz", "z")],
            ),
            yes(),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::Io(_)), "{err:?}");
    assert!(w.read(SKILL).contains("v1"), "rolled back");
    assert_eq!(w.read(".sekisho/lock.json"), lock);
}

#[cfg(unix)]
#[test]
fn a_rollback_keeps_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt as _;
    let w = World::new();
    let mut tool = w.skill("deploy", "v1");
    let script = w.blob("run.sh", b"#!/bin/sh\necho v1\n");
    tool.files.push(DeliveredFile {
        executable: true,
        ..script
    });
    w.install(&delivery("a", "sonnet", vec![tool]), Options::default())
        .unwrap();
    let run = ".claude-code/skills/deploy/run.sh";
    assert_ne!(
        std::fs::metadata(w.path(run)).unwrap().permissions().mode() & 0o111,
        0
    );
    let mut changed = w.skill("deploy", "v1");
    let script = w.blob("run.sh", b"#!/bin/sh\necho v2\n");
    changed.files.push(DeliveredFile {
        executable: true,
        ..script
    });
    std::fs::create_dir_all(w.path(".claude-code/skills/zzz/SKILL.md")).unwrap();
    let err = w
        .install(
            &delivery("a", "sonnet", vec![changed, w.skill("zzz", "z")]),
            yes(),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::Io(_)), "{err:?}");
    assert!(w.read(run).contains("v1"));
    assert_ne!(
        std::fs::metadata(w.path(run)).unwrap().permissions().mode() & 0o111,
        0,
        "put back as it was"
    );
}
