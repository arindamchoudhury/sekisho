//! What a source holds once checked: entries with their specs and files,
//! the items refused and why, warnings, and Obot's virtual servers.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::environment::EnvironmentDoc;
use crate::launch::Launch;
use crate::names::{EntryKind, EntryName, EntryRef};
use crate::pins::{ImageRef, NpmPackage, PythonPackage};
use crate::tree::TreeFile;

/// An entry's definition, stored with each revision. A skill's, plugin's
/// or prompt's files are in [`CheckedEntry::files`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntrySpec {
    McpServer(McpServerSpec),
    A2aAgent(A2aAgentSpec),
    Skill(SkillSpec),
    Plugin(PluginSpec),
    Prompt(PromptSpec),
    Toolkit(ToolkitSpec),
    Instruction(InstructionSpec),
}

/// A toolkit: other catalog entries, never models or toolkits (D182,
/// D185). References are kept as written; the store binds them to entries
/// when a sync records the revision, outside its digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolkitSpec {
    pub description: String,
    pub mcp_servers: Vec<ToolkitServer>,
    pub a2a_agents: Vec<EntryRef>,
    pub skills: Vec<EntryRef>,
    pub plugins: Vec<EntryRef>,
    pub prompts: Vec<EntryRef>,
    /// In the order written (D220). Left out of the stored form when empty,
    /// so toolkits stored before instructions keep their digests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<EntryRef>,
}

impl ToolkitSpec {
    /// Every reference with the kind it names, in the order written: MCP
    /// servers, A2A agents, skills, plugins, prompts, instructions.
    pub fn references(&self) -> Vec<(EntryKind, &EntryRef)> {
        let mut out: Vec<(EntryKind, &EntryRef)> = self
            .mcp_servers
            .iter()
            .map(|s| (EntryKind::McpServer, &s.server))
            .collect();
        for (kind, refs) in [
            (EntryKind::A2aAgent, &self.a2a_agents),
            (EntryKind::Skill, &self.skills),
            (EntryKind::Plugin, &self.plugins),
            (EntryKind::Prompt, &self.prompts),
            (EntryKind::Instruction, &self.instructions),
        ] {
            out.extend(refs.iter().map(|r| (kind, r)));
        }
        out
    }
}

/// An MCP server a toolkit lists, with the tools it allows; `None` allows
/// them all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolkitServer {
    pub server: EntryRef,
    pub tools: Option<Vec<String>>,
}

/// An Agent Skill's frontmatter, as Obot reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSpec {
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
}

/// A Claude Code plugin, an APM bundle among them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginSpec {
    pub description: String,
    pub version: Option<String>,
    /// The MCP servers it starts or reaches, from `.mcp.json` and its
    /// manifest's `mcpServers`.
    pub mcp_servers: Vec<PluginMcpServer>,
    /// Its files matched an APM lock file's `pack.bundle_files` (D169).
    pub apm_checked: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginMcpServer {
    pub name: String,
    pub launch: Launch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSpec {
    pub description: String,
}

/// An instruction (D220): its one file is the text. The description is
/// the text's first Markdown heading, if it starts with one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionSpec {
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpServerSpec {
    pub description: String,
    pub runtime: McpRuntime,
    pub config: Vec<ConfigField>,
    pub tool_preview: Vec<ToolPreview>,
    /// Tools the catalog says don't work with Sekisho's clients.
    pub unsupported_tools: Vec<String>,
    /// The server needs an OAuth client an admin registers (Obot's
    /// `staticOAuthRequired`).
    pub static_oauth: bool,
}

/// How a server runs: reached over the network, or started on the
/// developer's machine from an exact version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpRuntime {
    Remote {
        url: String,
    },
    Npx {
        package: NpmPackage,
        args: Vec<String>,
    },
    Uvx {
        package: PythonPackage,
        command: Option<String>,
        args: Vec<String>,
    },
    Container {
        image: ImageRef,
        command: Option<String>,
        args: Vec<String>,
        port: Option<u16>,
        path: Option<String>,
    },
}

/// A setting the server takes, as an environment variable or a header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigField {
    /// `GITHUB_TOKEN` for a variable, `X-API-KEY` for a header (Obot's
    /// normal form).
    pub key: String,
    pub name: String,
    pub description: String,
    pub required: bool,
    /// The person supplies it; a catalog never holds a sensitive value.
    pub sensitive: bool,
    /// A fixed value set by the catalog.
    pub value: Option<String>,
    pub target: ConfigTarget,
    /// Put before the person's value, such as `Bearer `.
    pub prefix: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigTarget {
    Env,
    Header,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPreview {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct A2aAgentSpec {
    pub description: String,
    pub url: String,
    pub card_url: Option<String>,
    /// The card as fetched and checked at sync (D163); never from Git.
    pub card: Option<PinnedCard>,
}

/// An agent's card as pinned at sync ([`crate::card::check_card`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PinnedCard {
    pub json: serde_json::Value,
    /// Protocol versions the card lists, in its order.
    pub versions: Vec<String>,
    /// Protocol bindings (transports), such as `JSONRPC`.
    pub bindings: Vec<String>,
}

/// An entry a source holds and Sekisho accepts.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckedEntry {
    pub kind: EntryKind,
    pub name: EntryName,
    /// Where in the source it was found: a catalog file, or an entry's folder.
    pub path: String,
    pub spec: EntrySpec,
    /// The entry's own files; none for MCP servers and A2A agents.
    pub files: Vec<TreeFile>,
}

impl CheckedEntry {
    /// The revision digest: SHA-256, in hex, over the canonical JSON of
    /// `{spec, files: [[path, sha256, executable]]}`, files sorted by path.
    pub fn digest(&self) -> String {
        let mut files: Vec<&TreeFile> = self.files.iter().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let files: Vec<serde_json::Value> = files
            .into_iter()
            .map(|f| serde_json::json!([f.path, sha256_hex(&f.bytes), f.executable]))
            .collect();
        let doc = serde_json::json!({
            "spec": serde_json::to_value(&self.spec).expect("specs serialize"),
            "files": files,
        });
        let mut text = String::new();
        canonical(&doc, &mut text);
        sha256_hex(text.as_bytes())
    }
}

/// SHA-256 in lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// JSON with every object's keys sorted, whatever order `serde_json` keeps
/// (the workspace turns on `preserve_order`).
fn canonical(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// An item of a source that isn't an entry, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub path: String,
    pub kind: Option<EntryKind>,
    pub name: Option<String>,
    pub reason: String,
}

/// Something accepted, with a remark: an ignored field, a hidden character
/// that doesn't refuse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub path: String,
    pub message: String,
}

/// An Obot virtual MCP server, offered as an environment (Task 9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObotVmcp {
    pub key: EntryName,
    pub display_name: String,
    pub description: String,
    pub components: Vec<VmcpComponent>,
    /// What Sekisho can't carry over, one line each.
    pub differences: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmcpComponent {
    /// An MCP server entry of the same source.
    pub server: EntryName,
    /// The enabled tools; `None` is all of them.
    pub tools: Option<Vec<String>>,
}

/// What [`crate::check::check_source`] found in a source.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceCheck {
    pub entries: Vec<CheckedEntry>,
    pub refused: Vec<Refusal>,
    pub warnings: Vec<Warning>,
    pub vmcps: Vec<ObotVmcp>,
    /// Git mode's environments: each file's path and what it holds.
    pub environments: Vec<(String, EnvironmentDoc)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_at_every_depth() {
        let mut out = String::new();
        canonical(
            &serde_json::json!({"b": [{"z": 1, "a": "x\"y"}], "a": null}),
            &mut out,
        );
        assert_eq!(out, r#"{"a":null,"b":[{"a":"x\"y","z":1}]}"#);
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn specs_carry_their_kind() {
        let spec = EntrySpec::A2aAgent(A2aAgentSpec {
            description: String::new(),
            url: "https://a".into(),
            card_url: None,
            card: None,
        });
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["kind"], "a2a_agent");
        assert_eq!(serde_json::from_value::<EntrySpec>(json).unwrap(), spec);
    }
}
