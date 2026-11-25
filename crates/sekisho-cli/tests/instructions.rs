//! Instructions on machines (plan 4, Task 7b; D220–D225): the files of
//! Sekisho's own each tool loads, through the install engine with the real
//! adapters. Never a repository's own `AGENTS.md` or `CLAUDE.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sekisho_catalog::delivery::{Defaults, DeliveredEntry, DeliveredFile, Delivery, Registries};
use sekisho_catalog::model::{EntrySpec, InstructionSpec, PluginSpec, sha256_hex};
use sekisho_catalog::names::{EntryKind, EntryName, SourceName};
use sekisho_cli::adapters::{
    Adapter, AdapterError, ClaudeCode, ContentKind, FileWrite, GeminiCli, Layering, Origin,
    QwenCode, RelPath, RenderContext, Rendered, Root, Tool, UnmanagedItem,
};
use sekisho_cli::install::{self, InstallError, Options, Report, Scope, ToolHomes};
use sekisho_core::names::EnvName;
use serde_json::Value;
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

    fn global_scope(&self) -> Scope {
        Scope::global(&self.config, self.homes(&[]))
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

    fn instruction(&self, source: &str, name: &str, text: &str) -> DeliveredEntry {
        entry(
            EntryKind::Instruction,
            source,
            name,
            EntrySpec::Instruction(InstructionSpec {
                description: String::new(),
            }),
            vec![self.file(&format!("{name}.md"), text)],
        )
    }

    /// A plugin with APM instructions (one for every file, one path-scoped,
    /// one without frontmatter) and a top-level AGENTS.md.
    fn plugin(&self, name: &str) -> DeliveredEntry {
        entry(
            EntryKind::Plugin,
            "team",
            name,
            EntrySpec::Plugin(PluginSpec {
                description: "Formats.".into(),
                version: None,
                mcp_servers: Vec::new(),
                apm_checked: true,
            }),
            vec![
                self.file(
                    ".claude-plugin/plugin.json",
                    &format!(r#"{{"name": "{name}"}}"#),
                ),
                self.file("AGENTS.md", "Obey the plugin.\n"),
                self.file(
                    "instructions/always.instructions.md",
                    "---\ndescription: Everywhere\napplyTo: \"**\"\n---\nFormat before you commit.\n",
                ),
                self.file(
                    "instructions/rust.instructions.md",
                    "---\napplyTo: \"**/*.rs, src/**\"\n---\n# Rust\n\nUse rustfmt.\n",
                ),
                self.file("instructions/plain.instructions.md", "Keep lines short.\n"),
                self.file("skills/tidy/SKILL.md", "---\nname: tidy\n---\nTidy.\n"),
            ],
        )
    }

    fn install_with(
        &self,
        adapter: &dyn Adapter,
        scope: &Scope,
        d: &Delivery,
        options: &Options,
    ) -> Result<Report, InstallError> {
        let fetch = |sha: &str| -> Result<Vec<u8>, InstallError> {
            Ok(self.blobs.lock().unwrap()[sha].clone())
        };
        install::install(d, &[adapter], scope, options, &fetch)
    }

    fn install(
        &self,
        adapter: &dyn Adapter,
        scope: &Scope,
        d: &Delivery,
    ) -> Result<Report, InstallError> {
        self.install_with(adapter, scope, d, &Options::default())
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
        digest: sha256_hex(format!("{source}{name}{files:?}").as_bytes()),
        applied_at: OffsetDateTime::UNIX_EPOCH,
        tools: None,
        spec,
        files,
    }
}

fn delivery(env: &str, entries: Vec<DeliveredEntry>) -> Delivery {
    Delivery {
        environment: EnvName::parse(env).unwrap(),
        notice: String::new(),
        revision: String::new(),
        gateway_url: format!("https://gw.example.com/env/{env}"),
        models: Vec::new(),
        defaults: Defaults::default(),
        mcp: None,
        entries,
        quarantined: Vec::new(),
        registries: Registries::default(),
    }
    .seal()
}

fn text_at(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn json_at(path: &Path) -> Value {
    serde_json::from_str(&text_at(path)).unwrap()
}

/// Every file under `dir`, relative, `/`-separated.
fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(items) = std::fs::read_dir(&d) else {
            continue;
        };
        for item in items {
            let path = item.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out.sort();
    out
}

const STYLE: &str = "# Payments style\n\nShort functions.\n";
const STYLE_FILE: &str =
    "# Payments style (Sekisho environment a, instruction style)\n\nShort functions.\n";
const REVIEW: &str = "Ask for a test with every fix.\n";
const REVIEW_FILE: &str =
    "# review (Sekisho environment a, instruction review)\n\nAsk for a test with every fix.\n";

fn style_and_review(w: &World) -> Delivery {
    delivery(
        "a",
        vec![
            w.instruction("team", "style", STYLE),
            w.instruction("team", "review", REVIEW),
        ],
    )
}

#[test]
fn claude_code_gets_one_rule_file_each_with_its_origin() {
    let w = World::new();
    let report = w
        .install(&ClaudeCode, &w.project_scope(), &style_and_review(&w))
        .unwrap();
    assert_eq!(
        text_at(&w.project.join(".claude/rules/sekisho-a-style.md")),
        STYLE_FILE
    );
    assert_eq!(
        text_at(&w.project.join(".claude/rules/sekisho-a-review.md")),
        REVIEW_FILE
    );
    assert!(
        !report.warnings.iter().any(|x| x.contains("instruction")),
        "{:?}",
        report.warnings
    );
    // Globally, the user's rules folder.
    w.install(&ClaudeCode, &w.global_scope(), &style_and_review(&w))
        .unwrap();
    assert_eq!(
        text_at(&w.home.join(".claude/rules/sekisho-a-style.md")),
        STYLE_FILE
    );
}

#[test]
fn an_instruction_with_an_invisible_character_is_refused_on_the_machine() {
    let w = World::new();
    let d = delivery("a", vec![w.instruction("team", "hid", "Say hi\u{200B}.\n")]);
    let err = w.install(&ClaudeCode, &w.project_scope(), &d).unwrap_err();
    let InstallError::Refused(why) = err else {
        panic!("{err:?}")
    };
    assert!(why.contains("U+200B") && why.contains("hid"), "{why}");
    assert!(!w.project.join(".claude").exists());
}

const BLOCK_START: &str = "<!-- sekisho:start: managed by `sekisho use`; edits are overwritten -->";
const BLOCK_END: &str = "<!-- sekisho:end -->";

#[test]
fn gemini_cli_gets_an_import_block_and_never_edits_outside_it() {
    let w = World::new();
    let scope = w.project_scope();
    w.install(&GeminiCli, &scope, &style_and_review(&w))
        .unwrap();
    assert_eq!(
        text_at(&w.project.join(".gemini/sekisho/a/style.md")),
        STYLE_FILE
    );
    assert_eq!(
        text_at(&w.project.join("GEMINI.md")),
        format!(
            "{BLOCK_START}\n@./.gemini/sekisho/a/style.md\n@./.gemini/sekisho/a/review.md\n{BLOCK_END}\n"
        )
    );
    // Excluded from Git like every file Sekisho makes.
    std::fs::create_dir_all(w.project.join(".git/info")).unwrap();
    w.install(&GeminiCli, &scope, &style_and_review(&w))
        .unwrap();
    assert!(text_at(&w.project.join(".git/info/exclude")).contains("GEMINI.md"));

    // The person writes around the block; another environment adds a line.
    let gemini_md = w.project.join("GEMINI.md");
    let mine = format!("# Mine\n\n{}\nAfter.\n", text_at(&gemini_md));
    std::fs::write(&gemini_md, &mine).unwrap();
    let b = delivery("b", vec![w.instruction("team", "units", "Micro-USD.\n")]);
    w.install(&GeminiCli, &scope, &b).unwrap();
    assert_eq!(
        text_at(&gemini_md),
        format!(
            "# Mine\n\n{BLOCK_START}\n@./.gemini/sekisho/a/style.md\n@./.gemini/sekisho/a/review.md\n\
             @./.gemini/sekisho/b/units.md\n{BLOCK_END}\n\nAfter.\n"
        )
    );

    // Uninstalling takes out only its lines, then the block; the person's
    // text stays.
    install::uninstall(
        &scope,
        Some(&[EnvName::parse("a").unwrap()]),
        None,
        &Options::default(),
    )
    .unwrap();
    assert_eq!(
        text_at(&gemini_md),
        format!("# Mine\n\n{BLOCK_START}\n@./.gemini/sekisho/b/units.md\n{BLOCK_END}\n\nAfter.\n")
    );
    assert!(!w.project.join(".gemini/sekisho/a").exists());
    install::uninstall(&scope, None, None, &Options::default()).unwrap();
    assert_eq!(text_at(&gemini_md), "# Mine\n\n\nAfter.\n");
}

#[test]
fn a_gemini_md_sekisho_made_goes_with_its_block() {
    let w = World::new();
    let scope = w.project_scope();
    w.install(&GeminiCli, &scope, &style_and_review(&w))
        .unwrap();
    install::uninstall(&scope, None, None, &Options::default()).unwrap();
    assert!(!w.project.join("GEMINI.md").exists());
}

#[test]
fn a_gemini_md_the_person_wrote_is_asked_about() {
    let w = World::new();
    let scope = w.project_scope();
    std::fs::write(w.project.join("GEMINI.md"), "# Mine\n").unwrap();
    let err = w
        .install(&GeminiCli, &scope, &style_and_review(&w))
        .unwrap_err();
    let InstallError::NeedsConsent(items) = err else {
        panic!("{err:?}")
    };
    assert_eq!(items, ["GEMINI.md"]);
    assert_eq!(text_at(&w.project.join("GEMINI.md")), "# Mine\n");
    let yes = Options {
        yes: true,
        ..Options::default()
    };
    w.install_with(&GeminiCli, &scope, &style_and_review(&w), &yes)
        .unwrap();
    assert_eq!(
        text_at(&w.project.join("GEMINI.md")),
        format!(
            "# Mine\n\n{BLOCK_START}\n@./.gemini/sekisho/a/style.md\n@./.gemini/sekisho/a/review.md\n{BLOCK_END}\n"
        )
    );
    // Once the block is there, a later environment isn't asked about.
    let b = delivery("b", vec![w.instruction("team", "units", "Micro-USD.\n")]);
    w.install(&GeminiCli, &scope, &b).unwrap();
}

#[test]
fn gemini_cli_skips_instructions_when_its_context_file_name_leaves_out_gemini_md() {
    let w = World::new();
    let scope = w.project_scope();
    std::fs::create_dir_all(w.project.join(".gemini")).unwrap();
    std::fs::write(
        w.project.join(".gemini/settings.json"),
        r#"{"context": {"fileName": ["AGENTS.md", "CONTEXT.md"]}}"#,
    )
    .unwrap();
    let report = w
        .install(&GeminiCli, &scope, &style_and_review(&w))
        .unwrap();
    assert!(!w.project.join("GEMINI.md").exists());
    assert!(!w.project.join(".gemini/sekisho").exists());
    assert!(!w.project.join("AGENTS.md").exists());
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("context.fileName") && x.contains("style")),
        "{:?}",
        report.warnings
    );
    // GEMINI.md among the names: imported as usual.
    std::fs::write(
        w.project.join(".gemini/settings.json"),
        r#"{"context": {"fileName": ["AGENTS.md", "GEMINI.md"]}}"#,
    )
    .unwrap();
    w.install(&GeminiCli, &scope, &style_and_review(&w))
        .unwrap();
    assert!(w.project.join("GEMINI.md").exists());
}

#[test]
fn gemini_cli_global_gets_an_extension() {
    let w = World::new();
    let d = style_and_review(&w);
    w.install(&GeminiCli, &w.global_scope(), &d).unwrap();
    let ext = w.home.join(".gemini/extensions/sekisho-a");
    let manifest = json_at(&ext.join("gemini-extension.json"));
    assert_eq!(manifest["name"], "sekisho-a");
    assert_eq!(manifest["version"], &d.revision[..12]);
    assert_eq!(manifest["contextFileName"], "SEKISHO.md");
    assert_eq!(
        text_at(&ext.join("SEKISHO.md")),
        format!("{STYLE_FILE}\n{REVIEW_FILE}")
    );
    assert!(!w.home.join(".gemini/GEMINI.md").exists());
}

#[test]
fn qwen_code_gets_rule_files() {
    let w = World::new();
    w.install(&QwenCode, &w.project_scope(), &style_and_review(&w))
        .unwrap();
    assert_eq!(
        text_at(&w.project.join(".qwen/rules/sekisho-a-style.md")),
        STYLE_FILE
    );
    // QWEN_HOME is the `.qwen` folder itself.
    let qwen_home = w.tmp.path().join("qwen-home");
    let scope = Scope::global(
        &w.config,
        w.homes(&[("QWEN_HOME", qwen_home.to_str().unwrap())]),
    );
    w.install(&QwenCode, &scope, &style_and_review(&w)).unwrap();
    assert_eq!(
        text_at(&qwen_home.join("rules/sekisho-a-review.md")),
        REVIEW_FILE
    );
}

#[test]
fn qwen_code_says_when_the_folder_is_not_the_repositorys_root() {
    let w = World::new();
    assert_eq!(QwenCode.note(&w.project_scope()), None);
    // A folder inside a repository, below its root.
    let repo = w.tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let sub = repo.join("services/pay");
    std::fs::create_dir_all(&sub).unwrap();
    let scope = Scope::project(&sub, &w.config, w.homes(&[]));
    let note = QwenCode.note(&scope).unwrap();
    assert!(note.contains(".qwen/rules"), "{note}");
    assert!(note.contains("root"), "{note}");
    // At the repository's root: nothing to say.
    let scope = Scope::project(&repo, &w.config, w.homes(&[]));
    assert_eq!(QwenCode.note(&scope), None);
}

#[test]
fn path_scoped_plugin_instructions_reach_only_tools_that_scope() {
    let w = World::new();
    let scope = w.project_scope();
    let d = delivery("a", vec![w.plugin("fmt")]);

    let report = w.install(&ClaudeCode, &scope, &d).unwrap();
    let rules = w.project.join(".claude/rules");
    assert_eq!(
        text_at(&rules.join("sekisho-a-fmt-always.md")),
        "# always (Sekisho environment a, plugin fmt, instruction always)\n\n\
         Format before you commit.\n"
    );
    assert_eq!(
        text_at(&rules.join("sekisho-a-fmt-rust.md")),
        "---\npaths:\n  - \"**/*.rs\"\n  - \"src/**\"\n---\n\
         # Rust (Sekisho environment a, plugin fmt, instruction rust)\n\nUse rustfmt.\n"
    );
    assert_eq!(
        text_at(&rules.join("sekisho-a-fmt-plain.md")),
        "# plain (Sekisho environment a, plugin fmt, instruction plain)\n\nKeep lines short.\n"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("AGENTS.md") && x.contains("fmt")),
        "{:?}",
        report.warnings
    );

    w.install(&QwenCode, &scope, &d).unwrap();
    assert!(
        text_at(&w.project.join(".qwen/rules/sekisho-a-fmt-rust.md"))
            .starts_with("---\npaths:\n  - \"**/*.rs\"\n  - \"src/**\"\n---\n")
    );

    let report = w.install(&GeminiCli, &scope, &d).unwrap();
    let gemini_md = text_at(&w.project.join("GEMINI.md"));
    assert!(
        gemini_md.contains("@./.gemini/sekisho/a/fmt-always.md\n"),
        "{gemini_md}"
    );
    assert!(
        gemini_md.contains("@./.gemini/sekisho/a/fmt-plain.md\n"),
        "{gemini_md}"
    );
    assert!(!gemini_md.contains("rust"), "{gemini_md}");
    assert!(!w.project.join(".gemini/sekisho/a/fmt-rust.md").exists());
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("gemini-cli") && x.contains("rust") && x.contains("skipped")),
        "{:?}",
        report.warnings
    );
}

/// Writes one file of its choosing, as a later adapter might.
struct Forger {
    root: Root,
    path: &'static str,
}

impl Adapter for Forger {
    fn tool(&self) -> Tool {
        Tool::ClaudeCode
    }
    fn detect(&self, _scope: &Scope) -> bool {
        true
    }
    fn render(&self, _d: &Delivery, _ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        Ok(Rendered {
            files: vec![FileWrite {
                root: self.root,
                path: RelPath::parse(self.path).unwrap(),
                content: b"Forged.\n".to_vec(),
                executable: false,
                origin: Origin::Environment,
            }],
            ..Rendered::default()
        })
    }
    fn unmanaged(&self, _scope: &Scope, _locks: &[&install::lock::Lock]) -> Vec<UnmanagedItem> {
        Vec::new()
    }
    fn layering(&self) -> Layering {
        ClaudeCode.layering()
    }
}

#[test]
fn root_instruction_files_are_never_written() {
    let w = World::new();
    let d = delivery(
        "a",
        vec![w.instruction("team", "style", STYLE), w.plugin("fmt")],
    );
    for adapter in [&ClaudeCode as &dyn Adapter, &GeminiCli, &QwenCode] {
        for scope in [w.project_scope(), w.global_scope()] {
            w.install(adapter, &scope, &d).unwrap();
        }
    }
    let mut written = files_under(&w.project);
    written.extend(files_under(&w.home));
    written.extend(files_under(&w.config));
    for name in written {
        let base = name.rsplit('/').next().unwrap();
        assert!(
            ![
                "AGENTS.md",
                "CLAUDE.md",
                "AGENTS.override.md",
                "copilot-instructions.md"
            ]
            .contains(&base),
            "{name}"
        );
    }

    // The engine refuses a write to each, whatever adapter asks.
    for (root, path) in [
        (Root::Project, "AGENTS.md"),
        (Root::Project, "CLAUDE.md"),
        (Root::Project, ".claude/CLAUDE.md"),
        (Root::Project, "AGENTS.override.md"),
        (Root::Project, ".github/copilot-instructions.md"),
        (Root::Project, "agents.md"),
        (Root::ToolHome(Tool::ClaudeCode), "CLAUDE.md"),
        (Root::ToolHome(Tool::Codex), "AGENTS.md"),
        (Root::ToolHome(Tool::Codex), "AGENTS.override.md"),
    ] {
        let scope = match root {
            Root::Project => w.project_scope(),
            _ => w.global_scope(),
        };
        let err = w
            .install(&Forger { root, path }, &scope, &delivery("a", Vec::new()))
            .unwrap_err();
        let InstallError::Refused(why) = err else {
            panic!("{path}: {err:?}")
        };
        assert!(why.contains(path), "{why}");
    }
}

#[test]
fn two_environments_with_one_instruction_name_and_other_text_clash() {
    let w = World::new();
    let scope = w.project_scope();
    let a = delivery("a", vec![w.instruction("team", "style", STYLE)]);
    w.install(&ClaudeCode, &scope, &a).unwrap();
    let b = delivery(
        "b",
        vec![w.instruction("other", "style", "Long functions.\n")],
    );
    let err = w.install(&ClaudeCode, &scope, &b).unwrap_err();
    let InstallError::Clash(clashes) = err else {
        panic!("{err:?}")
    };
    assert_eq!(clashes.len(), 1);
    assert_eq!(clashes[0].kind, ContentKind::Instructions);
    assert_eq!(clashes[0].name, "style");
    assert_eq!(clashes[0].held_by.as_str(), "a");
    assert!(!w.project.join(".claude/rules/sekisho-b-style.md").exists());
    // The same entry at another digest is no clash (D219).
    let newer = delivery(
        "d",
        vec![w.instruction("team", "style", "# Payments style\n\nNewer.\n")],
    );
    w.install(&ClaudeCode, &scope, &newer).unwrap();
    install::uninstall(
        &scope,
        Some(&[EnvName::parse("d").unwrap()]),
        None,
        &Options::default(),
    )
    .unwrap();
    // Nor is the same text from another source.
    let same = delivery("c", vec![w.instruction("other", "style", STYLE)]);
    let same = Delivery {
        entries: vec![DeliveredEntry {
            digest: a.entries[0].digest.clone(),
            ..same.entries[0].clone()
        }],
        ..same
    };
    w.install(&ClaudeCode, &scope, &same).unwrap();
}

#[test]
fn over_16_kib_of_always_loaded_text_warns() {
    let w = World::new();
    let scope = w.project_scope();
    let big = |env: &str, names: [&str; 3]| {
        delivery(
            env,
            names
                .iter()
                .map(|n| w.instruction("team", n, &"x".repeat(3000)))
                .collect(),
        )
    };
    let report = w
        .install(&ClaudeCode, &scope, &big("a", ["one", "two", "three"]))
        .unwrap();
    assert!(
        !report.warnings.iter().any(|x| x.contains("16384")),
        "{:?}",
        report.warnings
    );
    let report = w
        .install(&ClaudeCode, &scope, &big("b", ["four", "five", "six"]))
        .unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|x| x.contains("claude-code") && x.contains("16384")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn a_dropped_instruction_is_removed() {
    let w = World::new();
    let scope = w.project_scope();
    for adapter in [&ClaudeCode as &dyn Adapter, &GeminiCli] {
        w.install(adapter, &scope, &style_and_review(&w)).unwrap();
    }
    let only_style = delivery("a", vec![w.instruction("team", "style", STYLE)]);
    for adapter in [&ClaudeCode as &dyn Adapter, &GeminiCli] {
        w.install(adapter, &scope, &only_style).unwrap();
    }
    assert!(w.project.join(".claude/rules/sekisho-a-style.md").exists());
    assert!(!w.project.join(".claude/rules/sekisho-a-review.md").exists());
    assert!(!w.project.join(".gemini/sekisho/a/review.md").exists());
    assert_eq!(
        text_at(&w.project.join("GEMINI.md")),
        format!("{BLOCK_START}\n@./.gemini/sekisho/a/style.md\n{BLOCK_END}\n")
    );
}
