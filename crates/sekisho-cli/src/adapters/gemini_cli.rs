//! Gemini CLI, from its source at `acfb42d` (2026-11-19).
//!
//! A project install writes `.gemini/settings.json` (`model.name`,
//! `security.auth.selectedType`, `mcpServers`), `.gemini/skills/` and
//! `.gemini/commands/`; a global one the same in `$GEMINI_CLI_HOME/.gemini`
//! or `~/.gemini`. Gemini CLI reads a folder's `.gemini/` only once the
//! person trusts the folder (its folder trust is on by default).
//!
//! The model key comes from `sekisho run gemini-cli` (D194): the run sets
//! `GOOGLE_GEMINI_BASE_URL` to its proxy and `GEMINI_API_KEY` to its
//! secret, which Gemini CLI sends as `x-goog-api-key`. The `gemini-api-key`
//! auth type is chosen, not `gateway`: Gemini CLI's own check refuses
//! `gateway` outside ACP, while the API-key type also takes its base URL
//! from `GOOGLE_GEMINI_BASE_URL`.

use serde_json::{Value, json};

use sekisho_catalog::delivery::Delivery;

use super::translate::{self, Layout, RemoteKey, Servers, UserVars};
use super::{
    Adapter, AdapterError, FileWrite, Format, KeyWrite, Layer, Layering, Origin, RelPath,
    RenderContext, Rendered, Root, RunSpec, ScopeKind, Skipped, Tool, UnmanagedItem, on_path,
};
use crate::install::instructions::{self, Instruction};
use crate::install::{Scope, merge};

/// The context file names Gemini CLI reads, from `context.fileName` in the
/// workspace's settings, else the user's; `None` when neither sets it, so
/// GEMINI.md alone.
fn context_file_names(ctx: &RenderContext<'_>) -> Option<Vec<String>> {
    let key = ["context".to_owned(), "fileName".to_owned()];
    let read = |root: Root, file: &str| {
        let text = (ctx.read)(root, &RelPath::parse(file).ok()?)?;
        merge::get(Format::Jsonc, &text, &key).ok().flatten()
    };
    let project = ctx.scope.kind() == ScopeKind::Project;
    let value = project
        .then(|| read(Root::Project, ".gemini/settings.json"))
        .flatten()
        .or_else(|| read(Root::ToolHome(Tool::GeminiCli), "settings.json"))?;
    match value {
        Value::String(s) => Some(vec![s]),
        Value::Array(items) => Some(
            items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        ),
        _ => None,
    }
}

/// Gemini CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct GeminiCli;

impl GeminiCli {
    fn layout(scope: &Scope) -> Layout {
        let project = scope.kind() == ScopeKind::Project;
        Layout {
            tool: Tool::GeminiCli,
            root: if project {
                Root::Project
            } else {
                Root::ToolHome(Tool::GeminiCli)
            },
            base: if project { ".gemini/" } else { "" },
            remote: RemoteKey::UrlAndType,
            // Servers get only a sanitised copy of Gemini CLI's environment
            // (no *TOKEN*, *KEY*, *SECRET* names), so every variable is
            // named; an unset one becomes empty.
            user_vars: UserVars::Reference(|_, name, _| Some(format!("${{{name}}}"))),
            skills: if project {
                (Root::Project, ".gemini/skills/")
            } else {
                (Root::ToolHome(Tool::GeminiCli), "skills/")
            },
            servers: Servers {
                root: if project {
                    Root::Project
                } else {
                    Root::ToolHome(Tool::GeminiCli)
                },
                file: if project {
                    ".gemini/settings.json"
                } else {
                    "settings.json"
                },
                format: Format::Jsonc,
                key: "mcpServers",
            },
            commands: true,
        }
    }
}

impl GeminiCli {
    /// Each instruction a file under `.gemini/sekisho/<env>/`, imported by
    /// one block in GEMINI.md, which Gemini CLI reads unless its
    /// `context.fileName` leaves it out (D222, D223).
    fn project_instructions(
        d: &Delivery,
        ctx: &RenderContext<'_>,
        every: &[&Instruction],
        out: &mut Rendered,
    ) -> Result<(), AdapterError> {
        if let Some(names) = context_file_names(ctx)
            && !names.iter().any(|n| n == "GEMINI.md")
        {
            for i in every {
                out.skipped.push(Skipped {
                    what: i.label.clone(),
                    why: format!(
                        "Gemini CLI's context.fileName setting reads {} and not GEMINI.md, \
                         which imports Sekisho's instructions; Sekisho never writes those \
                         files",
                        names.join(", ")
                    ),
                });
            }
            return Ok(());
        }
        let gemini_md = RelPath::parse("GEMINI.md")?;
        for i in every {
            let file = format!(".gemini/sekisho/{}/{}.md", d.environment, i.stem);
            out.always_loaded += i.text.len() as u64;
            out.files.push(FileWrite {
                root: Root::Project,
                path: RelPath::parse(&file)?,
                content: i.text.clone().into_bytes(),
                executable: false,
                origin: i.origin.clone(),
            });
            out.keys.push(KeyWrite {
                root: Root::Project,
                file: gemini_md.clone(),
                format: Format::MarkdownBlock,
                key: vec![format!("@./{file}")],
                value: Value::Bool(true),
                origin: i.origin.clone(),
            });
        }
        Ok(())
    }

    /// An extension of the environment's own, whose context file holds its
    /// instructions: Gemini CLI's extensions are user-level only.
    fn extension(
        d: &Delivery,
        every: &[&Instruction],
        out: &mut Rendered,
    ) -> Result<(), AdapterError> {
        let name = format!("sekisho-{}", d.environment);
        let text = instructions::joined(every);
        out.always_loaded += text.len() as u64;
        let version = d.revision.get(..12).unwrap_or("0");
        let mut manifest = serde_json::to_vec_pretty(&json!({
            "name": name,
            "version": version,
            "description": format!("Sekisho environment {}'s instructions", d.environment),
            "contextFileName": "SEKISHO.md",
        }))
        .expect("JSON values serialize");
        manifest.push(b'\n');
        for (file, content) in [
            ("gemini-extension.json", manifest),
            ("SEKISHO.md", text.into_bytes()),
        ] {
            out.files.push(FileWrite {
                root: Root::ToolHome(Tool::GeminiCli),
                path: RelPath::parse(&format!("extensions/{name}/{file}"))?,
                content,
                executable: false,
                origin: Origin::Environment,
            });
        }
        Ok(())
    }
}

impl Adapter for GeminiCli {
    fn tool(&self) -> Tool {
        Tool::GeminiCli
    }

    fn detect(&self, scope: &Scope) -> bool {
        let folder = match scope.kind() {
            ScopeKind::Project => scope.root(Root::Project).map(|p| p.join(".gemini")),
            ScopeKind::Global => scope.root(Root::ToolHome(Tool::GeminiCli)),
        };
        folder.is_some_and(|f| f.is_dir()) || on_path("gemini")
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        let layout = Self::layout(ctx.scope);
        let mut out = Rendered::default();
        if ctx.models {
            match &d.defaults.gemini {
                None => out.skipped.push(Skipped {
                    what: "models".into(),
                    why: "the environment has no model the Gemini API reaches".into(),
                }),
                Some(default) => {
                    out.keys.push(layout.key(
                        &["model", "name"],
                        json!(default.to_string()),
                        Origin::Models,
                    )?);
                    out.keys.push(layout.key(
                        &["security", "auth", "selectedType"],
                        json!("gemini-api-key"),
                        Origin::Models,
                    )?);
                    out.run = self.run_spec(d);
                }
            }
        }
        translate::content(d, ctx, &layout, &mut out)?;
        let (instructions, skipped) = instructions::collect(d, ctx)?;
        out.skipped.extend(skipped);
        let (every, scoped): (Vec<&Instruction>, Vec<&Instruction>) =
            instructions.iter().partition(|i| i.paths.is_none());
        for i in scoped {
            out.skipped.push(Skipped {
                what: i.label.clone(),
                why: "Gemini CLI has no instructions for some files only".into(),
            });
        }
        if every.is_empty() {
            return Ok(out);
        }
        if ctx.scope.kind() == ScopeKind::Project {
            Self::project_instructions(d, ctx, &every, &mut out)?;
        } else {
            Self::extension(d, &every, &mut out)?;
        }
        Ok(out)
    }

    fn unmanaged(
        &self,
        _scope: &Scope,
        _locks: &[&crate::install::lock::Lock],
    ) -> Vec<UnmanagedItem> {
        // Task 10 (`sekisho status`) reads the tool's config for these.
        Vec::new()
    }

    /// Workspace settings override user settings, `mcpServers` merged by
    /// name; workspace skills come last and win; a command in both scopes
    /// is kept twice, one renamed with a prefix.
    fn layering(&self) -> Layering {
        let both = |wins| Layer {
            both_load: true,
            same_name_wins: wins,
        };
        Layering {
            models: Layer {
                both_load: false,
                same_name_wins: Some(ScopeKind::Project),
            },
            mcp_servers: both(Some(ScopeKind::Project)),
            skills: both(Some(ScopeKind::Project)),
            prompts: both(None),
            plugins: both(Some(ScopeKind::Project)),
            instructions: both(None),
        }
    }

    fn run_spec(&self, d: &Delivery) -> Option<RunSpec> {
        d.defaults.gemini.as_ref()?;
        Some(RunSpec {
            program: "gemini".into(),
            args: Vec::new(),
            env: [
                (
                    "GOOGLE_GEMINI_BASE_URL".to_owned(),
                    format!("${{SEKISHO_RUN_URL}}/env/{}/gemini", d.environment),
                ),
                ("GEMINI_API_KEY".to_owned(), "${SEKISHO_RUN_KEY}".to_owned()),
            ]
            .into(),
            // Gemini CLI reaches Sekisho only through the run's proxy,
            // which names it to the gateway (D227).
            tool_header: true,
        })
    }

    fn note(&self, scope: &Scope) -> Option<String> {
        (scope.kind() == ScopeKind::Project).then(|| {
            "Gemini CLI reads this folder's .gemini/ only once you trust the folder: accept its \
             prompt, or use /permissions."
                .to_owned()
        })
    }
}
