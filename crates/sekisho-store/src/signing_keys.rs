//! Ed25519 keys for session tokens, with their seeds sealed.
//!
//! A new key is published [`KEY_LEAD`] before it signs, so every replica
//! knows it first; a retired key stays published for [`KEY_TAIL`], until the
//! last token it signed has expired.

use sekisho_core::secret_box::SecretBox;
use sekisho_core::session_token::{PublicKey, SigningKey};
use sqlx::PgConnection;
use time::{Duration, OffsetDateTime};

use crate::error::bad_column;
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store};

/// How long a key signs before the next one is made.
pub const KEY_ROTATION: Duration = Duration::days(30);
/// How long a new key is published before it signs.
pub const KEY_LEAD: Duration = Duration::seconds(60);
/// How long a retired key stays published: an access token's hour and a
/// margin.
pub const KEY_TAIL: Duration = Duration::minutes(65);
/// `pg_advisory_xact_lock` key that serialises rotations across replicas.
const ROTATION_LOCK: i64 = 0x5e4b_1501;

fn x(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| bad_column("signing key", "not 32 bytes"))
}

/// The keys published at `now`, oldest first.
pub(crate) async fn published(
    conn: &mut PgConnection,
    now: OffsetDateTime,
) -> Result<Vec<PublicKey>> {
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "select kid, public_x from signing_keys \
         where retired_at is null or retired_at > $1 order by not_before, kid",
    )
    .bind(now - KEY_TAIL)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|(kid, public_x)| {
            Ok(PublicKey {
                kid,
                x: x(public_x)?,
            })
        })
        .collect()
}

async fn insert(
    conn: &mut PgConnection,
    secret_box: &SecretBox,
    not_before: OffsetDateTime,
) -> Result<()> {
    let key = SigningKey::generate();
    sqlx::query(
        "insert into signing_keys (kid, seed_ciphertext, public_x, not_before) \
         values ($1, $2, $3, $4)",
    )
    .bind(key.kid())
    .bind(secret_box.seal(key.seed()))
    .bind(&key.public().x[..])
    .bind(not_before)
    .execute(conn)
    .await?;
    Ok(())
}

impl Store {
    /// Makes a signing key when there is none, or when `force` is set or the
    /// newest has signed for [`KEY_ROTATION`]; then the new key signs from
    /// `now` + [`KEY_LEAD`], when the older keys retire. Deletes keys retired
    /// more than [`KEY_TAIL`] ago. `true` if it made a key. Replicas rotating
    /// at once make one key.
    pub async fn rotate_signing_key(
        &self,
        secret_box: &SecretBox,
        now: OffsetDateTime,
        force: bool,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("select pg_advisory_xact_lock($1)")
            .bind(ROTATION_LOCK)
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query("delete from signing_keys where retired_at <= $1")
            .bind(now - KEY_TAIL)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let newest: Option<OffsetDateTime> =
            sqlx::query_scalar("select max(not_before) from signing_keys where retired_at is null")
                .fetch_one(&mut *tx)
                .await?;
        let made = match newest {
            None => {
                insert(&mut tx, secret_box, now).await?;
                true
            }
            Some(newest) if force || now - newest >= KEY_ROTATION => {
                let switch = now + KEY_LEAD;
                sqlx::query("update signing_keys set retired_at = $1 where retired_at is null")
                    .bind(switch)
                    .execute(&mut *tx)
                    .await?;
                insert(&mut tx, secret_box, switch).await?;
                true
            }
            Some(_) => false,
        };
        if made || deleted > 0 {
            notify(&mut tx, ChangeKind::SigningKeys).await?;
        }
        tx.commit().await?;
        Ok(made)
    }

    /// The key that signs at `now`: the newest whose time has come.
    pub async fn active_signing_key(
        &self,
        secret_box: &SecretBox,
        now: OffsetDateTime,
    ) -> Result<Option<SigningKey>> {
        let row: Option<(String, Vec<u8>)> = sqlx::query_as(
            "select kid, seed_ciphertext from signing_keys \
             where not_before <= $1 order by not_before desc limit 1",
        )
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(kid, sealed)| {
            let seed = x(secret_box.open(&sealed)?)?;
            Ok(SigningKey::from_seed(kid, seed))
        })
        .transpose()
    }

    /// The keys to publish at `now`: every key not retired for longer than
    /// [`KEY_TAIL`], including one that doesn't sign yet.
    pub async fn published_keys(&self, now: OffsetDateTime) -> Result<Vec<PublicKey>> {
        let mut conn = self.pool.acquire().await?;
        published(&mut conn, now).await
    }
}
