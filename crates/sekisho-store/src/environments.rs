//! Environments: their models, default models, labels, and the catalog
//! entries and toolkits they list (plan 3, Task 8), and what they deliver.

use std::collections::{HashMap, HashSet};

use sekisho_catalog::environment::{DefaultModels, EnvironmentDoc, McpServerUse};
use sekisho_catalog::labels::Labels;
use sekisho_catalog::names::{EntryKind, EntryRef};
use sekisho_core::access::{GROUP_RULE, is_group_reference};
use sekisho_core::formats::{ApiFormat, ProviderKind, reaches_format};
use sekisho_core::names::{EnvName, ModelName};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalog::{EntryKey, entry_kind, entry_name, source_name, toolkits};
use crate::error::{FieldError, bad_column, check};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// What an admin sets on an environment. Lists keep their order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentSpec {
    pub name: EnvName,
    pub description: String,
    /// Shown to people when they start using the environment.
    pub notice: String,
    pub access_groups: Vec<String>,
    pub models: Vec<ModelName>,
    /// Each format's default, named; [`Environment::effective_defaults`]
    /// has them with their fallbacks.
    pub default_models: DefaultModels,
    /// For tools that call a model in the background.
    pub small_model: Option<ModelName>,
    pub labels: Labels,
    pub mcp_servers: Vec<McpServerUse>,
    pub a2a_agents: Vec<EntryRef>,
    pub skills: Vec<EntryRef>,
    pub plugins: Vec<EntryRef>,
    pub prompts: Vec<EntryRef>,
    pub toolkits: Vec<EntryRef>,
    /// The user who owns it. `None` keeps the stored owner, or makes whoever
    /// saves it the owner when there is none.
    pub owner: Option<Uuid>,
}

impl EnvironmentSpec {
    /// An environment with nothing in it but its name.
    pub fn new(name: EnvName) -> Self {
        Self {
            name,
            description: String::new(),
            notice: String::new(),
            access_groups: Vec::new(),
            models: Vec::new(),
            default_models: DefaultModels::default(),
            small_model: None,
            labels: Labels::default(),
            mcp_servers: Vec::new(),
            a2a_agents: Vec::new(),
            skills: Vec::new(),
            plugins: Vec::new(),
            prompts: Vec::new(),
            toolkits: Vec::new(),
            owner: None,
        }
    }

    /// The environment format's document, as Git mode and `GET` in YAML
    /// write it.
    pub fn to_doc(&self) -> EnvironmentDoc {
        EnvironmentDoc {
            api_version: sekisho_catalog::environment::API_VERSION.to_owned(),
            kind: Some(sekisho_catalog::environment::KIND.to_owned()),
            name: self.name.clone(),
            description: self.description.clone(),
            notice: self.notice.clone(),
            access_groups: self.access_groups.clone(),
            models: self.models.clone(),
            default_models: self.default_models.clone(),
            small_model: self.small_model.clone(),
            labels: self.labels.clone(),
            mcp_servers: self.mcp_servers.clone(),
            a2a_agents: self.a2a_agents.clone(),
            skills: self.skills.clone(),
            plugins: self.plugins.clone(),
            prompts: self.prompts.clone(),
            toolkits: self.toolkits.clone(),
        }
    }

    /// The document's fields, with the owner Sekisho keeps apart.
    pub fn from_doc(doc: EnvironmentDoc, owner: Option<Uuid>) -> Self {
        Self {
            name: doc.name,
            description: doc.description,
            notice: doc.notice,
            access_groups: doc.access_groups,
            models: doc.models,
            default_models: doc.default_models,
            small_model: doc.small_model,
            labels: doc.labels,
            mcp_servers: doc.mcp_servers,
            a2a_agents: doc.a2a_agents,
            skills: doc.skills,
            plugins: doc.plugins,
            prompts: doc.prompts,
            toolkits: doc.toolkits,
            owner,
        }
    }

    /// Every catalog reference with its kind, its field as the API names
    /// it, and an MCP server's tools: MCP servers, A2A agents, skills,
    /// plugins, prompts, toolkits.
    fn references(&self) -> Vec<Reference<'_>> {
        let mut out: Vec<Reference<'_>> = self
            .mcp_servers
            .iter()
            .enumerate()
            .map(|(i, u)| Reference {
                kind: EntryKind::McpServer,
                position: i,
                field: format!("mcp_servers[{i}]"),
                reference: &u.server,
                tools: u.tools.clone(),
            })
            .collect();
        for (kind, list, refs) in [
            (EntryKind::A2aAgent, "a2a_agents", &self.a2a_agents),
            (EntryKind::Skill, "skills", &self.skills),
            (EntryKind::Plugin, "plugins", &self.plugins),
            (EntryKind::Prompt, "prompts", &self.prompts),
            (EntryKind::Toolkit, "toolkits", &self.toolkits),
        ] {
            out.extend(refs.iter().enumerate().map(|(i, r)| Reference {
                kind,
                position: i,
                field: format!("{list}[{i}]"),
                reference: r,
                tools: None,
            }));
        }
        out
    }
}

struct Reference<'a> {
    kind: EntryKind,
    position: usize,
    field: String,
    reference: &'a EntryRef,
    tools: Option<Vec<String>>,
}

/// A catalog entry or toolkit an environment lists, as bound when saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvEntry {
    pub kind: EntryKind,
    /// The reference as saved: `name` or `name@source`.
    pub written: String,
    pub key: EntryKey,
    /// An MCP server's allowed tools; `None` allows them all.
    pub tools: Option<Vec<String>>,
    pub quarantined: bool,
    pub removed: bool,
}

/// Each format's default model as named, or else the first listed model
/// that every API of the format reaches; `None` when none does. The small
/// model only as named: tools fall back to their default model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveDefaults {
    pub anthropic: Option<ModelName>,
    pub openai: Option<ModelName>,
    pub gemini: Option<ModelName>,
    pub small: Option<ModelName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub id: Uuid,
    /// As stored: `spec.owner` is [`Environment::owner`].
    pub spec: EnvironmentSpec,
    /// `None` only for an environment saved before plan 2 and not since.
    pub owner: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// What it lists from the catalog, in the spec's order (kinds as in
    /// [`EnvironmentSpec`]).
    pub entries: Vec<EnvEntry>,
    pub effective_defaults: EffectiveDefaults,
}

/// One entry an environment delivers (D186): listed by the environment
/// itself, or by one of its toolkits that isn't quarantined. Each entry
/// once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub key: EntryKey,
    /// The applied revision; `None` for a toolkit's member whose source
    /// hasn't applied it yet.
    pub revision: Option<Uuid>,
    /// An MCP server's allowed tools, the union of every grant; `None`
    /// when any grant allows them all.
    pub tools: Option<Vec<String>>,
    /// The environment lists it itself.
    pub listed: bool,
    /// The toolkits that bring it, in the environment's order.
    pub via: Vec<EntryKey>,
    pub quarantined: bool,
    pub removed: bool,
}

/// Expands to the environment query up to its filter; finish it with
/// `group by e.id`.
macro_rules! select_environments {
    () => {
        "select e.id, e.name, e.description, e.notice, e.access_groups, e.owner_id, \
         e.created_at, e.updated_at, e.labels, \
         (select name from models where id = e.default_anthropic_model_id) as default_anthropic, \
         (select name from models where id = e.default_openai_model_id) as default_openai, \
         (select name from models where id = e.default_gemini_model_id) as default_gemini, \
         (select name from models where id = e.small_model_id) as small_model, \
         coalesce(array_agg(m.name order by em.position) \
         filter (where m.id is not null), '{}') as models \
         from environments e \
         left join environment_models em on em.environment_id = e.id \
         left join models m on m.id = em.model_id"
    };
}

/// An environment name agentgateway's Gemini paths can't carry.
const RESERVED_NAME: &str = "models";

/// Kinds in the order environments list them.
const KIND_ORDER: &str = "array['mcp_server', 'a2a_agent', 'skill', 'plugin', 'prompt', 'toolkit']";

/// The listed models that exist, by name, with their ids. An environment
/// may list models of any providers (plan 1d): agentgateway's router picks
/// each one's provider.
fn validate(
    spec: &EnvironmentSpec,
    known: &HashMap<String, (Uuid, ProviderKind)>,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    // agentgateway v1.7.0 takes a Gemini path's model from after its first
    // `/models/` (`extract_model_from_path`), so in
    // `/env/models/gemini/v1beta/models/<model>:…` it would read the wrong
    // one, and Sekisho would refuse or misprice every Gemini call.
    if spec.name.as_str() == RESERVED_NAME {
        errors.push(FieldError::new(
            "name",
            "models is reserved: in a Gemini path the gateway reads the model from after \
             /models/",
        ));
    }
    let mut seen = HashSet::new();
    for (i, model) in spec.models.iter().enumerate() {
        let field = format!("models[{i}]");
        if !seen.insert(model.as_str()) {
            errors.push(FieldError::new(field, format!("{model} is listed twice")));
        } else if !known.contains_key(model.as_str()) {
            errors.push(FieldError::new(field, format!("no model named {model}")));
        }
    }

    let mut groups = HashSet::new();
    for (i, group) in spec.access_groups.iter().enumerate() {
        let field = format!("access_groups[{i}]");
        if group.trim().is_empty() {
            errors.push(FieldError::new(field, "must not be empty"));
        } else if !is_group_reference(group) {
            errors.push(FieldError::new(field, GROUP_RULE));
        } else if !groups.insert(group.as_str()) {
            errors.push(FieldError::new(field, format!("{group} is listed twice")));
        }
    }
    // The format's own checks: defaults listed, references and tools not
    // repeated. A field already refused above isn't refused twice.
    for p in spec.to_doc().check() {
        if !errors.iter().any(|e| e.field == p.field) {
            errors.push(FieldError::new(p.field, p.message));
        }
    }
    // A default must be reached by every API of its format.
    for (format, field, model) in [
        (
            ApiFormat::Anthropic,
            "default_models.anthropic",
            &spec.default_models.anthropic,
        ),
        (
            ApiFormat::OpenAi,
            "default_models.openai",
            &spec.default_models.openai,
        ),
        (
            ApiFormat::Gemini,
            "default_models.gemini",
            &spec.default_models.gemini,
        ),
    ] {
        let Some(model) = model else { continue };
        let Some((_, kind)) = known.get(model.as_str()) else {
            continue;
        };
        if !reaches_format(format, *kind, model.as_str())
            && !errors.iter().any(|e| e.field == field)
        {
            errors.push(FieldError::new(
                field,
                format!(
                    "{model} on a {} provider isn't reached by every API of this format",
                    kind.as_str()
                ),
            ));
        }
    }
    errors
}

fn from_row(row: &PgRow) -> Result<Environment> {
    let name: String = row.try_get("name")?;
    let model = |m: &str| ModelName::parse(m).map_err(|_| bad_column("model name", m));
    let optional = |column: &str| -> Result<Option<ModelName>> {
        row.try_get::<Option<String>, _>(column)?
            .map(|m| model(&m))
            .transpose()
    };
    let models: Vec<String> = row.try_get("models")?;
    let models = models.iter().map(|m| model(m)).collect::<Result<_>>()?;
    let labels: serde_json::Value = row.try_get("labels")?;
    let labels: std::collections::BTreeMap<String, String> =
        serde_json::from_value(labels).map_err(|e| bad_column("labels", &e.to_string()))?;
    let labels = Labels::new(labels).map_err(|_| bad_column("labels", "not valid labels"))?;
    let owner: Option<Uuid> = row.try_get("owner_id")?;
    let mut spec = EnvironmentSpec::new(
        EnvName::parse(&name).map_err(|_| bad_column("environment name", &name))?,
    );
    spec.description = row.try_get("description")?;
    spec.notice = row.try_get("notice")?;
    spec.access_groups = row.try_get("access_groups")?;
    spec.models = models;
    spec.default_models = DefaultModels {
        anthropic: optional("default_anthropic")?,
        openai: optional("default_openai")?,
        gemini: optional("default_gemini")?,
    };
    spec.small_model = optional("small_model")?;
    spec.labels = labels;
    spec.owner = owner;
    Ok(Environment {
        id: row.try_get("id")?,
        spec,
        owner,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        entries: Vec::new(),
        effective_defaults: EffectiveDefaults::default(),
    })
}

/// Each format's default as named, or the first listed model the format
/// reaches.
fn effective_defaults(
    spec: &EnvironmentSpec,
    kinds: &HashMap<String, ProviderKind>,
) -> EffectiveDefaults {
    let pick = |format: ApiFormat, named: &Option<ModelName>| {
        named.clone().or_else(|| {
            spec.models
                .iter()
                .find(|m| {
                    kinds
                        .get(m.as_str())
                        .is_some_and(|k| reaches_format(format, *k, m.as_str()))
                })
                .cloned()
        })
    };
    EffectiveDefaults {
        anthropic: pick(ApiFormat::Anthropic, &spec.default_models.anthropic),
        openai: pick(ApiFormat::OpenAi, &spec.default_models.openai),
        gemini: pick(ApiFormat::Gemini, &spec.default_models.gemini),
        small: spec.small_model.clone(),
    }
}

/// An entry's state when an environment saves a reference to it.
struct EntryState {
    quarantined: bool,
    removed: bool,
    applied: bool,
}

async fn entry_state(conn: &mut PgConnection, id: Uuid) -> Result<EntryState> {
    let row = sqlx::query(
        "select quarantined_at is not null as quarantined, removed_at is not null as removed, \
         applied_revision_id is not null as applied from catalog_entries where id = $1 \
         for key share",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(EntryState {
        quarantined: row.try_get("quarantined")?,
        removed: row.try_get("removed")?,
        applied: row.try_get("applied")?,
    })
}

/// A reference bound to an entry, ready to store.
struct Bound {
    kind: EntryKind,
    position: i32,
    written: String,
    entry_id: Uuid,
    tools: Option<Vec<String>>,
}

/// Binds every reference of `spec`. A reference saved before, written the
/// same way, keeps its entry (D167); any other is resolved now, and must
/// name an applied entry that is neither quarantined nor removed. Problems
/// go to `errors`, one per field.
async fn bind_references(
    conn: &mut PgConnection,
    environment: Option<Uuid>,
    spec: &EnvironmentSpec,
    errors: &mut Vec<FieldError>,
) -> Result<Vec<Bound>> {
    let stored: Vec<(String, String, Uuid)> =
        match environment {
            Some(id) => sqlx::query_as(
                "select kind, written, entry_id from environment_entries where environment_id = $1",
            )
            .bind(id)
            .fetch_all(&mut *conn)
            .await?,
            None => Vec::new(),
        };
    let kept: HashSet<Uuid> = stored.iter().map(|(_, _, id)| *id).collect();
    let stored: HashMap<(String, String), Uuid> = stored
        .into_iter()
        .map(|(kind, written, id)| ((kind, written), id))
        .collect();
    let mut bound = Vec::new();
    let mut first: HashMap<Uuid, String> = HashMap::new();
    for r in spec.references() {
        if errors.iter().any(|e| e.field == r.field) {
            continue;
        }
        let written = r.reference.to_string();
        let entry_id = match stored.get(&(r.kind.as_str().to_owned(), written.clone())) {
            Some(id) => *id,
            None => match toolkits::resolve_in(conn, r.kind, r.reference).await {
                Ok(id) => id,
                Err(e) => {
                    let message = toolkits::not_resolved(r.kind, r.reference, e)?;
                    errors.push(FieldError::new(r.field, message));
                    continue;
                }
            },
        };
        if !kept.contains(&entry_id) {
            let state = entry_state(conn, entry_id).await?;
            let refusal = if state.quarantined {
                Some(format!("{written} is quarantined"))
            } else if state.removed {
                Some(format!("{written} was removed from its source"))
            } else if !state.applied {
                Some(format!("{written} hasn't been applied yet"))
            } else {
                None
            };
            if let Some(message) = refusal {
                errors.push(FieldError::new(r.field, message));
                continue;
            }
        }
        if let Some(earlier) = first.get(&entry_id) {
            errors.push(FieldError::new(
                r.field.clone(),
                format!(
                    "{written} names the same {} as {earlier}",
                    toolkits::kind_words(r.kind)
                ),
            ));
            continue;
        }
        first.insert(entry_id, r.field.clone());
        bound.push(Bound {
            kind: r.kind,
            position: r.position as i32,
            written,
            entry_id,
            tools: r.tools,
        });
    }
    Ok(bound)
}

impl Store {
    /// Creates the environment, or replaces the spec of the one with this
    /// name (keeping its id, keys and budgets). `actor` is who saves it: the
    /// owner of a new environment unless the spec names one, and of an
    /// environment that has none. The owner must be an enabled user.
    pub async fn upsert_environment(
        &self,
        spec: EnvironmentSpec,
        actor: Uuid,
    ) -> Result<Environment> {
        self.save_environment(spec, actor, false).await
    }

    /// Creates the environment, never replacing one: Conflict when the name
    /// is taken, even by a save that lands meanwhile.
    pub async fn create_environment(
        &self,
        spec: EnvironmentSpec,
        actor: Uuid,
    ) -> Result<Environment> {
        self.save_environment(spec, actor, true).await
    }

    async fn save_environment(
        &self,
        mut spec: EnvironmentSpec,
        actor: Uuid,
        create_only: bool,
    ) -> Result<Environment> {
        let names: Vec<String> = spec.models.iter().map(|m| m.as_str().to_owned()).collect();
        let name_text = spec.name.to_string();
        let taken = || StoreError::Conflict(format!("an environment named {name_text} exists"));
        let mut tx = self.pool.begin().await?;
        let stored: Option<(Uuid, Option<Uuid>)> =
            sqlx::query_as("select id, owner_id from environments where name = $1")
                .bind(spec.name.as_str())
                .fetch_optional(&mut *tx)
                .await?;
        if create_only && stored.is_some() {
            return Err(taken());
        }
        let owner = spec.owner.or(stored.and_then(|(_, o)| o)).unwrap_or(actor);
        // Locked before the environment (users before environments, as in
        // offboarding), so offboarding the owner waits for this save.
        let owner_disabled: Option<bool> =
            sqlx::query_scalar("select disabled_at is not null from users where id = $1 for share")
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
        // The share lock makes a concurrent delete_model of a listed model
        // wait, then find this environment and refuse.
        let rows = sqlx::query(
            "select m.id, m.name, p.kind from models m join providers p on p.id = m.provider_id \
             where m.name = any($1) for key share of m",
        )
        .bind(&names)
        .fetch_all(&mut *tx)
        .await?;
        let mut known = HashMap::new();
        for row in rows {
            let kind: String = row.try_get("kind")?;
            known.insert(
                row.try_get::<String, _>("name")?,
                (
                    row.try_get::<Uuid, _>("id")?,
                    ProviderKind::parse(&kind).ok_or_else(|| bad_column("provider kind", &kind))?,
                ),
            );
        }
        let mut errors = validate(&spec, &known);
        match owner_disabled {
            None => errors.push(FieldError::new("owner", format!("no user {owner}"))),
            Some(true) => errors.push(FieldError::new("owner", format!("{owner} is disabled"))),
            Some(false) => {}
        }
        let bound = bind_references(&mut tx, stored.map(|(id, _)| id), &spec, &mut errors).await?;
        check(errors)?;
        spec.owner = Some(owner);

        let id_of = |m: &Option<ModelName>| m.as_ref().map(|m| known[m.as_str()].0);
        let labels = serde_json::to_value(spec.labels.as_map()).expect("labels serialise");
        const INSERT: &str = "insert into environments (name, description, notice, \
             access_groups, owner_id, labels, default_anthropic_model_id, \
             default_openai_model_id, default_gemini_model_id, small_model_id) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ";
        const REPLACE: &str = "on conflict (name) do update set \
             description = excluded.description, \
             notice = excluded.notice, access_groups = excluded.access_groups, \
             owner_id = excluded.owner_id, labels = excluded.labels, \
             default_anthropic_model_id = excluded.default_anthropic_model_id, \
             default_openai_model_id = excluded.default_openai_model_id, \
             default_gemini_model_id = excluded.default_gemini_model_id, \
             small_model_id = excluded.small_model_id, updated_at = now() \
             returning id";
        // A create never replaces: a row saved meanwhile wins, and this is
        // Conflict.
        const KEEP: &str = "on conflict (name) do nothing returning id";
        let sql = format!("{INSERT}{}", if create_only { KEEP } else { REPLACE });
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(spec.name.as_str())
            .bind(&spec.description)
            .bind(&spec.notice)
            .bind(&spec.access_groups)
            .bind(owner)
            .bind(&labels)
            .bind(id_of(&spec.default_models.anthropic))
            .bind(id_of(&spec.default_models.openai))
            .bind(id_of(&spec.default_models.gemini))
            .bind(id_of(&spec.small_model))
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(taken)?;
        let id: Uuid = row.try_get("id")?;
        sqlx::query("delete from environment_models where environment_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let model_ids: Vec<Uuid> = spec.models.iter().map(|m| known[m.as_str()].0).collect();
        sqlx::query(
            "insert into environment_models (environment_id, model_id, position) \
             select $1, model_id, (position - 1)::integer \
             from unnest($2::uuid[]) with ordinality as listed (model_id, position)",
        )
        .bind(id)
        .bind(&model_ids)
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from environment_entries where environment_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for b in &bound {
            sqlx::query(
                "insert into environment_entries \
                 (environment_id, entry_id, kind, position, written, tools) \
                 values ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id)
            .bind(b.entry_id)
            .bind(b.kind.as_str())
            .bind(b.position)
            .bind(&b.written)
            .bind(&b.tools)
            .execute(&mut *tx)
            .await?;
        }
        notify(&mut tx, ChangeKind::Environments).await?;
        tx.commit().await?;
        self.get_environment(&spec.name).await
    }

    /// Hands the environment to `owner`, an enabled user; nothing else
    /// changes. Works in Git mode too, where owners aren't in the files.
    pub async fn set_environment_owner(&self, name: &EnvName, owner: Uuid) -> Result<Environment> {
        let mut tx = self.pool.begin().await?;
        // Users before environments, as everywhere.
        let disabled: Option<bool> =
            sqlx::query_scalar("select disabled_at is not null from users where id = $1 for share")
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
        let exists: bool =
            sqlx::query_scalar("select exists (select 1 from environments where name = $1)")
                .bind(name.as_str())
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(StoreError::NotFound);
        }
        match disabled {
            None => check(vec![FieldError::new("owner", format!("no user {owner}"))])?,
            Some(true) => check(vec![FieldError::new(
                "owner",
                format!("{owner} is disabled"),
            )])?,
            Some(false) => {}
        }
        sqlx::query("update environments set owner_id = $2, updated_at = now() where name = $1")
            .bind(name.as_str())
            .bind(owner)
            .execute(&mut *tx)
            .await?;
        notify(&mut tx, ChangeKind::Environments).await?;
        tx.commit().await?;
        self.get_environment(name).await
    }

    pub async fn get_environment(&self, name: &EnvName) -> Result<Environment> {
        let row = sqlx::query(concat!(
            select_environments!(),
            " where e.name = $1 group by e.id"
        ))
        .bind(name.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        let mut environments = vec![from_row(&row)?];
        self.complete(&mut environments).await?;
        Ok(environments.pop().expect("one"))
    }

    /// All environments, sorted by name.
    pub async fn list_environments(&self) -> Result<Vec<Environment>> {
        let mut environments = sqlx::query(concat!(
            select_environments!(),
            " group by e.id order by e.name"
        ))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(from_row)
        .collect::<Result<Vec<_>>>()?;
        self.complete(&mut environments).await?;
        Ok(environments)
    }

    /// Fills in the catalog references and effective defaults, in two
    /// queries for any number of environments.
    async fn complete(&self, environments: &mut [Environment]) -> Result<()> {
        let ids: Vec<Uuid> = environments.iter().map(|e| e.id).collect();
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select ee.environment_id, ee.kind, ee.written, ee.tools, s.name as source, \
             c.name, c.quarantined_at is not null as quarantined, \
             c.removed_at is not null as removed \
             from environment_entries ee \
             join catalog_entries c on c.id = ee.entry_id \
             join catalog_sources s on s.id = c.source_id \
             where ee.environment_id = any($1) \
             order by ee.environment_id, array_position({KIND_ORDER}, ee.kind), ee.position"
        )))
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let mut entries: HashMap<Uuid, Vec<EnvEntry>> = HashMap::new();
        for row in rows {
            let kind = entry_kind(&row.try_get::<String, _>("kind")?)?;
            entries
                .entry(row.try_get("environment_id")?)
                .or_default()
                .push(EnvEntry {
                    kind,
                    written: row.try_get("written")?,
                    key: EntryKey {
                        kind,
                        source: source_name(&row.try_get::<String, _>("source")?)?,
                        name: entry_name(&row.try_get::<String, _>("name")?)?,
                    },
                    tools: row.try_get("tools")?,
                    quarantined: row.try_get("quarantined")?,
                    removed: row.try_get("removed")?,
                });
        }
        let kinds: Vec<(Uuid, String, String)> = sqlx::query_as(
            "select em.environment_id, m.name, p.kind from environment_models em \
             join models m on m.id = em.model_id join providers p on p.id = m.provider_id \
             where em.environment_id = any($1)",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let mut model_kinds: HashMap<Uuid, HashMap<String, ProviderKind>> = HashMap::new();
        for (env, model, kind) in kinds {
            let kind =
                ProviderKind::parse(&kind).ok_or_else(|| bad_column("provider kind", &kind))?;
            model_kinds.entry(env).or_default().insert(model, kind);
        }
        for e in environments {
            let listed = entries.remove(&e.id).unwrap_or_default();
            let refs = |kind: EntryKind| -> Result<Vec<EntryRef>> {
                listed
                    .iter()
                    .filter(|l| l.kind == kind)
                    .map(|l| {
                        EntryRef::parse(&l.written)
                            .map_err(|_| bad_column("entry reference", &l.written))
                    })
                    .collect()
            };
            e.spec.mcp_servers = listed
                .iter()
                .filter(|l| l.kind == EntryKind::McpServer)
                .map(|l| {
                    Ok(McpServerUse {
                        server: EntryRef::parse(&l.written)
                            .map_err(|_| bad_column("entry reference", &l.written))?,
                        tools: l.tools.clone(),
                    })
                })
                .collect::<Result<_>>()?;
            e.spec.a2a_agents = refs(EntryKind::A2aAgent)?;
            e.spec.skills = refs(EntryKind::Skill)?;
            e.spec.plugins = refs(EntryKind::Plugin)?;
            e.spec.prompts = refs(EntryKind::Prompt)?;
            e.spec.toolkits = refs(EntryKind::Toolkit)?;
            e.effective_defaults =
                effective_defaults(&e.spec, &model_kinds.remove(&e.id).unwrap_or_default());
            e.entries = listed;
        }
        Ok(())
    }

    /// What the environment delivers (D186): its own entries and those of
    /// its toolkits that aren't quarantined, at their applied revisions,
    /// each entry once with its MCP server's allowed tools unioned. Its own
    /// entries come first, in its order, then what only toolkits bring.
    /// Toolkits themselves aren't delivered.
    pub async fn environment_catalog(&self, environment: Uuid) -> Result<Vec<Delivered>> {
        Ok(self
            .environment_catalogs(&[environment])
            .await?
            .remove(&environment)
            .unwrap_or_default())
    }

    /// [`Store::environment_catalog`] for several environments at once.
    pub async fn environment_catalogs(
        &self,
        environments: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<Delivered>>> {
        // One row per grant: the environment's own (toolkit null), then each
        // unquarantined listed toolkit's applied members.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select g.environment_id, g.toolkit_order, g.member_order, g.tools, g.toolkit_id, \
             ts.name as toolkit_source, t.name as toolkit_name, \
             c.id as entry_id, c.kind, s.name as source, c.name, c.applied_revision_id, \
             c.quarantined_at is not null as quarantined, c.removed_at is not null as removed \
             from ( \
               select ee.environment_id, ee.entry_id, ee.tools, null::uuid as toolkit_id, \
                 0 as toolkit_order, \
                 array_position({KIND_ORDER}, ee.kind) * 100000 + ee.position as member_order \
               from environment_entries ee \
               where ee.environment_id = any($1) and ee.kind <> 'toolkit' \
               union all \
               select ee.environment_id, m.entry_id, m.tools, t.id, 1 + ee.position, \
                 array_position({KIND_ORDER}, m.kind) * 100000 + m.position \
               from environment_entries ee \
               join catalog_entries t on t.id = ee.entry_id \
               join catalog_toolkit_members m on m.revision_id = t.applied_revision_id \
               where ee.environment_id = any($1) and ee.kind = 'toolkit' \
                 and t.quarantined_at is null \
             ) g \
             join catalog_entries c on c.id = g.entry_id \
             join catalog_sources s on s.id = c.source_id \
             left join catalog_entries t on t.id = g.toolkit_id \
             left join catalog_sources ts on ts.id = t.source_id \
             order by g.environment_id, g.toolkit_order, g.member_order"
        )))
        .bind(environments)
        .fetch_all(&self.pool)
        .await?;
        let mut out: HashMap<Uuid, Vec<Delivered>> = HashMap::new();
        // Where each entry is in its environment's list.
        let mut at: HashMap<(Uuid, Uuid), usize> = HashMap::new();
        for row in rows {
            let env: Uuid = row.try_get("environment_id")?;
            let entry: Uuid = row.try_get("entry_id")?;
            let tools: Option<Vec<String>> = row.try_get("tools")?;
            let via = match row.try_get::<Option<Uuid>, _>("toolkit_id")? {
                Some(_) => Some(EntryKey {
                    kind: EntryKind::Toolkit,
                    source: source_name(&row.try_get::<String, _>("toolkit_source")?)?,
                    name: entry_name(&row.try_get::<String, _>("toolkit_name")?)?,
                }),
                None => None,
            };
            let list = out.entry(env).or_default();
            let i = match at.get(&(env, entry)) {
                Some(i) => {
                    let d = &mut list[*i];
                    d.tools = match (d.tools.take(), tools) {
                        (Some(mut a), Some(b)) => {
                            for t in b {
                                if !a.contains(&t) {
                                    a.push(t);
                                }
                            }
                            a.sort();
                            Some(a)
                        }
                        _ => None,
                    };
                    *i
                }
                None => {
                    let kind = entry_kind(&row.try_get::<String, _>("kind")?)?;
                    list.push(Delivered {
                        key: EntryKey {
                            kind,
                            source: source_name(&row.try_get::<String, _>("source")?)?,
                            name: entry_name(&row.try_get::<String, _>("name")?)?,
                        },
                        revision: row.try_get("applied_revision_id")?,
                        tools: tools.map(|mut t| {
                            t.sort();
                            t
                        }),
                        listed: false,
                        via: Vec::new(),
                        quarantined: row.try_get("quarantined")?,
                        removed: row.try_get("removed")?,
                    });
                    at.insert((env, entry), list.len() - 1);
                    list.len() - 1
                }
            };
            match via {
                Some(toolkit) => {
                    if !list[i].via.contains(&toolkit) {
                        list[i].via.push(toolkit);
                    }
                }
                None => list[i].listed = true,
            }
        }
        Ok(out)
    }

    /// Deletes the environment with its keys, its budgets and the budgets of
    /// its keys.
    pub async fn delete_environment(&self, name: &EnvName) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("delete from environments where name = $1")
            .bind(name.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Err(StoreError::NotFound);
        }
        for kind in [
            ChangeKind::Environments,
            ChangeKind::Keys,
            ChangeKind::Budgets,
        ] {
            notify(&mut tx, kind).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
