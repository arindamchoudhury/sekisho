//! People for tests, signed in as Dex would sign them in.

use sekisho_store::{SignIn, SignInOutcome, Store};
use time::OffsetDateTime;
use uuid::Uuid;

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
