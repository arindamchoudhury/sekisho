//! What Codex and GitHub Copilot CLI get from an environment (plan 4,
//! Task 8; D192, D201, D208, D214, D227, D232), through the install engine
//! with the real adapters. Codex runs `sekisho token` for its key; Copilot
//! CLI starts through `sekisho run`. No file holds a secret.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sekisho_catalog::delivery::{
    Defaults, DeliveredEntry, DeliveredFile, DeliveredModel, Delivery, Registries,
};
use sekisho_catalog::model::{
    ConfigField, ConfigTarget, EntrySpec, McpRuntime, McpServerSpec, PluginSpec, PromptSpec,
    SkillSpec, sha256_hex,
};
use sekisho_catalog::names::{EntryKind, EntryName, SourceName};
use sekisho_catalog::pins::NpmPackage;
use sekisho_cli::adapters::{Adapter, ClaudeCode, Codex, ContentKind, CopilotCli, ScopeKind, Tool};
use sekisho_cli::install::{self, InstallError, Options, Report, Scope, ToolHomes};
use sekisho_core::formats::ClientApi;
use sekisho_core::names::{EnvName, ModelName};
use serde_json::Value;
use tempfile::TempDir;
use time::OffsetDateTime;

struct World {
    _tmp: TempDir,
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
            _tmp: tmp,
            blobs: Default::default(),
        }
    }

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

    fn skill(&self, name: &str) -> DeliveredEntry {
        entry(
            EntryKind::Skill,
            name,
            EntrySpec::Skill(SkillSpec {
                description: String::new(),
                license: None,
                compatibility: None,
            }),
            vec![self.file("SKILL.md", &format!("---\nname: {name}\n---\nDo it.\n"))],
        )
    }

    fn prompt(&self, name: &str, text: &str) -> DeliveredEntry {
        let (description, _) = sekisho_catalog::description_and_body(text).unwrap();
        entry(
            EntryKind::Prompt,
            name,
            EntrySpec::Prompt(PromptSpec { description }),
            vec![self.file(&format!("{name}.md"), text)],
        )
    }

    /// A plugin with a skill, a command, a hook, an npx server and a server
    /// of its own files.
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
                self.file(
                    "commands/run.md",
                    "---\ndescription: Run the formatter\n---\nFormat $ARGUMENTS now.\n",
                ),
                self.file("skills/tidy/SKILL.md", "---\nname: tidy\n---\nTidy.\n"),
                self.file("hooks/hooks.json", r#"{"hooks": {}}"#),
                self.file(
                    ".mcp.json",
                    r#"{"mcpServers": {
                        "lint": {"command": "npx", "args": ["-y", "@acme/lint@2.0.0"], "env": {"LINT_TOKEN": "${LINT_TOKEN}"}},
                        "local": {"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/server.js"]}
                    }}"#,
                ),
                self.file("server.js", "console.log('hi')\n"),
            ],
        )
    }

    fn install(
        &self,
        adapter: &dyn Adapter,
        scope: &Scope,
        d: &Delivery,
    ) -> Result<Report, InstallError> {
        let fetch = |sha: &str| -> Result<Vec<u8>, InstallError> {
            Ok(self.blobs.lock().unwrap()[sha].clone())
        };
        install::install(d, &[adapter], scope, &Options::default(), &fetch)
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

/// A catalog npx server with a required token and an optional setting.
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
            config: vec![ConfigField {
                key: "FETCH_TOKEN".into(),
                name: "Token".into(),
                description: String::new(),
                required: true,
                sensitive: true,
                value: None,
                target: ConfigTarget::Env,
                prefix: None,
            }],
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

fn name(n: &str) -> Option<ModelName> {
    Some(ModelName::parse(n).unwrap())
}

fn delivery(env: &str, defaults: Defaults, entries: Vec<DeliveredEntry>) -> Delivery {
    Delivery {
        environment: EnvName::parse(env).unwrap(),
        notice: String::new(),
        revision: String::new(),
        gateway_url: format!("https://gw.example.com/env/{env}"),
        models: vec![
            model("sonnet", &[ClientApi::Messages]),
            model("haiku", &[ClientApi::Messages]),
            model("gpt", &[ClientApi::ChatCompletions, ClientApi::Responses]),
            model("flash", &[ClientApi::Gemini]),
        ],
        defaults,
        mcp: None,
        entries,
        quarantined: Vec::new(),
        registries: Registries::default(),
    }
    .seal()
}

fn every_default() -> Defaults {
    Defaults {
        anthropic: name("sonnet"),
        openai: name("gpt"),
        gemini: name("flash"),
        small: name("haiku"),
    }
}

fn json_at(path: &Path) -> Value {
    serde_json::from_slice(
        &std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

fn text_at(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn global(w: &World) -> Scope {
    Scope::global(&w.config, w.homes(&[]))
}

fn toml_at(path: &Path) -> toml_edit::DocumentMut {
    text_at(path).parse().unwrap()
}

// ---- Codex ----

/// D192: Codex ignores a model provider in a project's config, so a project
/// install's provider is the profile `sekisho-<env>` in Codex's home,
/// which `sekisho run codex` selects; the project's `.codex/config.toml`
/// holds its MCP servers, never a provider.
#[test]
fn codex_gets_a_profile_and_the_project_file_holds_no_provider() {
    let w = World::new();
    let d = delivery(
        "a",
        every_default(),
        vec![
            w.skill("deploy"),
            w.prompt("review", "Review $ARGUMENTS.\n"),
            server("fetch"),
        ],
    );
    let report = w.install(&Codex, &w.project_scope(), &d).unwrap();

    let profile = toml_at(&w.home.join(".codex/sekisho-a.config.toml"));
    assert_eq!(profile["model"].as_str(), Some("gpt"));
    assert_eq!(profile["model_provider"].as_str(), Some("sekisho-a"));
    let p = &profile["model_providers"]["sekisho-a"];
    assert_eq!(
        p["base_url"].as_str(),
        Some("https://gw.example.com/env/a/openai/v1")
    );
    assert_eq!(p["wire_api"].as_str(), Some("responses"));
    assert_eq!(p["auth"]["command"].as_str(), Some("sekisho"));
    let args: Vec<&str> = p["auth"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(args, ["token"]);
    assert_eq!(p["auth"]["refresh_interval_ms"].as_integer(), Some(300_000));
    // D227: the tool names itself to the gateway.
    assert_eq!(p["http_headers"]["x-sekisho-tool"].as_str(), Some("codex"));

    let project = text_at(&w.project.join(".codex/config.toml"));
    assert!(!project.contains("model_provider"), "{project}");
    let project: toml_edit::DocumentMut = project.parse().unwrap();
    let fetch = &project["mcp_servers"]["fetch"];
    assert_eq!(fetch["command"].as_str(), Some("npx"));
    // Codex hands its servers only a few variables of its own, so the
    // person's is forwarded by name, never written.
    let vars: Vec<&str> = fetch["env_vars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(vars, ["FETCH_TOKEN"]);
    assert!(
        fetch
            .get("env")
            .and_then(|e| e.get("FETCH_TOKEN"))
            .is_none()
    );
    assert!(w.project.join(".agents/skills/deploy/SKILL.md").exists());
    // Codex has no custom prompts.
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("review") && x.contains("skipped") && x.contains("Codex")),
        "{:?}",
        report.warnings
    );

    let run = Codex.run_spec(&d).unwrap();
    assert_eq!(run.program, "codex");
    assert_eq!(run.args, ["--profile", "sekisho-a"]);
    assert!(!run.tool_header);
    let hint = Codex.start_hint(&d).unwrap();
    assert!(hint.contains("codex --profile sekisho-a"), "{hint}");
}

/// D192, D202: a global install writes the same keys into the person's
/// own `config.toml`, whose comments and other keys stay, and asks before
/// replacing a `model` of theirs.
#[test]
fn codex_globally_keeps_the_persons_config_toml() {
    let w = World::new();
    let config = w.home.join(".codex/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        "# my settings\nmodel_reasoning_effort = \"high\" # keep\n",
    )
    .unwrap();
    let d = delivery("a", every_default(), vec![server("fetch")]);
    w.install(&Codex, &global(&w), &d).unwrap();
    let text = text_at(&config);
    assert!(text.contains("# my settings"), "{text}");
    assert!(
        text.contains("model_reasoning_effort = \"high\" # keep"),
        "{text}"
    );
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    assert_eq!(doc["model"].as_str(), Some("gpt"));
    assert_eq!(doc["model_provider"].as_str(), Some("sekisho-a"));
    assert_eq!(
        doc["model_providers"]["sekisho-a"]["wire_api"].as_str(),
        Some("responses")
    );
    assert_eq!(doc["mcp_servers"]["fetch"]["command"].as_str(), Some("npx"));
    // `sekisho run codex` selects the profile, there too.
    assert!(w.home.join(".codex/sekisho-a.config.toml").exists());

    install::uninstall(&global(&w), None, None, &Options::default()).unwrap();
    let text = text_at(&config);
    assert!(text.contains("# my settings"), "{text}");
    assert!(!text.contains("model_provider"), "{text}");
    assert!(!w.home.join(".codex/sekisho-a.config.toml").exists());

    // A model of the person's own is asked about.
    let w = World::new();
    let config = w.home.join(".codex/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "model = \"o4\"\n").unwrap();
    let err = w.install(&Codex, &global(&w), &d).unwrap_err();
    assert!(matches!(err, InstallError::NeedsConsent(_)), "{err:?}");
}

/// Codex reads a project's `.codex/config.toml` only in a trusted folder:
/// `use` says so until the person trusts it, and never trusts it itself.
#[test]
fn codex_says_when_the_folder_is_not_trusted() {
    let w = World::new();
    let note = Codex.note(&w.project_scope()).expect("a note");
    assert!(note.contains("trust"), "{note}");
    let config = w.home.join(".codex/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let key = w.project.to_string_lossy().replace('\\', "\\\\");
    std::fs::write(
        &config,
        format!("[projects.\"{key}\"]\ntrust_level = \"trusted\"\n"),
    )
    .unwrap();
    assert_eq!(Codex.note(&w.project_scope()), None);
    assert_eq!(Codex.note(&global(&w)), None);
}

/// A model the Responses API can't reach leaves Codex without models.
#[test]
fn codex_without_a_responses_model_gets_no_models() {
    let w = World::new();
    let d = Delivery {
        models: vec![
            model("sonnet", &[ClientApi::Messages]),
            model("gpt", &[ClientApi::ChatCompletions]),
        ],
        ..delivery("a", every_default(), vec![])
    }
    .seal();
    let report = w.install(&Codex, &w.project_scope(), &d).unwrap();
    assert!(!w.home.join(".codex/sekisho-a.config.toml").exists());
    assert!(
        report.warnings.iter().any(|x| x.contains("models skipped")),
        "{:?}",
        report.warnings
    );
    assert_eq!(Codex.run_spec(&d), None);
}

/// Plugins are taken apart for Codex: their skills and MCP servers, not
/// their hooks or commands.
#[test]
fn plugins_are_translated_for_codex() {
    let w = World::new();
    let d = delivery("a", every_default(), vec![w.plugin("fmt")]);
    let report = w.install(&Codex, &w.project_scope(), &d).unwrap();
    assert!(w.project.join(".agents/skills/tidy/SKILL.md").exists());
    let project = toml_at(&w.project.join(".codex/config.toml"));
    assert!(project["mcp_servers"].get("fmt-lint").is_some());
    assert!(project["mcp_servers"].get("fmt-local").is_some());
    let skipped = &report.warnings;
    assert!(skipped.iter().any(|s| s.contains("hooks")), "{skipped:?}");
    assert!(skipped.iter().any(|s| s.contains("command")), "{skipped:?}");
}

// ---- Copilot CLI ----

/// D208, D232: Copilot CLI takes one provider from its environment
/// variables, so it starts through `sekisho run`, whose proxy holds the
/// token and names the tool (D227).
#[test]
fn copilot_cli_starts_through_the_run() {
    let d = delivery("a", every_default(), vec![]);
    let run = CopilotCli.run_spec(&d).unwrap();
    assert_eq!(run.program, "copilot");
    assert!(run.tool_header);
    let env = |k: &str| run.env.get(k).map(String::as_str);
    assert_eq!(env("COPILOT_PROVIDER_TYPE"), Some("anthropic"));
    assert_eq!(
        env("COPILOT_PROVIDER_BASE_URL"),
        Some("${SEKISHO_RUN_URL}/env/a/anthropic")
    );
    assert_eq!(env("COPILOT_PROVIDER_API_KEY"), Some("${SEKISHO_RUN_KEY}"));
    assert_eq!(env("COPILOT_MODEL"), Some("sonnet"));
    assert_eq!(env("COPILOT_PROVIDER_API_KEY_COMMAND"), None);

    let openai = delivery(
        "a",
        Defaults {
            anthropic: None,
            small: None,
            ..every_default()
        },
        vec![],
    );
    let run = CopilotCli.run_spec(&openai).unwrap();
    assert_eq!(
        run.env.get("COPILOT_PROVIDER_TYPE").map(String::as_str),
        Some("openai")
    );
    assert_eq!(
        run.env.get("COPILOT_PROVIDER_BASE_URL").map(String::as_str),
        Some("${SEKISHO_RUN_URL}/env/a/openai/v1")
    );
    assert_eq!(
        run.env.get("COPILOT_MODEL").map(String::as_str),
        Some("gpt")
    );

    let none = delivery(
        "a",
        Defaults {
            anthropic: None,
            openai: None,
            small: None,
            ..every_default()
        },
        vec![],
    );
    assert_eq!(CopilotCli.run_spec(&none), None);
}

/// D201: Copilot CLI reads a project's `.mcp.json` as Claude Code does, so
/// both share its entries, written once; prompts are skipped.
#[test]
fn copilot_cli_shares_mcp_json_with_claude_code() {
    let w = World::new();
    let d = delivery(
        "a",
        every_default(),
        vec![
            w.skill("deploy"),
            w.prompt("review", "Review $ARGUMENTS.\n"),
            server("fetch"),
        ],
    );
    let fetch =
        |sha: &str| -> Result<Vec<u8>, InstallError> { Ok(w.blobs.lock().unwrap()[sha].clone()) };
    let report = install::install(
        &d,
        &[&ClaudeCode, &CopilotCli],
        &w.project_scope(),
        &Options::default(),
        &fetch,
    )
    .unwrap();
    let mcp = json_at(&w.project.join(".mcp.json"));
    assert_eq!(mcp["mcpServers"]["fetch"]["command"], "npx");
    assert_eq!(
        mcp["mcpServers"]["fetch"]["env"]["FETCH_TOKEN"],
        "${FETCH_TOKEN}"
    );
    assert!(w.project.join(".agents/skills/deploy/SKILL.md").exists());
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("review") && x.contains("skipped") && x.contains("Copilot")),
        "{:?}",
        report.warnings
    );
    // Uninstalling Copilot CLI keeps what Claude Code still uses.
    install::uninstall(
        &w.project_scope(),
        None,
        Some(&[Tool::CopilotCli]),
        &Options::default(),
    )
    .unwrap();
    let mcp = json_at(&w.project.join(".mcp.json"));
    assert!(mcp["mcpServers"].get("fetch").is_some());

    // Globally, Copilot CLI's own MCP file and the shared skills folder.
    let w = World::new();
    let d = delivery(
        "a",
        every_default(),
        vec![w.skill("deploy"), server("fetch")],
    );
    w.install(&CopilotCli, &global(&w), &d).unwrap();
    let mcp = json_at(&w.home.join(".copilot/mcp-config.json"));
    assert_eq!(mcp["mcpServers"]["fetch"]["command"], "npx");
    assert!(w.home.join(".agents/skills/deploy/SKILL.md").exists());
}

/// Codex and Copilot CLI read the same `.agents/skills/`: one copy.
#[test]
fn codex_and_copilot_cli_share_their_skills_folder() {
    let w = World::new();
    let d = delivery("a", every_default(), vec![w.skill("deploy")]);
    let fetch =
        |sha: &str| -> Result<Vec<u8>, InstallError> { Ok(w.blobs.lock().unwrap()[sha].clone()) };
    install::install(
        &d,
        &[&Codex, &CopilotCli],
        &w.project_scope(),
        &Options::default(),
        &fetch,
    )
    .unwrap();
    assert!(w.project.join(".agents/skills/deploy/SKILL.md").exists());
    install::uninstall(
        &w.project_scope(),
        None,
        Some(&[Tool::Codex]),
        &Options::default(),
    )
    .unwrap();
    assert!(w.project.join(".agents/skills/deploy/SKILL.md").exists());
}

#[test]
fn no_file_holds_a_secret_and_the_layering_is_each_tools() {
    let w = World::new();
    let d = delivery(
        "a",
        every_default(),
        vec![w.skill("deploy"), server("fetch")],
    );
    let fetch =
        |sha: &str| -> Result<Vec<u8>, InstallError> { Ok(w.blobs.lock().unwrap()[sha].clone()) };
    install::install(
        &d,
        &[&Codex, &CopilotCli],
        &w.project_scope(),
        &Options::default(),
        &fetch,
    )
    .unwrap();
    for file in [
        w.project.join(".codex/config.toml"),
        w.project.join(".mcp.json"),
        w.project.join(".sekisho/lock.json"),
        w.home.join(".codex/sekisho-a.config.toml"),
    ] {
        let text = text_at(&file);
        assert!(!text.contains("sek_"), "{}", file.display());
        assert!(!text.contains("Bearer"), "{}", file.display());
    }

    let codex = Codex.layering();
    // The profile applies only with `--profile`, so a project's and a
    // global install's models never meet.
    assert!(!codex.get(ContentKind::Models).both_load);
    assert_eq!(
        codex.get(ContentKind::McpServers).same_name_wins,
        Some(ScopeKind::Project)
    );
    // One `developer_instructions` per scope: the project's replaces it.
    assert!(!codex.get(ContentKind::Instructions).both_load);
    assert_eq!(
        codex.get(ContentKind::Instructions).same_name_wins,
        Some(ScopeKind::Project)
    );
    let copilot = CopilotCli.layering();
    assert!(copilot.get(ContentKind::McpServers).both_load);
    assert!(copilot.get(ContentKind::Instructions).both_load);
    assert_eq!(Codex.tool(), Tool::Codex);
    assert_eq!(CopilotCli.tool(), Tool::CopilotCli);
}
