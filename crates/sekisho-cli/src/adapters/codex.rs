//! Codex, from its source at `rust-v0.172.0` (2026-11-26).
//!
//! Codex ignores `model_provider`, `model_providers` and `profile(s)` in a
//! project's `.codex/config.toml` (`PROJECT_LOCAL_CONFIG_DENYLIST`), so a
//! project install's model settings are a profile in Codex's home,
//! `$CODEX_HOME/sekisho-<env>.config.toml`, which `codex --profile
//! sekisho-<env>` and `sekisho run codex` select (D192); a global install
//! writes the same keys into `$CODEX_HOME/config.toml` as well. The
//! provider runs `sekisho token` for its key (`auth.command`) and names
//! the tool in `http_headers` (D227).
//!
//! The project's `.codex/config.toml` (or the global `config.toml`) holds
//! `mcp_servers` and `developer_instructions`, each environment's
//! instructions in a section of its own (D222). Skills go in the shared
//! `.agents/skills/` (`~/.agents/skills/` globally). Codex has no custom
//! prompts, and a plugin works only once Codex itself installs it into its
//! cache, so plugins are taken apart (D205). Codex reads a project's
//! config only once the folder is trusted.

use std::path::Path;

use serde_json::{Value, json};

use sekisho_catalog::delivery::Delivery;
use sekisho_core::formats::ClientApi;

use super::translate::{self, Layout, RemoteKey, Servers, UserVars};
use super::{
    Adapter, AdapterError, FileWrite, Format, KeyWrite, Layer, Layering, Origin, RelPath,
    RenderContext, Rendered, Root, RunSpec, ScopeKind, Skipped, Tool, UnmanagedItem, on_path,
};
use crate::install::Scope;
use crate::install::instructions;
use crate::install::merge::section_key;

/// How often Codex runs `sekisho token` again: 5 minutes, its default,
/// well inside a session token's hour.
const REFRESH_INTERVAL_MS: u64 = 300_000;
/// How long `sekisho token` may take, which can refresh at the server:
/// above Codex's 5-second default.
const TOKEN_TIMEOUT_MS: u64 = 15_000;

/// Codex.
#[derive(Clone, Copy, Debug, Default)]
pub struct Codex;

impl Codex {
    fn layout(scope: &Scope) -> Layout {
        let project = scope.kind() == ScopeKind::Project;
        let root = if project {
            Root::Project
        } else {
            Root::ToolHome(Tool::Codex)
        };
        Layout {
            tool: Tool::Codex,
            root,
            base: if project { ".codex/" } else { "" },
            remote: RemoteKey::UrlOnly,
            user_vars: UserVars::Forward,
            skills: if project {
                (Root::Project, ".agents/skills/")
            } else {
                (Root::UserHome, ".agents/skills/")
            },
            servers: Servers {
                root,
                file: if project {
                    ".codex/config.toml"
                } else {
                    "config.toml"
                },
                format: Format::Toml,
                key: "mcp_servers",
            },
            commands: false,
        }
    }

    /// The profile's name, and the provider's.
    fn profile(d: &Delivery) -> String {
        format!("sekisho-{}", d.environment)
    }

    /// The environment's `openai` default, when the Responses API reaches
    /// it: Codex speaks nothing else.
    fn model(d: &Delivery) -> Option<String> {
        let default = d.defaults.openai.as_ref()?;
        d.models
            .iter()
            .any(|m| m.name == *default && m.apis.contains(&ClientApi::Responses))
            .then(|| default.to_string())
    }

    fn provider(d: &Delivery, sekisho: &str) -> Value {
        json!({
            "name": format!("Sekisho {}", d.environment),
            "base_url": format!("{}/openai/v1", d.gateway_url),
            "wire_api": "responses",
            "http_headers": { (sekisho_core::TOOL_HEADER): Tool::Codex.as_str() },
            "auth": {
                "command": sekisho,
                "args": ["token"],
                "refresh_interval_ms": REFRESH_INTERVAL_MS,
                "timeout_ms": TOKEN_TIMEOUT_MS,
            },
        })
    }

    /// The profile file, Sekisho's own.
    fn profile_file(model: &str, name: &str, provider: &Value) -> Result<Vec<u8>, AdapterError> {
        let mut text = None;
        for (key, value) in [
            (vec!["model"], json!(model)),
            (vec!["model_provider"], json!(name)),
            (vec!["model_providers", name], provider.clone()),
        ] {
            let key: Vec<String> = key.into_iter().map(str::to_owned).collect();
            text = Some(
                crate::install::merge::set(Format::Toml, text.as_deref(), &key, &value)
                    .map_err(AdapterError)?,
            );
        }
        let mut text = text.unwrap_or_default();
        text.insert_str(
            0,
            &format!(
                "# Written by `sekisho use`: Codex's model settings for `codex --profile {name}`.\n"
            ),
        );
        Ok(text.into_bytes())
    }
}

/// Codex's keys for a folder's trust decision, as it compares them: the
/// path, case-folded on Windows.
fn trust_key(path: &Path) -> String {
    let s = path
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_owned();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

impl Adapter for Codex {
    fn tool(&self) -> Tool {
        Tool::Codex
    }

    fn detect(&self, scope: &Scope) -> bool {
        let folder = match scope.kind() {
            ScopeKind::Project => scope.root(Root::Project).map(|p| p.join(".codex")),
            ScopeKind::Global => scope.root(Root::ToolHome(Tool::Codex)),
        };
        folder.is_some_and(|f| f.is_dir()) || on_path("codex")
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        let project = ctx.scope.kind() == ScopeKind::Project;
        let layout = Self::layout(ctx.scope);
        let config = RelPath::parse(layout.servers.file)?;
        let mut out = Rendered::default();

        if ctx.models {
            match Self::model(d) {
                None => out.skipped.push(Skipped {
                    what: "models".into(),
                    why: "the environment has no openai default model the Responses API reaches"
                        .into(),
                }),
                Some(model) => {
                    let name = Self::profile(d);
                    let provider = Self::provider(d, ctx.sekisho);
                    out.files.push(FileWrite {
                        root: Root::ToolHome(Tool::Codex),
                        path: RelPath::parse(&format!("{name}.config.toml"))?,
                        content: Self::profile_file(&model, &name, &provider)?,
                        executable: false,
                        origin: Origin::Models,
                    });
                    if !project {
                        let key = |key: &[&str], value: Value| KeyWrite {
                            root: layout.root,
                            file: config.clone(),
                            format: Format::Toml,
                            key: key.iter().map(|k| (*k).to_owned()).collect(),
                            value,
                            origin: Origin::Models,
                        };
                        out.keys.push(key(&["model"], json!(model)));
                        out.keys.push(key(&["model_provider"], json!(name)));
                        out.keys.push(key(&["model_providers", &name], provider));
                    }
                    out.run = self.run_spec(d);
                }
            }
        }

        // One `developer_instructions` per scope: this environment's
        // section of it (D222). Codex loads no path-scoped instructions.
        let (list, skipped) = instructions::collect(d, ctx)?;
        out.skipped.extend(skipped);
        let mut every = Vec::new();
        for i in &list {
            if i.paths.is_some() {
                out.skipped.push(Skipped {
                    what: i.label.clone(),
                    why: "Codex loads no instructions for some files only".into(),
                });
            } else {
                every.push(i);
            }
        }
        if !every.is_empty() {
            let text = instructions::joined(&every);
            let text = text.trim_end_matches(['\n', '\r']).to_owned();
            out.always_loaded += text.len() as u64;
            out.keys.push(KeyWrite {
                root: layout.root,
                file: config,
                format: Format::Toml,
                key: section_key(&["developer_instructions"], d.environment.as_str()),
                value: Value::String(text),
                origin: Origin::Environment,
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

    /// A project's models are a profile, a global install's the user
    /// config: they never meet, since the profile applies only when chosen.
    /// The project's config layer goes over the user's, so its MCP servers
    /// and `developer_instructions` win; skills of both scopes are listed.
    fn layering(&self) -> Layering {
        Layering {
            models: Layer {
                both_load: false,
                same_name_wins: Some(ScopeKind::Project),
            },
            mcp_servers: Layer {
                both_load: true,
                same_name_wins: Some(ScopeKind::Project),
            },
            skills: Layer {
                both_load: true,
                same_name_wins: None,
            },
            prompts: Layer {
                both_load: false,
                same_name_wins: None,
            },
            plugins: Layer {
                both_load: true,
                same_name_wins: Some(ScopeKind::Project),
            },
            instructions: Layer {
                both_load: false,
                same_name_wins: Some(ScopeKind::Project),
            },
        }
    }

    /// The profile carries the provider and its `auth.command`, so the run
    /// only selects it.
    fn run_spec(&self, d: &Delivery) -> Option<RunSpec> {
        Self::model(d)?;
        Some(RunSpec {
            program: "codex".into(),
            args: vec!["--profile".into(), Self::profile(d)],
            ..RunSpec::default()
        })
    }

    /// Codex runs `sekisho token` itself: the profile is all it needs.
    fn start_hint(&self, d: &Delivery) -> Option<String> {
        self.run_spec(d)?;
        Some(format!(
            "Start Codex with `codex --profile {}`, or `sekisho run codex`.",
            Self::profile(d)
        ))
    }

    /// Codex reads a project's `.codex/config.toml`, its MCP servers and
    /// instructions, only in a folder the person trusts: said until then.
    /// Sekisho never trusts a folder for them.
    fn note(&self, scope: &Scope) -> Option<String> {
        let folder = scope.project_folder()?;
        let home = scope.root(Root::ToolHome(Tool::Codex))?;
        let trusted: Vec<String> = std::fs::read_to_string(home.join("config.toml"))
            .ok()
            .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
            .and_then(|doc| {
                let projects = doc.get("projects")?.as_table_like()?;
                Some(
                    projects
                        .iter()
                        .filter(|(_, v)| {
                            v.get("trust_level").and_then(|t| t.as_str()) == Some("trusted")
                        })
                        .map(|(k, _)| trust_key(Path::new(k)))
                        .collect(),
                )
            })
            .unwrap_or_default();
        // The folder, or a folder above it up to its Git repository's root.
        for dir in folder.ancestors() {
            if trusted.contains(&trust_key(dir)) {
                return None;
            }
            if dir.join(".git").exists() {
                break;
            }
        }
        Some(
            "Codex reads this folder's .codex/config.toml (its MCP servers and instructions) \
             only once you trust the folder: answer yes when Codex asks."
                .to_owned(),
        )
    }
}
