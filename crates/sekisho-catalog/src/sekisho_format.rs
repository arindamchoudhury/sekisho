//! Sekisho's own catalog format, `apiVersion: sekisho/v1alpha1`: MCP
//! servers (`kind: MCPServer`), A2A agents (`kind: A2AAgent`) and toolkits
//! (`kind: Toolkit`, D184). Unlike Obot's, unknown fields are refused.

use std::collections::HashSet;

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_yaml_ng::{Mapping, Value};

use crate::check::{CheckOptions, Item, ItemRead};
use crate::model::{
    A2aAgentSpec, EntrySpec, McpRuntime, McpServerSpec, ToolkitServer, ToolkitSpec,
};
use crate::names::{EntryKind, EntryName, EntryRef};
use crate::obot::{RawConfig, config_field};
use crate::pins::{ImageRef, NpmPackage, PythonPackage};
use crate::source::endpoint_url;

pub const API_VERSION: &str = "sekisho/v1alpha1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpServerDoc {
    #[serde(rename = "apiVersion")]
    _api_version: IgnoredAny,
    #[serde(rename = "kind")]
    _kind: IgnoredAny,
    name: String,
    #[serde(default)]
    description: String,
    remote: Option<RemoteDoc>,
    npx: Option<NpxDoc>,
    uvx: Option<UvxDoc>,
    container: Option<ContainerDoc>,
    #[serde(default)]
    config: Vec<ConfigDoc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteDoc {
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NpxDoc {
    package: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UvxDoc {
    package: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerDoc {
    image: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    port: Option<u16>,
    path: Option<String>,
}

/// Obot's configuration fields, so a block can be copied across.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDoc {
    name: String,
    #[serde(default)]
    description: String,
    key: Option<String>,
    value: Option<String>,
    #[serde(default)]
    sensitive: bool,
    #[serde(default)]
    required: bool,
    prefix: Option<String>,
    usage: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct A2aAgentDoc {
    #[serde(rename = "apiVersion")]
    _api_version: IgnoredAny,
    #[serde(rename = "kind")]
    _kind: IgnoredAny,
    name: String,
    #[serde(default)]
    description: String,
    url: String,
    card_url: Option<String>,
}

fn refused(kind: Option<EntryKind>, name: Option<String>, reason: String) -> ItemRead {
    ItemRead {
        item: Item::Refused { kind, name, reason },
        warnings: Vec::new(),
    }
}

/// One item with an `apiVersion`.
pub(crate) fn read_item(map: &Mapping, opts: &CheckOptions) -> ItemRead {
    let text = |key: &str| map.get(key).and_then(Value::as_str).map(str::to_owned);
    let name = text("name");
    let kind = match text("kind").as_deref() {
        Some("MCPServer") => Some(EntryKind::McpServer),
        Some("A2AAgent") => Some(EntryKind::A2aAgent),
        Some("Toolkit") => Some(EntryKind::Toolkit),
        _ => None,
    };
    let api_version = text("apiVersion");
    if api_version.as_deref() != Some(API_VERSION) {
        let shown = api_version.unwrap_or_else(|| "(not text)".to_owned());
        return refused(
            kind,
            name,
            format!("apiVersion {shown} isn't known; use {API_VERSION}"),
        );
    }
    let read = match kind {
        Some(EntryKind::McpServer) => server(map, opts),
        Some(EntryKind::A2aAgent) => agent(map, opts),
        Some(EntryKind::Toolkit) => toolkit(map),
        _ => {
            let shown = text("kind").unwrap_or_else(|| "(none)".to_owned());
            return refused(
                None,
                name,
                format!("kind {shown} isn't known; use MCPServer, A2AAgent or Toolkit"),
            );
        }
    };
    let kind = kind.expect("matched above");
    match read {
        Ok((name, spec)) => ItemRead {
            item: Item::Entry { kind, name, spec },
            warnings: Vec::new(),
        },
        Err(reason) => refused(Some(kind), name, reason),
    }
}

fn decode<T: for<'de> Deserialize<'de>>(map: &Mapping) -> Result<T, String> {
    serde_yaml_ng::from_value(Value::Mapping(map.clone())).map_err(|e| e.to_string())
}

fn server(map: &Mapping, opts: &CheckOptions) -> Result<(EntryName, EntrySpec), String> {
    let doc: McpServerDoc = decode(map)?;
    let name = EntryName::parse(&doc.name).map_err(|e| e.to_string())?;
    let given = [
        doc.remote.is_some(),
        doc.npx.is_some(),
        doc.uvx.is_some(),
        doc.container.is_some(),
    ];
    if given.iter().filter(|g| **g).count() != 1 {
        return Err("give exactly one of remote, npx, uvx or container".to_owned());
    }
    let pin = |e: crate::pins::PinError| e.to_string();
    let runtime = if let Some(r) = doc.remote {
        McpRuntime::Remote {
            url: endpoint_url(&r.url, "remote URL", opts.allow_plain_http)
                .map_err(|e| e.to_string())?,
        }
    } else if let Some(n) = doc.npx {
        McpRuntime::Npx {
            package: NpmPackage::parse_exact(&n.package).map_err(pin)?,
            args: n.args,
        }
    } else if let Some(u) = doc.uvx {
        McpRuntime::Uvx {
            package: PythonPackage::parse_exact(&u.package).map_err(pin)?,
            command: u.command,
            args: u.args,
        }
    } else {
        let c = doc.container.expect("exactly one was given");
        McpRuntime::Container {
            image: ImageRef::parse(&c.image).map_err(pin)?,
            command: c.command,
            args: c.args,
            port: c.port.filter(|p| *p != 0),
            path: c.path,
        }
    };
    let config = doc
        .config
        .into_iter()
        .map(|c| {
            config_field(RawConfig {
                name: c.name,
                description: c.description,
                key: c.key,
                value: c.value,
                sensitive: c.sensitive,
                required: c.required,
                prefix: c.prefix,
                usage: Some(c.usage),
            })
        })
        .collect::<Result<_, _>>()?;
    Ok((
        name,
        EntrySpec::McpServer(McpServerSpec {
            description: doc.description,
            runtime,
            config,
            tool_preview: Vec::new(),
            unsupported_tools: Vec::new(),
            static_oauth: false,
        }),
    ))
}

fn agent(map: &Mapping, opts: &CheckOptions) -> Result<(EntryName, EntrySpec), String> {
    let doc: A2aAgentDoc = decode(map)?;
    let name = EntryName::parse(&doc.name).map_err(|e| e.to_string())?;
    let url =
        endpoint_url(&doc.url, "agent URL", opts.allow_plain_http).map_err(|e| e.to_string())?;
    let card_url = doc
        .card_url
        .map(|u| endpoint_url(&u, "card URL", opts.allow_plain_http))
        .transpose()
        .map_err(|e| e.to_string())?;
    Ok((
        name,
        EntrySpec::A2aAgent(A2aAgentSpec {
            description: doc.description,
            url,
            card_url,
            card: None,
        }),
    ))
}

/// The fields a toolkit may have.
const TOOLKIT_FIELDS: [&str; 9] = [
    "apiVersion",
    "kind",
    "name",
    "description",
    "mcp_servers",
    "a2a_agents",
    "skills",
    "plugins",
    "prompts",
];

/// Problems found in a toolkit, each `field: message`.
#[derive(Default)]
struct Problems(Vec<String>);

impl Problems {
    fn push(&mut self, field: &str, message: &str) {
        self.0.push(format!("{field}: {message}"));
    }

    fn reference(&mut self, field: &str, value: &Value) -> Option<EntryRef> {
        match value.as_str() {
            Some(t) => EntryRef::parse(t)
                .map_err(|e| self.push(field, &e.to_string()))
                .ok(),
            None => {
                self.push(field, "must be an entry's name, or name@source");
                None
            }
        }
    }

    fn list(&mut self, map: &Mapping, key: &str) -> Vec<Value> {
        match map.get(key) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Sequence(items)) => items.clone(),
            Some(_) => {
                self.push(key, "must be a list");
                Vec::new()
            }
        }
    }

    /// A list of references, none written twice.
    fn references(&mut self, map: &Mapping, key: &str) -> Vec<EntryRef> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (i, item) in self.list(map, key).iter().enumerate() {
            let field = format!("{key}[{i}]");
            if let Some(r) = self.reference(&field, item) {
                if seen.insert(r.to_string()) {
                    out.push(r);
                } else {
                    self.push(&field, "is listed twice");
                }
            }
        }
        out
    }

    fn tools(&mut self, field: &str, value: Option<&Value>) -> Option<Vec<String>> {
        let items = match value {
            None | Some(Value::Null) => return None,
            Some(Value::Sequence(items)) => items,
            Some(_) => {
                self.push(&format!("{field}.tools"), "must be a list");
                return None;
            }
        };
        if items.is_empty() {
            self.push(
                &format!("{field}.tools"),
                "give at least one tool, or leave tools out to allow them all",
            );
        }
        let mut tools = Vec::new();
        let mut seen = HashSet::new();
        for (j, t) in items.iter().enumerate() {
            let at = format!("{field}.tools[{j}]");
            match t.as_str() {
                Some(t) if t.trim().is_empty() => self.push(&at, "a tool's name can't be empty"),
                Some(t) if !seen.insert(t.to_owned()) => self.push(&at, "is listed twice"),
                Some(t) => tools.push(t.to_owned()),
                None => self.push(&at, "must be a tool's name"),
            }
        }
        Some(tools)
    }

    /// MCP servers, each a name or `{server, tools}`, none written twice.
    fn servers(&mut self, map: &Mapping) -> Vec<ToolkitServer> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (i, item) in self.list(map, "mcp_servers").iter().enumerate() {
            let field = format!("mcp_servers[{i}]");
            let server = match item {
                Value::Mapping(m) => {
                    for key in m.keys() {
                        let key = key.as_str().unwrap_or_default();
                        if key != "server" && key != "tools" {
                            self.push(
                                &format!("{field}.{key}"),
                                "isn't a field: use server and tools",
                            );
                        }
                    }
                    let server = match m.get("server") {
                        Some(v) => self.reference(&format!("{field}.server"), v),
                        None => {
                            self.push(&format!("{field}.server"), "is required");
                            None
                        }
                    };
                    let tools = self.tools(&field, m.get("tools"));
                    server.map(|server| ToolkitServer { server, tools })
                }
                Value::String(_) => self.reference(&field, item).map(|server| ToolkitServer {
                    server,
                    tools: None,
                }),
                _ => {
                    self.push(&field, "must be a server's name, or {server, tools}");
                    None
                }
            };
            if let Some(server) = server {
                if seen.insert(server.server.to_string()) {
                    out.push(server);
                } else {
                    self.push(&field, "is listed twice");
                }
            }
        }
        out
    }
}

/// A toolkit (D184), with every problem at once as `field: message; …`.
fn toolkit(map: &Mapping) -> Result<(EntryName, EntrySpec), String> {
    let mut p = Problems::default();
    for key in map.keys() {
        let key = key.as_str().unwrap_or_default();
        match key {
            k if TOOLKIT_FIELDS.contains(&k) => {}
            "models" | "default_models" | "small_model" => p.push(
                key,
                "a toolkit holds catalog entries only; models belong to environments",
            ),
            "toolkits" => p.push(key, "a toolkit can't hold toolkits"),
            other => p.push(other, "isn't a field of a toolkit"),
        }
    }
    let name = match map.get("name").and_then(Value::as_str) {
        Some(n) => EntryName::parse(n)
            .map_err(|e| p.push("name", &e.to_string()))
            .ok(),
        None => {
            p.push("name", "is required");
            None
        }
    };
    let description = match map.get("description") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(d)) => d.clone(),
        Some(_) => {
            p.push("description", "must be text");
            String::new()
        }
    };
    let spec = ToolkitSpec {
        description,
        mcp_servers: p.servers(map),
        a2a_agents: p.references(map, "a2a_agents"),
        skills: p.references(map, "skills"),
        plugins: p.references(map, "plugins"),
        prompts: p.references(map, "prompts"),
    };
    if p.0.is_empty() && spec.references().is_empty() {
        p.0.push("list at least one MCP server, A2A agent, skill, plugin or prompt".to_owned());
    }
    match name {
        Some(name) if p.0.is_empty() => Ok((name, EntrySpec::Toolkit(spec))),
        _ => Err(p.0.join("; ")),
    }
}
