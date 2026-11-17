//! A `plugins` source: a Claude Code marketplace
//! (`.claude-plugin/marketplace.json`) and the plugins it lists from its
//! own repository (D170), or plugins found by their manifest. APM's
//! bundles are plugins whose files are checked against their lock file
//! (D169).

use std::fs;
use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value};

use crate::apm::verify_bundle;
use crate::check::{CheckError, CheckOptions};
use crate::launch::{Launch, parse_stdio};
use crate::model::{CheckedEntry, EntrySpec, PluginMcpServer, PluginSpec, Refusal, SourceCheck};
use crate::names::{EntryKind, EntryName};
use crate::source::endpoint_url;
use crate::tree::{Limits, TreeFile, read_entry_files};

const MARKETPLACE: &str = ".claude-plugin/marketplace.json";
/// Where a plugin's manifest may be: Claude Code's place, then the root,
/// where `apm pack` writes it.
const MANIFESTS: [&str; 2] = [".claude-plugin/plugin.json", "plugin.json"];
const MCP_JSON: &str = ".mcp.json";
const PLUGIN_ROOT_VAR: &str = "${CLAUDE_PLUGIN_ROOT}/";

/// Finds the plugins under `root`.
pub(crate) fn check(
    root: &Path,
    options: &CheckOptions,
    limits: &Limits,
    out: &mut SourceCheck,
) -> Result<(), CheckError> {
    let marketplace = root.join(MARKETPLACE);
    match fs::symlink_metadata(&marketplace) {
        Ok(meta) if meta.is_file() => {
            let doc = read_json(&marketplace, limits.manifest_bytes).map_err(|reason| {
                CheckError::Unreadable {
                    reason: format!("{MARKETPLACE} {reason}"),
                }
            })?;
            from_marketplace(root, &doc, options, limits, out)
        }
        Ok(_) => Err(CheckError::Unreadable {
            reason: format!("{MARKETPLACE} is a symlink or not a file"),
        }),
        Err(_) => {
            if has_manifest(root) {
                push(
                    out,
                    ".",
                    None,
                    read_plugin(root, root, None, options, limits),
                );
            } else {
                for (name, dir) in child_folders(root)? {
                    if has_manifest(&dir) {
                        push(
                            out,
                            &name,
                            None,
                            read_plugin(root, &dir, None, options, limits),
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

fn push(
    out: &mut SourceCheck,
    rel: &str,
    listed_as: Option<&str>,
    read: Result<CheckedEntry, (Option<String>, String)>,
) {
    match read {
        Ok(entry) => out.entries.push(entry),
        Err((name, reason)) => out.refused.push(Refusal {
            path: rel.to_owned(),
            kind: Some(EntryKind::Plugin),
            name: listed_as.map(str::to_owned).or(name),
            reason,
        }),
    }
}

fn has_manifest(dir: &Path) -> bool {
    MANIFESTS
        .iter()
        .any(|m| fs::symlink_metadata(dir.join(m)).is_ok_and(|meta| meta.is_file()))
}

fn child_folders(root: &Path) -> Result<Vec<(String, std::path::PathBuf)>, CheckError> {
    let unreadable = |e: std::io::Error| CheckError::Unreadable {
        reason: format!(".: {}", e.kind()),
    };
    let mut items = fs::read_dir(root)
        .map_err(unreadable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(unreadable)?;
    items.sort_by_key(|i| i.file_name());
    let mut out = Vec::new();
    for item in items {
        let name = item.file_name().to_string_lossy().into_owned();
        if item.file_type().map_err(unreadable)?.is_dir() && !name.starts_with('.') {
            out.push((name, item.path()));
        }
    }
    Ok(out)
}

fn read_json(path: &Path, limit: u64) -> Result<Value, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("can't be read: {}", e.kind()))?;
    if bytes.len() as u64 > limit {
        return Err(format!("is larger than {limit} bytes"));
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("isn't valid JSON: {e}"))
}

/// A path inside the repository or plugin, normalised: `/`-separated,
/// `.` and empty segments dropped; `None` for an absolute path, a
/// backslash or a `..` segment.
fn inside(path: &str) -> Option<Vec<&str>> {
    if path.starts_with('/') || path.contains('\\') {
        return None;
    }
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    (!parts.contains(&"..")).then_some(parts)
}

/// Why a marketplace entry from another repository is refused (D170).
fn elsewhere(source: &Map<String, Value>) -> String {
    let text = |k: &str| source.get(k).and_then(Value::as_str);
    let from = text("repo")
        .or_else(|| text("url"))
        .or_else(|| text("package"))
        .unwrap_or("another place");
    let kind = text("source").unwrap_or("an object");
    format!("it comes from {from} ({kind}), outside this repository; add it as a source of its own")
}

fn from_marketplace(
    root: &Path,
    doc: &Value,
    options: &CheckOptions,
    limits: &Limits,
    out: &mut SourceCheck,
) -> Result<(), CheckError> {
    let bad = |reason: &str| CheckError::Unreadable {
        reason: format!("{MARKETPLACE}: {reason}"),
    };
    let plugin_root = match doc.pointer("/metadata/pluginRoot") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(r)) => inside(r)
            .ok_or_else(|| bad("metadata.pluginRoot must be a path inside the repository"))?,
        Some(_) => return Err(bad("metadata.pluginRoot must be text")),
    };
    let plugins = doc
        .get("plugins")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("plugins must be a list"))?;
    for (i, item) in plugins.iter().enumerate() {
        let Some(item) = item.as_object() else {
            out.refused.push(Refusal {
                path: MARKETPLACE.to_owned(),
                kind: Some(EntryKind::Plugin),
                name: None,
                reason: format!("plugins[{i}] isn't an object"),
            });
            continue;
        };
        let listed = item.get("name").and_then(Value::as_str).unwrap_or("");
        let refuse = |out: &mut SourceCheck, reason: String| {
            out.refused.push(Refusal {
                path: MARKETPLACE.to_owned(),
                kind: Some(EntryKind::Plugin),
                name: Some(listed.to_owned()).filter(|n| !n.is_empty()),
                reason,
            })
        };
        let source = match item.get("source") {
            Some(Value::String(s)) => s,
            Some(Value::Object(other)) => {
                refuse(out, elsewhere(other));
                continue;
            }
            _ => {
                refuse(
                    out,
                    "its source must be a path in this repository".to_owned(),
                );
                continue;
            }
        };
        let Some(parts) = inside(source) else {
            refuse(
                out,
                format!("its source {source:?} must be a path inside the repository"),
            );
            continue;
        };
        let rel_parts: Vec<&str> = plugin_root.iter().copied().chain(parts).collect();
        let rel = if rel_parts.is_empty() {
            ".".to_owned()
        } else {
            rel_parts.join("/")
        };
        let dir = rel_parts.iter().fold(root.to_path_buf(), |p, s| p.join(s));
        if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            refuse(
                out,
                format!("its source {source} isn't a folder in the repository"),
            );
            continue;
        }
        push(
            out,
            &rel,
            Some(listed).filter(|n| !n.is_empty()),
            read_plugin(root, &dir, Some(item), options, limits),
        );
    }
    Ok(())
}

/// Reads a plugin; on refusal, the name it goes by and why.
fn read_plugin(
    root: &Path,
    dir: &Path,
    listed: Option<&Map<String, Value>>,
    options: &CheckOptions,
    limits: &Limits,
) -> Result<CheckedEntry, (Option<String>, String)> {
    let listed_name = listed
        .and_then(|l| l.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let fail = |reason: String| (listed_name.clone(), reason);
    let files = read_entry_files(root, dir, limits).map_err(|e| fail(e.to_string()))?;
    let rel = dir
        .strip_prefix(root)
        .ok()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| ".".to_owned());

    // The plugin's own manifest, else the marketplace entry stands for it.
    let manifest = match MANIFESTS
        .iter()
        .find_map(|m| files.iter().find(|f| f.path == *m))
    {
        Some(file) => {
            if file.bytes.len() as u64 > limits.manifest_bytes {
                return Err(fail(format!(
                    "{} is larger than {} bytes",
                    file.path, limits.manifest_bytes
                )));
            }
            match serde_json::from_slice::<Value>(&file.bytes) {
                Ok(Value::Object(m)) => m,
                Ok(_) => return Err(fail(format!("{} isn't a JSON object", file.path))),
                Err(e) => return Err(fail(format!("{} isn't valid JSON: {e}", file.path))),
            }
        }
        None => match listed {
            Some(entry) => entry.clone(),
            None => return Err(fail("the plugin has no plugin.json".to_owned())),
        },
    };
    let text =
        |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).map(str::to_owned);

    let raw_name =
        text(&manifest, "name").ok_or_else(|| fail("the manifest needs a name".to_owned()))?;
    let name = EntryName::parse(&raw_name).map_err(|e| {
        (
            listed_name.clone().or(Some(raw_name.clone())),
            e.to_string(),
        )
    })?;
    if let Some(listed) = &listed_name
        && listed != name.as_str()
    {
        return Err(fail(format!(
            "its manifest names it {name}, but the marketplace lists it as {listed}"
        )));
    }
    let named = |reason: String| (Some(name.to_string()), reason);
    let description = text(&manifest, "description")
        .or_else(|| listed.and_then(|l| text(l, "description")))
        .unwrap_or_default();
    let version = text(&manifest, "version").or_else(|| listed.and_then(|l| text(l, "version")));

    let mut mcp_servers = Vec::new();
    if let Some(file) = files.iter().find(|f| f.path == MCP_JSON) {
        let doc: Value = serde_json::from_slice(&file.bytes)
            .map_err(|e| named(format!("{MCP_JSON} isn't valid JSON: {e}")))?;
        servers_from(&doc, MCP_JSON, options, &mut mcp_servers).map_err(named)?;
    }
    match manifest.get("mcpServers") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                manifest_servers(item, &files, options, &mut mcp_servers).map_err(named)?;
            }
        }
        Some(item) => manifest_servers(item, &files, options, &mut mcp_servers).map_err(named)?,
    }

    let apm_checked = verify_bundle(&files).map_err(|problems| {
        named(format!(
            "the APM bundle check failed: {}",
            problems.join("; ")
        ))
    })?;
    Ok(CheckedEntry {
        kind: EntryKind::Plugin,
        name,
        path: rel,
        spec: EntrySpec::Plugin(PluginSpec {
            description,
            version,
            mcp_servers,
            apm_checked,
        }),
        files,
    })
}

/// The manifest's `mcpServers`: servers inline, or a path to a JSON file
/// inside the plugin.
fn manifest_servers(
    item: &Value,
    files: &[TreeFile],
    options: &CheckOptions,
    out: &mut Vec<PluginMcpServer>,
) -> Result<(), String> {
    match item {
        Value::String(path) => {
            let trimmed = path.strip_prefix(PLUGIN_ROOT_VAR).unwrap_or(path);
            let parts = inside(trimmed)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| format!("mcpServers {path:?} must be a file inside the plugin"))?;
            let wanted = parts.join("/");
            let file = files
                .iter()
                .find(|f| f.path == wanted)
                .ok_or_else(|| format!("mcpServers names {path}, which isn't in the plugin"))?;
            let doc: Value = serde_json::from_slice(&file.bytes)
                .map_err(|e| format!("{wanted} isn't valid JSON: {e}"))?;
            servers_from(&doc, &wanted, options, out)
        }
        Value::Object(_) => servers_from(item, "plugin.json's mcpServers", options, out),
        _ => Err("mcpServers must be an object or a path".to_owned()),
    }
}

/// Servers from an MCP config: `{"mcpServers": {...}}` or the servers alone.
fn servers_from(
    doc: &Value,
    from: &str,
    options: &CheckOptions,
    out: &mut Vec<PluginMcpServer>,
) -> Result<(), String> {
    let servers = match doc.get("mcpServers") {
        Some(Value::Object(m)) => m,
        Some(_) => return Err(format!("{from}: mcpServers must be an object")),
        None => doc
            .as_object()
            .ok_or_else(|| format!("{from} must be a JSON object"))?,
    };
    for (name, def) in servers {
        if out.iter().any(|s| &s.name == name) {
            return Err(format!("MCP server {name} is defined twice"));
        }
        let def = def
            .as_object()
            .ok_or_else(|| format!("MCP server {name} must be an object"))?;
        let launch = if let Some(command) = def.get("command").and_then(Value::as_str) {
            let args = match def.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| format!("MCP server {name}: args must be text"))?,
                Some(_) => return Err(format!("MCP server {name}: args must be a list")),
            };
            parse_stdio(command, &args).map_err(|e| format!("MCP server {name}: {e}"))?
        } else if let Some(url) = def.get("url").and_then(Value::as_str) {
            Launch::Remote {
                url: endpoint_url(url, "MCP server URL", options.allow_plain_http)
                    .map_err(|e| format!("MCP server {name}: {e}"))?,
            }
        } else {
            return Err(format!("MCP server {name} needs a command or a url"));
        };
        out.push(PluginMcpServer {
            name: name.clone(),
            launch,
        });
    }
    Ok(())
}
