//! Git mode (D175): environments kept in one repository, read through the
//! catalog source named `environments`, and the GitHub token that opens
//! pull requests for changes.

use sekisho_catalog::names::{SourceKind, SourceName};
use sekisho_catalog::source::{Folder, GitRef, SourceUrl};
use sekisho_core::secret_box::SecretBox;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use super::sources::open_token;
use super::{NewSource, Source, SourcePatch};
use crate::error::{FieldError, check};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

const MAX_TOKEN: usize = 4096;

/// Git mode's settings: the repository, as for any source, and GitHub.
#[derive(Clone, Debug)]
pub struct GitModeSettings {
    pub url: SourceUrl,
    pub git_ref: Option<GitRef>,
    pub folder: Folder,
    pub username: Option<String>,
    /// Reads the repository.
    pub token: Option<String>,
    pub allow_private_network: bool,
    /// GitHub's REST API, such as `https://api.github.com`.
    pub github_api_url: String,
    /// Opens pull requests: contents and pull requests, read and write.
    pub github_token: String,
}

/// Git mode as it is on. Neither token is in it.
#[derive(Clone, Debug, PartialEq)]
pub struct GitMode {
    pub source: Source,
    pub github_api_url: String,
    /// Owns the environments Git mode creates.
    pub enabled_by: Uuid,
    pub enabled_at: OffsetDateTime,
}

impl Store {
    /// Turns Git mode on, or changes its settings: the `environments`
    /// source is added or replaced (a token not given is cleared), and
    /// `actor` becomes the owner of environments it creates from now on.
    pub async fn enable_git_mode(
        &self,
        secret_box: &SecretBox,
        settings: GitModeSettings,
        actor: Uuid,
    ) -> Result<GitMode> {
        let token = settings.github_token.as_str();
        if token.is_empty()
            || token.len() > MAX_TOKEN
            || token.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            check(vec![FieldError::new(
                "github_token",
                "use 1-4096 characters without spaces or control characters",
            )])?;
        }
        let name = SourceName::git_mode();
        let source = match self.get_source(&name).await {
            Ok(_) => {
                self.update_source(
                    secret_box,
                    &name,
                    SourcePatch {
                        url: Some(settings.url),
                        git_ref: Some(settings.git_ref),
                        folder: Some(settings.folder),
                        username: Some(settings.username),
                        token: Some(settings.token),
                        allow_private_network: Some(settings.allow_private_network),
                    },
                )
                .await?
            }
            Err(StoreError::NotFound) => {
                self.add_source(
                    secret_box,
                    NewSource {
                        name: name.clone(),
                        kind: SourceKind::Environments,
                        url: settings.url,
                        git_ref: settings.git_ref,
                        folder: settings.folder,
                        username: settings.username,
                        token: settings.token,
                        allow_private_network: settings.allow_private_network,
                    },
                    actor,
                )
                .await?
            }
            Err(e) => return Err(e),
        };
        let sealed = secret_box.seal(token.as_bytes());
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into git_mode (source_id, github_api_url, sealed_github_token, enabled_by) \
             values ($1, $2, $3, $4) \
             on conflict (singleton) do update set source_id = excluded.source_id, \
             github_api_url = excluded.github_api_url, \
             sealed_github_token = excluded.sealed_github_token, \
             enabled_by = excluded.enabled_by, enabled_at = now()",
        )
        .bind(source.id)
        .bind(&settings.github_api_url)
        .bind(sealed)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Environments).await?;
        tx.commit().await?;
        self.git_mode().await?.ok_or(StoreError::NotFound)
    }

    /// Turns Git mode off: its source goes, with its syncs; environments
    /// stay, as ordinary ones. NotFound when it is off.
    pub async fn disable_git_mode(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let source: Uuid = sqlx::query_scalar("select source_id from git_mode for update")
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        sqlx::query("update environments set git_path = null where git_path is not null")
            .execute(&mut *tx)
            .await?;
        // The git_mode row and the syncs go with the source.
        sqlx::query("delete from catalog_sources where id = $1")
            .bind(source)
            .execute(&mut *tx)
            .await?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        notify(&mut tx, ChangeKind::Environments).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Git mode's settings, or `None` when it is off.
    pub async fn git_mode(&self) -> Result<Option<GitMode>> {
        let Some(row) = sqlx::query("select github_api_url, enabled_by, enabled_at from git_mode")
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(GitMode {
            source: self.get_source(&SourceName::git_mode()).await?,
            github_api_url: row.try_get("github_api_url")?,
            enabled_by: row.try_get("enabled_by")?,
            enabled_at: row.try_get("enabled_at")?,
        }))
    }

    /// The GitHub token, for proposals alone.
    pub async fn github_token(&self, secret_box: &SecretBox) -> Result<Option<String>> {
        let sealed: Option<Vec<u8>> =
            sqlx::query_scalar("select sealed_github_token from git_mode")
                .fetch_optional(&self.pool)
                .await?;
        open_token(secret_box, sealed)
    }
}
