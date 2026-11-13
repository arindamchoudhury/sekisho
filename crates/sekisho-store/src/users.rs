//! People: users signed in through Dex, and local accounts.
//!
//! A user is found by Dex's subject, or, for a local account, by the Dex user
//! ID Sekisho gave it; never by email. Their groups are the identity
//! provider's, from the latest sign-in, plus the Sekisho groups they're in.

use std::collections::{BTreeSet, HashMap};

use sekisho_core::access::Access;
use sekisho_core::names::GroupName;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{FieldError, bad_column, check, is_unique_violation};
use crate::notify::{ChangeKind, notify};
use crate::sessions::end_user_sessions;
use crate::{Result, Store, StoreError};

/// Dex's connector for local accounts.
pub const LOCAL_CONNECTOR: &str = "local";

const EMAIL_MAX_CHARS: usize = 254;
const NAME_MAX_CHARS: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub local: bool,
    /// A local account's user ID in Dex's password database.
    pub local_dex_user_id: Option<String>,
    /// Marked as an admin. Admin groups are applied by the server.
    pub admin: bool,
    /// The identity provider's groups at the latest sign-in.
    pub idp_groups: Vec<String>,
    /// The Sekisho groups the user is in, sorted.
    pub groups: Vec<GroupName>,
    pub dex_subject: Option<String>,
    /// The connector of the latest sign-in.
    pub dex_connector: Option<String>,
    pub disabled_at: Option<OffsetDateTime>,
    pub idp_refused_at: Option<OffsetDateTime>,
    pub last_sign_in_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

impl User {
    /// What decides the user's access, for `sekisho_core::access`.
    pub fn access(&self) -> Access {
        access(
            self.disabled_at.is_some(),
            self.idp_refused_at.is_some(),
            &self.idp_groups,
            self.groups.iter().map(GroupName::as_str),
        )
    }
}

fn access<'a>(
    disabled: bool,
    idp_refused: bool,
    idp_groups: &[String],
    groups: impl IntoIterator<Item = &'a str>,
) -> Access {
    let mut all: BTreeSet<String> = idp_groups.iter().cloned().collect();
    all.extend(groups.into_iter().map(str::to_owned));
    Access {
        disabled,
        idp_refused,
        groups: all,
    }
}

/// Who Dex says signed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    /// Dex's `sub`.
    pub subject: String,
    pub email: String,
    pub name: String,
    /// The identity provider's groups.
    pub groups: Vec<String>,
    pub connector_id: String,
    /// The user's ID at the connector; for `local`, the Dex user ID Sekisho
    /// gave the account.
    pub connector_user_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInOutcome {
    /// Boxed: far larger than the other outcomes.
    User(Box<User>),
    /// The user is disabled.
    Disabled,
    /// Dex signed in a local account Sekisho doesn't know.
    UnknownLocalAccount,
}

/// One user's access and admin mark, for the auth cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserAccess {
    pub id: Uuid,
    pub access: Access,
    /// The admin's mark only; admin groups are applied by the server.
    pub admin_mark: bool,
}

/// Expands to the user query up to its filter; finish it with
/// `group by u.id`.
macro_rules! select_users {
    () => {
        "select u.id, u.email, u.name, u.local, u.local_dex_user_id, u.admin, u.idp_groups, \
         u.dex_subject, u.dex_connector, u.disabled_at, u.idp_refused_at, u.last_sign_in_at, \
         u.created_at, coalesce(array_agg(g.name order by g.name) \
         filter (where g.id is not null), '{}') as groups \
         from users u \
         left join group_members gm on gm.user_id = u.id \
         left join groups g on g.id = gm.group_id"
    };
}

fn from_row(row: &PgRow) -> Result<User> {
    let groups: Vec<String> = row.try_get("groups")?;
    let groups = groups
        .iter()
        .map(|g| GroupName::parse(g).map_err(|_| bad_column("group name", g)))
        .collect::<Result<_>>()?;
    Ok(User {
        id: row.try_get("id")?,
        email: row.try_get("email")?,
        name: row.try_get("name")?,
        local: row.try_get("local")?,
        local_dex_user_id: row.try_get("local_dex_user_id")?,
        admin: row.try_get("admin")?,
        idp_groups: row.try_get("idp_groups")?,
        groups,
        dex_subject: row.try_get("dex_subject")?,
        dex_connector: row.try_get("dex_connector")?,
        disabled_at: row.try_get("disabled_at")?,
        idp_refused_at: row.try_get("idp_refused_at")?,
        last_sign_in_at: row.try_get("last_sign_in_at")?,
        created_at: row.try_get("created_at")?,
    })
}

pub(crate) async fn read_user(conn: &mut PgConnection, id: &Uuid) -> Result<User> {
    let row = sqlx::query(concat!(select_users!(), " where u.id = $1 group by u.id"))
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or(StoreError::NotFound)?;
    from_row(&row)
}

/// The access and admin mark of the listed users, or of every user, by ID.
pub(crate) async fn user_accesses(
    conn: &mut PgConnection,
    ids: Option<&[Uuid]>,
) -> Result<Vec<UserAccess>> {
    let rows = sqlx::query(
        "select u.id, u.disabled_at is not null as disabled, \
         u.idp_refused_at is not null as refused, u.admin, u.idp_groups, \
         coalesce(array_agg(g.name) filter (where g.id is not null), '{}') as groups \
         from users u \
         left join group_members gm on gm.user_id = u.id \
         left join groups g on g.id = gm.group_id \
         where $1::uuid[] is null or u.id = any($1) \
         group by u.id order by u.id",
    )
    .bind(ids)
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|row| {
            let idp_groups: Vec<String> = row.try_get("idp_groups")?;
            let groups: Vec<String> = row.try_get("groups")?;
            Ok(UserAccess {
                id: row.try_get("id")?,
                access: access(
                    row.try_get("disabled")?,
                    row.try_get("refused")?,
                    &idp_groups,
                    groups.iter().map(String::as_str),
                ),
                admin_mark: row.try_get("admin")?,
            })
        })
        .collect()
}

/// The listed users' access by ID; users that don't exist are left out.
pub(crate) async fn accesses(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Access>> {
    Ok(user_accesses(conn, Some(ids))
        .await?
        .into_iter()
        .map(|u| (u.id, u.access))
        .collect())
}

/// A local account's email, trimmed and lowercased, or why it isn't one.
fn local_email(email: &str) -> std::result::Result<String, FieldError> {
    let email = email.trim().to_lowercase();
    let bad = |why: &str| Err(FieldError::new("email", why));
    if email.is_empty() {
        return bad("must not be empty");
    }
    if email.chars().count() > EMAIL_MAX_CHARS {
        return bad("use at most 254 characters");
    }
    if email.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad("must not contain spaces or control characters");
    }
    match email.split_once('@') {
        Some((local, domain))
            if !local.is_empty() && !domain.is_empty() && !domain.contains('@') =>
        {
            Ok(email)
        }
        _ => bad("must be an email address"),
    }
}

fn check_name(errors: &mut Vec<FieldError>, name: &str) {
    if name.chars().count() > NAME_MAX_CHARS {
        errors.push(FieldError::new("name", "use at most 100 characters"));
    } else if name.chars().any(char::is_control) {
        errors.push(FieldError::new(
            "name",
            "must not contain control characters",
        ));
    }
}

impl Store {
    /// Finds or creates the user Dex signed in, and records the sign-in.
    ///
    /// A local account is found by its Dex user ID; anyone else by Dex's
    /// subject, or created. Never by email. The identity provider's email (not
    /// a local account's), name and groups replace the stored ones, and an
    /// earlier refusal at the identity provider is cleared.
    pub async fn sign_in(&self, s: &SignIn, now: OffsetDateTime) -> Result<SignInOutcome> {
        let local = s.connector_id == LOCAL_CONNECTOR;
        let mut tx = self.pool.begin().await?;
        let mut attempts = 0;
        let (id, changed) = loop {
            attempts += 1;
            let found: Option<(Uuid, bool, Vec<String>, bool)> = sqlx::query_as(if local {
                "select id, disabled_at is not null, idp_groups, idp_refused_at is not null \
                 from users where local_dex_user_id = $1 for update"
            } else {
                "select id, disabled_at is not null, idp_groups, idp_refused_at is not null \
                 from users where dex_subject = $1 for update"
            })
            .bind(if local {
                &s.connector_user_id
            } else {
                &s.subject
            })
            .fetch_optional(&mut *tx)
            .await?;
            match found {
                Some((_, true, _, _)) => return Ok(SignInOutcome::Disabled),
                Some((id, false, idp_groups, refused)) => {
                    sqlx::query(
                        "update users set email = case when local then email else $2 end, \
                         name = $3, idp_groups = $4, dex_subject = $5, dex_connector = $6, \
                         last_sign_in_at = $7, idp_refused_at = null where id = $1",
                    )
                    .bind(id)
                    .bind(&s.email)
                    .bind(&s.name)
                    .bind(&s.groups)
                    .bind(&s.subject)
                    .bind(&s.connector_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                    break (id, idp_groups != s.groups || refused);
                }
                None if local => return Ok(SignInOutcome::UnknownLocalAccount),
                None => {
                    let inserted: Option<Uuid> = sqlx::query_scalar(
                        "insert into users (email, name, idp_groups, dex_subject, dex_connector, \
                         last_sign_in_at) values ($1, $2, $3, $4, $5, $6) \
                         on conflict (dex_subject) do nothing returning id",
                    )
                    .bind(&s.email)
                    .bind(&s.name)
                    .bind(&s.groups)
                    .bind(&s.subject)
                    .bind(&s.connector_id)
                    .bind(now)
                    .fetch_optional(&mut *tx)
                    .await?;
                    if let Some(id) = inserted {
                        break (id, true);
                    }
                    // The same person's first sign-in on another request
                    // committed meanwhile: find it on the next pass.
                    if attempts >= 3 {
                        return Err(StoreError::Conflict(
                            "the user changed while signing in; try again".into(),
                        ));
                    }
                }
            }
        };
        if changed {
            notify(&mut tx, ChangeKind::Users).await?;
        }
        let user = read_user(&mut tx, &id).await?;
        tx.commit().await?;
        Ok(SignInOutcome::User(Box::new(user)))
    }

    pub async fn user(&self, id: &Uuid) -> Result<User> {
        let mut conn = self.pool.acquire().await?;
        read_user(&mut conn, id).await
    }

    /// Every user, by email.
    pub async fn list_users(&self) -> Result<Vec<User>> {
        sqlx::query(concat!(
            select_users!(),
            " group by u.id order by u.email, u.created_at"
        ))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(from_row)
        .collect()
    }

    /// Marks or unmarks the user as an admin.
    pub async fn set_admin(&self, id: &Uuid, admin: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query("update users set admin = $2 where id = $1")
            .bind(id)
            .bind(admin)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if updated == 0 {
            return Err(StoreError::NotFound);
        }
        notify(&mut tx, ChangeKind::Users).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Disables the user, ending their sessions (`disabled`), or enables them
    /// again. Enabling brings no session back.
    pub async fn set_disabled(&self, id: &Uuid, disabled: bool, now: OffsetDateTime) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "update users set disabled_at = case when $2 then coalesce(disabled_at, $3) end \
             where id = $1",
        )
        .bind(id)
        .bind(disabled)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(StoreError::NotFound);
        }
        if disabled {
            end_user_sessions(&mut tx, id, "disabled", now).await?;
        }
        notify(&mut tx, ChangeKind::Users).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Makes a local account: its email trimmed and lowercased, and a new Dex
    /// user ID for its password entry. One local account per email.
    pub async fn create_local_user(&self, email: &str, name: &str) -> Result<User> {
        let mut errors = Vec::new();
        let email = local_email(email).map_err(|e| errors.push(e)).ok();
        check_name(&mut errors, name);
        check(errors)?;
        let email = email.expect("checked above");
        let mut tx = self.pool.begin().await?;
        let id: Uuid = sqlx::query_scalar(
            "insert into users (email, name, local, local_dex_user_id) \
             values ($1, $2, true, gen_random_uuid()::text) returning id",
        )
        .bind(&email)
        .bind(name)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::Conflict(format!("a local account for {email} already exists"))
            } else {
                e.into()
            }
        })?;
        notify(&mut tx, ChangeKind::Users).await?;
        let user = read_user(&mut tx, &id).await?;
        tx.commit().await?;
        Ok(user)
    }

    /// The local account with this email, in any case.
    pub async fn local_user_by_email(&self, email: &str) -> Result<Option<User>> {
        sqlx::query(concat!(
            select_users!(),
            " where u.local and lower(u.email) = lower(trim($1)) group by u.id"
        ))
        .bind(email)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(from_row)
        .transpose()
    }
}
