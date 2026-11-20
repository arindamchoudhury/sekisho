//! `sekisho keys`: the signed-in person's environment keys, over the `me`
//! routes. `create` prints the token once on stdout and keeps it nowhere;
//! `suspend`, `resume` and `revoke` take a key's ID, or a name that one of
//! their keys that isn't revoked has (`--env` when two environments' keys
//! share it).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time};

use super::Context;
use crate::output::{CliError, json as json_text, table, warn};

/// The longest `--expires <days>d` takes: about 100 years.
const MAX_DAYS: i64 = 36_500;

/// A key as `sekisho keys list --json` prints it. Never its token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRow {
    pub id: String,
    pub environment: String,
    pub name: String,
    /// `active`, `invalid`, `suspended`, `expired` or `revoked`.
    pub state: String,
    #[serde(default)]
    pub suspended_by_admin: bool,
    #[serde(default)]
    pub expires_at: Option<String>,
    pub created_at: String,
}

#[derive(Deserialize)]
struct Created {
    #[serde(flatten)]
    key: KeyRow,
    token: String,
}

/// What `keys suspend|resume|revoke` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Suspend,
    Resume,
    Revoke,
}

impl KeyAction {
    fn as_str(self) -> &'static str {
        match self {
            KeyAction::Suspend => "suspend",
            KeyAction::Resume => "resume",
            KeyAction::Revoke => "revoke",
        }
    }

    fn done(self) -> &'static str {
        match self {
            KeyAction::Suspend => "Suspended",
            KeyAction::Resume => "Resumed",
            KeyAction::Revoke => "Revoked",
        }
    }
}

async fn my_keys(ctx: &Context<'_>) -> Result<Vec<KeyRow>, CliError> {
    let (server, credential) = ctx.person().await?;
    Ok(server
        .get_json("/api/v1/me/keys", Some(&credential))
        .await?)
}

/// `sekisho keys list [--env <env>] [--json]`.
pub async fn keys_list(
    ctx: &Context<'_>,
    env: Option<&str>,
    as_json: bool,
) -> Result<String, CliError> {
    let keys: Vec<KeyRow> = my_keys(ctx)
        .await?
        .into_iter()
        .filter(|k| env.is_none_or(|e| k.environment == e))
        .collect();
    if as_json {
        return Ok(json_text(&keys));
    }
    if keys.is_empty() {
        return Ok(
            "You have no keys. Make one with `sekisho keys create <env> --name <name>`.\n"
                .to_owned(),
        );
    }
    let rows: Vec<Vec<String>> = keys
        .iter()
        .map(|k| {
            vec![
                k.id.clone(),
                k.name.clone(),
                k.environment.clone(),
                if k.suspended_by_admin && k.state == "suspended" {
                    "suspended by an admin".to_owned()
                } else {
                    k.state.clone()
                },
                k.expires_at.clone().unwrap_or_else(|| "never".to_owned()),
            ]
        })
        .collect();
    Ok(table(
        &["ID", "NAME", "ENVIRONMENT", "STATE", "EXPIRES"],
        &rows,
    ))
}

/// `--expires`: a number of days (`30d`), an RFC 3339 time, or a date
/// (midnight UTC); it must be in the future.
pub fn parse_expiry(text: &str, now: OffsetDateTime) -> Result<OffsetDateTime, CliError> {
    let shape = || {
        CliError::usage(format!("--expires {text:?} isn't a time")).with_hint(
            "use a number of days such as 30d, a date such as 2027-03-01, or an RFC 3339 time",
        )
    };
    let at = if let Some(days) = text.strip_suffix('d') {
        if days.is_empty() || !days.bytes().all(|b| b.is_ascii_digit()) {
            return Err(shape());
        }
        let days: i64 = days.parse().map_err(|_| shape())?;
        if !(1..=MAX_DAYS).contains(&days) {
            return Err(CliError::usage(format!(
                "--expires takes 1 to {MAX_DAYS} days"
            )));
        }
        now + Duration::days(days)
    } else if let Ok(at) = OffsetDateTime::parse(text, &Rfc3339) {
        at
    } else if let Some(date) = calendar_date(text) {
        PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc()
    } else {
        return Err(shape());
    };
    if at <= now {
        return Err(CliError::usage("--expires must be in the future"));
    }
    Ok(at)
}

/// `sekisho keys create <env> --name <name> [--expires <when>]`: the token
/// and a newline, the only time it is shown.
pub async fn keys_create(
    ctx: &Context<'_>,
    env: &str,
    name: &str,
    expires: Option<&str>,
) -> Result<String, CliError> {
    let expires_at = expires
        .map(|text| parse_expiry(text, ctx.now))
        .transpose()?
        .map(|at| at.format(&Rfc3339).expect("times format"));
    let (server, credential) = ctx.person().await?;
    let mut body = json!({ "environment": env, "name": name });
    if let Some(at) = expires_at {
        body["expires_at"] = Value::String(at);
    }
    let created: Created = server
        .post_json("/api/v1/me/keys", &body, Some(&credential))
        .await?;
    warn(&format!(
        "key {} ({}) for {}: this is the only time its token is shown, and Sekisho keeps only a hash of it; store it somewhere safe",
        created.key.name, created.key.id, created.key.environment
    ));
    Ok(format!("{}\n", created.token))
}

/// `sekisho keys suspend|resume|revoke <name or ID> [--env <env>]`.
pub async fn keys_act(
    ctx: &Context<'_>,
    action: KeyAction,
    which: &str,
    env: Option<&str>,
) -> Result<String, CliError> {
    let keys = my_keys(ctx).await?;
    let in_env = |k: &&KeyRow| env.is_none_or(|e| k.environment == e);
    let key = match keys.iter().filter(in_env).find(|k| k.id == which) {
        Some(key) => key,
        None => {
            let named: Vec<&KeyRow> = keys
                .iter()
                .filter(in_env)
                .filter(|k| k.name == which && k.state != "revoked")
                .collect();
            match named.as_slice() {
                [key] => *key,
                [] => {
                    let place = env.map(|e| format!(" in {e}")).unwrap_or_default();
                    return Err(CliError::error(format!(
                        "you have no key named {which:?}{place} that isn't revoked"
                    ))
                    .with_hint("see your keys with `sekisho keys list`"));
                }
                several => {
                    let envs: Vec<&str> = several.iter().map(|k| k.environment.as_str()).collect();
                    return Err(CliError::usage(format!(
                        "your keys in {} are all named {which:?}",
                        envs.join(", ")
                    ))
                    .with_hint("choose one with --env <env>, or give the key's ID"));
                }
            }
        }
    };
    let (server, credential) = ctx.person().await?;
    let _: Value = server
        .post_json(
            &format!("/api/v1/me/keys/{}/{}", key.id, action.as_str()),
            &json!({}),
            Some(&credential),
        )
        .await?;
    super::note(&format!(
        "{} key {} ({}) for {}.",
        action.done(),
        key.name,
        key.id,
        key.environment
    ));
    Ok(String::new())
}

/// `YYYY-MM-DD`, a real day.
fn calendar_date(text: &str) -> Option<Date> {
    fn digits(s: &str, n: usize) -> Option<&str> {
        (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then_some(s)
    }
    let mut parts = text.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let month = Month::try_from(digits(month, 2)?.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(
        digits(year, 4)?.parse().ok()?,
        month,
        digits(day, 2)?.parse().ok()?,
    )
    .ok()
}
