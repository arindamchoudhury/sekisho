//! What Claude Code gets from an environment (plan 4, Task 6; D193, D214,
//! D216), through the install engine with the real adapter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sekisho_catalog::delivery::{
    Defaults, DeliveredEntry, DeliveredFile, DeliveredModel, Delivery, Registries,
};
use sekisho_catalog::model::{
    EntrySpec, McpRuntime, McpServerSpec, PluginSpec, PromptSpec, SkillSpec, sha256_hex,
};
use sekisho_catalog::names::{EntryKind, EntryName, SourceName};
use sekisho_catalog::pins::NpmPackage;
use sekisho_cli::adapters::{ClaudeCode, ContentKind, ScopeKind, Tool};
use sekisho_cli::commands::use_notices;
use sekisho_cli::install::{self, InstallError, Options, Report, Scope, ToolHomes};
use sekisho_core::formats::ClientApi;
use sekisho_core::names::{EnvName, ModelName};
use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;

struct World {
    tmp: TempDir,
    project: PathBuf,
    config: PathBuf,
    home: PathBuf,
    blobs: std::sync::Mutex<HashMap<String, Vec<u8>>>,
}

impl World {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        Self {
            project,
            config: tmp.path().join("config"),
            home: tmp.path().join("home"),
            tmp,
            blobs: Default::default(),
        }
    }

    /// The tools' homes as the process's variables give them.
    fn homes(&self, vars: &[(&str, &str)]) -> ToolHomes {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        ToolHomes::from_env(&|k| vars.get(k).cloned(), &self.home)
    }

    fn project_scope(&self) -> Scope {
        Scope::project(&self.project, &self.config, self.homes(&[]))
    }

    fn file(&self, path: &str, text: &str) -> DeliveredFile {
        let sha = sha256_hex(text.as_bytes());
        self.blobs
            .lock()
            .unwrap()
            .insert(sha.clone(), text.as_bytes().to_vec());
        DeliveredFile {
            path: path.into(),
            sha256: sha,
            size: text.len() as u64,
            executable: false,
        }
    }

    fn skill(&self, name: &str, body: &str) -> DeliveredEntry {
        entry(
            EntryKind::Skill,
            name,
            EntrySpec::Skill(SkillSpec {
                description: String::new(),
                license: None,
                compatibility: None,
            }),
            vec![self.file("SKILL.md", &format!("---\nname: {name}\n---\n{body}\n"))],
        )
    }

    fn prompt(&self, name: &str) -> DeliveredEntry {
        entry(
            EntryKind::Prompt,
            name,
            EntrySpec::Prompt(PromptSpec {
                description: String::new(),
            }),
            vec![self.file(&format!("{name}.md"), "Review this.\n")],
        )
    }

    fn plugin(&self, name: &str) -> DeliveredEntry {
        entry(
            EntryKind::Plugin,
            name,
            EntrySpec::Plugin(PluginSpec {
                description: "Formats.".into(),
                version: None,
                mcp_servers: Vec::new(),
                apm_checked: false,
            }),
            vec![
                self.file(
                    ".claude-plugin/plugin.json",
                    &format!(r#"{{"name": "{name}"}}"#),
                ),
                self.file("commands/run.md", "Run it.\n"),
            ],
        )
    }

    fn install(
        &self,
        scope: &Scope,
        d: &Delivery,
        options: Options,
    ) -> Result<Report, InstallError> {
        let fetch = |sha: &str| -> Result<Vec<u8>, InstallError> {
            Ok(self.blobs.lock().unwrap()[sha].clone())
        };
        install::install(d, &[&ClaudeCode], scope, &options, &fetch)
    }
}

fn entry(
    kind: EntryKind,
    name: &str,
    spec: EntrySpec,
    files: Vec<DeliveredFile>,
) -> DeliveredEntry {
    DeliveredEntry {
        kind,
        source: SourceName::parse("team").unwrap(),
        name: EntryName::parse(name).unwrap(),
        commit: "c1".into(),
        digest: sha256_hex(format!("{name}{files:?}").as_bytes()),
        applied_at: OffsetDateTime::UNIX_EPOCH,
        tools: None,
        spec,
        files,
    }
}

fn server(name: &str) -> DeliveredEntry {
    entry(
        EntryKind::McpServer,
        name,
        EntrySpec::McpServer(McpServerSpec {
            description: String::new(),
            runtime: McpRuntime::Npx {
                package: NpmPackage::parse_exact(&format!("@acme/{name}@1.0.0")).unwrap(),
                args: Vec::new(),
            },
            config: Vec::new(),
            tool_preview: Vec::new(),
            unsupported_tools: Vec::new(),
            static_oauth: false,
        }),
        Vec::new(),
    )
}

fn model(name: &str, apis: &[ClientApi]) -> DeliveredModel {
    DeliveredModel {
        name: ModelName::parse(name).unwrap(),
        apis: apis.to_vec(),
    }
}

fn delivery(env: &str, default: Option<&str>, entries: Vec<DeliveredEntry>) -> Delivery {
    Delivery {
        environment: EnvName::parse(env).unwrap(),
        notice: String::new(),
        revision: String::new(),
        gateway_url: format!("https://gw.example.com/env/{env}"),
        models: vec![
            model("sonnet", &[ClientApi::Messages]),
            model("haiku", &[ClientApi::Messages]),
            model("gpt", &[ClientApi::ChatCompletions]),
        ],
        defaults: Defaults {
            anthropic: default.map(|m| ModelName::parse(m).unwrap()),
            openai: Some(ModelName::parse("gpt").unwrap()),
            gemini: None,
            small: Some(ModelName::parse("haiku").unwrap()),
        },
        mcp: None,
        entries,
        quarantined: Vec::new(),
        registries: Registries::default(),
    }
    .seal()
}

fn json_at(path: &Path) -> Value {
    serde_json::from_slice(
        &std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

#[test]
fn claude_code_gets_models_and_the_helper() {
    let w = World::new();
    let d = delivery(
        "a",
        Some("sonnet"),
        vec![
            w.skill("deploy", "Deploy."),
            w.prompt("review"),
            server("fetch"),
        ],
    );
    w.install(&w.project_scope(), &d, Options::default())
        .unwrap();
    let settings = json_at(&w.project.join(".claude/settings.local.json"));
    assert_eq!(settings["apiKeyHelper"], "sekisho token");
    assert_eq!(settings["model"], "sonnet");
    let env = &settings["env"];
    assert_eq!(
        env["ANTHROPIC_BASE_URL"],
        "https://gw.example.com/env/a/anthropic"
    );
    assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "sonnet");
    assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "sonnet");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "haiku");
    assert_eq!(env["CLAUDE_CODE_SUBAGENT_MODEL"], "sonnet");
    assert_eq!(env["CLAUDE_CODE_API_KEY_HELPER_TTL_MS"], "3000000");
    assert_eq!(settings["enabledMcpjsonServers"], json!(["fetch"]));
    let mcp = json_at(&w.project.join(".mcp.json"));
    assert_eq!(mcp["mcpServers"]["fetch"]["command"], "npx");
    assert!(w.project.join(".claude/skills/deploy/SKILL.md").exists());
    assert!(w.project.join(".claude/commands/review.md").exists());

    // A path with a space is quoted for the shell Claude Code runs it in.
    let w = World::new();
    let options = Options {
        sekisho: Some("/opt/my tools/sekisho".into()),
        ..Options::default()
    };
    w.install(
        &w.project_scope(),
        &delivery("a", Some("sonnet"), vec![]),
        options,
    )
    .unwrap();
    let helper = json_at(&w.project.join(".claude/settings.local.json"))["apiKeyHelper"].clone();
    let quoted = if cfg!(windows) {
        r#""/opt/my tools/sekisho" token"#
    } else {
        "'/opt/my tools/sekisho' token"
    };
    assert_eq!(helper, quoted);
}

#[test]
fn its_plugins_are_a_marketplace() {
    let w = World::new();
    w.install(
        &w.project_scope(),
        &delivery("a", Some("sonnet"), vec![w.plugin("fmt")]),
        Options::default(),
    )
    .unwrap();
    let market = w.project.join(".sekisho/claude-plugins/a");
    let listing = json_at(&market.join(".claude-plugin/marketplace.json"));
    assert_eq!(listing["name"], "sekisho-a");
    assert_eq!(listing["plugins"][0]["name"], "fmt");
    assert_eq!(listing["plugins"][0]["source"], "./plugins/fmt");
    assert!(
        market
            .join("plugins/fmt/.claude-plugin/plugin.json")
            .exists()
    );
    assert!(market.join("plugins/fmt/commands/run.md").exists());
    let settings = json_at(&w.project.join(".claude/settings.local.json"));
    let source = &settings["extraKnownMarketplaces"]["sekisho-a"]["source"];
    assert_eq!(source["source"], "directory");
    assert_eq!(Path::new(source["path"].as_str().unwrap()), market);
    assert_eq!(settings["enabledPlugins"]["fmt@sekisho-a"], true);
}

#[test]
fn no_anthropic_default_skips_models() {
    let w = World::new();
    let report = w
        .install(
            &w.project_scope(),
            &delivery("a", None, vec![w.skill("deploy", "d")]),
            Options::default(),
        )
        .unwrap();
    assert!(w.project.join(".claude/skills/deploy/SKILL.md").exists());
    let settings = w.project.join(".claude/settings.local.json");
    if settings.exists() {
        let s = json_at(&settings);
        for key in ["apiKeyHelper", "model", "env"] {
            assert!(s.get(key).is_none(), "{key}: {s}");
        }
    }
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("no model the Anthropic API reaches")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn a_global_install_honours_claude_config_dir() {
    let w = World::new();
    let dir = w.tmp.path().join("claude-work");
    let homes = w.homes(&[("CLAUDE_CONFIG_DIR", dir.to_str().unwrap())]);
    let global = Scope::global(&w.config, homes);
    let d = delivery(
        "a",
        Some("sonnet"),
        vec![w.skill("deploy", "d"), server("fetch"), w.plugin("fmt")],
    );
    w.install(&global, &d, Options::default()).unwrap();
    let settings = json_at(&dir.join("settings.json"));
    assert_eq!(settings["apiKeyHelper"], "sekisho token");
    assert!(dir.join("skills/deploy/SKILL.md").exists());
    assert_eq!(
        json_at(&dir.join(".claude.json"))["mcpServers"]["fetch"]["command"],
        "npx"
    );
    assert!(
        w.config
            .join("claude-plugins/a/.claude-plugin/marketplace.json")
            .exists()
    );
    assert!(!w.home.join(".claude.json").exists());

    // Without it: ~/.claude and ~/.claude.json.
    let w = World::new();
    let global = Scope::global(&w.config, w.homes(&[]));
    w.install(
        &global,
        &delivery("a", Some("sonnet"), vec![server("fetch")]),
        Options::default(),
    )
    .unwrap();
    assert!(w.home.join(".claude/settings.json").exists());
    assert_eq!(
        json_at(&w.home.join(".claude.json"))["mcpServers"]["fetch"]["command"],
        "npx"
    );
}

#[test]
fn a_global_skill_of_the_same_name_is_named_as_winning() {
    let w = World::new();
    let global = Scope::global(&w.config, w.homes(&[]));
    w.install(
        &global,
        &delivery("b", Some("sonnet"), vec![w.skill("deploy", "b's")]),
        Options::default(),
    )
    .unwrap();
    let report = w
        .install(
            &w.project_scope(),
            &delivery("a", Some("sonnet"), vec![w.skill("deploy", "a's")]),
            Options::default(),
        )
        .unwrap();
    let deploy = report
        .layered
        .iter()
        .find(|l| l.name == "deploy")
        .expect("deploy is layered");
    assert_eq!(
        (deploy.kind, deploy.used),
        (ContentKind::Skills, ScopeKind::Global)
    );
    let notices = use_notices(&report, &[], 1).join("\n");
    assert!(notices.contains("\"deploy\""), "{notices}");
    assert!(notices.contains("\"b\"'s"), "{notices}");
}

#[test]
fn a_second_environment_adds_no_model_settings() {
    let w = World::new();
    w.install(
        &w.project_scope(),
        &delivery("a", Some("sonnet"), vec![w.skill("deploy", "d")]),
        Options::default(),
    )
    .unwrap();
    let before = json_at(&w.project.join(".claude/settings.local.json"));
    let b = Delivery {
        defaults: Defaults {
            anthropic: Some(ModelName::parse("haiku").unwrap()),
            ..delivery("b", Some("haiku"), vec![]).defaults
        },
        ..delivery(
            "b",
            Some("haiku"),
            vec![w.skill("lint", "l"), server("logs"), w.plugin("fmt")],
        )
    }
    .seal();
    let report = w
        .install(&w.project_scope(), &b, Options::default())
        .unwrap();
    let after = json_at(&w.project.join(".claude/settings.local.json"));
    for key in ["apiKeyHelper", "model", "env"] {
        assert_eq!(after[key], before[key], "{key}");
    }
    assert!(w.project.join(".claude/skills/lint/SKILL.md").exists());
    assert_eq!(after["enabledPlugins"]["fmt@sekisho-b"], true);
    assert!(
        json_at(&w.project.join(".mcp.json"))["mcpServers"]
            .get("logs")
            .is_some()
    );
    assert_eq!(
        report.models,
        [(Tool::ClaudeCode, EnvName::parse("a").unwrap())]
    );
    let notices = use_notices(
        &report,
        &[(Tool::ClaudeCode, EnvName::parse("a").unwrap())],
        2,
    )
    .join("\n");
    assert!(notices.contains("models from \"a\""), "{notices}");

    // --models: b's model settings replace a's.
    w.install(
        &w.project_scope(),
        &b,
        Options {
            models: true,
            ..Options::default()
        },
    )
    .unwrap();
    let moved = json_at(&w.project.join(".claude/settings.local.json"));
    assert_eq!(moved["model"], "haiku");
    assert_eq!(
        moved["env"]["ANTHROPIC_BASE_URL"],
        "https://gw.example.com/env/b/anthropic"
    );
}

/// D227: Claude Code names itself to the gateway in a line of
/// `ANTHROPIC_CUSTOM_HEADERS`, beside the person's own lines, which stay
/// through `use` and `uninstall`. A committed `.claude/settings.json` that
/// sets the variable is hidden by the local file, so `use` says so.
#[test]
fn claude_code_sends_its_tool_header_beside_the_persons_own() {
    let w = World::new();
    let d = delivery("a", Some("sonnet"), vec![]);
    w.install(&w.project_scope(), &d, Options::default())
        .unwrap();
    let local = w.project.join(".claude/settings.local.json");
    assert_eq!(
        json_at(&local)["env"]["ANTHROPIC_CUSTOM_HEADERS"],
        "x-sekisho-tool: claude-code"
    );
    install::uninstall(&w.project_scope(), None, None, &Options::default()).unwrap();

    // The person's own lines stay, in the same file.
    let w = World::new();
    std::fs::create_dir_all(w.project.join(".claude")).unwrap();
    std::fs::write(
        local_of(&w),
        r#"{"env": {"ANTHROPIC_CUSTOM_HEADERS": "x-team: pay\nx-trace: on"}}"#,
    )
    .unwrap();
    w.install(&w.project_scope(), &d, Options::default())
        .unwrap();
    assert_eq!(
        json_at(&local_of(&w))["env"]["ANTHROPIC_CUSTOM_HEADERS"],
        "x-team: pay\nx-trace: on\nx-sekisho-tool: claude-code"
    );
    install::uninstall(&w.project_scope(), None, None, &Options::default()).unwrap();
    assert_eq!(
        json_at(&local_of(&w))["env"]["ANTHROPIC_CUSTOM_HEADERS"],
        "x-team: pay\nx-trace: on"
    );

    // A line of the person's naming the header is theirs: asked about.
    let w = World::new();
    std::fs::create_dir_all(w.project.join(".claude")).unwrap();
    std::fs::write(
        local_of(&w),
        r#"{"env": {"ANTHROPIC_CUSTOM_HEADERS": "X-Sekisho-Tool: mine"}}"#,
    )
    .unwrap();
    let err = w
        .install(&w.project_scope(), &d, Options::default())
        .unwrap_err();
    assert!(matches!(err, InstallError::NeedsConsent(_)), "{err:?}");

    // The committed file's value is hidden in the person's sessions.
    let w = World::new();
    std::fs::create_dir_all(w.project.join(".claude")).unwrap();
    std::fs::write(
        w.project.join(".claude/settings.json"),
        r#"{"env": {"ANTHROPIC_CUSTOM_HEADERS": "x-team: pay"}}"#,
    )
    .unwrap();
    let report = w
        .install(&w.project_scope(), &d, Options::default())
        .unwrap();
    let warned = report.warnings.join("\n");
    assert!(
        warned.contains("ANTHROPIC_CUSTOM_HEADERS") && warned.contains(".claude/settings.json"),
        "{warned}"
    );
}

fn local_of(w: &World) -> PathBuf {
    w.project.join(".claude/settings.local.json")
}
