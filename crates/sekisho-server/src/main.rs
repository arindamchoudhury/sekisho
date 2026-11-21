//! The `sekisho` binary.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::serve::ListenerExt;
use clap::{Parser, Subcommand};
use sekisho_auth::dex::{DexClient, DexConfig};
use sekisho_cli::commands::{self, KeyAction, run_here};
use sekisho_core::keys::hash_secret;
use sekisho_gateway::agent::{AgentConfig, Url, run_agent};
use sekisho_hooks::authz::{self, HooksState};
use sekisho_hooks::cache::AuthCache;
use sekisho_hooks::drain::Drain;
use sekisho_hooks::extproc::StreamGuard;
use sekisho_hooks::ratelimit::{Charger, TALLY_WAIT};
use sekisho_hooks::tally::Tallies;
use sekisho_proto::ext_proc::external_processor_server::ExternalProcessorServer;
use sekisho_proto::ratelimit::rate_limit_service_server::RateLimitServiceServer;
use sekisho_server::app::{
    AppState, IDP_SWEEP_EVERY, KEY_ROTATION_CHECK, register_with_dex, router, start_publisher,
    start_sign_in_upkeep,
};
use sekisho_server::catalog::outbound::install_crypto;
use sekisho_server::catalog::sync::start_scheduler;
use sekisho_server::config::ServerConfig;
use sekisho_store::{Store, StoreOptions};
use time::OffsetDateTime;
use tokio::net::TcpStream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;

#[derive(Parser)]
#[command(name = "sekisho", version, about = "Control plane for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the admin API and agentgateway's hooks, and publish its configuration.
    Serve,
    /// Apply database migrations (needs SEKISHO_DATABASE_URL).
    Migrate,
    /// Print a one-time setup link that makes a local admin, for an operator
    /// who has lost every admin (needs SEKISHO_DATABASE_URL and
    /// SEKISHO_PUBLIC_URL; asks nothing of Dex).
    AdminLink {
        /// The local account's email; the account is made if there is none.
        #[arg(long)]
        email: String,
    },
    /// Run next to agentgateway and keep its config file current (needs
    /// SEKISHO_GATEWAY_TOKEN).
    GatewayAgent {
        /// Sekisho's admin listener, such as http://sekisho:8080/.
        #[arg(long)]
        server: Url,
        /// The file agentgateway reads its config from.
        #[arg(long)]
        config_path: PathBuf,
    },
    /// Sign in to a Sekisho server from this machine; the tokens go to the
    /// OS keyring.
    Login {
        /// Sekisho's URL, such as https://sekisho.example.com (default: the
        /// server already set, or SEKISHO_URL).
        server: Option<String>,
        /// Sign in with a code typed into a browser anywhere, for a machine
        /// without one.
        #[arg(long)]
        device: bool,
        /// Print the sign-in link without opening a browser.
        #[arg(long, conflicts_with = "device")]
        no_browser: bool,
    },
    /// End this machine's session and forget its tokens.
    Logout,
    /// Print a fresh token on stdout, for coding tools' credential helpers.
    Token,
    /// The environments you may use.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Your environment keys, for CI and machines that can't sign in.
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
    },
    /// Install an environment into this folder's coding tools (or, with
    /// --global, your own), beside any already there.
    Use {
        /// The environment.
        env: String,
        #[command(flatten)]
        place: Place,
        /// Make it the one these tools take their models from.
        #[arg(long)]
        models: bool,
    },
    /// Remove environments from this folder (or, with --global, your own);
    /// every one when none is named.
    Uninstall {
        /// The environments.
        envs: Vec<String>,
        #[command(flatten)]
        place: Place,
    },
}

/// Where `use` and `uninstall` act, and how.
#[derive(clap::Args)]
struct Place {
    /// The tools, such as claude-code (default: those found here).
    #[arg(long = "target", value_delimiter = ',', value_parser = parse_tool)]
    targets: Vec<sekisho_cli::adapters::Tool>,
    /// Your own config, for every folder, instead of this folder's.
    #[arg(long)]
    global: bool,
    /// The project folder (default: the current one).
    #[arg(long, conflicts_with = "global")]
    dir: Option<PathBuf>,
    /// Show the changes, and each update's diff, without writing.
    #[arg(long)]
    dry_run: bool,
    /// Overwrite your own files and settings in the way without asking.
    #[arg(long)]
    yes: bool,
    /// Replace or remove files and settings edited since Sekisho wrote them.
    #[arg(long)]
    force: bool,
}

fn parse_tool(s: &str) -> Result<sekisho_cli::adapters::Tool, String> {
    sekisho_cli::adapters::Tool::parse(s).ok_or_else(|| {
        let all: Vec<&str> = sekisho_cli::adapters::Tool::ALL
            .iter()
            .map(|t| t.as_str())
            .collect();
        format!("{s} isn't a coding tool Sekisho knows: {}", all.join(", "))
    })
}

/// Asks on the terminal whether to overwrite the person's own files; never
/// without one, which `use` reports as refused (exit 3).
fn ask_terminal(items: &[String]) -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return false;
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "These are yours, and would be overwritten:");
    for item in items {
        let _ = writeln!(err, "  {item}");
    }
    let _ = write!(err, "Overwrite them? [y/N] ");
    let _ = err.flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
}

#[derive(Subcommand)]
enum EnvCommand {
    /// List them; `*` marks those installed in this folder.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum KeysCommand {
    /// List your keys (never their tokens).
    List {
        /// Only this environment's.
        #[arg(long)]
        env: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Make a key and print its token, this once.
    Create {
        /// The environment it is for.
        env: String,
        /// Its name, unique among your keys in the environment.
        #[arg(long)]
        name: String,
        /// When it stops working: days (30d), a date (2027-03-01) or an RFC
        /// 3339 time.
        #[arg(long)]
        expires: Option<String>,
    },
    /// Pause a key until you resume it.
    Suspend(KeyTarget),
    /// Let a suspended key work again.
    Resume(KeyTarget),
    /// End a key for good.
    Revoke(KeyTarget),
}

#[derive(clap::Args)]
struct KeyTarget {
    /// The key's name or ID.
    key: String,
    /// The environment, when keys in several share the name.
    #[arg(long)]
    env: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // The developer commands: no log lines, since stdout carries only
    // results (`sekisho token` prints the token alone).
    let result = match cli.command {
        Command::Login {
            server,
            device,
            no_browser,
        } => sekisho_cli::commands::login(server, device, no_browser).await,
        Command::Logout => sekisho_cli::commands::logout().await,
        Command::Token => sekisho_cli::commands::token().await,
        Command::Env {
            command: EnvCommand::List { json },
        } => run_here(async |ctx| commands::env_list(ctx, json).await).await,
        Command::Keys { command } => keys(command).await,
        Command::Use { env, place, models } => {
            let args = commands::UseArgs {
                env,
                targets: place.targets,
                models,
                global: place.global,
                dir: place.dir,
                dry_run: place.dry_run,
                yes: place.yes,
                force: place.force,
            };
            run_here(async |ctx| commands::use_(ctx, &args, &ask_terminal).await).await
        }
        Command::Uninstall { envs, place } => {
            let args = commands::UninstallArgs {
                envs,
                targets: place.targets,
                global: place.global,
                dir: place.dir,
                dry_run: place.dry_run,
                yes: place.yes,
                force: place.force,
            };
            run_here(async |ctx| commands::uninstall(ctx, &args).await).await
        }
        service => return run_service(service).await,
    };
    sekisho_cli::output::finish(result)
}

async fn keys(command: KeysCommand) -> Result<(), sekisho_cli::output::CliError> {
    let act = |action: KeyAction, target: KeyTarget| {
        run_here(async move |ctx| {
            commands::keys_act(ctx, action, &target.key, target.env.as_deref()).await
        })
    };
    match command {
        KeysCommand::List { env, json } => {
            run_here(async |ctx| commands::keys_list(ctx, env.as_deref(), json).await).await
        }
        KeysCommand::Create { env, name, expires } => {
            run_here(async |ctx| commands::keys_create(ctx, &env, &name, expires.as_deref()).await)
                .await
        }
        KeysCommand::Suspend(target) => act(KeyAction::Suspend, target).await,
        KeysCommand::Resume(target) => act(KeyAction::Resume, target).await,
        KeysCommand::Revoke(target) => act(KeyAction::Revoke, target).await,
    }
}

async fn run_service(command: Command) -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    // rustls' crypto for what Sekisho fetches itself: ring, not aws-lc-rs
    // (D178).
    install_crypto();
    let result = match command {
        Command::Serve => serve().await,
        Command::Migrate => migrate().await,
        Command::AdminLink { email } => admin_link(&email).await,
        Command::GatewayAgent {
            server,
            config_path,
        } => gateway_agent(server, config_path).await,
        Command::Login { .. }
        | Command::Logout
        | Command::Token
        | Command::Env { .. }
        | Command::Keys { .. }
        | Command::Use { .. }
        | Command::Uninstall { .. } => unreachable!("run above"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    let config = ServerConfig::from_env()?;
    let store = Store::connect(
        &config.database_url,
        StoreOptions {
            max_connections: config.database_max_connections,
        },
    )
    .await
    .context("connecting to SEKISHO_DATABASE_URL")?;

    // A key signs from the start, before any replica answers a sign-in.
    store
        .rotate_signing_key(&config.secret_box, OffsetDateTime::now_utc(), false)
        .await
        .context("making the first signing key")?;
    // Session tokens name SEKISHO_PUBLIC_URL, without a trailing slash.
    let issuer = config
        .settings
        .public_url
        .as_str()
        .trim_end_matches('/')
        .to_owned();
    let cache = AuthCache::load(&store, issuer.clone())
        .await
        .context("loading keys for the hooks")?;
    cache.spawn_refresh(store.clone());
    let clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync> = Arc::new(OffsetDateTime::now_utc);
    let hooks = HooksState {
        cache: Arc::clone(&cache),
        store: store.clone(),
        clock: Arc::clone(&clock),
    };
    // What each stream relayed, for the estimate when one ends early: the
    // stream hook tallies, the rate-limit service reads.
    let tallies = Tallies::new();
    // Running streams and charges, which a stop waits for (D189).
    let drain = Drain::new();
    let charger = Charger {
        cache: Arc::clone(&cache),
        store: store.clone(),
        clock: Arc::clone(&clock),
        charge_secret_sha256: hash_secret(&config.settings.hooks.charge_secret),
        tallies: tallies.clone(),
        tally_wait: TALLY_WAIT,
        drain: drain.clone(),
    };
    let guard = StreamGuard {
        cache,
        store: store.clone(),
        clock,
        tallies,
        drain: drain.clone(),
    };
    if config.settings.render.bare_route {
        tracing::warn!(
            "SEKISHO_E2E_BARE_ROUTE is on: /bare/openai serves a provider with no key or \
             budget checks; never use this outside the end-to-end tests"
        );
    }
    // Sekisho's client at Dex: its secret is made once and kept sealed, so
    // Dex's config file holds none.
    let dex = DexClient::new(DexConfig {
        issuer: config.dex.issuer.clone(),
        http_url: config.dex.http_url.clone(),
        grpc: config.dex.grpc.clone(),
        client_id: "sekisho".to_owned(),
        client_secret: store
            .dex_client_secret(&config.secret_box)
            .await
            .context("reading Sekisho's client secret at Dex")?,
        redirect_uri: Url::parse(&format!("{issuer}/oauth/callback"))
            .context("SEKISHO_PUBLIC_URL with /oauth/callback is not a URL")?,
    });
    let state = AppState::new(
        store,
        config.secret_box,
        config.settings,
        Arc::clone(&guard.cache),
        Arc::new(dex),
    );
    start_publisher(&state)
        .await
        .context("starting the gateway config publisher")?;
    // Sign-ins answer 503 until Sekisho is registered at Dex.
    register_with_dex(&state, Duration::from_secs(5));
    start_sign_in_upkeep(&state, KEY_ROTATION_CHECK, IDP_SWEEP_EVERY);
    if state.catalog().allow_plain_http {
        tracing::warn!(
            "SEKISHO_E2E_PLAIN_HTTP is on: catalog sources, registries and agents may be \
             reached over plain HTTP"
        );
    }
    start_scheduler(state.clone(), state.catalog().sync_every);

    let admin_listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding SEKISHO_LISTEN {}", config.listen))?;
    let hooks_listener = tokio::net::TcpListener::bind(config.hooks_http_listen)
        .await
        .with_context(|| {
            format!(
                "binding SEKISHO_HOOKS_HTTP_LISTEN {}",
                config.hooks_http_listen
            )
        })?;
    let grpc_listener = tokio::net::TcpListener::bind(config.hooks_grpc_listen)
        .await
        .with_context(|| {
            format!(
                "binding SEKISHO_HOOKS_GRPC_LISTEN {}",
                config.hooks_grpc_listen
            )
        })?;
    tracing::info!(
        admin = %config.listen,
        hooks_http = %config.hooks_http_listen,
        hooks_grpc = %config.hooks_grpc_listen,
        "listening"
    );

    // One signal stops every server, and ends waiting long polls.
    let stopping = state.clone();
    tokio::spawn(async move {
        shutdown().await;
        stopping.stop();
    });
    let stopped = state.clone();
    // TCP_NODELAY on every connection: with Nagle's algorithm, a small answer
    // written in two parts waits for agentgateway's delayed ACK, about 40 ms
    // per hook call.
    let admin = axum::serve(admin_listener.tap_io(|tcp| no_delay(tcp)), router(state))
        .with_graceful_shutdown(stopped.stopped());
    let hooks = axum::serve(
        hooks_listener.tap_io(|tcp| no_delay(tcp)),
        authz::router(hooks),
    )
    .with_graceful_shutdown(stopped.stopped());
    let grpc_incoming = TcpListenerStream::new(grpc_listener).map(|conn| {
        if let Ok(tcp) = &conn {
            no_delay(tcp);
        }
        conn
    });
    // The gRPC hooks keep serving through the drain: running streams relay
    // their answers, and those that finish report their usage here. Only
    // then do they stop.
    let grace = config.shutdown_grace;
    let drained = async move {
        stopped.stopped().await;
        tracing::info!(?grace, "waiting for running streams");
        let left = drain.run(grace, CHARGE_WAIT).await;
        if left.streams_ended > 0 {
            tracing::warn!(
                streams = left.streams_ended,
                "ended streams still running after SEKISHO_SHUTDOWN_GRACE"
            );
        }
        if left.charges_left > 0 {
            tracing::warn!(
                charges = left.charges_left,
                "stopped with spend still being recorded"
            );
        }
    };
    let grpc = tonic::transport::Server::builder()
        .add_service(RateLimitServiceServer::new(charger))
        .add_service(ExternalProcessorServer::new(guard))
        .serve_with_incoming_shutdown(grpc_incoming, drained);
    tokio::try_join!(
        async { admin.await.context("admin API") },
        async { hooks.await.context("extAuthz hook") },
        async { grpc.await.context("rate-limit and ext_proc hooks") },
    )?;
    Ok(())
}

/// How long a stop waits, after the streams, for spend still being
/// recorded: a stream ended at the grace's end is charged its estimate
/// after [`TALLY_WAIT`].
const CHARGE_WAIT: Duration = TALLY_WAIT.saturating_add(Duration::from_secs(2));

fn no_delay(tcp: &TcpStream) {
    if let Err(error) = tcp.set_nodelay(true) {
        tracing::warn!(%error, "could not set TCP_NODELAY");
    }
}

async fn gateway_agent(server_url: Url, config_path: PathBuf) -> anyhow::Result<()> {
    let gateway_token = std::env::var("SEKISHO_GATEWAY_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .context("SEKISHO_GATEWAY_TOKEN is not set")?;
    let config = AgentConfig {
        server_url,
        gateway_token,
        config_path,
        retry: Duration::from_secs(1),
        rewrite_after: sekisho_gateway::agent::REWRITE_AFTER,
    };
    tokio::select! {
        result = run_agent(config) => result?,
        () = shutdown() => {}
    }
    Ok(())
}

/// A variable that must be set.
fn required_env(name: &str) -> anyhow::Result<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .with_context(|| format!("{name} is not set"))
}

async fn admin_link(email: &str) -> anyhow::Result<()> {
    let public_url = required_env("SEKISHO_PUBLIC_URL")?;
    let public_url = Url::parse(&public_url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .context("SEKISHO_PUBLIC_URL is not an http or https URL")?;
    let store = Store::connect(
        &required_env("SEKISHO_DATABASE_URL")?,
        StoreOptions::default(),
    )
    .await
    .context("connecting to SEKISHO_DATABASE_URL")?;
    let link =
        sekisho_auth::local::admin_link(&store, &public_url, email, OffsetDateTime::now_utc())
            .await
            .context("making the setup link")?;
    let expires = link
        .expires_at
        .replace_nanosecond(0)
        .unwrap_or(link.expires_at)
        .format(&time::format_description::well_known::Rfc3339)
        .context("formatting the link's expiry")?;
    // For the operator's eyes only: printed, never logged.
    println!(
        "A one-time setup link for {}, which also makes it an admin. It works once, until \
         {expires}:\n{}",
        email.trim().to_lowercase(),
        link.url
    );
    Ok(())
}

async fn migrate() -> anyhow::Result<()> {
    let url = required_env("SEKISHO_DATABASE_URL")?;
    let store = Store::connect(&url, StoreOptions::default())
        .await
        .context("connecting to SEKISHO_DATABASE_URL")?;
    store.migrate().await?;
    tracing::info!("migrations applied");
    Ok(())
}

/// Ctrl-C everywhere, and SIGTERM (what Docker and Kubernetes send) on Unix.
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
