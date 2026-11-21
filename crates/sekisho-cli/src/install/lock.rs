//! The lock file (D201): `.sekisho/lock.json` in a project folder,
//! `<config dir>/global/lock.json` globally; one per scope whatever the
//! number of environments. [`installed`] reads only the part `sekisho env
//! list` shows, every other field being ignored:
//!
//! ```json
//! {"version": 1,
//!  "environments": {"<env>": {...}, ...},
//!  "models": {"<tool>": "<env>", ...}}
//! ```
//!
//! `environments` holds every environment installed in the scope (D215);
//! `models` each tool's model environment (D216), tools named as
//! `Tool::as_str` spells them (`claude-code`, `codex`, …). The rest is
//! [`Lock`]: per tool each whole file with its SHA-256 and each key Sekisho
//! owns in a shared file with the SHA-256 of the value it wrote, each with
//! the environments that deliver it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sekisho_catalog::names::{EntryKind, EntryName, SourceName};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::adapters::{Format, Origin, RelPath, Root, RunSpec, Tool};
use crate::output::CliError;

/// A project folder's lock file.
pub fn project_lock(folder: &Path) -> PathBuf {
    folder.join(".sekisho").join("lock.json")
}

/// What a lock says is installed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Installed {
    /// The environments installed, by name.
    pub environments: Vec<String>,
    /// Each tool's model environment.
    pub models: BTreeMap<String, String>,
}

impl Installed {
    pub fn has(&self, environment: &str) -> bool {
        self.environments.iter().any(|e| e == environment)
    }

    /// The tools that take their models from `environment`, sorted.
    pub fn model_tools(&self, environment: &str) -> Vec<String> {
        self.models
            .iter()
            .filter(|(_, env)| *env == environment)
            .map(|(tool, _)| tool.clone())
            .collect()
    }
}

#[derive(Deserialize)]
struct Summary {
    #[serde(default)]
    environments: BTreeMap<String, IgnoredAny>,
    #[serde(default)]
    models: BTreeMap<String, String>,
}

/// What the lock at `path` says is installed; nothing when there is no
/// lock. A lock that can't be read is an error naming it.
pub fn installed(path: &Path) -> Result<Installed, CliError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Installed::default()),
        Err(e) => {
            return Err(CliError::error(format!(
                "can't read {}: {e}",
                path.display()
            )));
        }
    };
    let summary: Summary = serde_json::from_slice(&bytes).map_err(|e| {
        CliError::error(format!("{} isn't a Sekisho lock file: {e}", path.display()))
    })?;
    Ok(Installed {
        environments: summary.environments.into_keys().collect(),
        models: summary.models,
    })
}

/// The lock's format version.
pub const VERSION: u32 = 1;

/// A scope's whole lock.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    #[serde(default)]
    pub environments: BTreeMap<String, EnvLock>,
    /// Each tool's model environment (D216).
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<FileLock>,
    #[serde(default)]
    pub keys: Vec<KeyLock>,
}

/// One installed environment.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvLock {
    /// The delivery document's revision.
    pub revision: String,
    /// When it was first installed, as a count: the earliest takes the
    /// models when the model environment goes (D216).
    pub order: u64,
    pub entries: Vec<EntryLock>,
    /// Per tool it was installed for: its model settings, written only
    /// while it is the tool's model environment and kept here so another
    /// environment's uninstall can hand the models to it offline.
    pub tools: BTreeMap<Tool, ToolLock>,
}

/// A delivered catalog entry at its revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryLock {
    pub kind: EntryKind,
    pub source: SourceName,
    pub name: EntryName,
    pub commit: String,
    pub digest: String,
}

/// One environment's model settings for one tool, and how `sekisho run`
/// starts the tool (Task 7).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolLock {
    #[serde(default)]
    pub model_files: Vec<StoredFile>,
    #[serde(default)]
    pub model_keys: Vec<StoredKey>,
    #[serde(default)]
    pub run: Option<RunSpec>,
}

/// A model settings file, kept whole: never a secret (Global Constraints).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFile {
    pub root: Root,
    pub path: RelPath,
    /// Base64.
    pub content: String,
    pub executable: bool,
}

/// A model settings key, kept whole.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredKey {
    pub root: Root,
    pub file: RelPath,
    pub format: Format,
    pub key: Vec<String>,
    pub value: serde_json::Value,
}

/// A whole file Sekisho wrote for a tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLock {
    pub tool: Tool,
    pub root: Root,
    pub path: RelPath,
    pub sha256: String,
    pub origin: Origin,
    /// The environments that deliver it, in install order.
    pub envs: Vec<String>,
}

/// A key Sekisho owns in a shared file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyLock {
    pub tool: Tool,
    pub root: Root,
    pub file: RelPath,
    pub format: Format,
    pub key: Vec<String>,
    /// Of the value's canonical JSON.
    pub sha256: String,
    pub origin: Origin,
    pub envs: Vec<String>,
    /// Sekisho made the file, so it goes when nothing is left in it.
    pub created_file: bool,
}

impl Lock {
    /// The lock at `path`; an empty one when there is none.
    pub fn read(path: &Path) -> Result<Lock, CliError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Lock {
                    version: VERSION,
                    ..Lock::default()
                });
            }
            Err(e) => {
                return Err(CliError::error(format!(
                    "can't read {}: {e}",
                    path.display()
                )));
            }
        };
        let lock: Lock = serde_json::from_slice(&bytes).map_err(|e| {
            CliError::error(format!("{} isn't a Sekisho lock file: {e}", path.display()))
        })?;
        if lock.version != VERSION {
            return Err(CliError::error(format!(
                "{} is a version {} lock; this sekisho reads version {VERSION}",
                path.display(),
                lock.version
            )));
        }
        Ok(lock)
    }

    /// The lock as written: pretty JSON with a final newline.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("locks serialize");
        out.push(b'\n');
        out
    }

    pub fn is_empty(&self) -> bool {
        self.environments.is_empty() && self.files.is_empty() && self.keys.is_empty()
    }
}
