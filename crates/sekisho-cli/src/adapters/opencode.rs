//! OpenCode, from its source and docs: V1 at `v1.19.6` (npm's
//! `opencode-ai`) and V2 at `8815bc9` (2.4), which reads V1's config
//! format from the same files. Kilo CLI, a fork of it, shares this code
//! with its own names ([`Flavor`]).
//!
//! A project install writes `.opencode/opencode.json`, which overrides
//! the repository's own `opencode.json`: a provider per format that has a
//! default (D208), `sekisho-<env>-anthropic` (`@ai-sdk/anthropic`),
//! `-openai` (`@ai-sdk/openai-compatible`, Chat Completions) and
//! `-gemini` (`@ai-sdk/google`), each at `{env:SEKISHO_RUN_URL}` with the
//! key `{env:SEKISHO_RUN_KEY}` and the `x-sekisho-tool` header, so OpenCode
//! starts through `sekisho run opencode` (D194); `model` and
//! `small_model`; `mcp` servers, which inherit OpenCode's environment;
//! commands in `.opencode/commands/`; instruction files under
//! `.opencode/sekisho/<env>/`, each an absolute `instructions` entry (a
//! relative one is searched only up to the Git root). Skills go in the
//! shared `.agents/skills/`. A global install writes the same under
//! `~/.config/opencode/`.
//!
//! Kilo CLI resolves `{env:}` only in the user's global config and loads a
//! project file holding one as empty, so its providers always go in the
//! global `kilo.json`, and a project's `.kilo/kilo.json` names its model by
//! their ID (D192).

use serde_json::{Map, Value, json};

use sekisho_catalog::delivery::Delivery;
use sekisho_core::formats::ClientApi;

use super::translate::{self, Commands, Layout, RemoteKey, Servers, UserVars};
use super::{
    Adapter, AdapterError, FileWrite, Format, KeyWrite, Layer, Layering, Origin, RelPath,
    RenderContext, Rendered, Root, RunSpec, ScopeKind, Skipped, Tool, UnmanagedItem, on_path,
};
use crate::install::Scope;
use crate::install::instructions;
use crate::install::merge::item_key;

/// What tells OpenCode and Kilo CLI apart here.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Flavor {
    pub tool: Tool,
    pub program: &'static str,
    /// The project's config folder, `.opencode` or `.kilo`.
    pub dir: &'static str,
    /// The same with a slash, where its files go.
    pub base: &'static str,
    /// The project's config file, in that folder.
    pub project_file: &'static str,
    /// The config file's name, `opencode.json` or `kilo.json`.
    pub file: &'static str,
    /// The tool reads `{env:}` only in its global config.
    pub global_providers: bool,
}

pub(crate) const OPENCODE: Flavor = Flavor {
    tool: Tool::OpenCode,
    program: "opencode",
    dir: ".opencode",
    base: ".opencode/",
    project_file: ".opencode/opencode.json",
    file: "opencode.json",
    global_providers: false,
};

/// One provider format: its name's suffix, its AI SDK package, the gateway
/// path its base URL ends with, and the API its models must reach.
struct ProviderFormat {
    suffix: &'static str,
    npm: &'static str,
    path: &'static str,
    api: ClientApi,
}

const FORMATS: [ProviderFormat; 3] = [
    ProviderFormat {
        suffix: "anthropic",
        npm: "@ai-sdk/anthropic",
        path: "anthropic/v1",
        api: ClientApi::Messages,
    },
    ProviderFormat {
        suffix: "openai",
        npm: "@ai-sdk/openai-compatible",
        path: "openai/v1",
        api: ClientApi::ChatCompletions,
    },
    ProviderFormat {
        suffix: "gemini",
        npm: "@ai-sdk/google",
        path: "gemini/v1beta",
        api: ClientApi::Gemini,
    },
];

/// The default of each format, in order: anthropic, openai, gemini.
fn defaults(d: &Delivery) -> [Option<String>; 3] {
    [
        d.defaults.anthropic.as_ref().map(ToString::to_string),
        d.defaults.openai.as_ref().map(ToString::to_string),
        d.defaults.gemini.as_ref().map(ToString::to_string),
    ]
}

fn provider_id(d: &Delivery, f: &ProviderFormat) -> String {
    format!("sekisho-{}-{}", d.environment, f.suffix)
}

/// The project's config, or the global one.
fn config(flavor: Flavor, scope: &Scope) -> (Root, &'static str) {
    match scope.kind() {
        ScopeKind::Project => (Root::Project, flavor.project_file),
        ScopeKind::Global => (Root::ToolHome(flavor.tool), flavor.file),
    }
}

fn layout(flavor: Flavor, scope: &Scope) -> Layout {
    let project = scope.kind() == ScopeKind::Project;
    let (root, file) = config(flavor, scope);
    Layout {
        tool: flavor.tool,
        root,
        base: if project { flavor.base } else { "" },
        remote: RemoteKey::OpenCode,
        // Local servers get `{...process.env, ...environment}`, so a
        // variable of the server's own name reaches it as it is; a
        // reference is written only for another name, and never for Kilo
        // CLI, which drops a project file holding one.
        user_vars: if flavor.global_providers {
            UserVars::Reference(|_, _, _| None)
        } else {
            UserVars::Reference(|key, name, _| (key != name).then(|| format!("{{env:{name}}}")))
        },
        skills: if project {
            (Root::Project, ".agents/skills/")
        } else {
            (Root::UserHome, ".agents/skills/")
        },
        servers: Servers {
            root,
            file,
            format: Format::Jsonc,
            key: "mcp",
            opencode: true,
        },
        commands: Commands::Markdown {
            dir: "commands/",
            nested: true,
        },
    }
}

pub(crate) fn render(
    flavor: Flavor,
    d: &Delivery,
    ctx: &RenderContext<'_>,
) -> Result<Rendered, AdapterError> {
    let layout = layout(flavor, ctx.scope);
    let (root, file) = config(flavor, ctx.scope);
    let file = RelPath::parse(file)?;
    let env = d.environment.as_str();
    let mut out = Rendered::default();
    let key =
        |root: Root, file: &RelPath, key: Vec<String>, value: Value, origin: Origin| KeyWrite {
            root,
            file: file.clone(),
            format: Format::Jsonc,
            key,
            value,
            origin,
        };

    if ctx.models {
        let defaults = defaults(d);
        let mut chosen: Option<String> = None;
        let mut small: Option<String> = None;
        // Kilo CLI's providers go in its global file, whatever the scope.
        let (proot, pfile) = if flavor.global_providers {
            (Root::ToolHome(flavor.tool), RelPath::parse(flavor.file)?)
        } else {
            (root, file.clone())
        };
        for (f, default) in FORMATS.iter().zip(&defaults) {
            let Some(default) = default else { continue };
            let models: Map<String, Value> = d
                .models
                .iter()
                .filter(|m| m.apis.contains(&f.api))
                .map(|m| (m.name.to_string(), json!({})))
                .collect();
            if !models.contains_key(default) {
                continue;
            }
            let id = provider_id(d, f);
            if chosen.is_none() {
                chosen = Some(format!("{id}/{default}"));
            }
            if small.is_none()
                && let Some(s) = &d.defaults.small
                && models.contains_key(s.as_str())
            {
                small = Some(format!("{id}/{s}"));
            }
            out.keys.push(key(
                proot,
                &pfile,
                vec!["provider".into(), id],
                json!({
                    "name": format!("Sekisho {env} ({})", f.suffix),
                    "npm": f.npm,
                    "options": {
                        "baseURL": format!("{{env:SEKISHO_RUN_URL}}/env/{env}/{}", f.path),
                        "apiKey": "{env:SEKISHO_RUN_KEY}",
                        "headers": { (sekisho_core::TOOL_HEADER): flavor.tool.as_str() },
                    },
                    "models": models,
                }),
                Origin::Models,
            ));
        }
        match chosen {
            None => out.skipped.push(Skipped {
                what: "models".into(),
                why: "the environment has no default model its APIs reach".into(),
            }),
            Some(model) => {
                out.keys.push(key(
                    root,
                    &file,
                    vec!["model".into()],
                    json!(model),
                    Origin::Models,
                ));
                if let Some(small) = small {
                    out.keys.push(key(
                        root,
                        &file,
                        vec!["small_model".into()],
                        json!(small),
                        Origin::Models,
                    ));
                }
                out.run = Some(RunSpec {
                    program: flavor.program.into(),
                    ..RunSpec::default()
                });
            }
        }
    }

    // Instruction files of Sekisho's own, each named by its absolute path
    // in `instructions`, beside the person's own entries (D222). Neither
    // tool loads a file for some paths only.
    let (list, skipped) = instructions::collect(d, ctx)?;
    out.skipped.extend(skipped);
    let base = ctx
        .scope
        .root(root)
        .ok_or_else(|| AdapterError("no folder for the tool's config".into()))?;
    for i in &list {
        if i.paths.is_some() {
            out.skipped.push(Skipped {
                what: i.label.clone(),
                why: format!(
                    "{} loads no instructions for some files only",
                    flavor.tool.title()
                ),
            });
            continue;
        }
        out.always_loaded += i.text.len() as u64;
        let path = RelPath::parse(&format!("{}sekisho/{env}/{}.md", layout.base, i.stem))?;
        // In the system's own separators, as the tool compares them.
        let mut absolute = base.clone();
        absolute.extend(path.as_str().split('/'));
        let absolute = absolute.to_string_lossy().into_owned();
        out.files.push(FileWrite {
            root,
            path,
            content: i.text.clone().into_bytes(),
            executable: false,
            origin: i.origin.clone(),
        });
        out.keys.push(key(
            root,
            &file,
            item_key(&["instructions"], &absolute),
            json!(absolute),
            i.origin.clone(),
        ));
    }

    translate::content(d, ctx, &layout, &mut out)?;
    Ok(out)
}

/// Global and project config files merge, the project's winning a key
/// both set; `instructions` entries of every file are combined.
pub(crate) fn layering() -> Layering {
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
        skills: both(Some(ScopeKind::Project)),
        prompts: both(Some(ScopeKind::Project)),
        plugins: both(Some(ScopeKind::Project)),
        instructions: both(None),
    }
}

pub(crate) fn run_spec(flavor: Flavor, d: &Delivery) -> Option<RunSpec> {
    FORMATS.iter().zip(defaults(d)).find_map(|(f, default)| {
        let default = default?;
        d.models
            .iter()
            .any(|m| m.name.as_str() == default && m.apis.contains(&f.api))
            .then(|| RunSpec {
                program: flavor.program.into(),
                ..RunSpec::default()
            })
    })
}

pub(crate) fn detect(flavor: Flavor, scope: &Scope) -> bool {
    let folder = match scope.kind() {
        ScopeKind::Project => scope.root(Root::Project).map(|p| p.join(flavor.dir)),
        ScopeKind::Global => scope.root(Root::ToolHome(flavor.tool)),
    };
    folder.is_some_and(|f| f.is_dir()) || on_path(flavor.program)
}

/// OpenCode.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenCode;

impl Adapter for OpenCode {
    fn tool(&self) -> Tool {
        Tool::OpenCode
    }

    fn detect(&self, scope: &Scope) -> bool {
        detect(OPENCODE, scope)
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        render(OPENCODE, d, ctx)
    }

    fn unmanaged(
        &self,
        _scope: &Scope,
        _locks: &[&crate::install::lock::Lock],
    ) -> Vec<UnmanagedItem> {
        // Task 10 (`sekisho status`) reads the tool's config for these.
        Vec::new()
    }

    fn layering(&self) -> Layering {
        layering()
    }

    fn run_spec(&self, d: &Delivery) -> Option<RunSpec> {
        run_spec(OPENCODE, d)
    }
}
