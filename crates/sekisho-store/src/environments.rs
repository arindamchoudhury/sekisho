use std::collections::{HashMap, HashSet};

use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::names::{EnvName, ModelName};
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{FieldError, bad_column, check};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// What an admin sets on an environment. `models` keeps its order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentSpec {
    pub name: EnvName,
    pub description: String,
    /// Shown to people when they start using the environment.
    pub notice: String,
    pub access_groups: Vec<String>,
    pub models: Vec<ModelName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub id: Uuid,
    pub spec: EnvironmentSpec,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Expands to the environment query up to its filter; finish it with
/// `group by e.id`.
macro_rules! select_environments {
    () => {
        "select e.id, e.name, e.description, e.notice, e.access_groups, e.created_at, \
         e.updated_at, coalesce(array_agg(m.name order by em.position) \
         filter (where m.id is not null), '{}') as models \
         from environments e \
         left join environment_models em on em.environment_id = e.id \
         left join models m on m.id = em.model_id"
    };
}

/// A listed model that exists, with what the format rule needs.
struct KnownModel {
    id: Uuid,
    provider: String,
    format: ApiFormat,
}

fn validate(spec: &EnvironmentSpec, known: &HashMap<String, KnownModel>) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    for (i, model) in spec.models.iter().enumerate() {
        let field = format!("models[{i}]");
        if !seen.insert(model.as_str()) {
            errors.push(FieldError::new(field, format!("{model} is listed twice")));
        } else if !known.contains_key(model.as_str()) {
            errors.push(FieldError::new(field, format!("no model named {model}")));
        }
    }

    // Release 1 routes each API format of an environment to one provider.
    let mut providers_by_format: Vec<(ApiFormat, Vec<&str>)> = Vec::new();
    for model in &spec.models {
        let Some(m) = known.get(model.as_str()) else {
            continue;
        };
        match providers_by_format.iter_mut().find(|(f, _)| *f == m.format) {
            Some((_, providers)) if !providers.contains(&m.provider.as_str()) => {
                providers.push(&m.provider)
            }
            Some(_) => {}
            None => providers_by_format.push((m.format, vec![&m.provider])),
        }
    }
    for (format, providers) in providers_by_format {
        if providers.len() > 1 {
            errors.push(FieldError::new(
                "models",
                format!(
                    "providers {} all serve the {} API; an environment can use one provider per API format",
                    providers.join(" and "),
                    format.path_segment()
                ),
            ));
        }
    }

    let mut groups = HashSet::new();
    for (i, group) in spec.access_groups.iter().enumerate() {
        let field = format!("access_groups[{i}]");
        if group.trim().is_empty() {
            errors.push(FieldError::new(field, "must not be empty"));
        } else if !groups.insert(group.as_str()) {
            errors.push(FieldError::new(field, format!("{group} is listed twice")));
        }
    }
    errors
}

fn from_row(row: &PgRow) -> Result<Environment> {
    let name: String = row.try_get("name")?;
    let models: Vec<String> = row.try_get("models")?;
    let models = models
        .iter()
        .map(|m| ModelName::parse(m).map_err(|_| bad_column("model name", m)))
        .collect::<Result<_>>()?;
    Ok(Environment {
        id: row.try_get("id")?,
        spec: EnvironmentSpec {
            name: EnvName::parse(&name).map_err(|_| bad_column("environment name", &name))?,
            description: row.try_get("description")?,
            notice: row.try_get("notice")?,
            access_groups: row.try_get("access_groups")?,
            models,
        },
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

impl Store {
    /// Creates the environment, or replaces the spec of the one with this
    /// name (keeping its id, keys and budgets).
    pub async fn upsert_environment(&self, spec: EnvironmentSpec) -> Result<Environment> {
        let names: Vec<String> = spec.models.iter().map(|m| m.as_str().to_owned()).collect();
        let mut tx = self.pool.begin().await?;
        // The share lock makes a concurrent delete_model of a listed model
        // wait, then find this environment and refuse.
        let rows = sqlx::query(
            "select m.id, m.name, p.name as provider, p.kind from models m \
             join providers p on p.id = m.provider_id \
             where m.name = any($1) for key share of m",
        )
        .bind(&names)
        .fetch_all(&mut *tx)
        .await?;
        let mut known = HashMap::new();
        for row in rows {
            let kind: String = row.try_get("kind")?;
            let kind =
                ProviderKind::parse(&kind).ok_or_else(|| bad_column("provider kind", &kind))?;
            known.insert(
                row.try_get::<String, _>("name")?,
                KnownModel {
                    id: row.try_get("id")?,
                    provider: row.try_get("provider")?,
                    format: kind.api_format(),
                },
            );
        }
        check(validate(&spec, &known))?;

        let row = sqlx::query(
            "insert into environments (name, description, notice, access_groups) \
             values ($1, $2, $3, $4) \
             on conflict (name) do update set description = excluded.description, \
             notice = excluded.notice, access_groups = excluded.access_groups, updated_at = now() \
             returning id, created_at, updated_at",
        )
        .bind(spec.name.as_str())
        .bind(&spec.description)
        .bind(&spec.notice)
        .bind(&spec.access_groups)
        .fetch_one(&mut *tx)
        .await?;
        let id: Uuid = row.try_get("id")?;
        sqlx::query("delete from environment_models where environment_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let model_ids: Vec<Uuid> = spec.models.iter().map(|m| known[m.as_str()].id).collect();
        sqlx::query(
            "insert into environment_models (environment_id, model_id, position) \
             select $1, model_id, (position - 1)::integer \
             from unnest($2::uuid[]) with ordinality as listed (model_id, position)",
        )
        .bind(id)
        .bind(&model_ids)
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Environments).await?;
        tx.commit().await?;
        Ok(Environment {
            id,
            spec,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
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
        from_row(&row)
    }

    /// All environments, sorted by name.
    pub async fn list_environments(&self) -> Result<Vec<Environment>> {
        sqlx::query(concat!(
            select_environments!(),
            " group by e.id order by e.name"
        ))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(from_row)
        .collect()
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
