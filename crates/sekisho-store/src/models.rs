use sekisho_core::catalog;
use sekisho_core::formats::{ProviderKind, is_claude};
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{ModelName, ProviderName};
use sekisho_core::pricing::{CacheDefaults, ModelPrices};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::error::{FieldError, bad_column, check, is_foreign_key_violation, is_unique_violation};
use crate::notify::{ChangeKind, notify};
use crate::providers::{
    CLAUDE_ON_AZURE_CREDENTIALS, CLAUDE_ON_AZURE_ENTRA, CLAUDE_ON_AZURE_FOUNDRY, CredentialKind,
    is_foundry, options_from_json,
};
use crate::{Result, Store, StoreError};

/// A model to add. Prices are micro-USD per 1,000,000 tokens; leave input
/// and output `None` to use agentgateway's catalog prices. An
/// OpenAI-compatible provider's models need prices unless the catalog has
/// the model under the provider's catalog key. Cache prices are optional and
/// only set with input and output prices; when unset, the model's usual
/// ratios apply ([`CacheDefaults::for_model`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewModel {
    pub name: ModelName,
    pub provider: ProviderName,
    pub input_price: Option<MicroUsd>,
    pub output_price: Option<MicroUsd>,
    pub cache_read_price: Option<MicroUsd>,
    pub cache_write_price: Option<MicroUsd>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: Uuid,
    pub name: ModelName,
    pub provider: ProviderName,
    pub provider_kind: ProviderKind,
    pub input_price: Option<MicroUsd>,
    pub output_price: Option<MicroUsd>,
    pub cache_read_price: Option<MicroUsd>,
    pub cache_write_price: Option<MicroUsd>,
}

impl Model {
    /// The admin's prices, or `None` when agentgateway's catalog prices it.
    pub fn prices(&self) -> Option<ModelPrices> {
        Some(ModelPrices {
            input: self.input_price?,
            output: self.output_price?,
            cache_read: self.cache_read_price,
            cache_write: self.cache_write_price,
        })
    }
}

/// What charging needs to know about a model: whose it is, where
/// agentgateway's catalog files its prices, where its missing cache prices
/// come from, and the admin's prices if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPricing {
    pub provider: ProviderName,
    pub kind: ProviderKind,
    /// The kind's own catalog key, or an OpenAI-compatible host's; `None`
    /// for a host without one.
    pub catalog_key: Option<String>,
    pub cache: CacheDefaults,
    pub prices: Option<ModelPrices>,
}

/// Expands to the model query without its filter, for building fixed SQL
/// with `concat!`.
macro_rules! select {
    () => {
        "select m.id, m.name, p.name as provider, p.kind as provider_kind, \
         p.catalog_key as provider_catalog_key, \
         m.input_price_micro_usd, m.output_price_micro_usd, \
         m.cache_read_price_micro_usd, m.cache_write_price_micro_usd \
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
    let priced = m.input_price.is_some() && m.output_price.is_some();
    for (field, price) in [
        ("cache_read_price", m.cache_read_price),
        ("cache_write_price", m.cache_write_price),
    ] {
        match price {
            Some(_) if !priced => {
                errors.push(FieldError::new(field, "set input and output prices first"))
            }
            Some(p) if p.0 < 0 => errors.push(FieldError::new(field, "must not be negative")),
            _ => {}
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
        cache_read_price: price("cache_read_price_micro_usd")?,
        cache_write_price: price("cache_write_price_micro_usd")?,
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
        // Share-locked, so a provider update that its models must follow
        // (see `update_provider`) waits for this model, or this waits for it.
        let provider = sqlx::query(
            "select id, kind, options::text as options, credential_kind, catalog_key \
             from providers where name = $1 for share",
        )
        .bind(new.provider.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(unknown_provider)?;
        let provider_id: Uuid = provider.try_get("id")?;
        let kind: String = provider.try_get("kind")?;
        let kind = ProviderKind::parse(&kind).ok_or_else(|| bad_column("provider kind", &kind))?;
        let mut errors = Vec::new();
        if kind == ProviderKind::OpenAiCompatible && new.input_price.is_none() {
            let catalog_key: Option<String> = provider.try_get("catalog_key")?;
            if !catalog_key
                .as_deref()
                .is_some_and(|key| catalog::knows(key, new.name.as_str()))
            {
                let why = match catalog_key {
                    Some(key) => format!(
                        "agentgateway's catalog has no prices for {} under {key}",
                        new.name
                    ),
                    None => "its provider has no catalog key, so agentgateway's catalog \
                         can't price it"
                        .to_owned(),
                };
                let message = format!("an OpenAI-compatible provider's model needs prices: {why}");
                errors.push(FieldError::new("input_price", message.clone()));
                errors.push(FieldError::new("output_price", message));
            }
        }
        if is_claude(kind, new.name.as_str()) && kind == ProviderKind::Azure {
            let options: String = provider.try_get("options")?;
            if !is_foundry(&options_from_json(kind, &options)?) {
                errors.push(FieldError::new("name", CLAUDE_ON_AZURE_FOUNDRY));
            }
            let credential: String = provider.try_get("credential_kind")?;
            let credential = CredentialKind::parse(&credential)
                .ok_or_else(|| bad_column("credential kind", &credential))?;
            if !CLAUDE_ON_AZURE_CREDENTIALS.contains(&credential) {
                errors.push(FieldError::new("name", CLAUDE_ON_AZURE_ENTRA));
            }
        }
        check(errors)?;
        let id: Uuid = sqlx::query_scalar(
            "insert into models (name, provider_id, input_price_micro_usd, \
             output_price_micro_usd, cache_read_price_micro_usd, cache_write_price_micro_usd) \
             values ($1, $2, $3, $4, $5, $6) returning id",
        )
        .bind(new.name.as_str())
        .bind(provider_id)
        .bind(new.input_price.map(|p| p.0))
        .bind(new.output_price.map(|p| p.0))
        .bind(new.cache_read_price.map(|p| p.0))
        .bind(new.cache_write_price.map(|p| p.0))
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

    /// The model named `name` with what charging needs, or `None` when there
    /// is no such model.
    pub async fn model_pricing(&self, name: &ModelName) -> Result<Option<ModelPricing>> {
        let Some(row) = sqlx::query(concat!(select!(), " where m.name = $1"))
            .bind(name.as_str())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let model = from_row(&row)?;
        let host_key: Option<String> = row.try_get("provider_catalog_key")?;
        Ok(Some(ModelPricing {
            catalog_key: model
                .provider_kind
                .catalog_key()
                .map(str::to_owned)
                .or(host_key),
            cache: CacheDefaults::for_model(model.provider_kind, model.name.as_str()),
            prices: model.prices(),
            provider: model.provider,
            kind: model.provider_kind,
        }))
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
