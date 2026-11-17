//! Container registries' credentials, used to resolve image tags to
//! digests at sync. Passwords are sealed.

use sekisho_core::secret_box::SecretBox;
use sqlx::Row;

use crate::error::{FieldError, bad_column, check};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// `host[:port]`, lowercase: DNS labels, and a port of 1-5 digits.
fn normal_host(host: &str) -> Option<String> {
    let host = host.to_lowercase();
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host.as_str(), None),
    };
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    };
    let ok = name.len() <= 253
        && name.split('.').all(label_ok)
        && port.is_none_or(|p| (1..=5).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_digit()));
    ok.then_some(host)
}

impl Store {
    /// Sets the credential for `host`, replacing any; answers the host as
    /// stored (lowercase).
    pub async fn set_registry_credential(
        &self,
        secret_box: &SecretBox,
        host: &str,
        username: &str,
        password: &str,
    ) -> Result<String> {
        let mut errors = Vec::new();
        let host = normal_host(host);
        if host.is_none() {
            errors.push(FieldError::new(
                "host",
                "use a registry's host, with an optional :port",
            ));
        }
        if username.is_empty() || username.chars().any(char::is_control) {
            errors.push(FieldError::new("username", "must not be empty"));
        }
        if password.is_empty() || password.chars().any(char::is_control) {
            errors.push(FieldError::new("password", "must not be empty"));
        }
        check(errors)?;
        let sealed = secret_box.seal(password.as_bytes());
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into registry_credentials (host, username, sealed_password) \
             values ($1, $2, $3) \
             on conflict (host) do update set username = excluded.username, \
             sealed_password = excluded.sealed_password, updated_at = now()",
        )
        .bind(&host)
        .bind(username)
        .bind(sealed)
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        tx.commit().await?;
        Ok(host.unwrap_or_default())
    }

    pub async fn delete_registry_credential(&self, host: &str) -> Result<()> {
        let host = normal_host(host).ok_or(StoreError::NotFound)?;
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("delete from registry_credentials where host = $1")
            .bind(host)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        notify(&mut tx, ChangeKind::Catalog).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Every host with a credential, and its username; never a password.
    pub async fn registry_hosts(&self) -> Result<Vec<(String, String)>> {
        Ok(
            sqlx::query_as("select host, username from registry_credentials order by host")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// The username and password for `host`, for the sync alone.
    pub async fn registry_credential(
        &self,
        secret_box: &SecretBox,
        host: &str,
    ) -> Result<Option<(String, String)>> {
        let Some(host) = normal_host(host) else {
            return Ok(None);
        };
        let row = sqlx::query(
            "select username, sealed_password from registry_credentials where host = $1",
        )
        .bind(host)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let sealed: Vec<u8> = row.try_get("sealed_password")?;
        let password = String::from_utf8(secret_box.open(&sealed)?)
            .map_err(|_| bad_column("sealed_password", "not UTF-8"))?;
        Ok(Some((row.try_get("username")?, password)))
    }
}
