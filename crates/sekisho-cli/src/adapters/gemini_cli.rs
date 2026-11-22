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

use serde_json::json;

use sekisho_catalog::delivery::Delivery;

use super::translate::{self, Layout, RemoteKey};
use super::{
    Adapter, AdapterError, Layer, Layering, Origin, RenderContext, Rendered, Root, RunSpec,
    ScopeKind, Skipped, Tool, UnmanagedItem, on_path,
};
use crate::install::Scope;

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
            user_var: |_, name, _| Some(format!("${{{name}}}")),
        }
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
