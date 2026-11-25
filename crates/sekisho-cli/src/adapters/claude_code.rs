//! Claude Code (D193), from its settings, settings reference, environment
//! variables, MCP, skills and plugin marketplace docs, read 2026-11-21.
//!
//! A project install writes `.claude/settings.local.json` (personal, kept
//! out of Git by Claude Code, applied over the committed
//! `.claude/settings.json`), `.mcp.json`, `.claude/skills/` and
//! `.claude/commands/`; a global one writes `settings.json`, `skills/` and
//! `commands/` in `$CLAUDE_CONFIG_DIR` or `~/.claude`, and user MCP servers
//! in `.claude.json` (in `$CLAUDE_CONFIG_DIR`, else `~/.claude.json`).
//! Plugins go in a directory marketplace of the environment's own,
//! `sekisho-<env>`, in Sekisho's folder of the scope.

use crate::install::instructions::{self, is_agents_md};
use serde_json::{Map, Value, json};

use sekisho_catalog::delivery::{Delivery, EntryAddress};
use sekisho_catalog::model::EntrySpec;
use sekisho_core::formats::ClientApi;

use super::{
    Adapter, AdapterError, FileWrite, Format, KeyWrite, Layer, Layering, Origin, RelPath,
    RenderContext, Rendered, Root, ScopeKind, Skipped, Tool, UnmanagedItem, on_path,
};
use crate::install::Scope;
use crate::install::launch::{EnvValue, LocalServer};
use crate::install::merge::{self, item_key, line_key};

/// `apiKeyHelper`'s cache lifetime: 50 minutes, inside a session token's
/// hour (D193).
const HELPER_TTL_MS: &str = "3000000";

/// The headers Claude Code adds to its model calls: `Name: Value` lines.
const CUSTOM_HEADERS: &str = "ANTHROPIC_CUSTOM_HEADERS";

/// Claude Code.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeCode;

/// `command` as Claude Code's shell reads it: `cmd` on Windows, `/bin/sh`
/// elsewhere. A plain path is left as it is.
fn shell_word(command: &str) -> String {
    let plain = command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/\\:._-".contains(c));
    if plain {
        command.to_owned()
    } else if cfg!(windows) {
        format!("\"{command}\"")
    } else {
        format!("'{}'", command.replace('\'', r"'\''"))
    }
}

/// A local server as `.mcp.json` and `.claude.json` write it: the person's
/// variables as `${VAR}`, or `${VAR:-}` when the server can do without.
fn server_value(s: &LocalServer) -> Value {
    let env: Map<String, Value> = s
        .env
        .iter()
        .map(|(k, v)| {
            let v = match v {
                EnvValue::Literal(x) => x.clone(),
                EnvValue::FromUser {
                    name,
                    required: true,
                } => format!("${{{name}}}"),
                EnvValue::FromUser {
                    name,
                    required: false,
                } => format!("${{{name}:-}}"),
            };
            (k.clone(), Value::String(v))
        })
        .collect();
    let mut value = json!({"command": s.command, "args": s.args});
    if !env.is_empty() {
        value["env"] = Value::Object(env);
    }
    value
}

impl ClaudeCode {
    fn path(rel: &str) -> Result<RelPath, AdapterError> {
        Ok(RelPath::parse(rel)?)
    }
}

/// Claude Code takes each `env` variable whole from the settings file that
/// wins, so the local file's `ANTHROPIC_CUSTOM_HEADERS` hides the committed
/// `.claude/settings.json`'s in this person's sessions: said, never copied.
fn hidden_headers(ctx: &RenderContext<'_>) -> Result<Option<String>, AdapterError> {
    let committed = ClaudeCode::path(".claude/settings.json")?;
    let Some(text) = (ctx.read)(Root::Project, &committed) else {
        return Ok(None);
    };
    let key = vec!["env".to_owned(), CUSTOM_HEADERS.to_owned()];
    Ok(match merge::get(Format::Json, &text, &key) {
        Ok(Some(Value::String(s))) if !s.trim().is_empty() => Some(format!(
            "claude-code: .claude/settings.local.json sets {CUSTOM_HEADERS} to add {}, so in \
             your sessions it replaces the one in .claude/settings.json; add that file's lines \
             to .claude/settings.local.json to keep sending them",
            sekisho_core::TOOL_HEADER
        )),
        _ => None,
    })
}

impl Adapter for ClaudeCode {
    fn tool(&self) -> Tool {
        Tool::ClaudeCode
    }

    fn detect(&self, scope: &Scope) -> bool {
        let folder = match scope.kind() {
            ScopeKind::Project => scope.root(Root::Project).map(|p| p.join(".claude")),
            ScopeKind::Global => scope.root(Root::ToolHome(Tool::ClaudeCode)),
        };
        folder.is_some_and(|f| f.is_dir()) || on_path("claude")
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        let project = ctx.scope.kind() == ScopeKind::Project;
        let (root, base) = if project {
            (Root::Project, ".claude/")
        } else {
            (Root::ToolHome(Tool::ClaudeCode), "")
        };
        let settings = if project {
            Self::path(".claude/settings.local.json")?
        } else {
            Self::path("settings.json")?
        };
        let (mcp_root, mcp_file) = if project {
            (Root::Project, Self::path(".mcp.json")?)
        } else {
            ctx.scope.homes().claude_json()
        };
        let env = d.environment.as_str();
        let market = format!("sekisho-{env}");
        let key = |key: Vec<String>, value: Value, origin: Origin| KeyWrite {
            root,
            file: settings.clone(),
            format: Format::Json,
            key,
            value,
            origin,
        };
        let mut out = Rendered::default();

        if ctx.models {
            match &d.defaults.anthropic {
                None => out.skipped.push(Skipped {
                    what: "models".into(),
                    why: "the environment has no model the Anthropic API reaches".into(),
                }),
                Some(default) => {
                    let default = default.to_string();
                    let small = d
                        .defaults
                        .small
                        .as_ref()
                        .filter(|s| {
                            d.models
                                .iter()
                                .any(|m| m.name == **s && m.apis.contains(&ClientApi::Messages))
                        })
                        .map_or_else(|| default.clone(), ToString::to_string);
                    let helper = format!("{} token", shell_word(ctx.sekisho));
                    out.keys.push(key(
                        vec!["apiKeyHelper".into()],
                        json!(helper),
                        Origin::Models,
                    ));
                    out.keys
                        .push(key(vec!["model".into()], json!(default), Origin::Models));
                    for (var, value) in [
                        ("ANTHROPIC_BASE_URL", format!("{}/anthropic", d.gateway_url)),
                        ("ANTHROPIC_DEFAULT_OPUS_MODEL", default.clone()),
                        ("ANTHROPIC_DEFAULT_SONNET_MODEL", default.clone()),
                        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", small),
                        ("CLAUDE_CODE_SUBAGENT_MODEL", default.clone()),
                        (
                            "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
                            HELPER_TTL_MS.to_owned(),
                        ),
                    ] {
                        out.keys.push(key(
                            vec!["env".into(), var.into()],
                            json!(value),
                            Origin::Models,
                        ));
                    }
                    // D227: the tool names itself to the gateway, in a line
                    // of its own beside the person's.
                    out.keys.push(key(
                        line_key(&["env", CUSTOM_HEADERS], sekisho_core::TOOL_HEADER),
                        json!(format!(
                            "{}: {}",
                            sekisho_core::TOOL_HEADER,
                            Tool::ClaudeCode.as_str()
                        )),
                        Origin::Models,
                    ));
                    if project {
                        out.warnings.extend(hidden_headers(ctx)?);
                    }
                }
            }
        }

        let blob = |sha: &str| {
            ctx.blobs
                .get(sha)
                .cloned()
                .ok_or_else(|| AdapterError(format!("no file {sha}")))
        };
        let mut plugins: Vec<Value> = Vec::new();
        for e in &d.entries {
            let origin = Origin::of(e);
            let address = EntryAddress {
                kind: e.kind,
                source: e.source.clone(),
                name: e.name.clone(),
            };
            match &e.spec {
                EntrySpec::Skill(_) => {
                    for f in &e.files {
                        out.files.push(FileWrite {
                            root,
                            path: Self::path(&format!("{base}skills/{}/{}", e.name, f.path))?,
                            content: blob(&f.sha256)?,
                            executable: f.executable,
                            origin: origin.clone(),
                        });
                    }
                }
                // Rules files, below.
                EntrySpec::Instruction(_) => {}
                EntrySpec::Prompt(_) => {
                    for f in &e.files {
                        out.files.push(FileWrite {
                            root,
                            path: Self::path(&format!("{base}commands/{}", f.path))?,
                            content: blob(&f.sha256)?,
                            executable: false,
                            origin: origin.clone(),
                        });
                    }
                }
                EntrySpec::Plugin(spec) => {
                    let name = ctx
                        .plugin_names
                        .get(&address)
                        .cloned()
                        .unwrap_or_else(|| e.name.to_string());
                    // A plugin's AGENTS.md is never written (D223).
                    for f in e.files.iter().filter(|f| !is_agents_md(&f.path)) {
                        out.files.push(FileWrite {
                            root: Root::SekishoConfig,
                            path: Self::path(&format!(
                                "claude-plugins/{env}/plugins/{name}/{}",
                                f.path
                            ))?,
                            content: blob(&f.sha256)?,
                            executable: f.executable,
                            origin: origin.clone(),
                        });
                    }
                    plugins.push(json!({
                        "name": name,
                        "source": format!("./plugins/{name}"),
                        "description": spec.description,
                    }));
                    out.keys.push(key(
                        vec!["enabledPlugins".into(), format!("{name}@{market}")],
                        json!(true),
                        origin,
                    ));
                }
                EntrySpec::McpServer(_) => {
                    // Remote servers come through the environment's MCP
                    // endpoint (plan 5).
                    let Some(server) = ctx.local_servers.iter().find(|s| s.entry == address) else {
                        continue;
                    };
                    out.keys.push(KeyWrite {
                        root: mcp_root,
                        file: mcp_file.clone(),
                        format: Format::Json,
                        key: vec!["mcpServers".into(), server.name.clone()],
                        value: server_value(server),
                        origin: origin.clone(),
                    });
                    if project {
                        let name = server.name.as_str();
                        out.keys.push(key(
                            item_key(&["enabledMcpjsonServers"], name),
                            json!(name),
                            origin,
                        ));
                    }
                }
                EntrySpec::A2aAgent(_) | EntrySpec::Toolkit(_) => {}
            }
        }

        if !plugins.is_empty() {
            let listing = json!({
                "name": market,
                "owner": {"name": "Sekisho"},
                "description": format!("The plugins of Sekisho environment {env}"),
                "plugins": plugins,
            });
            let mut bytes = serde_json::to_vec_pretty(&listing).expect("JSON values serialize");
            bytes.push(b'\n');
            out.files.push(FileWrite {
                root: Root::SekishoConfig,
                path: Self::path(&format!(
                    "claude-plugins/{env}/.claude-plugin/marketplace.json"
                ))?,
                content: bytes,
                executable: false,
                origin: Origin::Environment,
            });
            let mut dir = ctx
                .scope
                .root(Root::SekishoConfig)
                .ok_or_else(|| AdapterError("no folder for Sekisho's files".into()))?;
            dir.push("claude-plugins");
            dir.push(env);
            out.keys.push(key(
                vec!["extraKnownMarketplaces".into(), market.clone()],
                json!({"source": {"source": "directory", "path": dir.to_string_lossy()}}),
                Origin::Environment,
            ));
        }

        // Instructions as rules, loaded at launch with `.claude/CLAUDE.md`'s
        // priority without counting as a CLAUDE.md, so a repository's
        // AGENTS.md still loads; a plugin's path-scoped ones with `paths:`
        // (D222, D224). Plugins carry no instructions of their own there.
        let (instructions, skipped) = instructions::collect(d, ctx)?;
        out.skipped.extend(skipped);
        for i in &instructions {
            if i.paths.is_none() {
                out.always_loaded += i.text.len() as u64;
            }
            out.files.push(FileWrite {
                root,
                path: Self::path(&format!("{base}rules/sekisho-{env}-{}.md", i.stem))?,
                content: i.with_paths().into_bytes(),
                executable: false,
                origin: i.origin.clone(),
            });
        }

        // The environment's MCP endpoint, once plan 5 serves it (D216).
        if let Some(endpoint) = &d.mcp {
            let name = format!("sekisho-{env}");
            let mut value = json!({"type": "http", "url": endpoint.url});
            if let Some(client) = &endpoint.client_id {
                value["oauth"] = json!({"clientId": client});
            }
            out.keys.push(KeyWrite {
                root: mcp_root,
                file: mcp_file.clone(),
                format: Format::Json,
                key: vec!["mcpServers".into(), name.clone()],
                value,
                origin: Origin::Environment,
            });
            if project {
                out.keys.push(key(
                    item_key(&["enabledMcpjsonServers"], &name),
                    json!(name),
                    Origin::Environment,
                ));
            }
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

    /// Local settings override user settings key by key and an install
    /// writes every model variable, so a global install's models never fall
    /// through; MCP servers resolve local, then project, then user; skills
    /// and commands personal over project; plugins sit in each
    /// environment's own marketplace, so their names never meet.
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
            skills: both(Some(ScopeKind::Global)),
            prompts: both(Some(ScopeKind::Global)),
            plugins: both(None),
            instructions: both(None),
        }
    }
}
