//! What OpenCode, Kilo CLI and Pi get from an environment (plan 4, Task 9;
//! D192, D194, D208, D214, D227, D233), through the install engine with the
//! real adapters. All three start through `sekisho run`, so no file holds
//! a secret.

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
use sekisho_cli::adapters::{Adapter, ContentKind, KiloCli, OpenCode, Pi, ScopeKind, Tool};
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

fn with_prompt_and_server(w: &World) -> Delivery {
    delivery(
        "a",
        every_default(),
        vec![
            w.skill("deploy"),
            w.prompt("review", REVIEW),
            server("fetch"),
        ],
    )
}

const REVIEW: &str = "---\ndescription: Review a change\n---\nReview $ARGUMENTS carefully.\n";

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

// ---- OpenCode ----

/// D208: OpenCode takes a provider per format with a default, each through
/// `sekisho run`'s variables, in `.opencode/opencode.json`, which
/// overrides a repository's root `opencode.json`.
#[test]
fn opencode_gets_providers_mcp_skills_and_commands() {
    let w = World::new();
    let d = with_prompt_and_server(&w);
    w.install(&OpenCode, &w.project_scope(), &d).unwrap();
    let config = json_at(&w.project.join(".opencode/opencode.json"));
    let p = &config["provider"];
    let anthropic = &p["sekisho-a-anthropic"];
    assert_eq!(anthropic["npm"], "@ai-sdk/anthropic");
    assert_eq!(
        anthropic["options"]["baseURL"],
        "{env:SEKISHO_RUN_URL}/env/a/anthropic/v1"
    );
    assert_eq!(anthropic["options"]["apiKey"], "{env:SEKISHO_RUN_KEY}");
    // D227: the tool names itself to the gateway.
    assert_eq!(
        anthropic["options"]["headers"]["x-sekisho-tool"],
        "opencode"
    );
    assert!(anthropic["models"].get("sonnet").is_some());
    assert!(anthropic["models"].get("haiku").is_some());
    assert!(anthropic["models"].get("gpt").is_none());
    let openai = &p["sekisho-a-openai"];
    assert_eq!(openai["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(
        openai["options"]["baseURL"],
        "{env:SEKISHO_RUN_URL}/env/a/openai/v1"
    );
    assert!(openai["models"].get("gpt").is_some());
    let gemini = &p["sekisho-a-gemini"];
    assert_eq!(gemini["npm"], "@ai-sdk/google");
    assert_eq!(
        gemini["options"]["baseURL"],
        "{env:SEKISHO_RUN_URL}/env/a/gemini/v1beta"
    );
    assert_eq!(config["model"], "sekisho-a-anthropic/sonnet");
    assert_eq!(config["small_model"], "sekisho-a-anthropic/haiku");

    let fetch = &config["mcp"]["fetch"];
    assert_eq!(fetch["type"], "local");
    assert_eq!(fetch["enabled"], true);
    let command = strs(&fetch["command"]);
    assert_eq!(command[0], "npx");
    assert!(
        command.iter().any(|a| a == "@acme/fetch@1.0.0"),
        "{command:?}"
    );
    // OpenCode's servers inherit its environment: the person's variable
    // reaches the server as it is.
    assert!(
        fetch
            .get("environment")
            .is_none_or(|e| e.get("FETCH_TOKEN").is_none())
    );
    assert!(w.project.join(".agents/skills/deploy/SKILL.md").exists());
    assert_eq!(
        text_at(&w.project.join(".opencode/commands/review.md")),
        REVIEW
    );

    let run = OpenCode.run_spec(&d).unwrap();
    assert_eq!(run.program, "opencode");
    assert!(!run.tool_header);

    // Globally, OpenCode's own config folder.
    let w = World::new();
    let d = with_prompt_and_server(&w);
    w.install(&OpenCode, &global(&w), &d).unwrap();
    let config = json_at(&w.home.join(".config/opencode/opencode.json"));
    assert!(config["provider"].get("sekisho-a-anthropic").is_some());
    assert!(w.home.join(".config/opencode/commands/review.md").exists());
    assert!(w.home.join(".agents/skills/deploy/SKILL.md").exists());
}

/// Without an anthropic default, OpenCode starts on the first format that
/// has one.
#[test]
fn opencode_starts_on_the_first_default_it_has() {
    let w = World::new();
    let d = delivery(
        "a",
        Defaults {
            anthropic: None,
            small: None,
            ..every_default()
        },
        vec![],
    );
    w.install(&OpenCode, &w.project_scope(), &d).unwrap();
    let config = json_at(&w.project.join(".opencode/opencode.json"));
    assert_eq!(config["model"], "sekisho-a-openai/gpt");
    assert!(config["provider"].get("sekisho-a-anthropic").is_none());
    assert!(config.get("small_model").is_none());
}

// ---- Kilo CLI ----

/// D192: Kilo resolves `{env:}` only in the user's global config, and
/// ignores a project file holding one, so the providers go to the global
/// `kilo.json` and the project file names its model by their ID.
#[test]
fn kilo_puts_its_providers_in_the_global_file() {
    let w = World::new();
    let d = with_prompt_and_server(&w);
    w.install(&KiloCli, &w.project_scope(), &d).unwrap();
    let global_file = w.home.join(".config/kilo/kilo.json");
    let global_config = json_at(&global_file);
    let anthropic = &global_config["provider"]["sekisho-a-anthropic"];
    assert_eq!(
        anthropic["options"]["baseURL"],
        "{env:SEKISHO_RUN_URL}/env/a/anthropic/v1"
    );
    assert_eq!(
        anthropic["options"]["headers"]["x-sekisho-tool"],
        "kilo-cli"
    );
    let project_text = text_at(&w.project.join(".kilo/kilo.json"));
    assert!(!project_text.contains("{env:"), "{project_text}");
    let project: Value = serde_json::from_str(&project_text).unwrap();
    assert_eq!(project["model"], "sekisho-a-anthropic/sonnet");
    assert_eq!(project["mcp"]["fetch"]["type"], "local");
    assert!(w.project.join(".kilo/commands/review.md").exists());
    assert!(w.project.join(".agents/skills/deploy/SKILL.md").exists());
    let run = KiloCli.run_spec(&d).unwrap();
    assert_eq!(run.program, "kilo");

    install::uninstall(&w.project_scope(), None, None, &Options::default()).unwrap();
    assert!(
        !global_file.exists()
            || json_at(&global_file)["provider"]
                .get("sekisho-a-anthropic")
                .is_none()
    );
}

// ---- Pi ----

/// D233: Pi takes `baseUrl` literally and caches a `!command` key, so its
/// providers come from an extension that registers them only inside
/// `sekisho run pi`, which picks the model.
#[test]
fn pi_gets_an_extension_and_a_run_spec() {
    let w = World::new();
    let d = with_prompt_and_server(&w);
    w.install(&Pi, &w.project_scope(), &d).unwrap();
    let ext = text_at(&w.project.join(".pi/extensions/sekisho-a.ts"));
    for needle in [
        "process.env.SEKISHO_RUN_URL",
        "registerProvider(\"sekisho-a-anthropic\"",
        "\"$SEKISHO_RUN_KEY\"",
        "\"anthropic-messages\"",
        "\"openai-completions\"",
        "\"google-generative-ai\"",
        "\"x-sekisho-tool\": \"pi\"",
        "\"sonnet\"",
        "/env/a/anthropic",
    ] {
        assert!(ext.contains(needle), "{needle} in {ext}");
    }
    let run = Pi.run_spec(&d).unwrap();
    assert_eq!(run.program, "pi");
    assert_eq!(
        run.args,
        ["--provider", "sekisho-a-anthropic", "--model", "sonnet"]
    );
    let mcp = json_at(&w.project.join(".pi/mcp.json"));
    assert_eq!(mcp["mcpServers"]["fetch"]["command"], "npx");
    assert_eq!(
        mcp["mcpServers"]["fetch"]["env"]["FETCH_TOKEN"],
        "${FETCH_TOKEN}"
    );
    assert_eq!(text_at(&w.project.join(".pi/prompts/review.md")), REVIEW);
    assert!(w.project.join(".agents/skills/deploy/SKILL.md").exists());

    let w = World::new();
    let d = with_prompt_and_server(&w);
    w.install(&Pi, &global(&w), &d).unwrap();
    assert!(w.home.join(".pi/agent/extensions/sekisho-a.ts").exists());
    assert!(w.home.join(".pi/agent/mcp.json").exists());
    assert!(w.home.join(".pi/agent/prompts/review.md").exists());
}

#[test]
fn plugins_are_translated_for_opencode_kilo_and_pi() {
    for adapter in [&OpenCode as &dyn Adapter, &KiloCli, &Pi] {
        let w = World::new();
        let d = delivery("a", every_default(), vec![w.plugin("fmt")]);
        let report = w.install(adapter, &w.project_scope(), &d).unwrap();
        assert!(w.project.join(".agents/skills/tidy/SKILL.md").exists());
        let warned = report.warnings.join("\n");
        assert!(warned.contains("hooks"), "{warned}");
        let command = match adapter.tool() {
            Tool::OpenCode => ".opencode/commands/fmt/run.md",
            Tool::KiloCli => ".kilo/commands/fmt/run.md",
            _ => ".pi/prompts/fmt-run.md",
        };
        assert!(w.project.join(command).exists(), "{command}");
    }
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
        &[&OpenCode, &KiloCli, &Pi],
        &w.project_scope(),
        &Options::default(),
        &fetch,
    )
    .unwrap();
    for file in [
        w.project.join(".opencode/opencode.json"),
        w.project.join(".kilo/kilo.json"),
        w.home.join(".config/kilo/kilo.json"),
        w.project.join(".pi/extensions/sekisho-a.ts"),
        w.project.join(".sekisho/lock.json"),
    ] {
        let text = text_at(&file);
        assert!(!text.contains("sek_"), "{}", file.display());
        assert!(!text.contains("Bearer"), "{}", file.display());
    }
    for adapter in [&OpenCode as &dyn Adapter, &KiloCli] {
        let layering = adapter.layering();
        assert!(layering.get(ContentKind::Instructions).both_load);
        assert_eq!(
            layering.get(ContentKind::McpServers).same_name_wins,
            Some(ScopeKind::Project)
        );
    }
    // Pi's project APPEND_SYSTEM.md replaces the global one.
    let pi = Pi.layering();
    assert!(!pi.get(ContentKind::Instructions).both_load);
    assert_eq!(
        pi.get(ContentKind::Instructions).same_name_wins,
        Some(ScopeKind::Project)
    );
    assert_eq!(OpenCode.tool(), Tool::OpenCode);
    assert_eq!(KiloCli.tool(), Tool::KiloCli);
    assert_eq!(Pi.tool(), Tool::Pi);
}
