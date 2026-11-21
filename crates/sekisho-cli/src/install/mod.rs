//! Installing environments into a folder or the user's global config: the
//! engine behind `sekisho use` and `sekisho uninstall` (plan 4, Task 4).
//!
//! An install fetches and checks every file first (SHA-256, size, hidden
//! characters, paths), renders every adapter, compares what they want with
//! the lock and the disk, stops on a clash with another environment (D216)
//! or on the user's own content (D202), then writes: whole files atomically,
//! shared files through one read-merge-write each, removals, and the lock
//! last. A failed write puts back what it had already written.

use std::fmt;

use sekisho_catalog::delivery::Delivery;
use sekisho_core::names::EnvName;

use crate::adapters::{Adapter, ContentKind, ScopeKind, Tool};
use crate::output::{CliError, Exit};

pub mod exclude;
pub mod files;
pub mod launch;
pub mod lock;
pub mod merge;
mod plan;
pub mod scope;

pub use scope::{Scope, ToolHomes};

/// How an install or uninstall runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Check and report, write nothing.
    pub dry_run: bool,
    /// Overwrite the user's own files and keys without asking (D202).
    pub yes: bool,
    /// Replace or remove files and keys edited since Sekisho wrote them
    /// (D203).
    pub force: bool,
    /// Make this environment the model environment of its tools (D216).
    pub models: bool,
    /// `sekisho` as the user would type it; `sekisho` when unset.
    pub sekisho: Option<String>,
}

/// What changed, or would.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sign {
    Added,
    Updated,
    Removed,
    /// Edited since Sekisho wrote it, so left alone (D203).
    Kept,
}

impl Sign {
    /// `+ ~ - !` (D203).
    pub fn mark(self) -> char {
        match self {
            Sign::Added => '+',
            Sign::Updated => '~',
            Sign::Removed => '-',
            Sign::Kept => '!',
        }
    }
}

/// One file or key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub tool: Tool,
    pub sign: Sign,
    /// The file (`.claude/skills/pdf/SKILL.md`) or the key
    /// (`.mcp.json mcpServers.db`).
    pub what: String,
    /// A text file's or key's unified diff, for an update.
    pub diff: Option<String>,
}

/// A name a session in this folder also gets from the other scope (D214).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layered {
    pub tool: Tool,
    pub other_env: EnvName,
    pub kind: ContentKind,
    pub name: String,
    /// Which scope's the tool uses.
    pub used: ScopeKind,
}

/// What an install or uninstall did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub items: Vec<Item>,
    pub warnings: Vec<String>,
    /// Each tool's model environment after the change.
    pub models: Vec<(Tool, EnvName)>,
    pub layered: Vec<Layered>,
}

/// A name two environments deliver with different content (D216).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clash {
    pub tool: Tool,
    pub kind: ContentKind,
    pub name: String,
    pub path_or_key: String,
    pub held_by: EnvName,
}

/// Why an install or uninstall stopped. Nothing was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    Clash(Vec<Clash>),
    /// The user's own files or keys would be overwritten: ask, then run
    /// again with `yes` (D202).
    NeedsConsent(Vec<String>),
    /// A check failed: a hash, a path, a symlink, a hidden character, a
    /// limit, a file that can't be read.
    Refused(String),
    /// Reading or writing failed; what was written is put back.
    Io(String),
}

impl InstallError {
    pub fn exit(&self) -> Exit {
        match self {
            InstallError::Io(_) => Exit::Error,
            _ => Exit::Refused,
        }
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::Clash(clashes) => {
                let lines: Vec<String> = clashes
                    .iter()
                    .map(|c| {
                        format!(
                            "{} \"{}\" is also delivered by environment \"{}\" ({}, {})",
                            c.kind.word(),
                            c.name,
                            c.held_by,
                            c.tool,
                            c.path_or_key
                        )
                    })
                    .collect();
                f.write_str(&lines.join("\n"))
            }
            InstallError::NeedsConsent(items) => write!(
                f,
                "these are yours and would be overwritten: {}",
                items.join(", ")
            ),
            InstallError::Refused(why) | InstallError::Io(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<InstallError> for CliError {
    fn from(e: InstallError) -> Self {
        let hint = match &e {
            InstallError::Clash(_) => {
                Some("ask an admin to rename one, or sekisho uninstall the other environment")
            }
            InstallError::NeedsConsent(_) => Some("run again with --yes to overwrite them"),
            _ => None,
        };
        let error = CliError::new(e.exit(), e.to_string());
        match hint {
            Some(h) => error.with_hint(h),
            None => error,
        }
    }
}

/// A delivered file's bytes, by SHA-256.
pub type Fetch<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, InstallError>;

/// Installs one environment for `adapters`' tools, beside the others in
/// the scope (D215).
pub fn install(
    d: &Delivery,
    adapters: &[&dyn Adapter],
    scope: &Scope,
    options: &Options,
    fetch: Fetch<'_>,
) -> Result<Report, InstallError> {
    plan::install(d, adapters, scope, options, fetch)
}

/// Removes `envs` (every one when `None`) for `tools` (every one when
/// `None`) from the scope.
pub fn uninstall(
    scope: &Scope,
    envs: Option<&[EnvName]>,
    tools: Option<&[Tool]>,
    options: &Options,
) -> Result<Report, InstallError> {
    plan::uninstall(scope, envs, tools, options)
}
