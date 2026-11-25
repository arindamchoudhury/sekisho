//! What Gemini CLI and Qwen Code get from an environment (plan 4, Task 7;
//! D194, D208, D214, D216), through the install engine with the real
//! adapters. Both take their key from `sekisho run`'s variables, so their
//! files hold references to them, never a secret.

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
use sekisho_cli::adapters::{Adapter, ContentKind, GeminiCli, QwenCode, ScopeKind, Tool};
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

fn lock(w: &World) -> Value {
    json_at(&w.project.join(".sekisho/lock.json"))
}

#[test]
fn gemini_cli_gets_its_model_its_auth_and_a_run_spec() {
    let w = World::new();
    let d = delivery(
        "a",
        every_default(),
        vec![
            w.skill("deploy"),
            w.prompt(
                "review",
                "---\ndescription: Review a change\n---\nReview $ARGUMENTS carefully.\n",
            ),
            server("fetch"),
        ],
    );
    w.install(&GeminiCli, &w.project_scope(), &d).unwrap();

    let settings = json_at(&w.project.join(".gemini/settings.json"));
    assert_eq!(settings["model"]["name"], "flash");
    // `gateway` is refused by the CLI's own check; the API-key mode also
    // takes its base URL from GOOGLE_GEMINI_BASE_URL.
    assert_eq!(
        settings["security"]["auth"]["selectedType"],
        "gemini-api-key"
    );
    let fetch = &settings["mcpServers"]["fetch"];
    assert_eq!(fetch["command"], "npx");
    assert!(
        fetch["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "@acme/fetch@1.0.0")
    );
    assert_eq!(fetch["env"]["FETCH_TOKEN"], "${FETCH_TOKEN}");
    assert!(w.project.join(".gemini/skills/deploy/SKILL.md").exists());

    let command: toml_edit::DocumentMut = text_at(&w.project.join(".gemini/commands/review.toml"))
        .parse()
        .unwrap();
    assert_eq!(command["description"].as_str(), Some("Review a change"));
    assert_eq!(
        command["prompt"].as_str(),
        Some("Review {{args}} carefully.\n")
    );

    let run = &lock(&w)["environments"]["a"]["tools"]["gemini-cli"]["run"];
    assert_eq!(run["program"], "gemini");
    assert_eq!(
        run["env"]["GOOGLE_GEMINI_BASE_URL"],
        "${SEKISHO_RUN_URL}/env/a/gemini"
    );
    assert_eq!(run["env"]["GEMINI_API_KEY"], "${SEKISHO_RUN_KEY}");
    // `sekisho run` takes it from the adapter and the server, never from
    // the lock, which a repository could carry.
    assert_eq!(
        GeminiCli.run_spec(&d).as_ref(),
        Some(&serde_json::from_value(run.clone()).unwrap())
    );
}

#[test]
fn gemini_cli_without_a_gemini_model_gets_no_models_and_no_run() {
    let w = World::new();
    let defaults = Defaults {
        gemini: None,
        ..every_default()
    };
    let report = w
        .install(
            &GeminiCli,
            &w.project_scope(),
            &delivery("a", defaults, vec![w.skill("deploy")]),
        )
        .unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("gemini-cli") && x.contains("Gemini API")),
        "{:?}",
        report.warnings
    );
    let settings = w.project.join(".gemini/settings.json");
    assert!(!settings.exists() || json_at(&settings).get("model").is_none());
    assert!(w.project.join(".gemini/skills/deploy/SKILL.md").exists());
    assert!(lock(&w)["environments"]["a"]["tools"]["gemini-cli"]["run"].is_null());
}

#[test]
fn gemini_cli_globally_follows_gemini_cli_home() {
    let w = World::new();
    let elsewhere = w.home.join("gemini-home");
    let scope = Scope::global(
        &w.config,
        w.homes(&[("GEMINI_CLI_HOME", elsewhere.to_str().unwrap())]),
    );
    w.install(
        &GeminiCli,
        &scope,
        &delivery("a", every_default(), vec![w.skill("deploy")]),
    )
    .unwrap();
    let settings = json_at(&elsewhere.join(".gemini/settings.json"));
    assert_eq!(settings["model"]["name"], "flash");
    assert!(elsewhere.join(".gemini/skills/deploy/SKILL.md").exists());
}

#[test]
fn a_prompt_gemini_would_run_or_inject_is_skipped() {
    let w = World::new();
    let d = delivery(
        "a",
        every_default(),
        vec![
            w.prompt("status", "Show !{git status} here.\n"),
            w.prompt("read", "Read @{README.md} first.\n"),
            w.prompt("plain", "Just text.\n"),
        ],
    );
    let report = w.install(&GeminiCli, &w.project_scope(), &d).unwrap();
    for skipped in ["status", "read"] {
        assert!(
            !w.project
                .join(format!(".gemini/commands/{skipped}.toml"))
                .exists()
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|x| x.contains(skipped) && x.contains("skipped")),
            "{:?}",
            report.warnings
        );
    }
    // A prompt without $ARGUMENTS: the tool adds what follows the command.
    let plain: toml_edit::DocumentMut = text_at(&w.project.join(".gemini/commands/plain.toml"))
        .parse()
        .unwrap();
    assert_eq!(plain["prompt"].as_str(), Some("Just text.\n"));
}

#[test]
fn plugins_are_translated_for_gemini_cli() {
    let w = World::new();
    let d = delivery("a", every_default(), vec![w.plugin("fmt")]);
    let report = w.install(&GeminiCli, &w.project_scope(), &d).unwrap();

    // Its skill and command, the command under the plugin's namespace.
    assert!(w.project.join(".gemini/skills/tidy/SKILL.md").exists());
    let command: toml_edit::DocumentMut = text_at(&w.project.join(".gemini/commands/fmt/run.toml"))
        .parse()
        .unwrap();
    assert_eq!(command["description"].as_str(), Some("Run the formatter"));
    assert_eq!(command["prompt"].as_str(), Some("Format {{args}} now.\n"));

    // Its servers: a package runner held to the applied time, and one of
    // its own files, which lives in Sekisho's folder.
    let settings = json_at(&w.project.join(".gemini/settings.json"));
    let lint = &settings["mcpServers"]["fmt-lint"];
    assert_eq!(lint["command"], "npx");
    assert_eq!(lint["args"][0], "--before=1970-01-01T00:00:00Z");
    assert_eq!(lint["env"]["LINT_TOKEN"], "${LINT_TOKEN}");
    let local = &settings["mcpServers"]["fmt-local"];
    assert_eq!(local["command"], "node");
    let script = PathBuf::from(local["args"][0].as_str().unwrap());
    assert!(script.is_absolute(), "{script:?}");
    assert_eq!(text_at(&script), "console.log('hi')\n");
    assert!(script.starts_with(w.project.join(".sekisho")));

    // Hooks have no place outside Claude Code (D205).
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("fmt") && x.contains("hooks")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn qwen_code_gets_an_anthropic_provider_through_the_run() {
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
    w.install(&QwenCode, &w.project_scope(), &d).unwrap();

    let settings = json_at(&w.project.join(".qwen/settings.json"));
    assert_eq!(settings["security"]["auth"]["selectedType"], "anthropic");
    assert_eq!(settings["model"]["name"], "sonnet");
    let providers = settings["modelProviders"]["anthropic"].as_array().unwrap();
    let ids: Vec<&str> = providers
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["sonnet", "haiku"]);
    for p in providers {
        assert_eq!(p["baseUrl"], "${SEKISHO_RUN_URL}/env/a/anthropic");
        assert_eq!(p["envKey"], "SEKISHO_RUN_KEY");
    }
    // Qwen Code's servers inherit the person's environment, so a variable
    // of the same name needs no reference.
    let fetch = &settings["mcpServers"]["fetch"];
    assert_eq!(fetch["command"], "npx");
    assert!(
        fetch
            .get("env")
            .is_none_or(|e| e.get("FETCH_TOKEN").is_none())
    );
    assert!(w.project.join(".qwen/skills/deploy/SKILL.md").exists());
    assert!(w.project.join(".qwen/commands/review.toml").exists());

    let run = &lock(&w)["environments"]["a"]["tools"]["qwen-code"]["run"];
    assert_eq!(run["program"], "qwen");
}

#[test]
fn qwen_code_takes_chat_completions_without_an_anthropic_default() {
    let w = World::new();
    let defaults = Defaults {
        anthropic: None,
        small: None,
        ..every_default()
    };
    w.install(
        &QwenCode,
        &w.project_scope(),
        &delivery("a", defaults, vec![]),
    )
    .unwrap();
    let settings = json_at(&w.project.join(".qwen/settings.json"));
    assert_eq!(settings["security"]["auth"]["selectedType"], "openai");
    assert_eq!(settings["model"]["name"], "gpt");
    let gpt = &settings["modelProviders"]["openai"][0];
    assert_eq!(gpt["id"], "gpt");
    assert_eq!(gpt["baseUrl"], "${SEKISHO_RUN_URL}/env/a/openai/v1");
    assert_eq!(gpt["wireApi"], "chat-completions");
    assert_eq!(gpt["envKey"], "SEKISHO_RUN_KEY");
}

#[test]
fn qwen_code_globally_follows_qwen_home() {
    let w = World::new();
    let elsewhere = w.home.join("qwen-home");
    let scope = Scope::global(
        &w.config,
        w.homes(&[("QWEN_HOME", elsewhere.to_str().unwrap())]),
    );
    w.install(
        &QwenCode,
        &scope,
        &delivery("a", every_default(), vec![w.skill("deploy")]),
    )
    .unwrap();
    assert_eq!(
        json_at(&elsewhere.join("settings.json"))["model"]["name"],
        "sonnet"
    );
    assert!(elsewhere.join("skills/deploy/SKILL.md").exists());
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
        &[&GeminiCli, &QwenCode],
        &w.project_scope(),
        &Options::default(),
        &fetch,
    )
    .unwrap();
    for file in [
        ".gemini/settings.json",
        ".qwen/settings.json",
        ".sekisho/lock.json",
    ] {
        let text = text_at(&w.project.join(file));
        assert!(!text.contains("sek_"), "{file}");
        assert!(!text.contains("Bearer"), "{file}");
    }
    for adapter in [&GeminiCli as &dyn Adapter, &QwenCode] {
        let layering = adapter.layering();
        assert!(!layering.get(ContentKind::Models).both_load);
        for kind in [ContentKind::McpServers, ContentKind::Skills] {
            assert_eq!(
                layering.get(kind).same_name_wins,
                Some(ScopeKind::Project),
                "{:?} {kind:?}",
                adapter.tool()
            );
        }
        // Both tools keep a command two scopes define, renaming one.
        assert_eq!(layering.get(ContentKind::Prompts).same_name_wins, None);
    }
    assert_eq!(GeminiCli.tool(), Tool::GeminiCli);
    assert_eq!(QwenCode.tool(), Tool::QwenCode);
}

/// D227: Gemini CLI reaches Sekisho only through `sekisho run`, whose proxy
/// adds `x-sekisho-tool`; Qwen Code's provider names it in
/// `generationConfig.customHeaders`.
#[test]
fn gemini_cli_and_qwen_code_send_their_tool_headers() {
    let d = delivery("a", every_default(), vec![]);
    let run = GeminiCli.run_spec(&d).unwrap();
    assert!(run.tool_header);
    assert!(!run.env.contains_key("GEMINI_CLI_CUSTOM_HEADERS"));

    let w = World::new();
    w.install(&QwenCode, &w.project_scope(), &d).unwrap();
    let settings = json_at(&w.project.join(".qwen/settings.json"));
    let providers = settings["modelProviders"].as_object().unwrap();
    assert!(!providers.is_empty());
    for list in providers.values() {
        for p in list.as_array().unwrap() {
            assert_eq!(
                p["generationConfig"]["customHeaders"]["x-sekisho-tool"], "qwen-code",
                "{p}"
            );
        }
    }
    assert!(!QwenCode.run_spec(&d).unwrap().tool_header);
}
