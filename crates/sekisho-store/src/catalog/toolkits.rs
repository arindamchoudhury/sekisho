//! Toolkits (D181–D186): binding a toolkit revision's references to
//! entries at sync, reading them back, and the toolkits that list an entry.

use std::collections::HashMap;

use sekisho_catalog::model::ToolkitSpec;
use sekisho_catalog::names::{EntryKind, EntryRef, SourceName};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::{EntryKey, ResolveError, entry_kind, entry_name, source_name};
use crate::{Result, Store};

/// One entry a toolkit revision lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolkitMember {
    pub kind: EntryKind,
    /// The reference as written: `name` or `name@source`.
    pub written: String,
    /// The entry it was bound to at sync.
    pub key: EntryKey,
    /// An MCP server's allowed tools; `None` allows them all.
    pub tools: Option<Vec<String>>,
    pub quarantined: bool,
    pub removed: bool,
}

/// A reference bound to an entry, ready to store.
pub(super) struct Binding {
    kind: EntryKind,
    position: i32,
    written: String,
    entry_id: Uuid,
    tools: Option<Vec<String>>,
}

/// How a kind is named in messages.
fn kind_words(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::McpServer => "MCP server",
        EntryKind::A2aAgent => "A2A agent",
        EntryKind::Skill => "skill",
        EntryKind::Plugin => "plugin",
        EntryKind::Prompt => "prompt",
        EntryKind::Toolkit => "toolkit",
    }
}

/// The toolkit format's list of a kind.
fn list_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::McpServer => "mcp_servers",
        EntryKind::A2aAgent => "a2a_agents",
        EntryKind::Skill => "skills",
        EntryKind::Plugin => "plugins",
        EntryKind::Prompt => "prompts",
        EntryKind::Toolkit => "toolkits",
    }
}

/// The entry a reference names (D167), inside a transaction.
pub(super) async fn resolve_in(
    conn: &mut PgConnection,
    kind: EntryKind,
    reference: &EntryRef,
) -> std::result::Result<Uuid, ResolveError> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "select e.id, s.name from catalog_entries e join catalog_sources s on s.id = e.source_id \
         where e.kind = $1 and e.name = $2 and ($3::text is null or s.name = $3) \
         order by s.name",
    )
    .bind(kind.as_str())
    .bind(reference.name.as_str())
    .bind(reference.source.as_ref().map(SourceName::as_str))
    .fetch_all(&mut *conn)
    .await?;
    match rows.as_slice() {
        [] => Err(ResolveError::NotFound),
        [(id, _)] => Ok(*id),
        many => Err(ResolveError::Ambiguous(
            many.iter()
                .map(|(_, s)| source_name(s))
                .collect::<Result<Vec<_>>>()?,
        )),
    }
}

/// Binds every reference of `spec`, or answers why the toolkit is refused:
/// each problem as `field: message`, joined by `; `.
pub(super) async fn bind(
    conn: &mut PgConnection,
    spec: &ToolkitSpec,
) -> Result<std::result::Result<Vec<Binding>, String>> {
    let mut bindings = Vec::new();
    let mut problems = Vec::new();
    let mut positions: HashMap<EntryKind, i32> = HashMap::new();
    // Where each bound entry was first named, to refuse naming it twice.
    let mut named: HashMap<(EntryKind, Uuid), String> = HashMap::new();
    let tools: HashMap<String, Option<Vec<String>>> = spec
        .mcp_servers
        .iter()
        .map(|s| (s.server.to_string(), s.tools.clone()))
        .collect();
    for (kind, reference) in spec.references() {
        let position = positions.entry(kind).or_insert(0);
        let field = format!("{}[{position}]", list_name(kind));
        let written = reference.to_string();
        match resolve_in(conn, kind, reference).await {
            Ok(entry_id) => match named.get(&(kind, entry_id)) {
                Some(first) => problems.push(format!(
                    "{field}: {written} names the same {} as {first}",
                    kind_words(kind)
                )),
                None => {
                    named.insert((kind, entry_id), field.clone());
                    bindings.push(Binding {
                        kind,
                        position: *position,
                        tools: if kind == EntryKind::McpServer {
                            tools.get(&written).cloned().flatten()
                        } else {
                            None
                        },
                        written,
                        entry_id,
                    });
                }
            },
            Err(ResolveError::NotFound) => {
                problems.push(format!("{field}: no {} named {written}", kind_words(kind)))
            }
            Err(ResolveError::Ambiguous(sources)) => {
                let names: Vec<&str> = sources.iter().map(SourceName::as_str).collect();
                let forms: Vec<String> = names
                    .iter()
                    .map(|s| format!("{}@{s}", reference.name))
                    .collect();
                problems.push(format!(
                    "{field}: {} is in sources {}: write {}",
                    reference.name,
                    names.join(", "),
                    forms.join(" or ")
                ));
            }
            Err(ResolveError::Store(e)) => return Err(e),
        }
        *position += 1;
    }
    Ok(if problems.is_empty() {
        Ok(bindings)
    } else {
        Err(problems.join("; "))
    })
}

/// Stores a new revision's bindings.
pub(super) async fn store_bindings(
    conn: &mut PgConnection,
    revision_id: Uuid,
    bindings: &[Binding],
) -> Result<()> {
    for b in bindings {
        sqlx::query(
            "insert into catalog_toolkit_members \
             (revision_id, kind, position, written, entry_id, tools) \
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(revision_id)
        .bind(b.kind.as_str())
        .bind(b.position)
        .bind(&b.written)
        .bind(b.entry_id)
        .bind(&b.tools)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Whether a revision of the entry has this digest already: its bindings
/// stand, so the toolkit's references aren't resolved again (D185).
pub(super) async fn revision_exists(
    conn: &mut PgConnection,
    source_id: Uuid,
    name: &str,
    digest: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "select exists (select 1 from catalog_revisions r \
         join catalog_entries e on e.id = r.entry_id \
         where e.source_id = $1 and e.kind = 'toolkit' and e.name = $2 and r.digest = $3)",
    )
    .bind(source_id)
    .bind(name)
    .bind(digest)
    .fetch_one(&mut *conn)
    .await?)
}

/// The toolkits of other sources whose applied or pending revision lists
/// an entry of `source_id`, as `toolkits/<source>/<name>`.
pub(super) async fn toolkits_using_source(
    conn: &mut PgConnection,
    source_id: Uuid,
) -> Result<Vec<String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select distinct ts.name, t.name from catalog_toolkit_members m \
         join catalog_entries member on member.id = m.entry_id \
         join catalog_entries t \
           on t.applied_revision_id = m.revision_id or t.latest_revision_id = m.revision_id \
         join catalog_sources ts on ts.id = t.source_id \
         where member.source_id = $1 and t.source_id <> $1 \
         order by ts.name, t.name",
    )
    .bind(source_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(source, name)| format!("toolkits/{source}/{name}"))
        .collect())
}

impl Store {
    /// The entries a toolkit revision lists, by kind (MCP servers, A2A
    /// agents, skills, plugins, prompts) and then as written.
    pub async fn toolkit_members(&self, revision_id: Uuid) -> Result<Vec<ToolkitMember>> {
        sqlx::query(
            "select m.kind, m.written, m.tools, s.name as source, e.name, \
             e.quarantined_at is not null as quarantined, e.removed_at is not null as removed \
             from catalog_toolkit_members m \
             join catalog_entries e on e.id = m.entry_id \
             join catalog_sources s on s.id = e.source_id \
             where m.revision_id = $1 \
             order by array_position(array['mcp_server', 'a2a_agent', 'skill', 'plugin', \
             'prompt'], m.kind), m.position",
        )
        .bind(revision_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            let kind = entry_kind(&r.try_get::<String, _>("kind")?)?;
            Ok(ToolkitMember {
                kind,
                written: r.try_get("written")?,
                key: EntryKey {
                    kind,
                    source: source_name(&r.try_get::<String, _>("source")?)?,
                    name: entry_name(&r.try_get::<String, _>("name")?)?,
                },
                tools: r.try_get("tools")?,
                quarantined: r.try_get("quarantined")?,
                removed: r.try_get("removed")?,
            })
        })
        .collect()
    }

    /// The toolkits whose applied revision lists entry `id`.
    pub(super) async fn toolkits_listing(&self, id: Uuid) -> Result<Vec<EntryKey>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "select s.name, t.name from catalog_toolkit_members m \
             join catalog_entries t on t.applied_revision_id = m.revision_id \
             join catalog_sources s on s.id = t.source_id \
             where m.entry_id = $1 order by s.name, t.name",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|(source, name)| {
                Ok(EntryKey {
                    kind: EntryKind::Toolkit,
                    source: source_name(source)?,
                    name: entry_name(name)?,
                })
            })
            .collect()
    }
}
