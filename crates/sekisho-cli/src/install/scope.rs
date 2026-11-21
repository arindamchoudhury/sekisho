//! Where an install goes: a project folder, or the user's global config
//! (D214). Each tool's home honours the tool's own variable; the defaults
//! here are each adapter task's to confirm against the tool (plan 4, Tasks
//! 6–9).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::adapters::{Root, ScopeKind, Tool};

/// Each tool's own config folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolHomes(BTreeMap<Tool, PathBuf>);

impl ToolHomes {
    /// Every tool must be given.
    pub fn new(homes: BTreeMap<Tool, PathBuf>) -> Self {
        assert!(
            Tool::ALL.iter().all(|t| homes.contains_key(t)),
            "every tool needs a home"
        );
        Self(homes)
    }

    /// From the tools' variables, else their default folders under `home`.
    pub fn from_env(var: &dyn Fn(&str) -> Option<String>, home: &Path) -> Self {
        let set = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let config = set("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let homes = Tool::ALL
            .into_iter()
            .map(|tool| {
                let dir = match tool {
                    Tool::ClaudeCode => {
                        set("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"))
                    }
                    Tool::Codex => set("CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
                    // GEMINI_CLI_HOME stands in for the home folder.
                    Tool::GeminiCli => set("GEMINI_CLI_HOME")
                        .unwrap_or_else(|| home.to_path_buf())
                        .join(".gemini"),
                    Tool::QwenCode => home.join(".qwen"),
                    Tool::OpenCode => config.join("opencode"),
                    Tool::KiloCli => config.join("kilo"),
                    Tool::CopilotCli => {
                        set("COPILOT_HOME").unwrap_or_else(|| home.join(".copilot"))
                    }
                    Tool::Pi => {
                        set("PI_CODING_AGENT_DIR").unwrap_or_else(|| home.join(".pi").join("agent"))
                    }
                };
                (tool, dir)
            })
            .collect();
        Self(homes)
    }

    pub fn get(&self, tool: Tool) -> &Path {
        &self.0[&tool]
    }
}

/// A project folder's install, or the global one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    kind: ScopeKind,
    /// The project folder; `None` globally.
    project: Option<PathBuf>,
    /// Sekisho's config folder (`SEKISHO_CONFIG_DIR` or the platform's).
    config: PathBuf,
    homes: ToolHomes,
}

impl Scope {
    pub fn project(folder: &Path, config: &Path, homes: ToolHomes) -> Self {
        Self {
            kind: ScopeKind::Project,
            project: Some(folder.to_path_buf()),
            config: config.to_path_buf(),
            homes,
        }
    }

    pub fn global(config: &Path, homes: ToolHomes) -> Self {
        Self {
            kind: ScopeKind::Global,
            project: None,
            config: config.to_path_buf(),
            homes,
        }
    }

    pub fn kind(&self) -> ScopeKind {
        self.kind
    }

    /// The project folder, in a project scope.
    pub fn project_folder(&self) -> Option<&Path> {
        self.project.as_deref()
    }

    /// The folder a root names; `None` for the project in the global scope.
    pub fn root(&self, root: Root) -> Option<PathBuf> {
        match root {
            Root::Project => self.project.clone(),
            Root::ToolHome(tool) => Some(self.homes.get(tool).to_path_buf()),
            Root::SekishoConfig => Some(match &self.project {
                Some(p) => p.join(".sekisho"),
                None => self.config.clone(),
            }),
        }
    }

    /// The scope's lock file (D201).
    pub fn lock_path(&self) -> PathBuf {
        match &self.project {
            Some(p) => super::lock::project_lock(p),
            None => self.config.join("global").join("lock.json"),
        }
    }

    /// The other scope a session in this one also loads: the global one
    /// for a project; none for the global one (D214).
    pub fn other(&self) -> Option<Scope> {
        self.project
            .as_ref()
            .map(|_| Scope::global(&self.config, self.homes.clone()))
    }

    pub fn homes(&self) -> &ToolHomes {
        &self.homes
    }
}
