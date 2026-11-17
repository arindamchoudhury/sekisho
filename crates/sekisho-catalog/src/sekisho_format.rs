//! Sekisho's own catalog format, `apiVersion: sekisho/v1alpha1`: MCP
//! servers (`kind: MCPServer`) and A2A agents (`kind: A2AAgent`). Unlike
//! Obot's, unknown fields are refused.

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_yaml_ng::{Mapping, Value};

use crate::check::{CheckOptions, Item, ItemRead};
use crate::model::{A2aAgentSpec, EntrySpec, McpRuntime, McpServerSpec};
use crate::names::{EntryKind, EntryName};
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
        _ => {
            let shown = text("kind").unwrap_or_else(|| "(none)".to_owned());
            return refused(
                None,
                name,
                format!("kind {shown} isn't known; use MCPServer or A2AAgent"),
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
