//! What each coding tool gets from an environment (D192–D216): one adapter
//! per tool turns a delivery document into whole files and keys in shared
//! config files, and the install engine ([`crate::install`]) checks and
//! writes them. Adapters do no I/O.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use sekisho_catalog::delivery::{Delivery, EntryAddress};
use sekisho_catalog::names::EntryKind;
use serde::{Deserialize, Serialize};

use crate::install::Scope;

mod test_adapter;

pub use test_adapter::TestAdapter;

/// The eight coding tools of release 1 (D19).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    ClaudeCode,
    Codex,
    GeminiCli,
    OpenCode,
    CopilotCli,
    Pi,
    QwenCode,
    KiloCli,
}

impl Tool {
    pub const ALL: [Tool; 8] = [
        Tool::ClaudeCode,
        Tool::Codex,
        Tool::GeminiCli,
        Tool::OpenCode,
        Tool::CopilotCli,
        Tool::Pi,
        Tool::QwenCode,
        Tool::KiloCli,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Tool::ClaudeCode => "claude-code",
            Tool::Codex => "codex",
            Tool::GeminiCli => "gemini-cli",
            Tool::OpenCode => "opencode",
            Tool::CopilotCli => "copilot-cli",
            Tool::Pi => "pi",
            Tool::QwenCode => "qwen-code",
            Tool::KiloCli => "kilo-cli",
        }
    }

    pub fn parse(s: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a tool loads, as `use` and `status` group it (D214).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Models,
    McpServers,
    Skills,
    Prompts,
    Plugins,
}

impl ContentKind {
    /// What a catalog entry of `kind` is to a tool; A2A agents reach tools
    /// as MCP tools (D100), and toolkits are never delivered themselves.
    pub fn of(kind: EntryKind) -> ContentKind {
        match kind {
            EntryKind::McpServer | EntryKind::A2aAgent => ContentKind::McpServers,
            EntryKind::Skill | EntryKind::Toolkit => ContentKind::Skills,
            EntryKind::Plugin => ContentKind::Plugins,
            EntryKind::Prompt => ContentKind::Prompts,
        }
    }

    /// One of them, in words.
    pub fn word(self) -> &'static str {
        match self {
            ContentKind::Models => "model setting",
            ContentKind::McpServers => "MCP server",
            ContentKind::Skills => "skill",
            ContentKind::Prompts => "prompt",
            ContentKind::Plugins => "plugin",
        }
    }
}

/// A project install or the user's global one (D214).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Project,
    Global,
}

/// How a tool combines one kind of content from a project and a global
/// install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    /// The tool loads both scopes' content of this kind.
    pub both_load: bool,
    /// Which scope's wins a name both define; `None` when names don't meet
    /// (plugins in separately named marketplaces, say).
    pub same_name_wins: Option<ScopeKind>,
}

/// A tool's [`Layer`] for each kind of content, from its docs or source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layering {
    pub models: Layer,
    pub mcp_servers: Layer,
    pub skills: Layer,
    pub prompts: Layer,
    pub plugins: Layer,
}

impl Layering {
    pub fn get(&self, kind: ContentKind) -> Layer {
        match kind {
            ContentKind::Models => self.models,
            ContentKind::McpServers => self.mcp_servers,
            ContentKind::Skills => self.skills,
            ContentKind::Prompts => self.prompts,
            ContentKind::Plugins => self.plugins,
        }
    }
}

/// Where a path is relative to: the project folder, a tool's own home, or
/// Sekisho's folder in the scope (`.sekisho/` in a project, the config
/// folder globally).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Root {
    Project,
    ToolHome(Tool),
    SekishoConfig,
}

/// A relative, `/`-separated path that can't leave its root (D207).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

/// Why a path was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathError(pub String);

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PathError {}

/// Windows' device names, which no file may take, with any extension.
#[cfg(windows)]
const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

impl RelPath {
    /// Refuses an empty or absolute path, `.` and `..` segments, empty
    /// segments, backslashes and NUL, and on Windows `:`, device names and
    /// segments ending in a dot or a space.
    pub fn parse(s: &str) -> Result<RelPath, PathError> {
        let bad = |why: &str| Err(PathError(format!("the path {s:?} {why}")));
        if s.is_empty() {
            return bad("is empty");
        }
        if s.starts_with('/') {
            return bad("is absolute");
        }
        if s.contains('\\') {
            return bad("has a backslash");
        }
        if s.contains('\0') {
            return bad("has a NUL character");
        }
        for segment in s.split('/') {
            if segment.is_empty() {
                return bad("has an empty segment");
            }
            if segment == "." || segment == ".." {
                return bad("has a . or .. segment");
            }
            #[cfg(windows)]
            {
                if segment.contains(':') {
                    return bad("has a colon");
                }
                if segment.ends_with('.') || segment.ends_with(' ') {
                    return bad("has a segment ending in a dot or a space");
                }
                let stem = segment.split('.').next().unwrap_or("").to_ascii_lowercase();
                if RESERVED.contains(&stem.trim_end()) {
                    return bad("names a Windows device");
                }
            }
        }
        Ok(RelPath(s.to_owned()))
    }

    /// `self/rest`, checked like [`RelPath::parse`].
    pub fn join(&self, rest: &str) -> Result<RelPath, PathError> {
        RelPath::parse(&format!("{}/{rest}", self.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path's segments.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RelPath {
    type Error = PathError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        RelPath::parse(&s)
    }
}

impl From<RelPath> for String {
    fn from(p: RelPath) -> String {
        p.0
    }
}

/// A shared config file's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Json,
    /// JSON with comments and trailing commas, which are kept.
    Jsonc,
    Toml,
}

/// What a write is for: a catalog entry, the environment itself (its MCP
/// endpoint, say), or the tool's model settings, which only the tool's
/// model environment writes (D216). The engine tells a clash from the same
/// entry at another digest (D219) by it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Origin {
    Entry(EntryAddress),
    Environment,
    Models,
}

impl Origin {
    pub fn of(entry: &sekisho_catalog::delivery::DeliveredEntry) -> Origin {
        Origin::Entry(EntryAddress {
            kind: entry.kind,
            source: entry.source.clone(),
            name: entry.name.clone(),
        })
    }

    /// The kind of content and its name, for reports; `None` for the
    /// environment's own settings.
    pub fn named(&self) -> Option<(ContentKind, String)> {
        match self {
            Origin::Entry(a) => Some((ContentKind::of(a.kind), a.name.to_string())),
            Origin::Models => Some((ContentKind::Models, "models".to_owned())),
            Origin::Environment => None,
        }
    }
}

/// A whole file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileWrite {
    pub root: Root,
    pub path: RelPath,
    pub content: Vec<u8>,
    pub executable: bool,
    pub origin: Origin,
}

/// One key in a shared config file, the rest of which is the user's.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyWrite {
    pub root: Root,
    pub file: RelPath,
    pub format: Format,
    /// From the document's top: `["mcpServers", "db"]`.
    pub key: Vec<String>,
    pub value: serde_json::Value,
    pub origin: Origin,
}

/// Content a tool can't use (D205), listed once at the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub what: String,
    pub why: String,
}

/// How `sekisho run` starts a tool (D191, D194).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// What an adapter makes of one environment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rendered {
    pub files: Vec<FileWrite>,
    pub keys: Vec<KeyWrite>,
    pub skipped: Vec<Skipped>,
    pub run: Option<RunSpec>,
}

/// What an adapter renders with, besides the delivery document.
pub struct RenderContext<'a> {
    pub scope: &'a Scope,
    /// Every delivered file's bytes by SHA-256, already checked.
    pub blobs: &'a BTreeMap<String, Vec<u8>>,
    /// Each delivered plugin's installed name (D204).
    pub plugin_names: &'a HashMap<EntryAddress, String>,
    /// The environment's local MCP servers, as each is started (Task 5).
    pub local_servers: &'a [crate::install::launch::LocalServer],
    /// This environment is the tool's model environment: only then does
    /// the adapter render model settings and a [`RunSpec`] (D216).
    pub models: bool,
    /// `sekisho` as the user would type it: `sekisho` on `PATH`, else its
    /// absolute path.
    pub sekisho: &'a str,
}

/// Why an adapter couldn't render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterError(pub String);

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AdapterError {}

impl From<PathError> for AdapterError {
    fn from(e: PathError) -> Self {
        AdapterError(e.0)
    }
}

/// A server or marketplace in a tool's config that Sekisho didn't write
/// (D211).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmanagedItem {
    pub tool: Tool,
    pub kind: ContentKind,
    pub name: String,
    pub file: String,
}

/// One coding tool.
pub trait Adapter {
    fn tool(&self) -> Tool;

    /// The tool is there: its folder exists, or its program is on `PATH`.
    fn detect(&self, scope: &Scope) -> bool;

    /// The tool's files and keys for one environment. No I/O.
    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError>;

    /// What the tool's config holds that no install wrote (D211); `locks`
    /// are both scopes', so the other scope's install is never listed.
    fn unmanaged(&self, scope: &Scope, locks: &[&crate::install::lock::Lock])
    -> Vec<UnmanagedItem>;

    /// How the tool combines a project and a global install (D214).
    fn layering(&self) -> Layering;
}
