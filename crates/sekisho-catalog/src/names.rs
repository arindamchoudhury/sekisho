//! Names of catalog sources and entries, references to entries, and the
//! kinds of each.

use std::fmt;

pub use sekisho_core::names::{EntryName, NameError, SourceName};

/// An entry named in an environment: `<entry>`, or `<entry>@<source>` when
/// the bare name is held by more than one source.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryRef {
    pub name: EntryName,
    pub source: Option<SourceName>,
}

impl EntryRef {
    pub fn parse(s: &str) -> Result<Self, NameError> {
        let (name, source) = match s.split_once('@') {
            Some((name, source)) => (name, Some(SourceName::parse(source)?)),
            None => (s, None),
        };
        Ok(Self {
            name: EntryName::parse(name)?,
            source,
        })
    }
}

/// Written as its text, `name` or `name@source`.
impl serde::Serialize for EntryRef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for EntryRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        EntryRef::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for EntryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(f, "{}@{}", self.name, source),
            None => write!(f, "{}", self.name),
        }
    }
}

/// What a catalog entry is.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    McpServer,
    A2aAgent,
    Skill,
    Plugin,
    Prompt,
    /// A reusable set of other entries (D181), in `catalog` sources.
    Toolkit,
}

impl EntryKind {
    pub const ALL: [EntryKind; 6] = [
        EntryKind::McpServer,
        EntryKind::A2aAgent,
        EntryKind::Skill,
        EntryKind::Plugin,
        EntryKind::Prompt,
        EntryKind::Toolkit,
    ];

    /// The kind as stored and as the API's `kind` field spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::McpServer => "mcp_server",
            EntryKind::A2aAgent => "a2a_agent",
            EntryKind::Skill => "skill",
            EntryKind::Plugin => "plugin",
            EntryKind::Prompt => "prompt",
            EntryKind::Toolkit => "toolkit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The kind's segment in API paths, such as `/api/v1/mcp-servers`.
    pub fn path_segment(self) -> &'static str {
        match self {
            EntryKind::McpServer => "mcp-servers",
            EntryKind::A2aAgent => "a2a-agents",
            EntryKind::Skill => "skills",
            EntryKind::Plugin => "plugins",
            EntryKind::Prompt => "prompts",
            EntryKind::Toolkit => "toolkits",
        }
    }

    pub fn from_path_segment(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.path_segment() == s)
    }
}

/// What a catalog source's repository holds (D164).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Obot's MCP catalog format, and Sekisho's own for MCP servers, A2A
    /// agents and toolkits.
    Catalog,
    /// Agent Skills: folders with a `SKILL.md`.
    Skills,
    /// A Claude Code plugin marketplace, APM's bundles included.
    Plugins,
    Prompts,
    /// Git mode's environments; only [`SourceName::git_mode`] has it.
    Environments,
}

impl SourceKind {
    pub const ALL: [SourceKind; 5] = [
        SourceKind::Catalog,
        SourceKind::Skills,
        SourceKind::Plugins,
        SourceKind::Prompts,
        SourceKind::Environments,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Catalog => "catalog",
            SourceKind::Skills => "skills",
            SourceKind::Plugins => "plugins",
            SourceKind::Prompts => "prompts",
            SourceKind::Environments => "environments",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The kinds of entry a source of this kind can hold.
    pub fn entry_kinds(self) -> &'static [EntryKind] {
        match self {
            SourceKind::Catalog => &[
                EntryKind::McpServer,
                EntryKind::A2aAgent,
                EntryKind::Toolkit,
            ],
            SourceKind::Skills => &[EntryKind::Skill],
            SourceKind::Plugins => &[EntryKind::Plugin],
            SourceKind::Prompts => &[EntryKind::Prompt],
            SourceKind::Environments => &[],
        }
    }
}
