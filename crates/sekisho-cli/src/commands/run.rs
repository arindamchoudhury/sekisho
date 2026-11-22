//! `sekisho run <tool> [-- <args>…]` (D191, D194), as the binary runs it:
//! the run's credential, the model environment's delivery document, the
//! proxy and the tool. See [`crate::run`].

use std::sync::Arc;

use sekisho_catalog::delivery::Delivery;

use super::home_dir;
use crate::adapters::{self, Tool};
use crate::config::{Config, Dirs, Vars};
use crate::credentials::{MemoryStore, Store, open_store};
use crate::install::{Scope, ToolHomes};
use crate::login::{self, Method, TerminalUi, WAIT};
use crate::output::{CliError, Exit, note};
use crate::run::{self, TokenFn};
use crate::server::Server;
use crate::token::{self, Sources};

/// Runs `tool` with `args`: the tool's exit code.
pub async fn run_tool(tool: Tool, args: &[String]) -> Result<i32, CliError> {
    let vars = Vars::from_process();
    let dirs = Dirs::from_vars(&vars)?;
    let folder = std::env::current_dir()
        .map_err(|e| CliError::error(format!("can't read the current folder: {e}")))?;
    let home = home_dir()?;
    let homes = ToolHomes::from_env(&|k| vars.get(k).map(str::to_owned), &home);
    let scope = Scope::project(&folder, &dirs.config, homes);
    let adapter = adapters::all()
        .into_iter()
        .find(|a| a.tool() == tool)
        .ok_or_else(|| CliError::usage(format!("sekisho can't start {tool} yet")))?;
    let plan = run::plan(tool, &scope)?;

    let server = Server::new(Config::server(&vars, &dirs)?)?;
    let sources = Sources::from_vars(&vars);
    let opened = open_store();
    // With no keyring, the run signs in for its own lifetime and keeps the
    // tokens in memory (D195); the device flow, since a machine without a
    // keyring rarely has a browser either.
    let store: Arc<dyn Store> =
        if opened.no_keyring && sources.key.is_none() && sources.run.is_none() {
            let memory = Arc::new(MemoryStore::default());
            note("No keyring here: signing in for this run only.");
            let ui = TerminalUi {
                open_browser: false,
            };
            login::login(&server, Method::Device, &ui, memory.as_ref(), &dirs, WAIT).await?;
            memory
        } else {
            Arc::from(opened.store)
        };
    let lock = dirs.refresh_lock();
    let tokens: TokenFn = {
        let (server, store, sources, lock) = (server.clone(), store, sources, lock);
        Arc::new(move || {
            let (server, store, sources, lock) = (
                server.clone(),
                Arc::clone(&store),
                sources.clone(),
                lock.clone(),
            );
            Box::pin(async move {
                let credential = token::current(&server, store.as_ref(), &sources, &lock).await?;
                Ok(credential.secret().to_owned())
            })
        })
    };

    // What runs and where tokens go come from the server, never the lock.
    // (An environment key's secret is sent the same way as a token.)
    let credential = crate::credentials::Credential::Session(tokens().await?);
    let d: Delivery = server
        .get_json(
            &format!("/api/v1/me/environments/{}/delivery", plan.env),
            Some(&credential),
        )
        .await?;
    if d.compute_revision() != d.revision || d.environment.as_str() != plan.env {
        return Err(CliError::new(
            Exit::Refused,
            "the server's delivery document doesn't match its revision",
        ));
    }
    let spec = adapter.run_spec(&d).ok_or_else(|| {
        CliError::error(format!(
            "{} takes its models from \"{}\", which has none it can use",
            tool.title(),
            plan.env
        ))
        .with_hint(format!(
            "ask an admin to add one, or run `sekisho use <env> --models --target {tool}` with another environment"
        ))
    })?;
    let gateways = run::gateways(&d, &plan.envs)?;
    let proxy = run::start(gateways, tokens).await?;
    let code = run::run_child(&spec, args, &proxy).await;
    proxy.stop().await;
    code
}
