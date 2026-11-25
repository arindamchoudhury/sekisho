//! Qwen Code, from its source at `1de5c0b3` (2026-11-19), a fork of Gemini
//! CLI with the same layout under `.qwen/`.
//!
//! A project install writes `.qwen/settings.json` (`modelProviders`,
//! `security.auth.selectedType`, `model.name`, `mcpServers`), `.qwen/skills/`
//! and `.qwen/commands/`; a global one the same in `$QWEN_HOME` or
//! `~/.qwen`.
//!
//! Qwen Code takes one provider per session (D208): Anthropic Messages when
//! the environment has an `anthropic` default, else Chat Completions with
//! the `openai` default. Its settings expand `${VAR}` from the process when
//! they load, so each model's `baseUrl` is `${SEKISHO_RUN_URL}/env/<env>/…`
//! and its `envKey` `SEKISHO_RUN_KEY`: both are set only inside `sekisho
//! run qwen-code` (D194), and no file holds a secret.

use serde_json::{Value, json};

use sekisho_catalog::delivery::Delivery;
use sekisho_core::formats::ClientApi;

use super::translate::{self, Layout, RemoteKey};
use super::{
    Adapter, AdapterError, FileWrite, Layer, Layering, Origin, RenderContext, Rendered, Root,
    RunSpec, ScopeKind, Skipped, Tool, UnmanagedItem, on_path,
};
use crate::install::{Scope, instructions};

/// Qwen Code.
#[derive(Clone, Copy, Debug, Default)]
pub struct QwenCode;

impl QwenCode {
    fn layout(scope: &Scope) -> Layout {
        let project = scope.kind() == ScopeKind::Project;
        Layout {
            tool: Tool::QwenCode,
            root: if project {
                Root::Project
            } else {
                Root::ToolHome(Tool::QwenCode)
            },
            base: if project { ".qwen/" } else { "" },
            remote: RemoteKey::HttpUrl,
            // Servers inherit Qwen Code's whole environment (less its own
            // secrets), so a variable of the server's own name reaches it
            // as it is; an unset `${VAR}` would stay as that text.
            user_var: |key, name, _| (key != name).then(|| format!("${{{name}}}")),
        }
    }
}

impl Adapter for QwenCode {
    fn tool(&self) -> Tool {
        Tool::QwenCode
    }

    fn detect(&self, scope: &Scope) -> bool {
        let folder = match scope.kind() {
            ScopeKind::Project => scope.root(Root::Project).map(|p| p.join(".qwen")),
            ScopeKind::Global => scope.root(Root::ToolHome(Tool::QwenCode)),
        };
        folder.is_some_and(|f| f.is_dir()) || on_path("qwen")
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        let layout = Self::layout(ctx.scope);
        let mut out = Rendered::default();
        if ctx.models {
            let chosen = match (&d.defaults.anthropic, &d.defaults.openai) {
                (Some(m), _) => Some(("anthropic", m, ClientApi::Messages, "anthropic")),
                (None, Some(m)) => Some(("openai", m, ClientApi::ChatCompletions, "openai/v1")),
                (None, None) => None,
            };
            match chosen {
                None => out.skipped.push(Skipped {
                    what: "models".into(),
                    why: "the environment has no model the Anthropic Messages or Chat \
                          Completions API reaches"
                        .into(),
                }),
                Some((protocol, default, api, path)) => {
                    let base = format!("${{SEKISHO_RUN_URL}}/env/{}/{path}", d.environment);
                    let models: Vec<Value> = d
                        .models
                        .iter()
                        .filter(|m| m.apis.contains(&api))
                        .map(|m| {
                            let mut v = json!({
                                "id": m.name.to_string(),
                                "name": m.name.to_string(),
                                "baseUrl": base,
                                "envKey": "SEKISHO_RUN_KEY",
                            });
                            if api == ClientApi::ChatCompletions {
                                v["wireApi"] = json!("chat-completions");
                            }
                            v
                        })
                        .collect();
                    out.keys.push(layout.key(
                        &["modelProviders", protocol],
                        Value::Array(models),
                        Origin::Models,
                    )?);
                    out.keys.push(layout.key(
                        &["security", "auth", "selectedType"],
                        json!(protocol),
                        Origin::Models,
                    )?);
                    out.keys.push(layout.key(
                        &["model", "name"],
                        json!(default.to_string()),
                        Origin::Models,
                    )?);
                    out.run = self.run_spec(d);
                }
            }
        }
        translate::content(d, ctx, &layout, &mut out)?;
        // Instructions as rules (`rulesDiscovery.ts`): without `paths:` at
        // session start, a plugin's path-scoped ones when a matching file
        // is touched (D222, D224).
        let (instructions, skipped) = instructions::collect(d, ctx)?;
        out.skipped.extend(skipped);
        for i in &instructions {
            if i.paths.is_none() {
                out.always_loaded += i.text.len() as u64;
            }
            out.files.push(FileWrite {
                root: layout.root,
                path: layout.path(&format!("rules/sekisho-{}-{}.md", d.environment, i.stem))?,
                content: i.with_paths().into_bytes(),
                executable: false,
                origin: i.origin.clone(),
            });
        }
        Ok(out)
    }

    /// Qwen Code reads a project's `.qwen/rules` only at its repository's
    /// root (`findProjectRoot`: the nearest folder above with `.git`), so a
    /// folder below it gets instructions Qwen Code doesn't load.
    fn note(&self, scope: &Scope) -> Option<String> {
        let folder = scope.project_folder()?;
        if folder.join(".git").exists() {
            return None;
        }
        let root = folder
            .ancestors()
            .skip(1)
            .find(|a| a.join(".git").exists())?;
        Some(format!(
            "Qwen Code reads .qwen/rules only at the repository's root, {}, so the \
             instructions installed here don't load in Qwen Code: run sekisho use there \
             for them.",
            root.display()
        ))
    }

    fn unmanaged(
        &self,
        _scope: &Scope,
        _locks: &[&crate::install::lock::Lock],
    ) -> Vec<UnmanagedItem> {
        // Task 10 (`sekisho status`) reads the tool's config for these.
        Vec::new()
    }

    /// Its settings carry the run's variables, so the run itself needs only
    /// the program.
    fn run_spec(&self, d: &Delivery) -> Option<RunSpec> {
        (d.defaults.anthropic.is_some() || d.defaults.openai.is_some()).then(|| RunSpec {
            program: "qwen".into(),
            ..RunSpec::default()
        })
    }

    /// As Gemini CLI's: workspace settings and skills win, commands in both
    /// scopes are both kept.
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
}
