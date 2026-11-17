//! One sync of a source, and the scheduler that runs them.
//!
//! A sync claims its source (one replica at a time), checks the host's
//! addresses, asks git for the ref's commit and, unless nothing could have
//! changed, checks the commit out into a temporary folder, reads it with
//! `sekisho-catalog`, pins images and agent cards, and records the result.

use std::time::Duration;

use sekisho_catalog::check::{CheckOptions, check_source_with};
use sekisho_catalog::launch::Launch;
use sekisho_catalog::model::{CheckedEntry, EntrySpec, McpRuntime, Refusal};
use sekisho_catalog::names::{SourceKind, SourceName};
use sekisho_catalog::pins::ImageRef;
use sekisho_store::address::resolve_checked;
use sekisho_store::{
    FoundEntry, Source, SyncFound, SyncOutcome, SyncOutcomeKind, SyncRecord, SyncReport,
};
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use super::cards::fetch_card;
use super::git::{DEFAULT_USERNAME, Git, GitSource};
use super::outbound::Outbound;
use super::registry::resolve_digest;
use crate::ApiError;
use crate::app::AppState;

/// How long a replica holds a source it syncs: past the sync's own limit.
pub(crate) const LEASE: time::Duration = time::Duration::minutes(10);
/// The longest a sync may take (D165).
const SYNC_LIMIT: Duration = Duration::from_secs(5 * 60);
/// Git mode's source is checked every minute (D175).
const ENVIRONMENTS_EVERY: Duration = Duration::from_secs(60);
/// The scheduler looks for due sources at least this often.
const TICK: Duration = Duration::from_secs(30);

/// Syncs `name` now. `force` is an admin's sync: it claims the source here,
/// and is Conflict while another sync holds it. The scheduler claims first
/// and passes `false`.
pub async fn sync_source(
    state: &AppState,
    name: &SourceName,
    force: bool,
) -> Result<SyncReport, ApiError> {
    let store = state.store();
    if force
        && !store
            .claim_source(name, OffsetDateTime::now_utc(), LEASE)
            .await?
    {
        return Err(ApiError::Conflict(format!("a sync of {name} is running")));
    }
    let source = store.get_source(name).await?;
    let settings = state.catalog();
    let started_at = OffsetDateTime::now_utc();
    let (commit, outcome) =
        match tokio::time::timeout(SYNC_LIMIT, read_source(state, &source)).await {
            Ok(read) => read,
            Err(_) => (
                None,
                SyncOutcome::Failed(format!(
                    "the sync took longer than {} minutes",
                    SYNC_LIMIT.as_secs() / 60
                )),
            ),
        };
    let every = if source.kind == SourceKind::Environments {
        ENVIRONMENTS_EVERY
    } else {
        settings.sync_every
    };
    let finished_at = OffsetDateTime::now_utc();
    let record = SyncRecord {
        commit,
        started_at,
        finished_at,
        outcome,
    };
    let report = store
        .record_sync(state.secret_box(), name, record, finished_at + every)
        .await?;
    tracing::info!(
        source = %name,
        outcome = report.outcome.as_str(),
        refused = report.refused.len(),
        "catalog source synced"
    );
    Ok(report)
}

fn failed(message: impl Into<String>) -> (Option<String>, SyncOutcome) {
    (None, SyncOutcome::Failed(message.into()))
}

/// Reads the source as it is now: its commit and what it holds.
async fn read_source(state: &AppState, source: &Source) -> (Option<String>, SyncOutcome) {
    let settings = state.catalog();
    let token = match state
        .store()
        .source_token(state.secret_box(), &source.name)
        .await
    {
        Ok(token) => token,
        Err(_) => return failed("the source's token can't be read"),
    };
    let pins = match resolve_checked(
        &source.url.host(),
        source.url.port(),
        source.allow_private_network,
    )
    .await
    {
        Ok(pins) => pins,
        Err(e) => return failed(e),
    };
    let git = Git::new(settings.git_program.clone(), settings.allow_plain_http);
    let src = GitSource {
        url: &source.url,
        git_ref: source.git_ref.as_ref(),
        username: source.username.as_deref().unwrap_or(DEFAULT_USERNAME),
        token: token.as_deref(),
        pins: &pins,
    };
    let commit = match git.ls_remote(&src).await {
        Ok(commit) => commit,
        Err(e) => return failed(e.to_string()),
    };
    // Sources whose entries look outside (images, cards) are read every
    // time; the rest only when the commit or the source's settings moved.
    let looks_outside = matches!(source.kind, SourceKind::Catalog | SourceKind::Plugins);
    if !looks_outside
        && let Some(last) = &source.latest_sync
        && matches!(
            last.outcome,
            SyncOutcomeKind::Ok | SyncOutcomeKind::Unchanged
        )
        && last.commit.as_deref() == Some(commit.as_str())
        && last.finished_at >= source.updated_at
    {
        return (Some(commit), SyncOutcome::Unchanged);
    }
    let checkout = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => return failed(format!("can't make a folder for the checkout: {e}")),
    };
    let commit = match git.checkout(&src, checkout.path()).await {
        Ok(commit) => commit,
        Err(e) => return failed(e.to_string()),
    };
    let root = source.folder.join(checkout.path());
    let options = CheckOptions {
        allow_plain_http: settings.allow_plain_http,
    };
    let check = match check_source_with(source.kind, &root, &options) {
        Ok(check) => check,
        Err(e) => return failed(e.to_string()),
    };
    if source.kind == SourceKind::Environments {
        // Git mode's applier (plan 3, Task 10) takes these.
        return (Some(commit), SyncOutcome::Ok(SyncFound::default()));
    }
    let out = Outbound::new(settings.allow_plain_http);
    let mut refused = check.refused;
    let mut entries = Vec::with_capacity(check.entries.len());
    for mut entry in check.entries {
        match look_outside(state, &out, &mut entry, source.allow_private_network).await {
            Ok(()) => entries.push(FoundEntry::from_checked(entry)),
            Err(reason) => refused.push(Refusal {
                path: entry.path,
                kind: Some(entry.kind),
                name: Some(entry.name.to_string()),
                reason,
            }),
        }
    }
    (
        Some(commit),
        SyncOutcome::Ok(SyncFound {
            entries,
            refused,
            warnings: check.warnings,
            vmcps: check.vmcps,
            environments: serde_json::Value::Null,
        }),
    )
}

/// Pins what an entry names outside its repository: each container image
/// to its digest, an agent's card as it is now.
async fn look_outside(
    state: &AppState,
    out: &Outbound,
    entry: &mut CheckedEntry,
    allow_private: bool,
) -> Result<(), String> {
    match &mut entry.spec {
        EntrySpec::McpServer(server) => {
            if let McpRuntime::Container { image, .. } = &mut server.runtime {
                *image = pin_image(state, out, image, allow_private).await?;
            }
        }
        EntrySpec::Plugin(plugin) => {
            for server in &mut plugin.mcp_servers {
                if let Launch::Docker { image } = &mut server.launch {
                    *image = pin_image(state, out, image, allow_private).await?;
                }
            }
        }
        EntrySpec::A2aAgent(agent) => {
            let card = fetch_card(out, agent, allow_private)
                .await
                .map_err(|e| format!("the agent's card: {e}"))?;
            agent.card = Some(card);
        }
        EntrySpec::Skill(_) | EntrySpec::Prompt(_) => {}
    }
    Ok(())
}

async fn pin_image(
    state: &AppState,
    out: &Outbound,
    image: &ImageRef,
    allow_private: bool,
) -> Result<ImageRef, String> {
    let creds = state
        .store()
        .registry_credential(state.secret_box(), image.registry())
        .await
        .map_err(|_| format!("image {image}: its registry's credential can't be read"))?;
    let digest = resolve_digest(out, image, creds, allow_private)
        .await
        .map_err(|e| format!("image {image}: {e}"))?;
    image.with_digest(digest).map_err(|e| e.to_string())
}

/// Every so often, syncs each source whose sync is due, one at a time.
/// `every` is how long after a sync the next is due; the scheduler looks
/// at least every 30 s.
pub fn start_scheduler(state: AppState, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every.min(TICK));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            loop {
                let due = state
                    .store()
                    .claim_due_source(OffsetDateTime::now_utc(), LEASE)
                    .await;
                match due {
                    Ok(Some(name)) => {
                        if let Err(e) = sync_source(&state, &name, false).await {
                            tracing::warn!(source = %name, error = ?e, "catalog sync failed");
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "can't look for due catalog sources");
                        break;
                    }
                }
            }
        }
    })
}
