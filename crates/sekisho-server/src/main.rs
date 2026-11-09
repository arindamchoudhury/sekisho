//! The `sekisho` binary.

use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use sekisho_server::app::{AppState, router, start_publisher};
use sekisho_server::config::ServerConfig;
use sekisho_store::Store;

#[derive(Parser)]
#[command(name = "sekisho", version, about = "Control plane for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the admin API and publish agentgateway's configuration.
    Serve,
    /// Apply database migrations (needs SEKISHO_DATABASE_URL).
    Migrate,
    /// Run next to agentgateway and keep its config file current.
    GatewayAgent,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let result = match Cli::parse().command {
        Command::Serve => serve().await,
        Command::Migrate => migrate().await,
        Command::GatewayAgent => Err(anyhow::anyhow!(
            "gateway-agent is not implemented yet (plan 1, Task 11)"
        )),
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
    let state = AppState::new(store, config.secret_box, config.settings);
    start_publisher(&state)
        .await
        .context("starting the gateway config publisher")?;
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding SEKISHO_LISTEN {}", config.listen))?;
    tracing::info!(address = %config.listen, "admin API listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
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
