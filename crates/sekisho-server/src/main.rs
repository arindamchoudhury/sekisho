//! The `sekisho` binary.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::serve::ListenerExt;
use clap::{Parser, Subcommand};
use sekisho_core::keys::hash_secret;
use sekisho_gateway::agent::{AgentConfig, Url, run_agent};
use sekisho_hooks::authz::{self, HooksState};
use sekisho_hooks::cache::AuthCache;
use sekisho_hooks::extproc::StreamGuard;
use sekisho_hooks::ratelimit::Charger;
use sekisho_proto::ext_proc::external_processor_server::ExternalProcessorServer;
use sekisho_proto::ratelimit::rate_limit_service_server::RateLimitServiceServer;
use sekisho_server::app::{AppState, router, start_publisher};
use sekisho_server::config::ServerConfig;
use sekisho_store::Store;
use time::OffsetDateTime;
use tokio::net::TcpStream;
use tokio::sync::watch;
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
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let result = match Cli::parse().command {
        Command::Serve => serve().await,
        Command::Migrate => migrate().await,
        Command::GatewayAgent {
            server,
            config_path,
        } => gateway_agent(server, config_path).await,
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
    let store = Store::connect(&config.database_url)
        .await
        .context("connecting to SEKISHO_DATABASE_URL")?;

    let cache = AuthCache::load(&store)
        .await
        .context("loading keys for the hooks")?;
    cache.spawn_refresh(store.clone());
    let clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync> = Arc::new(OffsetDateTime::now_utc);
    let hooks = HooksState {
        cache: Arc::clone(&cache),
        store: store.clone(),
        clock: Arc::clone(&clock),
    };
    let charger = Charger {
        cache: Arc::clone(&cache),
        store: store.clone(),
        clock: Arc::clone(&clock),
        charge_secret_sha256: hash_secret(&config.settings.hooks.charge_secret),
    };
    let guard = StreamGuard { cache, clock };
    if config.settings.render.bare_route {
        tracing::warn!(
            "SEKISHO_E2E_BARE_ROUTE is on: /bare/openai serves a provider with no key or \
             budget checks; never use this outside the end-to-end tests"
        );
    }
    let state = AppState::new(store, config.secret_box, config.settings);
    start_publisher(&state)
        .await
        .context("starting the gateway config publisher")?;

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

    // One signal stops every server.
    let (stop, stopped) = watch::channel(false);
    tokio::spawn(async move {
        shutdown().await;
        let _ = stop.send(true);
    });
    let until_stopped = |mut stopped: watch::Receiver<bool>| async move {
        let _ = stopped.wait_for(|s| *s).await;
    };
    // TCP_NODELAY on every connection: with Nagle's algorithm, a small answer
    // written in two parts waits for agentgateway's delayed ACK, about 40 ms
    // per hook call.
    let admin = axum::serve(admin_listener.tap_io(|tcp| no_delay(tcp)), router(state))
        .with_graceful_shutdown(until_stopped(stopped.clone()));
    let hooks = axum::serve(
        hooks_listener.tap_io(|tcp| no_delay(tcp)),
        authz::router(hooks),
    )
    .with_graceful_shutdown(until_stopped(stopped.clone()));
    let grpc_incoming = TcpListenerStream::new(grpc_listener).map(|conn| {
        if let Ok(tcp) = &conn {
            no_delay(tcp);
        }
        conn
    });
    let grpc = tonic::transport::Server::builder()
        .add_service(RateLimitServiceServer::new(charger))
        .add_service(ExternalProcessorServer::new(guard))
        .serve_with_incoming_shutdown(grpc_incoming, until_stopped(stopped));
    tokio::try_join!(
        async { admin.await.context("admin API") },
        async { hooks.await.context("extAuthz hook") },
        async { grpc.await.context("rate-limit and ext_proc hooks") },
    )?;
    Ok(())
}

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
    };
    tokio::select! {
        result = run_agent(config) => result?,
        () = shutdown() => {}
    }
    Ok(())
}

async fn migrate() -> anyhow::Result<()> {
    let url = std::env::var("SEKISHO_DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .context("SEKISHO_DATABASE_URL is not set")?;
    let store = Store::connect(&url)
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
