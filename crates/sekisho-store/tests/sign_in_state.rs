mod common;

use std::collections::{BTreeSet, HashSet};
use std::time::Duration as StdDuration;

use sekisho_core::keys::hash_secret;
use sekisho_core::names::{EnvName, ModelName};
use sekisho_store::{
    AuthFlow, ChangeKind, DevicePoll, EnvironmentSpec, NewAuthRequest, NewCode, RefreshCheck, Store,
};
use time::{Duration, OffsetDateTime};
use tokio_stream::StreamExt;
use uuid::Uuid;

const CLI: &str = "sekisho-cli";
const CONSONANTS: &str = "BCDFGHJKLMNPQRSTVWXZ";

fn t0() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Database timestamps keep microseconds.
fn close(a: OffsetDateTime, b: OffsetDateTime) -> bool {
    (a - b).abs() < Duration::milliseconds(1)
}

async fn person(store: &Store, email: &str) -> Uuid {
    common::person(store, email, &["eng"]).await
}

async fn ended(store: &Store, session: Uuid) -> Option<String> {
    sqlx::query_scalar("select end_reason from sessions where id = $1")
        .bind(session)
        .fetch_one(store.pool())
        .await
        .unwrap()
}

async fn next_change(
    rx: &mut (impl tokio_stream::Stream<Item = ChangeKind> + Unpin),
) -> ChangeKind {
    tokio::time::timeout(StdDuration::from_secs(2), rx.next())
        .await
        .expect("a change notification")
        .expect("the stream is open")
}

/// Rotates `token` at `at` and returns the new one.
async fn rotated(store: &Store, token: &str, at: OffsetDateTime) -> String {
    match store.begin_refresh(token, CLI, at).await.unwrap() {
        RefreshCheck::Live(mut lock) => {
            let new = lock.rotate(at).await.unwrap();
            lock.commit().await.unwrap();
            new
        }
        other => panic!("expected Live, got {other:?}"),
    }
}

fn is_live(check: &RefreshCheck) -> bool {
    matches!(check, RefreshCheck::Live(_))
}

#[tokio::test]
async fn refresh_tokens_rotate_and_are_stored_hashed() {
    let (store, _) = common::store().await;
    let ada = person(&store, "ada@example.com").await;
    let t = t0();
    let (session, token) = store.create_session(ada, CLI, t).await.unwrap();
    assert_eq!((session.user_id, session.client_id.as_str()), (ada, CLI));
    assert!(close(session.idle_expires_at, t + Duration::days(30)));
    assert_eq!(token.len(), 43);
    let stored: Vec<u8> = sqlx::query_scalar("select refresh_sha256 from sessions where id = $1")
        .bind(session.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(stored, hash_secret(&token));

    let later = t + Duration::hours(1);
    let check = store.begin_refresh(&token, CLI, later).await.unwrap();
    let RefreshCheck::Live(mut lock) = check else {
        panic!("expected Live, got {check:?}");
    };
    assert_eq!(lock.session().id, session.id);
    let new = lock.rotate(later).await.unwrap();
    lock.commit().await.unwrap();
    assert_ne!(new, token);
    let stored: (Vec<u8>, Vec<u8>, OffsetDateTime) = sqlx::query_as(
        "select refresh_sha256, previous_refresh_sha256, idle_expires_at from sessions where id = $1",
    )
    .bind(session.id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        (stored.0, stored.1),
        (hash_secret(&new).to_vec(), hash_secret(&token).to_vec())
    );
    assert!(close(stored.2, later + Duration::days(30)));

    assert!(is_live(
        &store.begin_refresh(&new, CLI, later).await.unwrap()
    ));
    assert!(matches!(
        store
            .begin_refresh(&new, "another-client", later)
            .await
            .unwrap(),
        RefreshCheck::OtherClient
    ));
    assert!(matches!(
        store
            .begin_refresh("not-a-token", CLI, later)
            .await
            .unwrap(),
        RefreshCheck::Unknown
    ));

    // RFC 7009: revoking the token ends its session, once, for its own client.
    assert!(
        !store
            .end_session_by_refresh(&new, "another-client", later)
            .await
            .unwrap()
    );
    assert!(
        store
            .end_session_by_refresh(&new, CLI, later)
            .await
            .unwrap()
    );
    assert!(
        !store
            .end_session_by_refresh(&new, CLI, later)
            .await
            .unwrap()
    );
    assert_eq!(ended(&store, session.id).await.as_deref(), Some("revoked"));
    assert!(matches!(
        store.begin_refresh(&new, CLI, later).await.unwrap(),
        RefreshCheck::Ended
    ));
}

#[tokio::test]
async fn a_reused_refresh_token_within_the_grace_keeps_the_session() {
    let (store, _) = common::store().await;
    let ada = person(&store, "ada@example.com").await;
    let t = t0();
    let (session, old) = store.create_session(ada, CLI, t).await.unwrap();
    let t1 = t + Duration::minutes(1);
    let new = rotated(&store, &old, t1).await;
    assert!(matches!(
        store
            .begin_refresh(&old, CLI, t1 + Duration::seconds(5))
            .await
            .unwrap(),
        RefreshCheck::ReusedInGrace
    ));
    assert_eq!(ended(&store, session.id).await, None);
    assert!(is_live(
        &store
            .begin_refresh(&new, CLI, t1 + Duration::seconds(6))
            .await
            .unwrap()
    ));
}

#[tokio::test]
async fn a_reused_refresh_token_after_the_grace_ends_the_session() {
    let (store, _) = common::store().await;
    let ada = person(&store, "ada@example.com").await;
    let t = t0();
    let (session, old) = store.create_session(ada, CLI, t).await.unwrap();
    let t1 = t + Duration::minutes(1);
    let new = rotated(&store, &old, t1).await;
    let mut rx = store.listen().await.unwrap();
    assert!(matches!(
        store
            .begin_refresh(&old, CLI, t1 + Duration::seconds(11))
            .await
            .unwrap(),
        RefreshCheck::Reused
    ));
    assert_eq!(next_change(&mut rx).await, ChangeKind::Sessions);
    assert_eq!(ended(&store, session.id).await.as_deref(), Some("reused"));
    assert!(matches!(
        store
            .begin_refresh(&new, CLI, t1 + Duration::seconds(12))
            .await
            .unwrap(),
        RefreshCheck::Ended
    ));
}

#[tokio::test]
async fn sessions_end_after_30_idle_days() {
    let (store, _) = common::store().await;
    let ada = person(&store, "ada@example.com").await;
    let t = t0();
    let (_, token) = store.create_session(ada, CLI, t).await.unwrap();
    // Each refresh moves the limit: 29 days, then 29 more, is fine.
    let t1 = t + Duration::days(29);
    let token = rotated(&store, &token, t1).await;
    let (idle, idle_token) = store.create_session(ada, CLI, t1).await.unwrap();
    let t2 = t1 + Duration::days(29);
    assert!(is_live(
        &store.begin_refresh(&token, CLI, t2).await.unwrap()
    ));

    let late = t1 + Duration::days(30) + Duration::seconds(1);
    assert!(matches!(
        store.begin_refresh(&idle_token, CLI, late).await.unwrap(),
        RefreshCheck::Idle
    ));
    assert_eq!(ended(&store, idle.id).await.as_deref(), Some("idle"));
    assert!(matches!(
        store.begin_refresh(&idle_token, CLI, late).await.unwrap(),
        RefreshCheck::Ended
    ));
}

#[tokio::test]
async fn requests_and_codes_work_once_and_expire() {
    let (store, sb) = common::store().await;
    let ada = person(&store, "ada@example.com").await;
    let t = t0();
    let request = || NewAuthRequest {
        client_id: CLI.into(),
        flow: AuthFlow::Code {
            redirect_uri: "http://127.0.0.1:8123/callback".into(),
            client_state: Some("client-state".into()),
            code_challenge: "client-challenge".into(),
        },
        dex_verifier: "dex-pkce-verifier-value".into(),
        nonce: "dex-nonce".into(),
    };
    let state = store.save_auth_request(&sb, request(), t).await.unwrap();
    let (state_sha, verifier): (Vec<u8>, Vec<u8>) =
        sqlx::query_as("select state_sha256, verifier_ciphertext from auth_requests")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(state_sha, hash_secret(&state));
    assert!(!String::from_utf8_lossy(&verifier).contains("dex-pkce-verifier-value"));

    let taken = store
        .take_auth_request(&sb, &state, t + Duration::minutes(9))
        .await
        .unwrap()
        .expect("the request");
    let want = request();
    assert_eq!(
        (
            &taken.client_id,
            &taken.flow,
            &taken.dex_verifier,
            &taken.nonce
        ),
        (&want.client_id, &want.flow, &want.dex_verifier, &want.nonce)
    );
    assert_eq!(store.take_auth_request(&sb, &state, t).await.unwrap(), None);
    let late = store.save_auth_request(&sb, request(), t).await.unwrap();
    assert_eq!(
        store
            .take_auth_request(&sb, &late, t + Duration::minutes(10))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store.take_auth_request(&sb, "unknown", t).await.unwrap(),
        None
    );

    let new_code = || NewCode {
        client_id: CLI.into(),
        redirect_uri: "http://127.0.0.1:8123/callback".into(),
        code_challenge: "client-challenge".into(),
        user: ada,
    };
    let code = store.save_code(new_code(), t).await.unwrap();
    let code_sha: Vec<u8> = sqlx::query_scalar("select code_sha256 from auth_codes")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(code_sha, hash_secret(&code));
    let grant = store
        .take_code(&code, t + Duration::seconds(59))
        .await
        .unwrap()
        .expect("the grant");
    let want = new_code();
    assert_eq!(
        (
            &grant.client_id,
            &grant.redirect_uri,
            &grant.code_challenge,
            grant.user
        ),
        (
            &want.client_id,
            &want.redirect_uri,
            &want.code_challenge,
            want.user
        )
    );
    assert_eq!(store.take_code(&code, t).await.unwrap(), None);
    let late = store.save_code(new_code(), t).await.unwrap();
    assert_eq!(
        store
            .take_code(&late, t + Duration::seconds(60))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn device_codes_poll_slow_down_and_issue_once() {
    let (store, sb) = common::store().await;
    let ada = person(&store, "ada@example.com").await;
    let t = t0();
    let start = store.create_device(CLI, t).await.unwrap();
    assert_eq!((start.expires_in, start.interval), (600, 5));
    let code = start.device_code.clone();
    let poll = |at: OffsetDateTime, client: &'static str| {
        let (store, code) = (&store, code.clone());
        async move { store.poll_device(&code, client, at).await.unwrap() }
    };
    assert_eq!(poll(t, CLI).await, DevicePoll::Pending);
    // Sooner than the interval: slow down, and the interval grows by 5 s.
    assert_eq!(
        poll(t + Duration::seconds(2), CLI).await,
        DevicePoll::SlowDown
    );
    assert_eq!(
        poll(t + Duration::seconds(9), CLI).await,
        DevicePoll::SlowDown
    );
    let t1 = t + Duration::seconds(24); // 15 s after the last poll
    assert_eq!(poll(t1, CLI).await, DevicePoll::Pending);
    assert_eq!(
        poll(t1 + Duration::seconds(15), "another-client").await,
        DevicePoll::Unknown
    );
    assert_eq!(
        store.poll_device("unknown", CLI, t1).await.unwrap(),
        DevicePoll::Unknown
    );

    let device = store
        .device_by_user_code(&start.user_code.to_lowercase(), t1)
        .await
        .unwrap()
        .expect("the device");
    assert_eq!(device.client_id, CLI);
    assert_eq!(device.user_code, start.user_code);
    store.approve_device(&device, ada).await.unwrap();
    // Approved: no longer waiting for anyone to enter the code.
    assert_eq!(
        store
            .device_by_user_code(&start.user_code, t1)
            .await
            .unwrap(),
        None
    );
    let t2 = t1 + Duration::seconds(30);
    assert_eq!(poll(t2, CLI).await, DevicePoll::Approved(ada));
    assert_eq!(
        poll(t2 + Duration::seconds(30), CLI).await,
        DevicePoll::Unknown
    );

    // A device's Dex sign-in names it.
    let other = store.create_device(CLI, t).await.unwrap();
    let pending = store
        .device_by_user_code(&other.user_code, t)
        .await
        .unwrap()
        .unwrap();
    let state = store
        .save_auth_request(
            &sb,
            NewAuthRequest {
                client_id: CLI.into(),
                flow: AuthFlow::Device(pending.clone()),
                dex_verifier: "v".into(),
                nonce: "n".into(),
            },
            t,
        )
        .await
        .unwrap();
    let taken = store
        .take_auth_request(&sb, &state, t)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.flow, AuthFlow::Device(pending.clone()));
    store.deny_device(&pending).await.unwrap();
    assert_eq!(
        store.poll_device(&other.device_code, CLI, t).await.unwrap(),
        DevicePoll::Denied
    );

    let late = store.create_device(CLI, t).await.unwrap();
    let expired = t + Duration::minutes(10);
    assert_eq!(
        store
            .device_by_user_code(&late.user_code, expired)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .poll_device(&late.device_code, CLI, expired)
            .await
            .unwrap(),
        DevicePoll::Expired
    );
    let device_sha: Vec<Vec<u8>> = sqlx::query_scalar("select device_sha256 from device_codes")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert!(device_sha.contains(&hash_secret(&late.device_code).to_vec()));
}

#[tokio::test]
async fn user_codes_use_twenty_consonants() {
    let (store, _) = common::store().await;
    let t = t0();
    let mut codes = HashSet::new();
    let mut letters = BTreeSet::new();
    for _ in 0..1000 {
        let code = store.create_device(CLI, t).await.unwrap().user_code;
        let (a, b) = code.split_once('-').expect("XXXX-XXXX");
        assert_eq!((a.len(), b.len()), (4, 4), "{code}");
        for c in a.chars().chain(b.chars()) {
            assert!(CONSONANTS.contains(c), "{code}");
            letters.insert(c);
        }
        codes.insert(code);
    }
    assert_eq!(codes.len(), 1000);
    assert_eq!(letters.len(), 20);

    let code = codes.iter().next().unwrap();
    let typed = code.to_lowercase().replace('-', " ");
    let found = store
        .device_by_user_code(&typed, t)
        .await
        .unwrap()
        .expect("found");
    assert_eq!(&found.user_code, code);
    let squashed = code.replace('-', "");
    assert!(
        store
            .device_by_user_code(&squashed, t)
            .await
            .unwrap()
            .is_some()
    );
    for bad in ["", "AAAA-AAAA", "BCDF-GHJ", "BCDF-GHJKL"] {
        assert_eq!(
            store.device_by_user_code(bad, t).await.unwrap(),
            None,
            "{bad}"
        );
    }
}

#[tokio::test]
async fn setup_links_work_once_expire_and_replace_older_ones() {
    let (store, _) = common::store().await;
    let ada = store
        .create_local_user("ada@example.com", "Ada")
        .await
        .unwrap();
    let t = t0();
    let (first, expires_at) = store.create_setup_link(ada.id, true, t).await.unwrap();
    assert!(close(expires_at, t + Duration::days(7)));
    let found = store
        .setup_link(&first, t + Duration::days(1))
        .await
        .unwrap();
    assert_eq!(found.map(|(u, admin)| (u.id, admin)), Some((ada.id, true)));

    let (second, _) = store.create_setup_link(ada.id, false, t).await.unwrap();
    assert_eq!(store.setup_link(&first, t).await.unwrap(), None);
    let stored: Vec<u8> = sqlx::query_scalar("select token_sha256 from setup_links")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(stored, hash_secret(&second));
    let found = store.setup_link(&second, t).await.unwrap();
    assert_eq!(found.map(|(u, admin)| (u.id, admin)), Some((ada.id, false)));
    assert!(
        store
            .use_setup_link(&second, t + Duration::days(2))
            .await
            .unwrap()
    );
    assert!(
        !store
            .use_setup_link(&second, t + Duration::days(2))
            .await
            .unwrap()
    );
    assert_eq!(store.setup_link(&second, t).await.unwrap(), None);

    let (third, _) = store.create_setup_link(ada.id, false, t).await.unwrap();
    let late = t + Duration::days(7) + Duration::seconds(1);
    assert_eq!(store.setup_link(&third, late).await.unwrap(), None);
    assert!(!store.use_setup_link(&third, late).await.unwrap());
    assert_eq!(store.setup_link("unknown", t).await.unwrap(), None);
}

#[tokio::test]
async fn install_settings_are_made_once() {
    let (store, sb) = common::store().await;
    let t = t0();
    assert!(!store.setup_completed().await.unwrap());
    store.complete_setup(t).await.unwrap();
    assert!(store.setup_completed().await.unwrap());
    assert!(store.complete_setup(t).await.is_err());

    let secret = store.dex_client_secret(&sb).await.unwrap();
    assert_eq!(secret.len(), 43);
    assert_eq!(store.dex_client_secret(&sb).await.unwrap(), secret);
    let sealed: Vec<u8> =
        sqlx::query_scalar("select value from install_settings where name = 'dex_client_secret'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains(&secret));
}

#[tokio::test]
async fn signing_keys_rotate_and_publish_ahead() {
    let (store, sb) = common::store().await;
    let t = t0();
    assert!(store.active_signing_key(&sb, t).await.unwrap().is_none());
    let mut rx = store.listen().await.unwrap();
    assert!(store.rotate_signing_key(&sb, t, false).await.unwrap());
    assert_eq!(next_change(&mut rx).await, ChangeKind::SigningKeys);
    // The first key signs at once.
    let first = store
        .active_signing_key(&sb, t)
        .await
        .unwrap()
        .expect("a key");
    assert_eq!(store.published_keys(t).await.unwrap(), [first.public()]);
    let seed: Vec<u8> = sqlx::query_scalar("select seed_ciphertext from signing_keys")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(seed.windows(32).all(|w| w != first.seed()));
    assert!(
        !store
            .rotate_signing_key(&sb, t + Duration::days(1), false)
            .await
            .unwrap()
    );

    // A forced rotation is published a minute before it signs.
    let t1 = t + Duration::days(2);
    assert!(store.rotate_signing_key(&sb, t1, true).await.unwrap());
    assert_eq!(
        store
            .active_signing_key(&sb, t1)
            .await
            .unwrap()
            .unwrap()
            .kid(),
        first.kid()
    );
    let published = store.published_keys(t1).await.unwrap();
    assert_eq!(published.len(), 2);
    let second = store
        .active_signing_key(&sb, t1 + Duration::seconds(60))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(second.kid(), first.kid());
    assert!(published.contains(&second.public()) && published.contains(&first.public()));
    let snapshot = store.auth_snapshot(t1).await.unwrap();
    assert_eq!(snapshot.public_keys, published);

    // The retired key stays published 65 minutes after it stopped signing.
    let switched = t1 + Duration::seconds(60);
    let kids = |keys: Vec<sekisho_core::session_token::PublicKey>| {
        keys.into_iter().map(|k| k.kid).collect::<Vec<_>>()
    };
    let soon = store
        .published_keys(switched + Duration::minutes(64))
        .await
        .unwrap();
    assert!(kids(soon).contains(&first.kid().to_owned()));
    let gone = switched + Duration::minutes(66);
    assert_eq!(
        kids(store.published_keys(gone).await.unwrap()),
        [second.kid().to_owned()]
    );
    store.rotate_signing_key(&sb, gone, false).await.unwrap();
    let rows: i64 = sqlx::query_scalar("select count(*) from signing_keys")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
    // Thirty days after the second key began to sign, the next is due.
    assert!(
        !store
            .rotate_signing_key(&sb, switched + Duration::days(29), false)
            .await
            .unwrap()
    );
    assert!(
        store
            .rotate_signing_key(&sb, switched + Duration::days(30), false)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn concurrent_rotations_make_one_key() {
    let (store, sb) = common::store().await;
    let t = t0();
    let (a, b) = tokio::join!(
        store.rotate_signing_key(&sb, t, false),
        store.rotate_signing_key(&sb, t, false)
    );
    assert_eq!(
        [a.unwrap(), b.unwrap()]
            .iter()
            .filter(|made| **made)
            .count(),
        1
    );
    assert_eq!(store.published_keys(t).await.unwrap().len(), 1);
}

#[tokio::test]
async fn dex_tokens_are_sealed_and_locked() {
    let (store, sb) = common::store().await;
    let ada = person(&store, "ada@example.com").await;
    store
        .save_dex_token(&sb, ada, "dex-refresh-one")
        .await
        .unwrap();
    let sealed: Vec<u8> = sqlx::query_scalar("select refresh_ciphertext from dex_tokens")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains("dex-refresh-one"));

    let mut first = store.lock_dex_token(&sb, ada).await.unwrap();
    assert_eq!(first.token(), Some("dex-refresh-one"));
    let second = {
        let (store, sb) = ((*store).clone(), sb.clone());
        tokio::spawn(async move {
            let lock = store.lock_dex_token(&sb, ada).await.unwrap();
            lock.token().map(str::to_owned)
        })
    };
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert!(!second.is_finished(), "the second lock waits for the first");
    first.replace("dex-refresh-two").await.unwrap();
    first.commit().await.unwrap();
    assert_eq!(second.await.unwrap().as_deref(), Some("dex-refresh-two"));

    let mut lock = store.lock_dex_token(&sb, ada).await.unwrap();
    lock.delete().await.unwrap();
    lock.commit().await.unwrap();
    assert_eq!(store.lock_dex_token(&sb, ada).await.unwrap().token(), None);

    store
        .save_dex_token(&sb, ada, "dex-refresh-three")
        .await
        .unwrap();
    store
        .save_dex_token(&sb, ada, "dex-refresh-four")
        .await
        .unwrap();
    assert_eq!(
        store.lock_dex_token(&sb, ada).await.unwrap().token(),
        Some("dex-refresh-four")
    );
    store.delete_dex_token(&ada).await.unwrap();
    assert_eq!(store.lock_dex_token(&sb, ada).await.unwrap().token(), None);

    // Offboarding deletes it in its own transaction.
    store
        .save_dex_token(&sb, ada, "dex-refresh-five")
        .await
        .unwrap();
    store.offboard(&ada, None, t0()).await.unwrap();
    assert_eq!(store.lock_dex_token(&sb, ada).await.unwrap().token(), None);
}

#[tokio::test]
async fn idp_checks_are_claimed_once_a_day() {
    let (store, sb) = common::store().await;
    let admin = common::admin(&store).await;
    let spec = EnvironmentSpec {
        name: EnvName::parse("data-team").unwrap(),
        description: String::new(),
        notice: String::new(),
        access_groups: vec!["eng".into()],
        models: Vec::<ModelName>::new(),
        owner: None,
    };
    let data = store
        .upsert_environment(spec, admin)
        .await
        .unwrap()
        .spec
        .name;
    let t = t0();
    // Who is due: an enabled user with an active key and a Dex token.
    let owner = |email: String, key: bool, token: bool| {
        let (store, sb, data) = (&store, &sb, data.clone());
        async move {
            let id = person(store, &email).await;
            if key {
                store.create_key(&data, id, &email, None).await.unwrap();
            }
            if token {
                store.save_dex_token(sb, id, "dex-token").await.unwrap();
            }
            id
        }
    };
    let ada = owner("ada@example.com".into(), true, true).await;
    owner("bob@example.com".into(), true, false).await; // no Dex token
    owner("carol@example.com".into(), false, true).await; // no key
    let dave = owner("dave@example.com".into(), true, true).await;
    let dave_key = store.list_user_keys(&dave).await.unwrap().remove(0);
    store.set_suspended(&dave_key.id, true).await.unwrap(); // no active key
    let erin = owner("erin@example.com".into(), true, true).await;
    store.set_disabled(&erin, true, t).await.unwrap();

    assert_eq!(store.claim_idp_checks(t, 10).await.unwrap(), [ada]);
    assert!(
        store
            .claim_idp_checks(t + Duration::hours(1), 10)
            .await
            .unwrap()
            .is_empty()
    );
    let next_day = t + Duration::hours(24) + Duration::seconds(1);
    assert_eq!(store.claim_idp_checks(next_day, 10).await.unwrap(), [ada]);
    store.release_idp_check(&ada).await.unwrap();
    assert_eq!(store.claim_idp_checks(next_day, 10).await.unwrap(), [ada]);

    // Refused at the identity provider: sessions end, the Dex token goes.
    let (session, _) = store.create_session(ada, CLI, t).await.unwrap();
    let mut rx = store.listen().await.unwrap();
    store.mark_idp_refused(ada, t).await.unwrap();
    let mut seen = BTreeSet::new();
    for _ in 0..2 {
        seen.insert(format!("{:?}", next_change(&mut rx).await));
    }
    assert_eq!(seen, ["Sessions", "Users"].map(String::from).into());
    assert!(store.user(&ada).await.unwrap().idp_refused_at.is_some());
    assert_eq!(
        ended(&store, session.id).await.as_deref(),
        Some("idp_refused")
    );
    assert_eq!(store.lock_dex_token(&sb, ada).await.unwrap().token(), None);

    // Two sweeps at once share no user.
    let mut due = Vec::new();
    for i in 0..20 {
        due.push(owner(format!("owner{i}@example.com"), true, true).await);
    }
    let later = next_day + Duration::days(2);
    let (a, b) = tokio::join!(
        store.claim_idp_checks(later, 20),
        store.claim_idp_checks(later, 20)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let a_set: HashSet<Uuid> = a.iter().copied().collect();
    assert!(b.iter().all(|id| !a_set.contains(id)), "{a:?} {b:?}");
    let all: HashSet<Uuid> = a.into_iter().chain(b).collect();
    assert_eq!(all, due.into_iter().collect::<HashSet<Uuid>>());
}
