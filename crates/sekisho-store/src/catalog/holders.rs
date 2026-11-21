//! Names unique across the install (D217, D218): a skill's, and a local MCP
//! server's. Five of the eight coding tools show skills with no namespace,
//! and a local server's name is a key in each tool's MCP config, so two
//! sources may not both deliver one. The entry applied first keeps it.

use sekisho_catalog::model::{EntrySpec, McpRuntime};
use sekisho_catalog::names::EntryKind;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::Result;

/// The advisory lock every sync record and apply takes before checking
/// names, so two at once can't both take one ("SEKINAME").
const NAMES_LOCK: i64 = 0x5345_4b49_4e41_4d45;

/// Waits for the names lock, held until the transaction ends.
pub(super) async fn lock(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(NAMES_LOCK)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// What an entry's name is called in a refusal when it must be unique
/// across the install, and what kind of names those are; `None` for the
/// kinds whose names stay unique only within their source.
pub(super) fn unique_kind(spec: &EntrySpec) -> Option<(&'static str, &'static str)> {
    match spec {
        EntrySpec::Skill(_) => Some(("skill", "skills'")),
        EntrySpec::McpServer(s) if !matches!(s.runtime, McpRuntime::Remote { .. }) => {
            Some(("MCP server", "local MCP servers'"))
        }
        _ => None,
    }
}

/// Another source holding `kind`/`name` with an applied version of the
/// same sort (for MCP servers, one that runs locally), removed upstream
/// or not; the refusal reason when there is one.
pub(super) async fn refusal(
    conn: &mut PgConnection,
    source_id: Uuid,
    kind: EntryKind,
    name: &str,
    spec: &EntrySpec,
) -> Result<Option<String>> {
    let Some((label, names)) = unique_kind(spec) else {
        return Ok(None);
    };
    let holder: Option<String> = sqlx::query_scalar(
        "select s.name from catalog_entries e \
         join catalog_sources s on s.id = e.source_id \
         join catalog_revisions r on r.id = e.applied_revision_id \
         where e.kind = $1 and e.name = $2 and e.source_id <> $3 \
         and ($1 <> 'mcp_server' or r.spec->'runtime'->>'type' <> 'remote') \
         order by s.name limit 1",
    )
    .bind(kind.as_str())
    .bind(name)
    .bind(source_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(holder.map(|source| {
        format!(
            "{label} \"{name}\" is already source \"{source}\"'s; {names} names are unique \
             across the install"
        )
    }))
}
