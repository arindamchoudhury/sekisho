//! Local accounts: the first admin on `/setup`, setup links, resets and
//! `sekisho admin-link`'s recovery link. Passwords go to Dex as bcrypt
//! hashes, and nowhere else.

mod common;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration as StdDuration, Instant};

use axum::http::{StatusCode, header};
use common::app::*;
use sekisho_auth::dex::DexError;
use sekisho_auth::local::{
    self, PasswordError, SetupLink, check_password, context_words, hash_password,
};
use sekisho_store::{RefreshCheck, Store};
use time::Duration;
use url::Url;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple";

/// Everything every test in this binary logs, at every level.
fn logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buffer);
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || Capture(Arc::clone(&writer)))
            .init();
        buffer
    })
    .clone()
}

struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every row of every table, as PostgreSQL prints it.
async fn database_text(store: &Store) -> String {
    let tables: Vec<String> = sqlx::query_scalar(
        "select table_name::text from information_schema.tables \
         where table_schema = 'public' and table_type = 'BASE TABLE'",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert!(tables.len() > 10, "{tables:?}");
    let mut text = String::new();
    for table in tables {
        // Table names from the catalog, quoted.
        let all = sqlx::AssertSqlSafe(format!("select t::text from \"{table}\" t"));
        let rows: Vec<String> = sqlx::query_scalar(all)
            .fetch_all(store.pool())
            .await
            .unwrap();
        for row in rows {
            text.push_str(&row);
            text.push('\n');
        }
    }
    text
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// The path of a setup link.
fn path(link: &SetupLink) -> String {
    link.url.path().to_owned()
}

async fn set_password(t: &T, link: &SetupLink, password: &str, confirm: &str) -> Reply {
    t.post_form(&path(link), &[("password", password), ("confirm", confirm)])
        .await
}

async fn first_admin(t: &T, token: &str, email: &str, password: &str, confirm: &str) -> Reply {
    t.post_form(
        "/setup",
        &[
            ("bootstrap_token", token),
            ("email", email),
            ("name", "Root"),
            ("password", password),
            ("confirm", confirm),
        ],
    )
    .await
}

/// A local account invited by an admin, and its link.
async fn invited(t: &T, email: &str) -> (sekisho_store::User, SetupLink) {
    local::invite(&t.state, email, "Ada").await.unwrap()
}

#[test]
fn password_rules() {
    let none: Vec<String> = Vec::new();
    let ada = context_words("ada.lovelace@example.com", "Ada Lovelace");
    assert!(ada.contains(&"lovelace".to_owned()) && ada.contains(&"sekisho".to_owned()));
    let check = |p: &str| check_password(p, &ada);

    assert_eq!(check(PASSWORD), Ok(()));
    assert_eq!(check("my cat eats tulips on fridays"), Ok(()));
    // 14 characters.
    assert_eq!(check("violet anchor "), Err(PasswordError::TooShort));
    // Characters, not bytes: eight two-byte letters are eight characters.
    assert_eq!(check("éèêëàâäç"), Err(PasswordError::TooShort));
    // 72 bytes is the most; 25 three-byte characters are 75.
    let long = "plumber galaxy orbit fern, violet anchor quietly drums; my cat eats tulips";
    assert!(long.len() > 72);
    assert_eq!(check(&long[..72]), Ok(()));
    assert_eq!(check(&long[..73]), Err(PasswordError::TooLong));
    assert_eq!(check(&"€".repeat(25)), Err(PasswordError::TooLong));

    // Long enough, but easy to guess: common passwords, patterns, repeats,
    // sequences, and the account's own words.
    for guessable in [
        "passwordpassword",
        "Password1234567!",
        "aaaaaaaaaaaaaaa",
        "qwertyuiopasdfgh",
        "123456789012345",
        "abcdefghijklmno",
        "iloveyou12345678",
        "sekishosekisho1",
        "adalovelace2026!",
    ] {
        assert!(
            matches!(check(guessable), Err(PasswordError::Guessable(_))),
            "{guessable}"
        );
    }
    // The account's email is only known as such through the context.
    assert_eq!(check_password("ada.lovelace@example.com", &none), Ok(()));
    assert!(matches!(
        check("ada.lovelace@example.com"),
        Err(PasswordError::Guessable(_))
    ));
    // zxcvbn's hint goes into the message.
    let message = check("aaaaaaaaaaaaaaa").unwrap_err().to_string();
    assert!(
        message.contains("too easy to guess") && message.contains("Repeats"),
        "{message}"
    );

    let hash = hash_password(PASSWORD);
    assert!(hash.starts_with("$2b$12$"), "{hash}");
    assert!(bcrypt::verify(PASSWORD, &hash).unwrap());
}

#[tokio::test]
async fn setup_creates_the_first_admin_once() {
    let t = setup().await;
    let form = t.get("/setup").await;
    assert_eq!(form.status, StatusCode::OK);
    assert_page(&form, "setup form");
    assert!(
        form.text().contains("name=\"bootstrap_token\""),
        "{}",
        form.text()
    );

    let done = first_admin(&t, BOOTSTRAP, " Root@Example.com ", PASSWORD, PASSWORD).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text());
    assert_page(&done, "set up");
    let root = t
        .store
        .local_user_by_email("root@example.com")
        .await
        .unwrap()
        .expect("the first admin");
    assert!(root.local && root.admin, "{root:?}");
    assert_eq!(root.email, "root@example.com");
    assert!(t.store.setup_completed().await.unwrap());
    let entry = t.dex.password("root@example.com").expect("Dex's entry");
    assert_eq!(Some(entry.user_id), root.local_dex_user_id);
    assert!(bcrypt::verify(PASSWORD, &entry.bcrypt_hash).unwrap());

    // Once only.
    for reply in [
        t.get("/setup").await,
        first_admin(&t, BOOTSTRAP, "second@example.com", PASSWORD, PASSWORD).await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert!(
            reply.text().contains("Sekisho is set up; sign in."),
            "{}",
            reply.text()
        );
        assert_page(&reply, "already set up");
    }
    assert!(
        t.store
            .local_user_by_email("second@example.com")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn setup_needs_the_bootstrap_token() {
    let t = setup().await;
    for token in ["not-the-token", ""] {
        let started = Instant::now();
        let reply = first_admin(&t, token, "root@example.com", PASSWORD, PASSWORD).await;
        assert!(started.elapsed() >= StdDuration::from_secs(1), "{token:?}");
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert!(
            reply.text().contains("name=\"bootstrap_token\""),
            "form again"
        );
        // What was typed stays, except the secrets.
        assert!(reply.text().contains("root@example.com"));
        assert!(!reply.text().contains(PASSWORD));
    }
    assert!(!t.store.setup_completed().await.unwrap());
    assert!(t.store.list_users().await.unwrap().is_empty());
    assert!(t.dex.password("root@example.com").is_none());
}

#[tokio::test]
async fn wrong_bootstrap_tokens_wait_their_turn() {
    let t = setup().await;
    let started = Instant::now();
    let email = "root@example.com";
    let statuses = tokio::join!(
        first_admin(&t, "guess one", email, PASSWORD, PASSWORD),
        first_admin(&t, "guess two", email, PASSWORD, PASSWORD),
        first_admin(&t, "guess three", email, PASSWORD, PASSWORD),
    );
    for reply in [statuses.0, statuses.1, statuses.2] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let took = started.elapsed();
    assert!(
        took >= StdDuration::from_secs(3),
        "parallel guesses waited only {took:?}"
    );
}

#[tokio::test]
async fn a_setup_link_sets_the_password_in_dex_once() {
    let t = setup().await;
    let (ada, link) = invited(&t, "ada@example.com").await;
    assert!(ada.local && !ada.admin);
    assert!(
        link.url.as_str().starts_with(&format!("{ISSUER}/setup/")),
        "{}",
        link.url
    );
    assert_eq!(link.expires_at, t.now() + Duration::days(7));
    // The link's token stays out of Debug output.
    assert!(!format!("{link:?}").contains(&path(&link)[7..]));

    let form = t.get(&path(&link)).await;
    assert_eq!(form.status, StatusCode::OK);
    assert_page(&form, "setup link form");
    assert!(form.text().contains("ada@example.com"), "{}", form.text());

    let done = set_password(&t, &link, PASSWORD, PASSWORD).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text());
    assert_page(&done, "password set");
    let entry = t.dex.password("ada@example.com").expect("Dex's entry");
    assert!(entry.bcrypt_hash.starts_with("$2"), "{}", entry.bcrypt_hash);
    assert_eq!(entry.bcrypt_hash.split('$').nth(2), Some("12"), "cost 12");
    assert!(bcrypt::verify(PASSWORD, &entry.bcrypt_hash).unwrap());
    assert_eq!(Some(entry.user_id.clone()), ada.local_dex_user_id);
    assert!(!t.store.user(&ada.id).await.unwrap().admin);

    // Once.
    for reply in [
        t.get(&path(&link)).await,
        set_password(
            &t,
            &link,
            "another password entirely",
            "another password entirely",
        )
        .await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_page(&reply, "used link");
    }
    assert!(
        bcrypt::verify(
            PASSWORD,
            &t.dex.password("ada@example.com").unwrap().bcrypt_hash
        )
        .unwrap()
    );

    // An expired or unknown link.
    let (_, late) = invited(&t, "late@example.com").await;
    t.advance(Duration::days(7));
    assert_eq!(t.get(&path(&late)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        t.get("/setup/not-a-token").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn passwords_follow_the_rules() {
    let t = setup().await;
    let (_, link) = invited(&t, "ada@example.com").await;
    let too_long = "€".repeat(25);
    for (password, confirm, says) in [
        ("violet anchor ", "violet anchor ", "at least 15 characters"),
        (
            "passwordpassword1",
            "passwordpassword1",
            "too easy to guess",
        ),
        (
            "ada@example.com 2026",
            "ada@example.com 2026",
            "too easy to guess",
        ),
        (too_long.as_str(), too_long.as_str(), "at most 72 bytes"),
        (
            PASSWORD,
            "correct horse battery stapler",
            "passwords don&#39;t match",
        ),
        ("", "", "at least 15 characters"),
    ] {
        let reply = set_password(&t, &link, password, confirm).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{says}");
        assert!(
            reply.text().contains("name=\"password\""),
            "form again: {says}"
        );
        assert!(reply.text().contains(says), "{says}: {}", reply.text());
        assert!(t.dex.password("ada@example.com").is_none(), "{says}");
    }
    // The link still works.
    assert_eq!(
        set_password(&t, &link, PASSWORD, PASSWORD).await.status,
        StatusCode::OK
    );

    // The same rules on /setup.
    let reply = first_admin(
        &t,
        BOOTSTRAP,
        "root@example.com",
        "violet anchor ",
        "violet anchor ",
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.text().contains("at least 15 characters"));
    assert!(!t.store.setup_completed().await.unwrap());
}

#[tokio::test]
async fn a_dex_failure_keeps_the_link() {
    let t = setup().await;
    let (_, link) = invited(&t, "ada@example.com").await;
    t.dex
        .fail_grpc(Some(DexError::Unreachable("gRPC call timed out".into())));
    let reply = set_password(&t, &link, PASSWORD, PASSWORD).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(reply.text().contains("name=\"password\""), "form again");
    t.dex.fail_grpc(None);
    assert_eq!(
        set_password(&t, &link, PASSWORD, PASSWORD).await.status,
        StatusCode::OK
    );

    // A first admin whose Dex password failed can try again.
    t.dex
        .fail_grpc(Some(DexError::Unavailable("gRPC Internal".into())));
    let reply = first_admin(&t, BOOTSTRAP, "root@example.com", PASSWORD, PASSWORD).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!t.store.setup_completed().await.unwrap());
    t.dex.fail_grpc(None);
    let reply = first_admin(&t, BOOTSTRAP, "root@example.com", PASSWORD, PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(t.store.setup_completed().await.unwrap());
}

#[tokio::test]
async fn the_password_never_reaches_the_database_or_the_log() {
    let logs = logs();
    let t = setup().await;
    let secret = "a password nobody would ever log 7f3a";
    let (ada, link) = invited(&t, "ada@example.com").await;
    let setup_token = path(&link)["/setup/".len()..].to_owned();
    assert_eq!(
        set_password(&t, &link, secret, secret).await.status,
        StatusCode::OK
    );
    let root = "another secret for the first admin 9c1e";
    assert_eq!(
        first_admin(&t, BOOTSTRAP, "root@example.com", root, root)
            .await
            .status,
        StatusCode::OK
    );
    // The rejected attempts too.
    first_admin(
        &t,
        BOOTSTRAP,
        "late@example.com",
        "a third password 5d2b",
        "x",
    )
    .await;
    first_admin(
        &t,
        "wrong bootstrap token 8e4f",
        "late@example.com",
        root,
        root,
    )
    .await;
    let hashes = [
        t.dex.password("ada@example.com").unwrap().bcrypt_hash,
        t.dex.password("root@example.com").unwrap().bcrypt_hash,
    ];

    let database = database_text(&t.store).await;
    let logged = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(
        logged.contains(&ada.id.to_string()),
        "the log names users by ID: {logged}"
    );
    for (what, value) in [
        ("Ada's password", secret.to_owned()),
        ("Ada's password in hex", hex(secret)),
        ("the first admin's password", root.to_owned()),
        ("the first admin's password in hex", hex(root)),
        ("a rejected password", "a third password 5d2b".to_owned()),
        (
            "a wrong bootstrap token",
            "wrong bootstrap token 8e4f".to_owned(),
        ),
        ("the bootstrap token", BOOTSTRAP.to_owned()),
        ("the setup link's token", setup_token.clone()),
        ("Ada's bcrypt hash", hashes[0].clone()),
        ("the first admin's bcrypt hash", hashes[1].clone()),
    ] {
        assert!(!database.contains(&value), "{what} is in the database");
        assert!(!logged.contains(&value), "{what} is in the log");
    }
}

#[tokio::test]
async fn a_reset_removes_the_old_password_and_ends_sessions() {
    let t = setup().await;
    let (ada, link) = invited(&t, "ada@example.com").await;
    assert_eq!(
        set_password(&t, &link, PASSWORD, PASSWORD).await.status,
        StatusCode::OK
    );
    let (_, refresh) = t.store.create_session(ada.id, CLI, t.now()).await.unwrap();
    t.store
        .save_dex_token(&t.sb, ada.id, "dex-refresh-token")
        .await
        .unwrap();
    // An unused link from before the reset.
    let earlier = local::reset(&t.state, &ada.id).await.unwrap();

    let link = local::reset(&t.state, &ada.id).await.unwrap();
    assert!(
        t.dex.password("ada@example.com").is_none(),
        "the old password is gone"
    );
    assert!(matches!(
        t.store.begin_refresh(&refresh, CLI, t.now()).await.unwrap(),
        RefreshCheck::Ended
    ));
    let reason: String = sqlx::query_scalar("select end_reason from sessions where user_id = $1")
        .bind(ada.id)
        .fetch_one(t.store.pool())
        .await
        .unwrap();
    assert_eq!(reason, "password_reset");
    assert_eq!(t.stored_dex_token(ada.id).await, None);
    // Only the newest link works.
    assert_eq!(t.get(&path(&earlier)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        set_password(&t, &link, "a brand new password", "a brand new password")
            .await
            .status,
        StatusCode::OK
    );
    assert!(t.dex.password("ada@example.com").is_some());

    // Dex down: nothing changes in Sekisho.
    let (_, refresh) = t.store.create_session(ada.id, CLI, t.now()).await.unwrap();
    t.dex
        .fail_grpc(Some(DexError::Unreachable("gRPC call timed out".into())));
    assert!(local::reset(&t.state, &ada.id).await.is_err());
    assert!(matches!(
        t.store.begin_refresh(&refresh, CLI, t.now()).await.unwrap(),
        RefreshCheck::Live(_)
    ));
}

#[tokio::test]
async fn deleting_a_local_account_removes_its_password_and_disables_it() {
    let t = setup().await;
    let (ada, link) = invited(&t, "ada@example.com").await;
    assert_eq!(
        set_password(&t, &link, PASSWORD, PASSWORD).await.status,
        StatusCode::OK
    );
    let (_, refresh) = t.store.create_session(ada.id, CLI, t.now()).await.unwrap();
    local::delete(&t.state, &ada.id).await.unwrap();
    assert!(t.dex.password("ada@example.com").is_none());
    assert!(t.store.user(&ada.id).await.unwrap().disabled_at.is_some());
    assert!(matches!(
        t.store.begin_refresh(&refresh, CLI, t.now()).await.unwrap(),
        RefreshCheck::Ended
    ));
}

#[tokio::test]
async fn only_local_accounts_are_reset_or_deleted() {
    let t = setup().await;
    let github = t.user_of(&t.session(ada()).await.access).await;
    assert!(matches!(
        local::reset(&t.state, &github).await,
        Err(local::LocalError::NotLocal)
    ));
    assert!(matches!(
        local::delete(&t.state, &github).await,
        Err(local::LocalError::NotLocal)
    ));
    assert!(matches!(
        local::reset(&t.state, &Uuid::from_u128(42)).await,
        Err(local::LocalError::Store(
            sekisho_store::StoreError::NotFound
        ))
    ));
}

#[tokio::test]
async fn admin_link_recovers_a_local_admin() {
    let t = setup().await;
    let public = Url::parse(ISSUER).unwrap();

    // Nobody has the address yet: a local account is made.
    let link = local::admin_link(&t.store, &public, "Root@Example.com", t.now())
        .await
        .unwrap();
    assert!(link.url.as_str().starts_with(&format!("{ISSUER}/setup/")));
    let root = t
        .store
        .local_user_by_email("root@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(!root.admin, "an admin only once the link is used");
    assert!(t.dex.password("root@example.com").is_none(), "no Dex call");
    assert_eq!(
        set_password(&t, &link, PASSWORD, PASSWORD).await.status,
        StatusCode::OK
    );
    assert!(t.store.user(&root.id).await.unwrap().admin);
    assert!(t.dex.password("root@example.com").is_some());

    // A disabled local account is enabled again, and made an admin.
    let (ada, _) = invited(&t, "ada@example.com").await;
    t.store.set_disabled(&ada.id, true, t.now()).await.unwrap();
    let link = local::admin_link(&t.store, &public, "ada@example.com", t.now())
        .await
        .unwrap();
    assert!(t.store.user(&ada.id).await.unwrap().disabled_at.is_none());
    assert_eq!(
        set_password(&t, &link, PASSWORD, PASSWORD).await.status,
        StatusCode::OK
    );
    assert!(t.store.user(&ada.id).await.unwrap().admin);

    // Someone signed in through GitHub with the address gets a separate
    // local account: accounts are never linked by email.
    let github = t
        .user_of(
            &t.session(person("github-eve", "eve@example.com", &[]))
                .await
                .access,
        )
        .await;
    local::admin_link(&t.store, &public, "eve@example.com", t.now())
        .await
        .unwrap();
    let eve = t
        .store
        .local_user_by_email("eve@example.com")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(eve.id, github);
    assert!(!t.store.user(&github).await.unwrap().admin);
}

#[tokio::test]
async fn setup_pages_carry_their_security_headers() {
    let t = setup().await;
    let (_, link) = invited(&t, "ada@example.com").await;
    for (what, reply) in [
        ("setup form", t.get("/setup").await),
        ("link form", t.get(&path(&link)).await),
        (
            "form again",
            set_password(&t, &link, "short", "short").await,
        ),
        ("unknown link", t.get("/setup/unknown").await),
    ] {
        assert_page(&reply, what);
        assert!(reply.header(header::LOCATION).is_none(), "{what}");
    }
}
