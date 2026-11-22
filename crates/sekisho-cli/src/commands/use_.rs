//! `sekisho use <env>` and `sekisho uninstall [<env>…]`: an environment's
//! models, MCP servers, skills, plugins and prompts written into each
//! coding tool's config, for one project folder or globally (D190–D216),
//! and taken out again.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use sekisho_catalog::delivery::Delivery;
use sekisho_core::names::EnvName;

use super::Context;
use crate::adapters::{self, Adapter, ScopeKind, Tool};
use crate::install::lock::Lock;
use crate::install::{self, InstallError, Options, Report, Scope, Sign};
use crate::output::{CliError, warn};

/// `use`'s options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UseArgs {
    pub env: String,
    /// The tools to install for; the detected ones when empty.
    pub targets: Vec<Tool>,
    pub models: bool,
    pub global: bool,
    pub dir: Option<PathBuf>,
    pub dry_run: bool,
    pub yes: bool,
    pub force: bool,
}

/// `uninstall`'s options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UninstallArgs {
    /// Every one installed when empty.
    pub envs: Vec<String>,
    pub targets: Vec<Tool>,
    pub global: bool,
    pub dir: Option<PathBuf>,
    pub dry_run: bool,
    pub yes: bool,
    pub force: bool,
}

/// The tools to install for: those named, or those found here.
fn chosen(targets: &[Tool], scope: &Scope) -> Result<Vec<&'static dyn Adapter>, CliError> {
    let all = adapters::all();
    let supported = || {
        all.iter()
            .map(|a| a.tool().as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if targets.is_empty() {
        let found: Vec<&'static dyn Adapter> =
            all.iter().copied().filter(|a| a.detect(scope)).collect();
        if found.is_empty() {
            return Err(CliError::usage("no coding tool found here")
                .with_hint(format!("name one with --target ({})", supported())));
        }
        return Ok(found);
    }
    targets
        .iter()
        .map(|t| {
            all.iter().copied().find(|a| a.tool() == *t).ok_or_else(|| {
                CliError::usage(format!("sekisho can't install for {t} yet"))
                    .with_hint(format!("--target takes {}", supported()))
            })
        })
        .collect()
}

fn env_name(name: &str) -> Result<EnvName, CliError> {
    EnvName::parse(name).map_err(|e| CliError::usage(e.to_string()))
}

fn models_of(lock: &Lock) -> Vec<(Tool, EnvName)> {
    lock.models
        .iter()
        .filter_map(|(t, e)| Some((Tool::parse(t)?, EnvName::parse(e).ok()?)))
        .collect()
}

/// The `+ ~ - !` list (D203), with each update's diff when asked.
fn list(report: &Report, diffs: bool) -> String {
    let mut out = String::new();
    for item in &report.items {
        let _ = writeln!(out, "{} {}: {}", item.sign.mark(), item.tool, item.what);
        if diffs
            && item.sign == Sign::Updated
            && let Some(diff) = &item.diff
        {
            for line in diff.lines() {
                let _ = writeln!(out, "    {line}");
            }
        }
    }
    out
}

/// What `use` says after its list: each tool's model environment, when it
/// changed or the folder holds several (D216), and what a session here also
/// gets from the global install, with whose copy wins a shared name (D214).
pub fn use_notices(
    report: &Report,
    models_before: &[(Tool, EnvName)],
    environments: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    for (tool, env) in &report.models {
        if environments > 1 || !models_before.contains(&(*tool, env.clone())) {
            out.push(format!("{} takes its models from \"{env}\".", tool.title()));
        }
    }
    let mut groups: BTreeMap<(Tool, String), Vec<String>> = BTreeMap::new();
    for l in &report.layered {
        let what = format!("{} \"{}\"", l.kind.word(), l.name);
        let item = match (l.here, l.used) {
            (true, ScopeKind::Global) => {
                format!("{what} (the session uses \"{}\"'s)", l.other_env)
            }
            (true, ScopeKind::Project) => format!("{what} (the session uses this folder's)"),
            (false, _) => what,
        };
        groups
            .entry((l.tool, l.other_env.to_string()))
            .or_default()
            .push(item);
    }
    for ((tool, env), items) in groups {
        out.push(format!(
            "{} also loads the global environment \"{env}\": {}.",
            tool.title(),
            items.join(", ")
        ));
    }
    out
}

/// Every file the delivery document lists, by SHA-256, from the server.
async fn fetch_files(
    ctx: &Context<'_>,
    d: &Delivery,
) -> Result<BTreeMap<String, Vec<u8>>, CliError> {
    let (server, credential) = ctx.caller().await?;
    let wanted: BTreeSet<&str> = d
        .entries
        .iter()
        .flat_map(|e| e.files.iter().map(|f| f.sha256.as_str()))
        .collect();
    let mut blobs = BTreeMap::new();
    for sha in wanted {
        let bytes = server
            .get_bytes(
                &format!("/api/v1/me/environments/{}/blobs/{sha}", d.environment),
                Some(&credential),
            )
            .await?;
        blobs.insert(sha.to_owned(), bytes);
    }
    Ok(blobs)
}

/// `sekisho use <env>`.
pub async fn use_(
    ctx: &Context<'_>,
    args: &UseArgs,
    ask: &dyn Fn(&[String]) -> bool,
) -> Result<String, CliError> {
    let env = env_name(&args.env)?;
    let scope = ctx.scope(args.global, args.dir.as_deref());
    let adapters = chosen(&args.targets, &scope)?;
    let (server, credential) = ctx.caller().await?;
    let d: Delivery = server
        .get_json(
            &format!("/api/v1/me/environments/{env}/delivery"),
            Some(&credential),
        )
        .await?;
    if d.compute_revision() != d.revision || d.environment != env {
        return Err(CliError::new(
            crate::output::Exit::Refused,
            "the server's delivery document doesn't match its revision",
        ));
    }
    let blobs = fetch_files(ctx, &d).await?;
    let fetch = |sha: &str| -> Result<Vec<u8>, InstallError> {
        blobs
            .get(sha)
            .cloned()
            .ok_or_else(|| InstallError::Io(format!("the server sent no file {sha}")))
    };
    let before = Lock::read(&scope.lock_path())?;
    let mut options = Options {
        dry_run: args.dry_run,
        yes: args.yes,
        force: args.force,
        models: args.models,
        sekisho: Some(ctx.sekisho.clone()),
    };
    let report = match install::install(&d, &adapters, &scope, &options, &fetch) {
        Err(InstallError::NeedsConsent(items)) if !args.dry_run && ask(&items) => {
            options.yes = true;
            install::install(&d, &adapters, &scope, &options, &fetch)?
        }
        other => other?,
    };
    for w in &report.warnings {
        warn(w);
    }
    let environments = if args.dry_run {
        before.environments.len() + usize::from(!before.environments.contains_key(env.as_str()))
    } else {
        Lock::read(&scope.lock_path())?.environments.len()
    };
    let mut out = list(&report, args.dry_run);
    if report.items.is_empty() {
        let _ = writeln!(out, "Nothing to change: {env} is already installed here.");
    }
    if !d.notice.is_empty() {
        let _ = writeln!(out, "{env}: {}", d.notice);
    }
    for notice in use_notices(&report, &models_of(&before), environments) {
        let _ = writeln!(out, "{notice}");
    }
    if !args.dry_run {
        for a in &adapters {
            if let Some(note) = a.note(&scope) {
                let _ = writeln!(out, "{note}");
            }
            if a.run_spec(&d).is_some() {
                let tool = a.tool();
                let _ = writeln!(
                    out,
                    "Start {} with `sekisho run {tool}`: it takes its key from the run.",
                    tool.title()
                );
            }
        }
    }
    Ok(out)
}

/// `sekisho uninstall [<env>…]`.
pub async fn uninstall(ctx: &Context<'_>, args: &UninstallArgs) -> Result<String, CliError> {
    let scope = ctx.scope(args.global, args.dir.as_deref());
    let lock = Lock::read(&scope.lock_path())?;
    let envs: Vec<EnvName> = args
        .envs
        .iter()
        .map(|e| env_name(e))
        .collect::<Result<_, _>>()?;
    if let Some(missing) = envs
        .iter()
        .find(|e| !lock.environments.contains_key(e.as_str()))
    {
        return Err(CliError::error(format!("{missing} isn't installed here")));
    }
    let options = Options {
        dry_run: args.dry_run,
        yes: args.yes,
        force: args.force,
        ..Options::default()
    };
    let report = install::uninstall(
        &scope,
        (!envs.is_empty()).then_some(envs.as_slice()),
        (!args.targets.is_empty()).then_some(args.targets.as_slice()),
        &options,
    )?;
    for w in &report.warnings {
        warn(w);
    }
    let mut out = list(&report, args.dry_run);
    if report.items.is_empty() {
        out.push_str("Nothing to remove.\n");
    }
    for (tool, env) in &report.models {
        let _ = writeln!(out, "{} takes its models from \"{env}\".", tool.title());
    }
    Ok(out)
}
