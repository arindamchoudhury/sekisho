//! The lock file (D201): `.sekisho/lock.json` in a project folder. Task 4
//! writes it; what is read here, every other field being ignored, is the
//! part `sekisho env list` shows:
//!
//! ```json
//! {"version": 1,
//!  "environments": {"<env>": {...}, ...},
//!  "models": {"<tool>": "<env>", ...}}
//! ```
//!
//! `environments` holds every environment installed in the scope (D215);
//! `models` each tool's model environment (D216), tools named as
//! `Tool::as_str` spells them (`claude-code`, `codex`, …).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::IgnoredAny;

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
