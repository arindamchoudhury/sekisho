//! Local MCP servers on machines (plan 4, Task 5): how a coding tool starts
//! each one. A catalog entry's package is already one exact version (D102,
//! D188); here its runner is also held to the entry's applied time where
//! the runner takes an absolute cutoff (D200), points at the install's
//! package registries (D199), runs a container by digest only, and takes
//! configuration values from the person's own environment, so no secret is
//! ever written.
//!
//! The cutoffs, checked on 2026-11-21: npm 12.2.2's `before` (also read by
//! `npm exec` and `npx`, which pass npm's options to pacote and Arborist);
//! uv 0.13.7's `--exclude-newer` on `uv tool run` and `uvx` (RFC 3339);
//! pip 26.0's `--uploaded-prior-to`, read by `pipx run` from
//! `PIP_UPLOADED_PRIOR_TO`, and uv's `UV_EXCLUDE_NEWER` for pipx 1.18's uv
//! backend, which refuses other pip options. `bunx`, `pnpm dlx` and
//! `yarn dlx` take only relative ages.

use sekisho_catalog::delivery::{Delivery, EntryAddress, Registries};
use sekisho_catalog::launch::{Launch, parse_stdio};
use sekisho_catalog::model::{ConfigField, ConfigTarget, EntrySpec, McpRuntime};
use sekisho_catalog::pins::ImageRef;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// An environment variable's value in a tool's MCP config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvValue {
    Literal(String),
    /// A configuration field the person sets in their own environment,
    /// written in each tool's reference syntax (`${VAR}`, `{env:VAR}`).
    FromUser {
        name: String,
        required: bool,
    },
}

/// One local MCP server, as a coding tool starts it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalServer {
    pub name: String,
    pub entry: EntryAddress,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, EnvValue)>,
    pub applied_at: OffsetDateTime,
}

/// A plugin server's own launch with its runner's cutoff, for the tools
/// that start plugins' servers from their translated config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pinned {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The runner, when it has no absolute cutoff: the package is exact,
    /// its dependencies aren't held to the applied time.
    pub no_cutoff: Option<&'static str>,
}

/// The cutoff as each runner reads it: RFC 3339 in UTC, to the second.
fn cutoff(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
        .expect("0 is a nanosecond")
        .format(&Rfc3339)
        .expect("times format as RFC 3339")
}

fn npm_registry(r: &Registries) -> Vec<(String, String)> {
    r.npm
        .iter()
        .map(|u| ("NPM_CONFIG_REGISTRY".to_owned(), u.clone()))
        .collect()
}

fn python_registry(r: &Registries) -> Vec<(String, String)> {
    r.python
        .iter()
        .flat_map(|u| {
            [
                ("UV_DEFAULT_INDEX".to_owned(), u.clone()),
                ("PIP_INDEX_URL".to_owned(), u.clone()),
            ]
        })
        .collect()
}

/// A configuration field's variable, if a local server takes it: headers
/// are for remote servers. A sensitive value is never written.
fn config_env(f: &ConfigField) -> Option<(String, EnvValue)> {
    if f.target == ConfigTarget::Header {
        return None;
    }
    let value = match (&f.value, f.sensitive) {
        (Some(v), false) => EnvValue::Literal(format!("{}{v}", f.prefix.as_deref().unwrap_or(""))),
        _ => EnvValue::FromUser {
            name: f.key.clone(),
            required: f.required,
        },
    };
    Some((f.key.clone(), value))
}

/// The image by digest, Docker Hub's as `docker.io/…` so a machine's
/// registry mirror applies; `None` without a digest.
fn by_digest(image: &ImageRef) -> Option<String> {
    let digest = image.digest()?;
    let registry = match image.registry() {
        "registry-1.docker.io" => "docker.io",
        other => other,
    };
    Some(format!("{registry}/{}@{digest}", image.repository()))
}

/// Every local MCP server the environment delivers, and why any isn't
/// started. Remote servers reach tools through the environment's MCP
/// endpoint (plan 5), and plugins' servers through their adapters.
pub fn local_servers(d: &Delivery) -> (Vec<LocalServer>, Vec<String>) {
    let mut servers = Vec::new();
    let mut warnings = Vec::new();
    for e in &d.entries {
        let EntrySpec::McpServer(spec) = &e.spec else {
            continue;
        };
        let at = cutoff(e.applied_at);
        let mut env: Vec<(String, EnvValue)> = spec.config.iter().filter_map(config_env).collect();
        let literal = |pairs: Vec<(String, String)>| {
            pairs
                .into_iter()
                .map(|(k, v)| (k, EnvValue::Literal(v)))
                .collect::<Vec<_>>()
        };
        let (command, args) = match &spec.runtime {
            McpRuntime::Remote { .. } => continue,
            McpRuntime::Npx { package, args } => {
                env.extend(literal(npm_registry(&d.registries)));
                let mut a = vec![
                    "--yes".to_owned(),
                    format!("--before={at}"),
                    package.to_string(),
                ];
                a.extend(args.iter().cloned());
                ("npx", a)
            }
            McpRuntime::Uvx {
                package,
                command,
                args,
            } => {
                env.extend(literal(python_registry(&d.registries)));
                let mut a = vec!["--exclude-newer".to_owned(), at];
                match command {
                    Some(c) => a.extend(["--from".to_owned(), package.to_string(), c.clone()]),
                    None => a.push(package.to_string()),
                }
                a.extend(args.iter().cloned());
                ("uvx", a)
            }
            McpRuntime::Container {
                image,
                command,
                args,
                port,
                ..
            } => {
                if let Some(port) = port {
                    warnings.push(format!(
                        "MCP server {}@{} serves on port {port}, which a coding tool can't start; \
                         it isn't installed",
                        e.name, e.source
                    ));
                    continue;
                }
                let Some(image) = by_digest(image) else {
                    warnings.push(format!(
                        "MCP server {}@{}: its image has no digest, so it isn't installed",
                        e.name, e.source
                    ));
                    continue;
                };
                let mut a: Vec<String> = ["run", "--rm", "-i"].map(str::to_owned).to_vec();
                for (key, _) in &env {
                    a.extend(["-e".to_owned(), key.clone()]);
                }
                a.push(image);
                a.extend(command.iter().cloned());
                a.extend(args.iter().cloned());
                ("docker", a)
            }
        };
        servers.push(LocalServer {
            name: e.name.to_string(),
            entry: EntryAddress {
                kind: e.kind,
                source: e.source.clone(),
                name: e.name.clone(),
            },
            command: command.to_owned(),
            args,
            env,
            applied_at: e.applied_at,
        });
    }
    (servers, warnings)
}

/// A runner `parse_stdio` names, as a static name.
fn runner_name(runner: &str) -> &'static str {
    match runner {
        "bunx" => "bunx",
        "pnpm dlx" => "pnpm dlx",
        "yarn dlx" => "yarn dlx",
        "npm exec" => "npm exec",
        "uv tool run" => "uv tool run",
        "pipx run" => "pipx run",
        _ => "a package runner",
    }
}

/// Inserts `extra` after the first argument that is `after`.
fn insert_after(args: &[String], after: &[&str], extra: &[String]) -> Vec<String> {
    let mut out = args.to_vec();
    if let Some(i) = args.iter().position(|a| after.contains(&a.as_str())) {
        for (j, x) in extra.iter().enumerate() {
            out.insert(i + 1 + j, x.clone());
        }
    }
    out
}

/// A plugin server's own `command` and `args` with its runner's cutoff and
/// the registries; an error for anything but a package runner.
pub fn pinned(
    command: &str,
    args: &[String],
    applied_at: OffsetDateTime,
    registries: &Registries,
) -> Result<Pinned, String> {
    let at = cutoff(applied_at);
    let launch = parse_stdio(command, args)?;
    let (args, env, no_cutoff) = match &launch {
        Launch::Npx(_) => {
            let mut a = vec![format!("--before={at}")];
            a.extend(args.iter().cloned());
            (a, npm_registry(registries), None)
        }
        Launch::NpmRunner { runner, .. } if runner == "npm exec" => (
            insert_after(args, &["exec", "x"], &[format!("--before={at}")]),
            npm_registry(registries),
            None,
        ),
        Launch::NpmRunner { runner, .. } => (
            args.to_vec(),
            npm_registry(registries),
            Some(runner_name(runner)),
        ),
        Launch::Uvx(_) => {
            let mut a = vec!["--exclude-newer".to_owned(), at];
            a.extend(args.iter().cloned());
            (a, python_registry(registries), None)
        }
        Launch::PythonRunner { runner, .. } if runner == "uv tool run" => (
            insert_after(args, &["run"], &["--exclude-newer".to_owned(), at]),
            python_registry(registries),
            None,
        ),
        Launch::PythonRunner { .. } => {
            let mut env = vec![
                ("UV_EXCLUDE_NEWER".to_owned(), at.clone()),
                ("PIP_UPLOADED_PRIOR_TO".to_owned(), at),
            ];
            env.extend(python_registry(registries));
            (args.to_vec(), env, None)
        }
        Launch::Docker { .. } | Launch::Remote { .. } | Launch::Other { .. } => {
            return Err(format!("{command} isn't a package runner"));
        }
    };
    Ok(Pinned {
        command: command.to_owned(),
        args,
        env,
        no_cutoff,
    })
}

/// One warning naming each runner that has no absolute cutoff, once.
pub fn no_cutoff_warning(runners: &[&str]) -> Option<String> {
    let mut seen: Vec<&str> = Vec::new();
    for r in runners {
        if !seen.contains(r) {
            seen.push(r);
        }
    }
    if seen.is_empty() {
        return None;
    }
    Some(format!(
        "{} start packages at their exact version, but can't hold their dependencies to the \
         entry's applied time: they have no absolute cutoff",
        seen.join(", ")
    ))
}
