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

impl fmt::Display for EntryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(f, "{}@{}", self.name, source),
            None => write!(f, "{}", self.name),
        }
    }
}

/// What a catalog entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryKind {
    McpServer,
    A2aAgent,
    Skill,
    Plugin,
    Prompt,
}

impl EntryKind {
    pub const ALL: [EntryKind; 5] = [
        EntryKind::McpServer,
        EntryKind::A2aAgent,
        EntryKind::Skill,
        EntryKind::Plugin,
        EntryKind::Prompt,
    ];

    /// The kind as stored and as the API's `kind` field spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::McpServer => "mcp_server",
            EntryKind::A2aAgent => "a2a_agent",
            EntryKind::Skill => "skill",
            EntryKind::Plugin => "plugin",
            EntryKind::Prompt => "prompt",
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
        }
    }

    pub fn from_path_segment(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.path_segment() == s)
    }
}

/// What a catalog source's repository holds (D164).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// Obot's MCP catalog format, and Sekisho's own for MCP servers and A2A
    /// agents.
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
            SourceKind::Catalog => &[EntryKind::McpServer, EntryKind::A2aAgent],
            SourceKind::Skills => &[EntryKind::Skill],
            SourceKind::Plugins => &[EntryKind::Plugin],
            SourceKind::Prompts => &[EntryKind::Prompt],
            SourceKind::Environments => &[],
        }
    }
}
