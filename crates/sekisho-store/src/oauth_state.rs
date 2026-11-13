//! Short-lived OAuth state: sign-ins waiting for Dex, the authorisation codes
//! Sekisho gives its clients, and RFC 8628 device codes.
//!
//! The values handed out (Dex `state`, codes, device codes) are stored only
//! as SHA-256; Sekisho's PKCE verifier towards Dex is sealed.

use std::fmt;

use sekisho_core::keys::{hash_secret, random_bytes, random_secret};
use sekisho_core::secret_box::SecretBox;
use sqlx::{PgConnection, Row};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{bad_column, is_unique_violation};
use crate::{Result, Store, StoreError};

/// How long a sign-in may wait for Dex.
pub const AUTH_REQUEST_LIFETIME: Duration = Duration::minutes(10);
/// How long an authorisation code works, once.
pub const CODE_LIFETIME: Duration = Duration::seconds(60);
/// How long a device code works.
pub const DEVICE_CODE_LIFETIME: Duration = Duration::minutes(10);
/// How often a device may poll, at first.
pub const DEVICE_INTERVAL: Duration = Duration::seconds(5);
/// What `slow_down` adds to a device's interval.
pub const SLOW_DOWN_STEP: Duration = Duration::seconds(5);
/// Device codes are kept this long after they expire, so a late poll is
/// told `expired_token` rather than that the code is unknown.
const EXPIRED_DEVICES_KEPT: Duration = Duration::hours(1);
/// User codes: 20 consonants, so they spell no words and avoid look-alikes.
const USER_CODE_LETTERS: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";
const USER_CODE_LEN: usize = 8;

/// What started a sign-in at Dex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthFlow {
    /// The authorisation code flow, for the client's redirect.
    Code {
        redirect_uri: String,
        /// The client's own `state`, returned with the code.
        client_state: Option<String>,
        /// The client's PKCE `S256` challenge.
        code_challenge: String,
    },
    /// The device flow: the device this sign-in approves.
    Device(Device),
}

/// A sign-in waiting for Dex.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthRequest {
    pub client_id: String,
    pub flow: AuthFlow,
    /// Sekisho's PKCE verifier towards Dex. Stored sealed.
    pub dex_verifier: String,
    /// The nonce Sekisho asked Dex to put in the ID token.
    pub nonce: String,
}

/// What [`Store::save_auth_request`] takes.
pub type NewAuthRequest = AuthRequest;

/// Shows everything but the verifier.
impl fmt::Debug for AuthRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthRequest")
            .field("client_id", &self.client_id)
            .field("flow", &self.flow)
            .field("nonce", &self.nonce)
            .finish_non_exhaustive()
    }
}

/// What an authorisation code grants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeGrant {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub user: Uuid,
}

/// What [`Store::save_code`] takes.
pub type NewCode = CodeGrant;

/// A new device code, for the device authorisation response.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceStart {
    /// For the device to poll with. Stored only as SHA-256.
    pub device_code: String,
    /// For the person to type, shown `XXXX-XXXX`.
    pub user_code: String,
    /// Seconds.
    pub expires_in: u64,
    /// Seconds between polls.
    pub interval: u64,
}

/// Shows everything but the device code.
impl fmt::Debug for DeviceStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceStart")
            .field("user_code", &self.user_code)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

/// A device code waiting for someone to approve it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// The SHA-256 of the device code.
    key: [u8; 32],
    pub client_id: String,
    /// Shown `XXXX-XXXX`.
    pub user_code: String,
    pub expires_at: OffsetDateTime,
}

/// What a device's poll gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevicePoll {
    Pending,
    /// Polled sooner than its interval, which has now grown by
    /// [`SLOW_DOWN_STEP`].
    SlowDown,
    /// Approved by this user: tokens are issued this once.
    Approved(Uuid),
    Denied,
    Expired,
    /// No such device code for this client, or its tokens were issued.
    Unknown,
}

/// A device code's row as a poll reads it: client, status, approving user,
/// interval in seconds, last poll and expiry.
type PollRow = (
    String,
    String,
    Option<Uuid>,
    i32,
    Option<OffsetDateTime>,
    OffsetDateTime,
);

fn seconds(d: Duration) -> u64 {
    u64::try_from(d.whole_seconds()).expect("a positive constant")
}

/// 8 letters from [`USER_CODE_LETTERS`], each equally likely.
fn new_user_code() -> String {
    let mut code = String::with_capacity(USER_CODE_LEN);
    while code.len() < USER_CODE_LEN {
        for b in random_bytes::<16>() {
            // 240 is the largest multiple of 20 below 256: rejecting the rest
            // keeps every letter equally likely.
            if b < 240 && code.len() < USER_CODE_LEN {
                code.push(char::from(USER_CODE_LETTERS[usize::from(b % 20)]));
            }
        }
    }
    code
}

fn shown(user_code: &str) -> String {
    format!("{}-{}", &user_code[..4], &user_code[4..])
}

/// What a person typed, as stored: case, dashes and spaces don't matter.
fn normalised(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == USER_CODE_LEN && code.bytes().all(|c| USER_CODE_LETTERS.contains(&c)))
        .then_some(code)
}

fn sha_column(value: Vec<u8>, column: &str) -> Result<[u8; 32]> {
    value
        .try_into()
        .map_err(|_| bad_column(column, "not 32 bytes"))
}

async fn device_by_key(conn: &mut PgConnection, key: &[u8; 32]) -> Result<Option<Device>> {
    let row: Option<(String, String, OffsetDateTime)> = sqlx::query_as(
        "select client_id, user_code, expires_at from device_codes where device_sha256 = $1",
    )
    .bind(&key[..])
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(client_id, user_code, expires_at)| Device {
        key: *key,
        client_id,
        user_code: shown(&user_code),
        expires_at,
    }))
}

impl Store {
    /// Keeps a sign-in for Dex's answer, and returns the `state` to send Dex.
    pub async fn save_auth_request(
        &self,
        secret_box: &SecretBox,
        request: NewAuthRequest,
        now: OffsetDateTime,
    ) -> Result<String> {
        let state = random_secret();
        let (kind, redirect_uri, client_state, code_challenge, device) = match &request.flow {
            AuthFlow::Code {
                redirect_uri,
                client_state,
                code_challenge,
            } => (
                "code",
                Some(redirect_uri.as_str()),
                client_state.as_deref(),
                Some(code_challenge.as_str()),
                None,
            ),
            AuthFlow::Device(device) => ("device", None, None, None, Some(&device.key[..])),
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from auth_requests where expires_at <= $1")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into auth_requests (state_sha256, kind, client_id, redirect_uri, \
             client_state, code_challenge, device_sha256, verifier_ciphertext, nonce, expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(&hash_secret(&state)[..])
        .bind(kind)
        .bind(&request.client_id)
        .bind(redirect_uri)
        .bind(client_state)
        .bind(code_challenge)
        .bind(device)
        .bind(secret_box.seal(request.dex_verifier.as_bytes()))
        .bind(&request.nonce)
        .bind(now + AUTH_REQUEST_LIFETIME)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(state)
    }

    /// Takes the sign-in Dex's answer names, once. `None` when unknown,
    /// already taken or older than [`AUTH_REQUEST_LIFETIME`].
    pub async fn take_auth_request(
        &self,
        secret_box: &SecretBox,
        state: &str,
        now: OffsetDateTime,
    ) -> Result<Option<AuthRequest>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "delete from auth_requests where state_sha256 = $1 returning kind, client_id, \
             redirect_uri, client_state, code_challenge, device_sha256, verifier_ciphertext, \
             nonce, expires_at",
        )
        .bind(&hash_secret(state)[..])
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let expires_at: OffsetDateTime = row.try_get("expires_at")?;
        if expires_at <= now {
            tx.commit().await?;
            return Ok(None);
        }
        let kind: String = row.try_get("kind")?;
        let flow = match kind.as_str() {
            "code" => AuthFlow::Code {
                redirect_uri: row
                    .try_get::<Option<String>, _>("redirect_uri")?
                    .ok_or_else(|| bad_column("auth request redirect_uri", "null"))?,
                client_state: row.try_get("client_state")?,
                code_challenge: row
                    .try_get::<Option<String>, _>("code_challenge")?
                    .ok_or_else(|| bad_column("auth request code_challenge", "null"))?,
            },
            "device" => {
                let key = row
                    .try_get::<Option<Vec<u8>>, _>("device_sha256")?
                    .ok_or_else(|| bad_column("auth request device", "null"))?;
                match device_by_key(&mut tx, &sha_column(key, "device_sha256")?).await? {
                    Some(device) => AuthFlow::Device(device),
                    None => {
                        tx.commit().await?;
                        return Ok(None);
                    }
                }
            }
            other => return Err(bad_column("auth request kind", other)),
        };
        let sealed: Vec<u8> = row.try_get("verifier_ciphertext")?;
        let dex_verifier = String::from_utf8(secret_box.open(&sealed)?)
            .map_err(|_| bad_column("PKCE verifier", "not UTF-8"))?;
        let request = AuthRequest {
            client_id: row.try_get("client_id")?,
            flow,
            dex_verifier,
            nonce: row.try_get("nonce")?,
        };
        tx.commit().await?;
        Ok(Some(request))
    }

    /// Makes a single-use authorisation code for a client.
    pub async fn save_code(&self, grant: NewCode, now: OffsetDateTime) -> Result<String> {
        let code = random_secret();
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from auth_codes where expires_at <= $1")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into auth_codes (code_sha256, client_id, redirect_uri, code_challenge, \
             user_id, expires_at) values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&hash_secret(&code)[..])
        .bind(&grant.client_id)
        .bind(&grant.redirect_uri)
        .bind(&grant.code_challenge)
        .bind(grant.user)
        .bind(now + CODE_LIFETIME)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(code)
    }

    /// What the code grants, once, within [`CODE_LIFETIME`].
    pub async fn take_code(&self, code: &str, now: OffsetDateTime) -> Result<Option<CodeGrant>> {
        let row: Option<(String, String, String, Uuid)> = sqlx::query_as(
            "update auth_codes set used_at = $2 \
             where code_sha256 = $1 and used_at is null and expires_at > $2 \
             returning client_id, redirect_uri, code_challenge, user_id",
        )
        .bind(&hash_secret(code)[..])
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(client_id, redirect_uri, code_challenge, user)| CodeGrant {
                client_id,
                redirect_uri,
                code_challenge,
                user,
            },
        ))
    }

    /// Starts the device flow for a client.
    pub async fn create_device(&self, client_id: &str, now: OffsetDateTime) -> Result<DeviceStart> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from device_codes where expires_at <= $1")
            .bind(now - EXPIRED_DEVICES_KEPT)
            .execute(&mut *tx)
            .await?;
        // A user code already in use is drawn again (about 1 in 2.5e10).
        for _ in 0..5 {
            let device_code = random_secret();
            let user_code = new_user_code();
            let inserted = sqlx::query(
                "insert into device_codes (device_sha256, user_code, client_id, interval_s, \
                 expires_at) values ($1, $2, $3, $4, $5)",
            )
            .bind(&hash_secret(&device_code)[..])
            .bind(&user_code)
            .bind(client_id)
            .bind(i32::try_from(DEVICE_INTERVAL.whole_seconds()).expect("a small constant"))
            .bind(now + DEVICE_CODE_LIFETIME)
            .execute(&mut *tx)
            .await;
            match inserted {
                Ok(_) => {
                    tx.commit().await?;
                    return Ok(DeviceStart {
                        device_code,
                        user_code: shown(&user_code),
                        expires_in: seconds(DEVICE_CODE_LIFETIME),
                        interval: seconds(DEVICE_INTERVAL),
                    });
                }
                Err(e) if is_unique_violation(&e) => {
                    // The failed insert aborted the transaction; start over.
                    tx.rollback().await?;
                    tx = self.pool.begin().await?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(StoreError::Conflict(
            "could not draw an unused user code".into(),
        ))
    }

    /// The pending, unexpired device whose user code this is, however it was
    /// typed.
    pub async fn device_by_user_code(
        &self,
        input: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Device>> {
        let Some(code) = normalised(input) else {
            return Ok(None);
        };
        let row: Option<(Vec<u8>, String, OffsetDateTime)> = sqlx::query_as(
            "select device_sha256, client_id, expires_at from device_codes \
             where user_code = $1 and status = 'pending' and expires_at > $2",
        )
        .bind(&code)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(key, client_id, expires_at)| {
            Ok(Device {
                key: sha_column(key, "device_sha256")?,
                client_id,
                user_code: shown(&code),
                expires_at,
            })
        })
        .transpose()
    }

    /// Approves a pending device for `user`; `false` if it isn't pending.
    pub async fn approve_device(&self, device: &Device, user: Uuid) -> Result<bool> {
        let updated = sqlx::query(
            "update device_codes set status = 'approved', user_id = $2 \
             where device_sha256 = $1 and status = 'pending'",
        )
        .bind(&device.key[..])
        .bind(user)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated > 0)
    }

    /// Denies a pending device; `false` if it isn't pending.
    pub async fn deny_device(&self, device: &Device) -> Result<bool> {
        let updated = sqlx::query(
            "update device_codes set status = 'denied' \
             where device_sha256 = $1 and status = 'pending'",
        )
        .bind(&device.key[..])
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated > 0)
    }

    /// A device's poll for its tokens (RFC 8628 section 3.5).
    pub async fn poll_device(
        &self,
        device_code: &str,
        client_id: &str,
        now: OffsetDateTime,
    ) -> Result<DevicePoll> {
        let mut tx = self.pool.begin().await?;
        let row: Option<PollRow> = sqlx::query_as(
            "select client_id, status, user_id, interval_s, last_polled_at, expires_at \
             from device_codes where device_sha256 = $1 for update",
        )
        .bind(&hash_secret(device_code)[..])
        .fetch_optional(&mut *tx)
        .await?;
        let Some((owner, status, user, interval_s, last_polled_at, expires_at)) = row else {
            return Ok(DevicePoll::Unknown);
        };
        if owner != client_id || status == "issued" {
            return Ok(DevicePoll::Unknown);
        }
        if expires_at <= now {
            return Ok(DevicePoll::Expired);
        }
        let interval = Duration::seconds(i64::from(interval_s));
        let too_soon = last_polled_at.is_some_and(|at| now - at < interval);
        let step = if too_soon {
            i32::try_from(SLOW_DOWN_STEP.whole_seconds()).expect("a small constant")
        } else {
            0
        };
        sqlx::query(
            "update device_codes set last_polled_at = $2, interval_s = interval_s + $3 \
             where device_sha256 = $1",
        )
        .bind(&hash_secret(device_code)[..])
        .bind(now)
        .bind(step)
        .execute(&mut *tx)
        .await?;
        let poll = if too_soon {
            DevicePoll::SlowDown
        } else {
            match (status.as_str(), user) {
                ("pending", _) => DevicePoll::Pending,
                ("denied", _) => DevicePoll::Denied,
                ("approved", Some(user)) => {
                    sqlx::query(
                        "update device_codes set status = 'issued' where device_sha256 = $1",
                    )
                    .bind(&hash_secret(device_code)[..])
                    .execute(&mut *tx)
                    .await?;
                    DevicePoll::Approved(user)
                }
                (other, _) => return Err(bad_column("device status", other)),
            }
        };
        tx.commit().await?;
        Ok(poll)
    }
}
