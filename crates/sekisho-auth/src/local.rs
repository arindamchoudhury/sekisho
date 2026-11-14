//! Local accounts: Dex's password database, run by Sekisho.
//!
//! An admin invites a person and hands them a one-time setup link; the
//! person sets their password on Sekisho's page, and Sekisho passes Dex only
//! its bcrypt hash. The first admin is made on `/setup` with the bootstrap
//! token, which works only until then; an operator who has lost every admin
//! runs `sekisho admin-link`. Passwords are never stored or logged, and
//! neither are setup links.

use std::fmt;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use sekisho_core::keys::verify_secret;
use sekisho_store::{Store, StoreError, User, end_user_sessions};
use time::OffsetDateTime;
use tokio::time::timeout;
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::AuthState;
use crate::dex::{DEX_TIMEOUT, DexError, DexPassword};
use crate::pages::{self, alert, escape, field};
use crate::tokens::form as form_params;

/// The shortest password, in characters: NIST SP 800-63B-4's minimum for a
/// password that is the only factor (D121).
pub const PASSWORD_MIN_CHARS: usize = 15;
/// The longest password, in bytes: bcrypt, which Dex checks passwords with,
/// ignores the rest.
pub const PASSWORD_MAX_BYTES: usize = 72;
/// bcrypt's cost for the hashes Sekisho gives Dex.
pub const BCRYPT_COST: u32 = 12;
/// zxcvbn's top score, more than 10^10 guesses (D122). Its score of 3 lets
/// `Password1234567!` through.
const MIN_SCORE: zxcvbn::Score = zxcvbn::Score::Four;
/// How long a wrong bootstrap token holds the queue.
const WRONG_TOKEN_DELAY: Duration = Duration::from_secs(1);

/// Bootstrap tokens are checked one at a time, and a wrong one keeps the
/// next waiting a second, so parallel guesses gain nothing.
static BOOTSTRAP_TURNS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const MISMATCH: &str = "The two passwords don't match.";

/// The rule, as the forms state it.
const PASSWORD_RULE: &str = "<p>At least 15 characters, and hard to guess: a few unrelated \
                             words work well. Your email or name, common passwords and \
                             keyboard patterns are refused.</p>\n";

/// Why a password can't be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    #[error("The password needs at least 15 characters.")]
    TooShort,
    #[error(
        "The password can have at most 72 bytes: letters outside plain ASCII take two to four \
         each."
    )]
    TooLong,
    /// A common or breached password, a pattern, or one built from the
    /// account's own words; with zxcvbn's hint, if it has one.
    #[error(
        "This password is too easy to guess.{} A few unrelated words, with your own twist, work \
         well.",
        .0.as_deref().map(|hint| format!(" {hint}")).unwrap_or_default()
    )]
    Guessable(Option<String>),
}

/// What a password must not be built from: the account's email and its
/// parts, its name and its parts, and Sekisho's name.
pub fn context_words(email: &str, name: &str) -> Vec<String> {
    let mut words = vec![email.trim().to_lowercase(), name.trim().to_lowercase()];
    words.extend(
        email
            .split(|c: char| !c.is_alphanumeric())
            .chain(name.split(|c: char| !c.is_alphanumeric()))
            .filter(|w| w.chars().count() >= 3)
            .map(str::to_lowercase),
    );
    words.push("sekisho".to_owned());
    words.retain(|w| !w.is_empty());
    words.sort();
    words.dedup();
    words
}

/// At least [`PASSWORD_MIN_CHARS`] characters, at most
/// [`PASSWORD_MAX_BYTES`] bytes, and hard to guess (D121): zxcvbn must give
/// it its top score, with `context` (see [`context_words`]) counted as known
/// words. No composition rules.
pub fn check_password(password: &str, context: &[String]) -> Result<(), PasswordError> {
    if password.chars().count() < PASSWORD_MIN_CHARS {
        return Err(PasswordError::TooShort);
    }
    if password.len() > PASSWORD_MAX_BYTES {
        return Err(PasswordError::TooLong);
    }
    let context: Vec<&str> = context.iter().map(String::as_str).collect();
    let estimate = zxcvbn::zxcvbn(password, &context);
    if estimate.score() < MIN_SCORE {
        let hint = estimate
            .feedback()
            .and_then(|f| f.warning())
            .map(|w| w.to_string());
        return Err(PasswordError::Guessable(hint));
    }
    Ok(())
}

/// The bcrypt hash of a password that [`check_password`] accepted.
pub fn hash_password(password: &str) -> String {
    bcrypt::hash(password, BCRYPT_COST).expect("bcrypt hashes any password of at most 72 bytes")
}

/// A one-time link where a person sets their password.
#[derive(Clone)]
pub struct SetupLink {
    /// `<SEKISHO_PUBLIC_URL>/setup/<token>`: show it once, never log it.
    pub url: Url,
    pub expires_at: OffsetDateTime,
}

/// Shows when it expires, not the link.
impl fmt::Debug for SetupLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetupLink")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Why a local account couldn't be changed.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("the user isn't a local account")]
    NotLocal,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Dex(#[from] DexError),
    #[error("the change was interrupted; try again")]
    Interrupted,
}

fn setup_url(public_url: &Url, token: &str) -> Url {
    Url::parse(&format!(
        "{}/setup/{token}",
        public_url.as_str().trim_end_matches('/')
    ))
    .expect("the public URL with a path is a URL")
}

/// A Dex call with Dex's timeout.
async fn dex_call<T>(call: impl Future<Output = Result<T, DexError>>) -> Result<T, DexError> {
    timeout(DEX_TIMEOUT, call).await.unwrap_or_else(|_| {
        Err(DexError::Unreachable(
            "Dex didn't answer in time".to_owned(),
        ))
    })
}

/// Runs `work` on a task of its own, so a caller that hangs up can't leave
/// Dex and Sekisho half changed.
async fn on_task<T: Send + 'static>(
    work: impl Future<Output = Result<T, LocalError>> + Send + 'static,
) -> Result<T, LocalError> {
    tokio::spawn(work).await.unwrap_or_else(|e| {
        error!(error = %e, "a local account change failed");
        Err(LocalError::Interrupted)
    })
}

/// The local account `id`, or why it isn't one.
async fn local_account(store: &Store, id: &Uuid) -> Result<User, LocalError> {
    let user = store.user(id).await?;
    if !user.local || user.local_dex_user_id.is_none() {
        return Err(LocalError::NotLocal);
    }
    Ok(user)
}

/// Makes a local account and the link where its person sets their password.
pub async fn invite(
    state: &AuthState,
    email: &str,
    name: &str,
) -> Result<(User, SetupLink), LocalError> {
    let user = state.store.create_local_user(email, name).await?;
    let (token, expires_at) = state
        .store
        .create_setup_link(user.id, false, state.now())
        .await?;
    info!(user = %user.id, "invited a local account");
    let link = SetupLink {
        url: setup_url(&state.public_url, &token),
        expires_at,
    };
    Ok((user, link))
}

/// Resets a local account's password: removes it from Dex at once, ends the
/// person's sessions and Dex sign-in, and makes a new setup link (earlier
/// unused ones stop working). If Dex can't be reached, nothing changes.
pub async fn reset(state: &AuthState, user: &Uuid) -> Result<SetupLink, LocalError> {
    let (state, id) = (state.clone(), *user);
    on_task(async move {
        let user = local_account(&state.store, &id).await?;
        dex_call(state.dex.delete_password(&user.email)).await?;
        let now = state.now();
        // Sessions before the Dex token, as everywhere.
        let mut conn = state
            .store
            .pool()
            .acquire()
            .await
            .map_err(StoreError::from)?;
        end_user_sessions(&mut conn, &id, "password_reset", now).await?;
        drop(conn);
        state.store.delete_dex_token(&id).await?;
        let (token, expires_at) = state.store.create_setup_link(id, false, now).await?;
        info!(user = %id, "reset a local account's password");
        Ok(SetupLink {
            url: setup_url(&state.public_url, &token),
            expires_at,
        })
    })
    .await
}

/// Deletes a local account: its password at Dex, and disables the user
/// (users are never deleted, so what they own keeps its person).
pub async fn delete(state: &AuthState, user: &Uuid) -> Result<(), LocalError> {
    let (state, id) = (state.clone(), *user);
    on_task(async move {
        let user = local_account(&state.store, &id).await?;
        dex_call(state.dex.delete_password(&user.email)).await?;
        state.store.set_disabled(&id, true, state.now()).await?;
        info!(user = %id, "deleted a local account");
        Ok(())
    })
    .await
}

/// `sekisho admin-link`: finds or makes the local account for `email`,
/// enables it, and makes a setup link that also marks it an admin. It asks
/// nothing of Dex, so it works while Sekisho's sign-in doesn't.
pub async fn admin_link(
    store: &Store,
    public_url: &Url,
    email: &str,
    now: OffsetDateTime,
) -> Result<SetupLink, StoreError> {
    let user = match store.local_user_by_email(email).await? {
        Some(user) => user,
        None => {
            let name = email.trim().split('@').next().unwrap_or_default();
            match store.create_local_user(email, name).await {
                Ok(user) => user,
                // Made meanwhile.
                Err(StoreError::Conflict(_)) => store
                    .local_user_by_email(email)
                    .await?
                    .ok_or(StoreError::NotFound)?,
                Err(e) => return Err(e),
            }
        }
    };
    if user.disabled_at.is_some() {
        store.set_disabled(&user.id, false, now).await?;
    }
    let (token, expires_at) = store.create_setup_link(user.id, true, now).await?;
    info!(user = %user.id, "made an admin setup link");
    Ok(SetupLink {
        url: setup_url(public_url, &token),
        expires_at,
    })
}

/// Hashes off the async threads (cost 12 takes a while), then gives Dex the
/// hash for this local account, creating or replacing its entry.
async fn put_password(state: &AuthState, user: &User, password: &str) -> Result<(), DexError> {
    let password = password.to_owned();
    let bcrypt_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|_| DexError::Invalid("hashing the password failed".to_owned()))?;
    let entry = DexPassword {
        email: user.email.clone(),
        bcrypt_hash,
        username: if user.name.trim().is_empty() {
            user.email.clone()
        } else {
            user.name.clone()
        },
        user_id: user
            .local_dex_user_id
            .clone()
            .ok_or_else(|| DexError::Invalid("not a local account".to_owned()))?,
    };
    dex_call(state.dex.put_password(&entry)).await
}

/// What a password and its confirmation are wrong for, if anything.
fn password_problem(password: &str, confirm: &str, email: &str, name: &str) -> Option<String> {
    if let Err(e) = check_password(password, &context_words(email, name)) {
        return Some(e.to_string());
    }
    (password != confirm).then(|| MISMATCH.to_owned())
}

/// The form answer for Dex failing to take a password.
fn dex_failed(e: &DexError) -> (StatusCode, &'static str) {
    warn!(error = %e, "Dex didn't take a local account's password");
    match e {
        DexError::Unavailable(_) | DexError::Unreachable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The sign-in service can't be reached right now. Try again in a minute.",
        ),
        DexError::Refused(_) | DexError::Invalid(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Sekisho couldn't save the password. Try again, or ask an admin.",
        ),
    }
}

fn store_failed(e: &StoreError) -> Response {
    error!(error = %e, "the store failed on a setup page");
    pages::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong",
        "Sekisho couldn't finish this step. Try again.",
    )
}

fn password_fields() -> String {
    field("Password", "password", "password", "", "new-password")
        + &field("Password again", "confirm", "password", "", "new-password")
}

// ---- /setup/{token}

fn link_gone() -> Response {
    pages::error(
        StatusCode::NOT_FOUND,
        "This link doesn't work",
        "This setup link was used, has expired or doesn't exist. Ask an admin for a new one.",
    )
}

fn link_form(
    state: &AuthState,
    token: &str,
    user: &User,
    status: StatusCode,
    problem: Option<&str>,
) -> Response {
    pages::page(
        status,
        "Set your password",
        &format!(
            "{alert}<p>Choose the password for <strong>{email}</strong>.</p>\n{PASSWORD_RULE}\
             <form method=\"post\" action=\"{action}\">\n\
             {fields}<p><button type=\"submit\">Set password</button></p>\n</form>",
            alert = problem.map(alert).unwrap_or_default(),
            email = escape(&user.email),
            action = escape(setup_url(&state.public_url, token).as_str()),
            fields = password_fields(),
        ),
    )
}

/// The local account a usable link is for, and whether it makes an admin.
async fn link_user(state: &AuthState, token: &str) -> Result<Option<(User, bool)>, StoreError> {
    Ok(state
        .store
        .setup_link(token, state.now())
        .await?
        .filter(|(user, _)| user.local && user.local_dex_user_id.is_some()))
}

/// `GET /setup/{token}`.
pub(crate) async fn link_page(
    State(state): State<AuthState>,
    Path(token): Path<String>,
) -> Response {
    match link_user(&state, &token).await {
        Ok(Some((user, _))) => link_form(&state, &token, &user, StatusCode::OK, None),
        Ok(None) => link_gone(),
        Err(e) => store_failed(&e),
    }
}

/// `POST /setup/{token}`: the password to Dex, then the link used up.
pub(crate) async fn set_link_password(
    State(state): State<AuthState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (user, make_admin) = match link_user(&state, &token).await {
        Ok(Some(found)) => found,
        Ok(None) => return link_gone(),
        Err(e) => return store_failed(&e),
    };
    let Ok(params) = form_params(&headers, &body) else {
        return link_form(
            &state,
            &token,
            &user,
            StatusCode::BAD_REQUEST,
            Some("Fill in the form."),
        );
    };
    let password = params.get("password").unwrap_or("");
    let confirm = params.get("confirm").unwrap_or("");
    if let Some(problem) = password_problem(password, confirm, &user.email, &user.name) {
        return link_form(
            &state,
            &token,
            &user,
            StatusCode::BAD_REQUEST,
            Some(&problem),
        );
    }
    if let Err(e) = put_password(&state, &user, password).await {
        let (status, message) = dex_failed(&e);
        return link_form(&state, &token, &user, status, Some(message));
    }
    match state.store.use_setup_link(&token, state.now()).await {
        Ok(true) => {}
        Ok(false) => return link_gone(),
        Err(e) => return store_failed(&e),
    }
    if make_admin && let Err(e) = state.store.set_admin(&user.id, true).await {
        return store_failed(&e);
    }
    info!(user = %user.id, admin = make_admin, "a local account's password was set");
    pages::page(
        StatusCode::OK,
        "Your password is set",
        &format!(
            "<p>Sign in to Sekisho as <strong>{}</strong> from your application or terminal.</p>",
            escape(&user.email)
        ),
    )
}

// ---- /setup

fn set_up() -> Response {
    pages::error(
        StatusCode::NOT_FOUND,
        "Sekisho is set up",
        "Sekisho is set up; sign in.",
    )
}

fn first_admin_form(
    state: &AuthState,
    status: StatusCode,
    problem: Option<&str>,
    email: &str,
    name: &str,
) -> Response {
    pages::page(
        status,
        "Set up Sekisho",
        &format!(
            "{alert}<p>Make the first admin's local account. The bootstrap token is \
             <code>SEKISHO_BOOTSTRAP_TOKEN</code>; it works only until this is done.</p>\n\
             {PASSWORD_RULE}<form method=\"post\" action=\"{action}\">\n\
             {token}{email}{name}{passwords}\
             <p><button type=\"submit\">Make the admin</button></p>\n</form>",
            alert = problem.map(alert).unwrap_or_default(),
            action = escape(&state.url("/setup")),
            token = field("Bootstrap token", "bootstrap_token", "password", "", "off"),
            email = field("Email", "email", "email", email, "email"),
            name = field("Name", "name", "text", name, "name"),
            passwords = password_fields(),
        ),
    )
}

/// `GET /setup`: the first admin's form, until there is one.
pub(crate) async fn setup_page(State(state): State<AuthState>) -> Response {
    match state.store.setup_completed().await {
        Ok(false) => first_admin_form(&state, StatusCode::OK, None, "", ""),
        Ok(true) => set_up(),
        Err(e) => store_failed(&e),
    }
}

/// `POST /setup`: with the bootstrap token, makes the first admin's local
/// account and Dex password, and closes `/setup`.
pub(crate) async fn first_admin(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match state.store.setup_completed().await {
        Ok(false) => {}
        Ok(true) => return set_up(),
        Err(e) => return store_failed(&e),
    }
    let params = form_params(&headers, &body).ok();
    let get = |name: &str| {
        params
            .as_ref()
            .and_then(|p| p.get(name))
            .unwrap_or("")
            .to_owned()
    };
    let (email, name, password) = (get("email"), get("name"), get("password"));
    let form =
        |status, problem: &str| first_admin_form(&state, status, Some(problem), &email, &name);
    {
        let _turn = BOOTSTRAP_TURNS.lock().await;
        if !verify_secret(&get("bootstrap_token"), &state.bootstrap_sha256) {
            warn!("a wrong bootstrap token at /setup");
            tokio::time::sleep(WRONG_TOKEN_DELAY).await;
            return form(StatusCode::UNAUTHORIZED, "That isn't the bootstrap token.");
        }
    }
    if let Some(problem) = password_problem(&password, &get("confirm"), &email, &name) {
        return form(StatusCode::BAD_REQUEST, &problem);
    }
    // A local account left by an earlier try (Dex was down) is used again.
    let user = match state.store.create_local_user(&email, &name).await {
        Ok(user) => user,
        Err(StoreError::Conflict(_)) => match state.store.local_user_by_email(&email).await {
            Ok(Some(user)) => user,
            Ok(None) => return form(StatusCode::CONFLICT, "Try again."),
            Err(e) => return store_failed(&e),
        },
        Err(StoreError::Validation(fields)) => {
            let problem = fields
                .iter()
                .map(|f| format!("{}: {}.", f.field, f.message))
                .collect::<Vec<_>>()
                .join(" ");
            return form(StatusCode::BAD_REQUEST, &problem);
        }
        Err(e) => return store_failed(&e),
    };
    if let Err(e) = put_password(&state, &user, &password).await {
        let (status, message) = dex_failed(&e);
        return form(status, message);
    }
    let now = state.now();
    if user.disabled_at.is_some()
        && let Err(e) = state.store.set_disabled(&user.id, false, now).await
    {
        return store_failed(&e);
    }
    if let Err(e) = state.store.set_admin(&user.id, true).await {
        return store_failed(&e);
    }
    match state.store.complete_setup(now).await {
        Ok(()) => {}
        Err(StoreError::Conflict(_)) => return set_up(),
        Err(e) => return store_failed(&e),
    }
    info!(user = %user.id, "made the first admin");
    pages::page(
        StatusCode::OK,
        "Sekisho is set up",
        &format!(
            "<p>Sign in to Sekisho as <strong>{}</strong>. The bootstrap token works no more.</p>",
            escape(&user.email)
        ),
    )
}
