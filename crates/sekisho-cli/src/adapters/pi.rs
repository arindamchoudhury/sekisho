//! Pi, from its source and docs at `earendil-works/pi` `ccda1f7`
//! (2026-11-27, npm `@earendil-works/pi-coding-agent` 1.6.1).
//!
//! Pi reads `models.json` only from its agent folder, takes a provider's
//! `baseUrl` literally and caches a `!command` key for the whole process,
//! so neither the run's URL nor a fresh token can reach it there. Instead
//! `sekisho use` writes a small fixed extension, `.pi/extensions/
//! sekisho-<env>.ts` (`<agent-dir>/extensions/` globally), which only
//! inside `sekisho run pi` registers a provider per format that has a
//! default at the run's proxy, with the `x-sekisho-tool` header; `sekisho
//! run pi` chooses the model (D233). Pi loads a project's extensions only
//! once the person trusts the project.
//!
//! MCP servers go in `.pi/mcp.json` (`<agent-dir>/mcp.json`), skills in the
//! shared `.agents/skills/`, prompts in `.pi/prompts/`, and instructions in
//! `.pi/APPEND_SYSTEM.md`, each environment's a section of its own (D222);
//! a project's file replaces the global one, "not combined".

use std::fmt::Write as _;

use sekisho_catalog::delivery::Delivery;
use sekisho_core::formats::ClientApi;

use super::translate::{self, Commands, Layout, RemoteKey, Servers, UserVars};
use super::{
    Adapter, AdapterError, FileWrite, Format, KeyWrite, Layer, Layering, Origin, RelPath,
    RenderContext, Rendered, Root, RunSpec, ScopeKind, Skipped, Tool, UnmanagedItem, on_path,
};
use crate::install::Scope;
use crate::install::instructions;
use crate::install::merge::file_section_key;

/// Pi.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pi;

/// One provider format: its name's suffix, Pi's API for it, the gateway
/// path its base URL ends with, and the API its models must reach.
const FORMATS: [(&str, &str, &str, ClientApi); 3] = [
    (
        "anthropic",
        "anthropic-messages",
        "anthropic",
        ClientApi::Messages,
    ),
    (
        "openai",
        "openai-completions",
        "openai/v1",
        ClientApi::ChatCompletions,
    ),
    (
        "gemini",
        "google-generative-ai",
        "gemini/v1beta",
        ClientApi::Gemini,
    ),
];

/// The provider and model `sekisho run pi` starts on: the first format,
/// in that order, whose default its API reaches.
fn start(d: &Delivery) -> Option<(String, String)> {
    let defaults = [
        d.defaults.anthropic.as_ref(),
        d.defaults.openai.as_ref(),
        d.defaults.gemini.as_ref(),
    ];
    FORMATS
        .iter()
        .zip(defaults)
        .find_map(|((suffix, _, _, api), default)| {
            let default = default?;
            d.models
                .iter()
                .any(|m| m.name == *default && m.apis.contains(api))
                .then(|| {
                    (
                        format!("sekisho-{}-{suffix}", d.environment),
                        default.to_string(),
                    )
                })
        })
}

fn quoted(s: &str) -> String {
    serde_json::to_string(s).expect("strings serialize")
}

/// The extension: fixed code, the environment's name and model IDs its
/// only data, each written as a JSON string.
fn extension(d: &Delivery) -> Option<String> {
    let env = d.environment.as_str();
    let defaults = [
        d.defaults.anthropic.as_ref(),
        d.defaults.openai.as_ref(),
        d.defaults.gemini.as_ref(),
    ];
    let mut providers = String::new();
    for ((suffix, api, path, client), default) in FORMATS.iter().zip(defaults) {
        if default.is_none() {
            continue;
        }
        let models: Vec<String> = d
            .models
            .iter()
            .filter(|m| m.apis.contains(client))
            .map(|m| m.name.to_string())
            .collect();
        if models.is_empty() {
            continue;
        }
        let list: String = models
            .iter()
            .map(|m| format!("model({})", quoted(m)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            providers,
            "  pi.registerProvider({}, {{\n    name: {},\n    baseUrl: url + {},\n    apiKey: \"$SEKISHO_RUN_KEY\",\n    api: {},\n    headers: {{ {}: {} }},\n    models: [{list}],\n  }});\n",
            quoted(&format!("sekisho-{env}-{suffix}")),
            quoted(&format!("Sekisho {env} ({suffix})")),
            quoted(&format!("/env/{env}/{path}")),
            quoted(api),
            quoted(sekisho_core::TOOL_HEADER),
            quoted(Tool::Pi.as_str()),
        );
    }
    if providers.is_empty() {
        return None;
    }
    Some(format!(
        "// Written by `sekisho use` (Sekisho environment {env}); edits are overwritten.\n\
         // Inside `sekisho run pi` only, it gives Pi the environment's models\n\
         // through the run's proxy, which swaps the run's key for a fresh token.\n\
         \n\
         function model(id: string) {{\n  \
           return {{ id, name: id, reasoning: false, input: [\"text\"], cost: {{ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }} }};\n\
         }}\n\
         \n\
         export default function (pi: any) {{\n  \
           const url = process.env.SEKISHO_RUN_URL;\n  \
           if (!url || !process.env.SEKISHO_RUN_KEY) return;\n\
         {providers}}}\n",
    ))
}

impl Pi {
    fn layout(scope: &Scope) -> Layout {
        let project = scope.kind() == ScopeKind::Project;
        let root = if project {
            Root::Project
        } else {
            Root::ToolHome(Tool::Pi)
        };
        Layout {
            tool: Tool::Pi,
            root,
            base: if project { ".pi/" } else { "" },
            remote: RemoteKey::UrlAndType,
            user_vars: UserVars::Reference(|_, name, _| Some(format!("${{{name}}}"))),
            skills: if project {
                (Root::Project, ".agents/skills/")
            } else {
                (Root::UserHome, ".agents/skills/")
            },
            servers: Servers {
                root,
                file: if project { ".pi/mcp.json" } else { "mcp.json" },
                format: Format::Json,
                key: "mcpServers",
                opencode: false,
            },
            commands: Commands::Markdown {
                dir: "prompts/",
                nested: false,
            },
        }
    }
}

impl Adapter for Pi {
    fn tool(&self) -> Tool {
        Tool::Pi
    }

    fn detect(&self, scope: &Scope) -> bool {
        let folder = match scope.kind() {
            ScopeKind::Project => scope.root(Root::Project).map(|p| p.join(".pi")),
            ScopeKind::Global => scope.root(Root::ToolHome(Tool::Pi)),
        };
        folder.is_some_and(|f| f.is_dir()) || on_path("pi")
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        let layout = Self::layout(ctx.scope);
        let env = d.environment.as_str();
        let mut out = Rendered::default();

        if ctx.models {
            match (extension(d), start(d)) {
                (Some(code), Some(_)) => {
                    out.files.push(FileWrite {
                        root: layout.root,
                        path: layout.path(&format!("extensions/sekisho-{env}.ts"))?,
                        content: code.into_bytes(),
                        executable: false,
                        origin: Origin::Models,
                    });
                    out.run = self.run_spec(d);
                }
                _ => out.skipped.push(Skipped {
                    what: "models".into(),
                    why: "the environment has no default model its APIs reach".into(),
                }),
            }
        }

        // One APPEND_SYSTEM.md per scope: this environment's section of it
        // (D222). Pi loads no instructions for some files only.
        let (list, skipped) = instructions::collect(d, ctx)?;
        out.skipped.extend(skipped);
        let mut every = Vec::new();
        for i in &list {
            if i.paths.is_some() {
                out.skipped.push(Skipped {
                    what: i.label.clone(),
                    why: "Pi loads no instructions for some files only".into(),
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
                file: RelPath::parse(&format!("{}APPEND_SYSTEM.md", layout.base))?,
                format: Format::MarkdownSections,
                key: file_section_key(env),
                value: serde_json::Value::String(text),
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

    /// A project's MCP server replaces a global one of its name; the first
    /// skill found wins; the project's APPEND_SYSTEM.md replaces the
    /// global one; extensions of both scopes load.
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
            prompts: both(Some(ScopeKind::Project)),
            plugins: both(Some(ScopeKind::Project)),
            instructions: Layer {
                both_load: false,
                same_name_wins: Some(ScopeKind::Project),
            },
        }
    }

    /// The extension registers the providers; the run picks the model.
    fn run_spec(&self, d: &Delivery) -> Option<RunSpec> {
        let (provider, model) = start(d)?;
        Some(RunSpec {
            program: "pi".into(),
            args: vec!["--provider".into(), provider, "--model".into(), model],
            ..RunSpec::default()
        })
    }
}
