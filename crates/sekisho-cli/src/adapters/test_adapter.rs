//! An adapter for no real tool, which the install engine's tests drive: it
//! writes under `.<tool>/` (in the project, or the tool's home globally)
//! skills and prompts as whole files, MCP servers as keys in `mcp.json`
//! (or `.shared/mcp.json` when shared), one key per environment in a
//! JSON-with-comments `settings.jsonc`, and the model settings as keys in
//! `config.toml`.

use sekisho_catalog::delivery::Delivery;
use sekisho_catalog::model::{EntrySpec, McpRuntime};
use serde_json::json;

use super::{
    Adapter, AdapterError, FileWrite, Format, KeyWrite, Layer, Layering, Origin, RelPath,
    RenderContext, Rendered, Root, RunSpec, ScopeKind, Skipped, Tool, UnmanagedItem,
};
use crate::install::Scope;
use crate::install::launch::EnvValue;

#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct TestAdapter {
    tool: Tool,
    shared_mcp: bool,
}

impl TestAdapter {
    pub fn new(tool: Tool) -> Self {
        Self {
            tool,
            shared_mcp: false,
        }
    }

    /// MCP servers go in `.shared/mcp.json`, a file another tool's test
    /// adapter writes too (D201).
    pub fn shared_mcp(mut self) -> Self {
        self.shared_mcp = true;
        self
    }

    fn root(&self, scope: &Scope) -> Root {
        match scope.kind() {
            ScopeKind::Project => Root::Project,
            ScopeKind::Global => Root::ToolHome(self.tool),
        }
    }

    fn dir(&self) -> String {
        format!(".{}", self.tool.as_str())
    }

    fn path(&self, rest: &str) -> Result<RelPath, AdapterError> {
        Ok(RelPath::parse(&self.dir())?.join(rest)?)
    }
}

impl Adapter for TestAdapter {
    fn tool(&self) -> Tool {
        self.tool
    }

    fn detect(&self, _scope: &Scope) -> bool {
        true
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        let root = self.root(ctx.scope);
        let mut out = Rendered::default();
        let blob = |sha: &str| {
            ctx.blobs
                .get(sha)
                .cloned()
                .ok_or_else(|| AdapterError(format!("no file {sha}")))
        };
        let mcp_file = if self.shared_mcp {
            RelPath::parse(".shared/mcp.json")?
        } else {
            self.path("mcp.json")?
        };
        for e in &d.entries {
            let origin = Origin::of(e);
            match &e.spec {
                EntrySpec::Skill(_) => {
                    for f in &e.files {
                        out.files.push(FileWrite {
                            root,
                            path: self.path(&format!("skills/{}/{}", e.name, f.path))?,
                            content: blob(&f.sha256)?,
                            executable: f.executable,
                            origin: origin.clone(),
                        });
                    }
                }
                EntrySpec::Prompt(_) => {
                    for f in &e.files {
                        out.files.push(FileWrite {
                            root,
                            path: self.path(&format!("prompts/{}", f.path))?,
                            content: blob(&f.sha256)?,
                            executable: false,
                            origin: origin.clone(),
                        });
                    }
                }
                EntrySpec::McpServer(s) => {
                    let value = match &s.runtime {
                        McpRuntime::Remote { url } => json!({"url": url}),
                        _ => match ctx.local_servers.iter().find(|l| l.entry.name == e.name) {
                            Some(l) => {
                                let env: serde_json::Map<String, serde_json::Value> = l
                                    .env
                                    .iter()
                                    .map(|(k, v)| {
                                        let v = match v {
                                            EnvValue::Literal(x) => x.clone(),
                                            EnvValue::FromUser { name, .. } => {
                                                format!("${{{name}}}")
                                            }
                                        };
                                        (k.clone(), json!(v))
                                    })
                                    .collect();
                                json!({"command": l.command, "args": l.args, "env": env})
                            }
                            None => continue,
                        },
                    };
                    out.keys.push(KeyWrite {
                        root,
                        file: mcp_file.clone(),
                        format: Format::Json,
                        key: vec!["mcpServers".into(), e.name.to_string()],
                        value,
                        origin,
                    });
                }
                _ => out.skipped.push(Skipped {
                    what: format!("{} {}", e.kind.as_str(), e.name),
                    why: "the test adapter takes skills, prompts and MCP servers".into(),
                }),
            }
        }
        out.keys.push(KeyWrite {
            root,
            file: self.path("settings.jsonc")?,
            format: Format::Jsonc,
            key: vec![
                "sekisho".into(),
                "environments".into(),
                d.environment.to_string(),
            ],
            value: json!(d.revision),
            origin: Origin::Environment,
        });
        if ctx.models {
            let model = d
                .defaults
                .anthropic
                .as_ref()
                .map(|m| m.to_string())
                .or_else(|| d.models.first().map(|m| m.name.to_string()))
                .unwrap_or_default();
            for (key, value) in [("model", model), ("base_url", d.gateway_url.clone())] {
                out.keys.push(KeyWrite {
                    root,
                    file: self.path("config.toml")?,
                    format: Format::Toml,
                    key: vec![key.into()],
                    value: json!(value),
                    origin: Origin::Models,
                });
            }
            out.run = Some(RunSpec {
                program: self.tool.as_str().to_owned(),
                args: Vec::new(),
                env: [("BASE".to_owned(), d.gateway_url.clone())].into(),
                ..RunSpec::default()
            });
        }
        Ok(out)
    }

    fn unmanaged(
        &self,
        _scope: &Scope,
        _locks: &[&crate::install::lock::Lock],
    ) -> Vec<UnmanagedItem> {
        Vec::new()
    }

    fn layering(&self) -> Layering {
        let both = |wins| Layer {
            both_load: true,
            same_name_wins: wins,
        };
        Layering {
            models: Layer {
                both_load: false,
                same_name_wins: Some(ScopeKind::Project),
            },
            mcp_servers: both(Some(ScopeKind::Project)),
            skills: both(Some(ScopeKind::Global)),
            prompts: both(Some(ScopeKind::Project)),
            plugins: both(None),
            instructions: both(None),
        }
    }
}
