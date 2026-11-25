//! What Gemini CLI and Qwen Code (a fork of it) take in the same layout,
//! from their sources (Gemini CLI `acfb42d`, Qwen Code `1de5c0b3`): skills
//! in `skills/<name>/`, prompts as TOML commands in `commands/`, MCP servers
//! in `settings.json`'s `mcpServers`, and plugins taken apart into those,
//! since neither reads Claude Code plugins nor has a project folder for its
//! own extensions (D205).

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use sekisho_catalog::delivery::{DeliveredEntry, Delivery, EntryAddress};
use sekisho_catalog::launch::{Launch, parse_stdio};
use sekisho_catalog::model::EntrySpec;

use super::{
    AdapterError, FileWrite, Format, KeyWrite, Origin, RelPath, RenderContext, Rendered, Root,
    Skipped, Tool,
};
use crate::install::instructions::is_agents_md;
use crate::install::launch::{self, EnvValue};

/// How a remote MCP server is written.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RemoteKey {
    /// Gemini CLI: `url` with `type` `http` or `sse` (`httpUrl` is
    /// deprecated there).
    UrlAndType,
    /// Qwen Code: `httpUrl` for streamable HTTP, `url` for SSE.
    HttpUrl,
}

/// Where one tool keeps things, in one scope.
pub(crate) struct Layout {
    pub tool: Tool,
    pub root: Root,
    /// `.gemini/` in a project, nothing in the tool's own folder.
    pub base: &'static str,
    pub remote: RemoteKey,
    /// A person's variable in a server's `env`: `None` leaves it to the
    /// environment the tool hands its servers. Given the server's variable,
    /// the person's, and whether the server needs it.
    pub user_var: fn(&str, &str, bool) -> Option<String>,
}

impl Layout {
    pub fn path(&self, rest: &str) -> Result<RelPath, AdapterError> {
        Ok(RelPath::parse(&format!("{}{rest}", self.base))?)
    }

    pub fn settings(&self) -> Result<RelPath, AdapterError> {
        self.path("settings.json")
    }

    /// A key in `settings.json`, which both tools read with comments.
    pub fn key(
        &self,
        key: &[&str],
        value: Value,
        origin: Origin,
    ) -> Result<KeyWrite, AdapterError> {
        Ok(KeyWrite {
            root: self.root,
            file: self.settings()?,
            format: Format::Jsonc,
            key: key.iter().map(|k| (*k).to_owned()).collect(),
            value,
            origin,
        })
    }
}

/// Why a prompt can't be a command here: both tools run `!{…}` as a shell
/// command and read `@{…}` as a file to inject, which a prompt written for
/// another tool never meant.
fn unsafe_for_commands(body: &str) -> Option<&'static str> {
    if body.contains("!{") {
        Some("its text has !{…}, which this tool would run as a shell command")
    } else if body.contains("@{") {
        Some("its text has @{…}, which this tool would read as a file to inject")
    } else {
        None
    }
}

/// A Markdown prompt as a TOML command: its description, and its body with
/// `$ARGUMENTS` as `{{args}}`. A body without `{{args}}` gets what follows
/// the command added by the tool itself.
fn toml_command(description: &str, body: &str) -> String {
    let mut doc = toml_edit::DocumentMut::new();
    if !description.is_empty() {
        doc["description"] = toml_edit::value(description);
    }
    doc["prompt"] = toml_edit::value(body.replace("$ARGUMENTS", "{{args}}"));
    doc.to_string()
}

/// `text`'s command file, or why it is skipped.
fn command(text: &[u8], description: Option<&str>) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(text).map_err(|_| "it isn't UTF-8 text".to_owned())?;
    let (found, body) = sekisho_catalog::description_and_body(text)?;
    if let Some(why) = unsafe_for_commands(&body) {
        return Err(why.to_owned());
    }
    Ok(toml_command(description.unwrap_or(&found), &body).into_bytes())
}

fn blob(ctx: &RenderContext<'_>, sha: &str) -> Result<Vec<u8>, AdapterError> {
    ctx.blobs
        .get(sha)
        .cloned()
        .ok_or_else(|| AdapterError(format!("no file {sha}")))
}

fn address(e: &DeliveredEntry) -> EntryAddress {
    EntryAddress {
        kind: e.kind,
        source: e.source.clone(),
        name: e.name.clone(),
    }
}

/// A remote server's value.
fn remote(layout: &Layout, url: &str, sse: bool, headers: Option<&Value>) -> Value {
    let mut value = match (layout.remote, sse) {
        (RemoteKey::UrlAndType, _) => {
            json!({"url": url, "type": if sse { "sse" } else { "http" }})
        }
        (RemoteKey::HttpUrl, false) => json!({"httpUrl": url}),
        (RemoteKey::HttpUrl, true) => json!({"url": url}),
    };
    if let Some(h) = headers.filter(|h| h.as_object().is_some_and(|m| !m.is_empty())) {
        value["headers"] = h.clone();
    }
    value
}

/// Skills, prompts, MCP servers and plugins, into `out`.
pub(crate) fn content(
    d: &Delivery,
    ctx: &RenderContext<'_>,
    layout: &Layout,
    out: &mut Rendered,
) -> Result<(), AdapterError> {
    let mut no_cutoff: Vec<&'static str> = Vec::new();
    for e in &d.entries {
        let origin = Origin::of(e);
        match &e.spec {
            EntrySpec::Skill(_) => {
                for f in &e.files {
                    out.files.push(FileWrite {
                        root: layout.root,
                        path: layout.path(&format!("skills/{}/{}", e.name, f.path))?,
                        content: blob(ctx, &f.sha256)?,
                        executable: f.executable,
                        origin: origin.clone(),
                    });
                }
            }
            // Each tool's own way, in its adapter.
            EntrySpec::Instruction(_) => {}
            EntrySpec::Prompt(spec) => {
                for f in &e.files {
                    let stem = f.path.strip_suffix(".md").unwrap_or(&f.path);
                    match command(&blob(ctx, &f.sha256)?, Some(&spec.description)) {
                        Ok(content) => out.files.push(FileWrite {
                            root: layout.root,
                            path: layout.path(&format!("commands/{stem}.toml"))?,
                            content,
                            executable: false,
                            origin: origin.clone(),
                        }),
                        Err(why) => out.skipped.push(Skipped {
                            what: format!("prompt {}", e.name),
                            why,
                        }),
                    }
                }
            }
            EntrySpec::McpServer(_) => {
                // Remote servers come through the environment's MCP
                // endpoint (plan 5).
                let Some(server) = ctx.local_servers.iter().find(|s| s.entry == address(e)) else {
                    continue;
                };
                let env: Map<String, Value> = server
                    .env
                    .iter()
                    .filter_map(|(k, v)| {
                        let v = match v {
                            EnvValue::Literal(x) => x.clone(),
                            EnvValue::FromUser { name, required } => {
                                (layout.user_var)(k, name, *required)?
                            }
                        };
                        Some((k.clone(), Value::String(v)))
                    })
                    .collect();
                let mut value = json!({"command": server.command, "args": server.args});
                if !env.is_empty() {
                    value["env"] = Value::Object(env);
                }
                out.keys
                    .push(layout.key(&["mcpServers", &server.name], value, origin)?);
            }
            EntrySpec::Plugin(_) => {
                plugin(d, e, ctx, layout, out, &mut no_cutoff)?;
            }
            EntrySpec::A2aAgent(_) | EntrySpec::Toolkit(_) => {}
        }
    }
    if let Some(w) = launch::no_cutoff_warning(&no_cutoff) {
        out.warnings.push(w);
    }

    // The environment's MCP endpoint, once plan 5 serves it (D216).
    if let Some(endpoint) = &d.mcp {
        let mut value = remote(layout, &endpoint.url, false, None);
        if let Some(client) = &endpoint.client_id {
            value["oauth"] = json!({"enabled": true, "clientId": client});
        }
        out.keys.push(layout.key(
            &["mcpServers", &format!("sekisho-{}", d.environment)],
            value,
            Origin::Environment,
        )?);
    }
    Ok(())
}

/// A plugin's server: its name and its definition.
type ServerDef = (String, Map<String, Value>);

/// `${CLAUDE_PLUGIN_ROOT}` in a plugin's server config.
const PLUGIN_ROOT: &str = "${CLAUDE_PLUGIN_ROOT}";

/// The servers a plugin defines, by name: its `.mcp.json`, then its
/// manifest's `mcpServers` (inline, or a path to a JSON file), as the
/// catalog read them at sync.
fn plugin_servers(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<ServerDef>, String> {
    let read = |path: &str| -> Result<Option<Value>, String> {
        files
            .get(path)
            .map(|b| serde_json::from_slice(b).map_err(|e| format!("{path} isn't valid JSON: {e}")))
            .transpose()
    };
    let mut docs: Vec<Value> = Vec::new();
    if let Some(doc) = read(".mcp.json")? {
        docs.push(doc);
    }
    if let Some(manifest) = read(".claude-plugin/plugin.json")? {
        let items = match manifest.get("mcpServers") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            Some(item) => vec![item.clone()],
        };
        for item in items {
            match item {
                Value::String(path) => {
                    let path = path
                        .strip_prefix(&format!("{PLUGIN_ROOT}/"))
                        .unwrap_or(&path)
                        .trim_start_matches("./")
                        .to_owned();
                    docs.push(read(&path)?.ok_or_else(|| format!("no file {path}"))?);
                }
                Value::Object(_) => docs.push(item),
                _ => return Err("mcpServers must be an object or a path".into()),
            }
        }
    }
    let mut out = Vec::new();
    for doc in docs {
        let servers = match doc.get("mcpServers") {
            Some(Value::Object(m)) => m.clone(),
            _ => doc.as_object().cloned().unwrap_or_default(),
        };
        for (name, def) in servers {
            if let Value::Object(def) = def {
                out.push((name, def));
            }
        }
    }
    Ok(out)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A plugin taken apart: its skills, its commands under its own name, its
/// MCP servers as `<plugin>-<server>`, and, when a server runs a file of
/// the plugin's, the whole plugin in Sekisho's folder for it to run from.
/// Hooks and agents have no place here (D205).
fn plugin(
    d: &Delivery,
    e: &DeliveredEntry,
    ctx: &RenderContext<'_>,
    layout: &Layout,
    out: &mut Rendered,
    no_cutoff: &mut Vec<&'static str>,
) -> Result<(), AdapterError> {
    let origin = Origin::of(e);
    let name = ctx
        .plugin_names
        .get(&address(e))
        .cloned()
        .unwrap_or_else(|| e.name.to_string());
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for f in &e.files {
        files.insert(f.path.clone(), blob(ctx, &f.sha256)?);
    }
    let skip = |out: &mut Rendered, what: &str, why: String| {
        out.skipped.push(Skipped {
            what: format!("plugin {name}'s {what}"),
            why,
        })
    };

    for f in &e.files {
        let bytes = &files[&f.path];
        if let Some(rest) = f.path.strip_prefix("skills/") {
            out.files.push(FileWrite {
                root: layout.root,
                path: layout.path(&format!("skills/{rest}"))?,
                content: bytes.clone(),
                executable: f.executable,
                origin: origin.clone(),
            });
        } else if let Some(rest) = f.path.strip_prefix("commands/")
            && let Some(stem) = rest.strip_suffix(".md")
        {
            match command(bytes, None) {
                Ok(content) => out.files.push(FileWrite {
                    root: layout.root,
                    path: layout.path(&format!("commands/{name}/{stem}.toml"))?,
                    content,
                    executable: false,
                    origin: origin.clone(),
                }),
                Err(why) => skip(out, &format!("command {stem}"), why),
            }
        }
    }
    let has = |dir: &str| e.files.iter().any(|f| f.path.starts_with(dir));
    let manifest: Option<Value> = files
        .get(".claude-plugin/plugin.json")
        .and_then(|b| serde_json::from_slice(b).ok());
    let declares = |key: &str| manifest.as_ref().is_some_and(|m| m.get(key).is_some());
    if has("hooks/") || declares("hooks") {
        skip(
            out,
            "hooks",
            format!("{} runs no Claude Code hooks", layout.tool.title()),
        );
    }
    if has("agents/") || declares("agents") {
        skip(
            out,
            "agents",
            format!("{} doesn't read Claude Code agents", layout.tool.title()),
        );
    }

    let servers =
        plugin_servers(&files).map_err(|e| AdapterError(format!("plugin {name}: {e}")))?;
    let plugin_dir = ctx
        .scope
        .root(Root::SekishoConfig)
        .ok_or_else(|| AdapterError("no folder for Sekisho's files".into()))?
        .join("plugins")
        .join(d.environment.as_str())
        .join(&name);
    let plugin_dir = plugin_dir.to_string_lossy().into_owned();
    let mut needs_files = false;
    for (server, def) in servers {
        let key = format!("{name}-{server}");
        let fill = |s: &str| s.replace(PLUGIN_ROOT, &plugin_dir);
        if let Some(url) = def.get("url").and_then(Value::as_str) {
            let sse = def.get("type").and_then(Value::as_str) == Some("sse");
            let value = remote(layout, url, sse, def.get("headers"));
            out.keys
                .push(layout.key(&["mcpServers", &key], value, origin.clone())?);
            continue;
        }
        let Some(command) = def.get("command").and_then(Value::as_str) else {
            continue;
        };
        let args = strings(def.get("args"));
        let mut env: Map<String, Value> = def
            .get("env")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let (command, args) = match parse_stdio(command, &args) {
            Ok(Launch::Other { .. }) => {
                needs_files = true;
                (fill(command), args.iter().map(|a| fill(a)).collect())
            }
            Ok(Launch::Docker { .. }) => {
                skip(
                    out,
                    &format!("MCP server {server}"),
                    "a container is started only from the catalog, where its image is held to a digest".into(),
                );
                continue;
            }
            Ok(Launch::Remote { .. }) => continue,
            Ok(_) => {
                let pinned =
                    launch::pinned(command, &args, e.applied_at, &d.registries).map_err(|why| {
                        AdapterError(format!("plugin {name}'s server {server}: {why}"))
                    })?;
                if let Some(runner) = pinned.no_cutoff {
                    no_cutoff.push(runner);
                }
                for (k, v) in pinned.env {
                    env.insert(k, Value::String(v));
                }
                (pinned.command, pinned.args)
            }
            Err(why) => {
                skip(out, &format!("MCP server {server}"), why);
                continue;
            }
        };
        for v in env.values_mut() {
            if let Value::String(s) = v {
                *s = fill(s);
            }
        }
        let mut value = json!({"command": command, "args": args});
        if !env.is_empty() {
            value["env"] = Value::Object(env);
        }
        out.keys
            .push(layout.key(&["mcpServers", &key], value, origin.clone())?);
    }
    if needs_files {
        // A plugin's AGENTS.md is never written (D223).
        for f in e.files.iter().filter(|f| !is_agents_md(&f.path)) {
            out.files.push(FileWrite {
                root: Root::SekishoConfig,
                path: RelPath::parse(&format!("plugins/{}/{name}/{}", d.environment, f.path))?,
                content: files[&f.path].clone(),
                executable: f.executable,
                origin: origin.clone(),
            });
        }
    }
    Ok(())
}
