//! GitHub Copilot CLI, from its command reference, its custom-instructions
//! page and its public changelog (`github/copilot-cli` `9e146c6`, version
//! 1.0.117, 2026-11-27); the CLI itself is closed source.
//!
//! Copilot CLI takes one custom provider per session, from its
//! `COPILOT_PROVIDER_*` variables only, so it starts through `sekisho run
//! copilot-cli`, whose proxy holds the token and names the tool (D208,
//! D227, D232). A project install writes MCP servers into `.mcp.json`,
//! shared with Claude Code in exactly Claude Code's form (D201), skills
//! into the shared `.agents/skills/`, and instructions as
//! `.github/instructions/sekisho-<env>-<name>.instructions.md`; a global one
//! `~/.copilot/mcp-config.json`, `~/.agents/skills/` and
//! `~/.copilot/instructions/`. Copilot CLI reads no prompt files, and its
//! plugins come from marketplaces it installs from itself, so plugins are
//! taken apart (D205).

use std::collections::BTreeMap;

use sekisho_catalog::delivery::Delivery;

use super::translate::{self, Layout, RemoteKey, Servers, UserVars};
use super::{
    Adapter, AdapterError, FileWrite, Format, Layer, Layering, RelPath, RenderContext, Rendered,
    Root, RunSpec, ScopeKind, Skipped, Tool, UnmanagedItem, on_path,
};
use crate::install::Scope;
use crate::install::instructions;

/// GitHub Copilot CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct CopilotCli;

impl CopilotCli {
    fn layout(scope: &Scope) -> Layout {
        let project = scope.kind() == ScopeKind::Project;
        let root = if project {
            Root::Project
        } else {
            Root::ToolHome(Tool::CopilotCli)
        };
        Layout {
            tool: Tool::CopilotCli,
            root,
            base: "",
            remote: RemoteKey::UrlAndType,
            // As Claude Code writes them, since both read `.mcp.json`.
            user_vars: UserVars::Reference(|_, name, required| {
                Some(if required {
                    format!("${{{name}}}")
                } else {
                    format!("${{{name}:-}}")
                })
            }),
            skills: if project {
                (Root::Project, ".agents/skills/")
            } else {
                (Root::UserHome, ".agents/skills/")
            },
            servers: Servers {
                root,
                file: if project {
                    ".mcp.json"
                } else {
                    "mcp-config.json"
                },
                format: Format::Json,
                key: "mcpServers",
            },
            commands: false,
        }
    }
}

impl Adapter for CopilotCli {
    fn tool(&self) -> Tool {
        Tool::CopilotCli
    }

    fn detect(&self, scope: &Scope) -> bool {
        let folder = match scope.kind() {
            ScopeKind::Project => None,
            ScopeKind::Global => scope.root(Root::ToolHome(Tool::CopilotCli)),
        };
        folder.is_some_and(|f| f.is_dir()) || on_path("copilot")
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        let project = ctx.scope.kind() == ScopeKind::Project;
        let layout = Self::layout(ctx.scope);
        let mut out = Rendered::default();

        if ctx.models {
            match self.run_spec(d) {
                None => out.skipped.push(Skipped {
                    what: "models".into(),
                    why: "the environment has no anthropic or openai default model".into(),
                }),
                run => out.run = run,
            }
        }

        // One `.instructions.md` each; `applyTo: "**"` for every path, a
        // plugin's path-scoped one keeping its globs (D222, D224).
        let (list, skipped) = instructions::collect(d, ctx)?;
        out.skipped.extend(skipped);
        let env = d.environment.as_str();
        for i in &list {
            if i.paths.is_none() {
                out.always_loaded += i.text.len() as u64;
            }
            let file = format!("sekisho-{env}-{}.instructions.md", i.stem);
            out.files.push(FileWrite {
                root: layout.root,
                path: RelPath::parse(&if project {
                    format!(".github/instructions/{file}")
                } else {
                    format!("instructions/{file}")
                })?,
                content: i.with_apply_to().into_bytes(),
                executable: false,
                origin: i.origin.clone(),
            });
        }

        translate::content(d, ctx, &layout, &mut out)?;
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

    /// Workspace and user MCP servers both load; the first skill of a name
    /// found wins, the project's; instructions of every level combine.
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
            prompts: Layer {
                both_load: false,
                same_name_wins: None,
            },
            plugins: both(Some(ScopeKind::Project)),
            instructions: both(None),
        }
    }

    /// One provider (D208): Anthropic Messages with the `anthropic`
    /// default, else Chat Completions with the `openai` one, through the
    /// run's proxy with the run's key (D232).
    fn run_spec(&self, d: &Delivery) -> Option<RunSpec> {
        let (kind, model, path) = match (&d.defaults.anthropic, &d.defaults.openai) {
            (Some(m), _) => ("anthropic", m, "anthropic"),
            (None, Some(m)) => ("openai", m, "openai/v1"),
            (None, None) => return None,
        };
        let env: BTreeMap<String, String> = [
            ("COPILOT_PROVIDER_TYPE", kind.to_owned()),
            (
                "COPILOT_PROVIDER_BASE_URL",
                format!("${{SEKISHO_RUN_URL}}/env/{}/{path}", d.environment),
            ),
            ("COPILOT_PROVIDER_API_KEY", "${SEKISHO_RUN_KEY}".to_owned()),
            ("COPILOT_MODEL", model.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        Some(RunSpec {
            program: "copilot".into(),
            args: Vec::new(),
            env,
            tool_header: true,
        })
    }
}
