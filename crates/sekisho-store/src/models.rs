use sekisho_core::formats::{ApiFormat, ProviderKind};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{ModelName, ProviderName};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::error::{FieldError, bad_column, check, is_foreign_key_violation, is_unique_violation};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// A model to add. Prices are micro-USD per 1,000,000 tokens; leave both
/// `None` to use agentgateway's catalog prices. An OpenAI-compatible
/// provider's models need prices: the catalog has none for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewModel {
    pub name: ModelName,
    pub provider: ProviderName,
    pub input_price: Option<MicroUsd>,
    pub output_price: Option<MicroUsd>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: Uuid,
    pub name: ModelName,
    pub provider: ProviderName,
    pub provider_kind: ProviderKind,
    pub input_price: Option<MicroUsd>,
    pub output_price: Option<MicroUsd>,
}

/// Expands to the model query without its filter, for building fixed SQL
/// with `concat!`.
macro_rules! select {
    () => {
        "select m.id, m.name, p.name as provider, p.kind as provider_kind, \
         m.input_price_micro_usd, m.output_price_micro_usd \
         from models m join providers p on p.id = m.provider_id"
    };
}

fn validate(m: &NewModel) -> Vec<FieldError> {
    const BOTH: &str = "set both prices, or neither to use agentgateway's catalog prices";
    let mut errors = Vec::new();
    match (m.input_price, m.output_price) {
        (None, Some(_)) => errors.push(FieldError::new("input_price", BOTH)),
        (Some(_), None) => errors.push(FieldError::new("output_price", BOTH)),
        _ => {}
    }
    for (field, price) in [
        ("input_price", m.input_price),
        ("output_price", m.output_price),
    ] {
        if price.is_some_and(|p| p.0 < 0) {
            errors.push(FieldError::new(field, "must not be negative"));
        }
    }
    errors
}

fn from_row(row: &PgRow) -> Result<Model> {
    let name: String = row.try_get("name")?;
    let provider: String = row.try_get("provider")?;
    let kind: String = row.try_get("provider_kind")?;
    let price = |column: &str| -> Result<Option<MicroUsd>> {
        Ok(row.try_get::<Option<i64>, _>(column)?.map(MicroUsd))
    };
    Ok(Model {
        id: row.try_get("id")?,
        name: ModelName::parse(&name).map_err(|_| bad_column("model name", &name))?,
        provider: ProviderName::parse(&provider)
            .map_err(|_| bad_column("provider name", &provider))?,
        provider_kind: ProviderKind::parse(&kind)
            .ok_or_else(|| bad_column("provider kind", &kind))?,
        input_price: price("input_price_micro_usd")?,
        output_price: price("output_price_micro_usd")?,
    })
}

impl Store {
    pub async fn create_model(&self, new: NewModel) -> Result<Model> {
        check(validate(&new))?;
        let unknown_provider = || {
            StoreError::Validation(vec![FieldError::new(
                "provider",
                format!("no provider named {}", new.provider),
            )])
        };
        let mut tx = self.pool.begin().await?;
        let (provider_id, kind, format): (Uuid, String, String) =
            sqlx::query_as("select id, kind, api_format from providers where name = $1")
                .bind(new.provider.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(unknown_provider)?;
        if kind == ProviderKind::OpenAiCompatible.as_str() && new.input_price.is_none() {
            const NEEDED: &str = "an OpenAI-compatible provider's models need prices:                                   agentgateway's catalog has none for them";
            return Err(StoreError::Validation(vec![
                FieldError::new("input_price", NEEDED),
                FieldError::new("output_price", NEEDED),
            ]));
        }
        // agentgateway sends an Azure deployment to Foundry's
        // /anthropic/v1/messages only when its name starts with "claude".
        if kind == ProviderKind::Azure.as_str()
            && format == ApiFormat::Anthropic.as_str()
            && !new.name.as_str().to_ascii_lowercase().starts_with("claude")
        {
            return Err(StoreError::Validation(vec![FieldError::new(
                "name",
                "Claude on Azure is served by deployments whose names start with claude",
            )]));
        }
        let id: Uuid = sqlx::query_scalar(
            "insert into models (name, provider_id, input_price_micro_usd, output_price_micro_usd) \
             values ($1, $2, $3, $4) returning id",
        )
        .bind(new.name.as_str())
        .bind(provider_id)
        .bind(new.input_price.map(|p| p.0))
        .bind(new.output_price.map(|p| p.0))
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::Conflict(format!("a model named {} already exists", new.name))
            } else if is_foreign_key_violation(&e) {
                unknown_provider() // deleted meanwhile
            } else {
                e.into()
            }
        })?;
        let row = sqlx::query(concat!(select!(), " where m.id = $1"))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        notify(&mut tx, ChangeKind::Models).await?;
        tx.commit().await?;
        from_row(&row)
    }

    /// All models with their providers, sorted by model name.
    pub async fn list_models(&self) -> Result<Vec<Model>> {
        sqlx::query(concat!(select!(), " order by m.name"))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(from_row)
            .collect()
    }

    /// Deletes a model that no environment lists.
    pub async fn delete_model(&self, name: &ModelName) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let id: Uuid = sqlx::query_scalar("select id from models where name = $1 for update")
            .bind(name.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let environments: Vec<String> = sqlx::query_scalar(
            "select e.name from environment_models em \
             join environments e on e.id = em.environment_id \
             where em.model_id = $1 order by e.name",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        if !environments.is_empty() {
            return Err(StoreError::InUse(format!(
                "environments: {}",
                environments.join(", ")
            )));
        }
        sqlx::query("delete from models where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if is_foreign_key_violation(&e) {
                    StoreError::InUse("environments".into())
                } else {
                    e.into()
                }
            })?;
        notify(&mut tx, ChangeKind::Models).await?;
        tx.commit().await?;
        Ok(())
    }
}
