use std::fmt;

use sekisho_core::formats::ProviderKind;
use sekisho_core::names::ProviderName;
use sekisho_core::secret_box::{SecretBox, SecretError};
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{FieldError, bad_column, check, is_foreign_key_violation, is_unique_violation};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

pub struct NewProvider {
    pub name: ProviderName,
    pub kind: ProviderKind,
    /// Required for `OpenAiCompatible`; otherwise overrides the provider's
    /// default endpoint.
    pub base_url: Option<String>,
    /// Stored encrypted and never returned by listings.
    pub api_key: String,
}

impl fmt::Debug for NewProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewProvider")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// A provider as listed. The API key is read only through
/// [`Store::provider_api_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: Uuid,
    pub name: ProviderName,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Expands to the provider columns, for building fixed SQL with `concat!`.
macro_rules! columns {
    () => {
        "id, name, kind, base_url, created_at"
    };
}

fn validate(p: &NewProvider) -> Vec<FieldError> {
    let mut errors = Vec::new();
    match &p.base_url {
        None if p.kind == ProviderKind::OpenAiCompatible => errors.push(FieldError::new(
            "base_url",
            "an OpenAI-compatible provider needs a base URL",
        )),
        Some(url) if !is_http_url(url) => errors.push(FieldError::new(
            "base_url",
            "use an http:// or https:// URL with a host",
        )),
        _ => {}
    }
    if p.api_key.is_empty() {
        errors.push(FieldError::new("api_key", "must not be empty"));
    }
    errors
}

fn is_http_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn from_row(row: &PgRow) -> Result<Provider> {
    let name: String = row.try_get("name")?;
    let kind: String = row.try_get("kind")?;
    Ok(Provider {
        id: row.try_get("id")?,
        name: ProviderName::parse(&name).map_err(|_| bad_column("provider name", &name))?,
        kind: ProviderKind::parse(&kind).ok_or_else(|| bad_column("provider kind", &kind))?,
        base_url: row.try_get("base_url")?,
        created_at: row.try_get("created_at")?,
    })
}

impl Store {
    /// Adds a provider, sealing its API key with `secret_box`.
    pub async fn create_provider(
        &self,
        secret_box: &SecretBox,
        new: NewProvider,
    ) -> Result<Provider> {
        check(validate(&new))?;
        let ciphertext = secret_box.seal(new.api_key.as_bytes());
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(concat!(
            "insert into providers (name, kind, base_url, api_key_ciphertext) \
             values ($1, $2, $3, $4) returning ",
            columns!()
        ))
        .bind(new.name.as_str())
        .bind(new.kind.as_str())
        .bind(&new.base_url)
        .bind(&ciphertext)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::Conflict(format!("a provider named {} already exists", new.name))
            } else {
                e.into()
            }
        })?;
        notify(&mut tx, ChangeKind::Providers).await?;
        tx.commit().await?;
        from_row(&row)
    }

    /// All providers, sorted by name.
    pub async fn list_providers(&self) -> Result<Vec<Provider>> {
        sqlx::query(concat!(
            "select ",
            columns!(),
            " from providers order by name"
        ))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(from_row)
        .collect()
    }

    pub async fn get_provider(&self, name: &ProviderName) -> Result<Provider> {
        let row = sqlx::query(concat!(
            "select ",
            columns!(),
            " from providers where name = $1"
        ))
        .bind(name.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        from_row(&row)
    }

    /// Decrypts a provider's API key, for rendering the gateway config.
    pub async fn provider_api_key(&self, secret_box: &SecretBox, id: Uuid) -> Result<String> {
        let sealed: Vec<u8> =
            sqlx::query_scalar("select api_key_ciphertext from providers where id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(StoreError::NotFound)?;
        let key = secret_box.open(&sealed)?;
        String::from_utf8(key).map_err(|_| StoreError::Secret(SecretError::Decrypt))
    }

    /// Deletes a provider that no model uses.
    pub async fn delete_provider(&self, name: &ProviderName) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // The row lock makes a concurrent create_model for this provider wait,
        // then fail on its foreign key.
        let id: Uuid = sqlx::query_scalar("select id from providers where name = $1 for update")
            .bind(name.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let models: Vec<String> =
            sqlx::query_scalar("select name from models where provider_id = $1 order by name")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        if !models.is_empty() {
            return Err(StoreError::InUse(format!("models: {}", models.join(", "))));
        }
        sqlx::query("delete from providers where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if is_foreign_key_violation(&e) {
                    StoreError::InUse("models".into())
                } else {
                    e.into()
                }
            })?;
        notify(&mut tx, ChangeKind::Providers).await?;
        tx.commit().await?;
        Ok(())
    }
}
