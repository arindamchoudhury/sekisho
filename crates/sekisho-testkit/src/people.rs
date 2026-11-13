//! People for tests, signed in as Dex would sign them in.

use sekisho_core::secret_box::SecretBox;
use sekisho_core::session_token::{ACCESS_TOKEN_LIFETIME, AUDIENCE, SessionClaims, issue};
use sekisho_store::{SignIn, SignInOutcome, Store};
use time::OffsetDateTime;
use uuid::Uuid;

/// The client the test sessions are opened through.
const CLIENT: &str = "sekisho-cli";

/// Signs `email` in through a GitHub connector with these identity-provider
/// groups, and returns their user ID. The same email is the same person on
/// every call; each call replaces their identity-provider groups.
pub async fn person(store: &Store, email: &str, groups: &[&str]) -> Uuid {
    let sign_in = SignIn {
        subject: format!("test|{email}"),
        email: email.to_owned(),
        name: email.split('@').next().unwrap_or(email).to_owned(),
        groups: groups.iter().map(|g| (*g).to_owned()).collect(),
        connector_id: "github".to_owned(),
        connector_user_id: email.to_owned(),
    };
    match store.sign_in(&sign_in, OffsetDateTime::now_utc()).await {
        Ok(SignInOutcome::User(user)) => user.id,
        other => panic!("{email} could not sign in: {other:?}"),
    }
}

/// A person marked as an admin.
pub async fn admin(store: &Store, email: &str) -> Uuid {
    let id = person(store, email, &[]).await;
    store.set_admin(&id, true).await.expect("mark the admin");
    id
}

/// A session for `user` and an access token for it, as Sekisho's token
/// endpoint issues them now: see [`session_token_at`].
pub async fn session_token(
    store: &Store,
    secret_box: &SecretBox,
    issuer: &str,
    user: Uuid,
) -> (String, Uuid) {
    session_token_at(store, secret_box, issuer, user, OffsetDateTime::now_utc()).await
}

/// A session for `user` opened at `now`, and an access token for it valid
/// for an hour from `now`, signed with the store's signing key (made if
/// there is none). Returns the token and the session ID.
pub async fn session_token_at(
    store: &Store,
    secret_box: &SecretBox,
    issuer: &str,
    user: Uuid,
    now: OffsetDateTime,
) -> (String, Uuid) {
    store
        .rotate_signing_key(secret_box, now, false)
        .await
        .expect("a signing key");
    let key = store
        .active_signing_key(secret_box, now)
        .await
        .expect("read the signing key")
        .expect("a signing key that signs at `now`");
    let (session, _refresh) = store
        .create_session(user, CLIENT, now)
        .await
        .expect("a session");
    let claims = SessionClaims {
        iss: issuer.to_owned(),
        sub: user.to_string(),
        aud: AUDIENCE.to_owned(),
        sid: session.id.to_string(),
        client_id: CLIENT.to_owned(),
        iat: now.unix_timestamp(),
        exp: (now + ACCESS_TOKEN_LIFETIME).unix_timestamp(),
    };
    (issue(&key, &claims), session.id)
}
