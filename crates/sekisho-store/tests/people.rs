mod common;

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::time::Duration;

use sekisho_core::keys::KeyState;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{EnvName, GroupName};
use sekisho_core::window::{BudgetWindow, WindowKind};
use sekisho_store::{
    BudgetScope, ChangeKind, EnvAccess, EnvironmentSpec, FieldError, NewBudget, SignIn,
    SignInOutcome, Store, StoreError, User,
};
use time::OffsetDateTime;
use tokio_stream::StreamExt;
use uuid::Uuid;

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn env(name: &str) -> EnvName {
    EnvName::parse(name).unwrap()
}

fn group(name: &str) -> GroupName {
    GroupName::parse(name).unwrap()
}

fn spec(name: &str, access_groups: &[&str]) -> EnvironmentSpec {
    EnvironmentSpec {
        name: env(name),
        description: String::new(),
        notice: String::new(),
        access_groups: access_groups.iter().map(|g| g.to_string()).collect(),
        models: Vec::new(),
        owner: None,
    }
}

fn github(subject: &str, email: &str, groups: &[&str]) -> SignIn {
    SignIn {
        subject: subject.into(),
        email: email.into(),
        name: "A person".into(),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        connector_id: "github".into(),
        connector_user_id: subject.into(),
    }
}

async fn signed_in(store: &Store, s: &SignIn) -> User {
    match store.sign_in(s, now()).await.unwrap() {
        SignInOutcome::User(u) => *u,
        other => panic!("expected a user, got {other:?}"),
    }
}

fn fields(err: StoreError) -> Vec<String> {
    match err {
        StoreError::Validation(f) => f
            .into_iter()
            .map(|FieldError { field, .. }| field)
            .collect(),
        other => panic!("expected Validation, got {other:?}"),
    }
}

fn message(err: StoreError) -> String {
    match err {
        StoreError::Validation(f) => f.into_iter().map(|e| e.message).collect(),
        other => panic!("expected Validation, got {other:?}"),
    }
}

/// A session as Task 4's sign-in will make one.
async fn session(store: &Store, user: Uuid) -> Uuid {
    sqlx::query_scalar(
        "insert into sessions (user_id, client_id, refresh_sha256, idle_expires_at) \
         values ($1, 'sekisho-cli', sha256(gen_random_uuid()::text::bytea), \
         now() + interval '30 days') returning id",
    )
    .bind(user)
    .fetch_one(store.pool())
    .await
    .unwrap()
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
    tokio::time::timeout(Duration::from_secs(2), rx.next())
        .await
        .expect("a change notification")
        .expect("the stream is open")
}

async fn quiet(rx: &mut (impl tokio_stream::Stream<Item = ChangeKind> + Unpin)) -> bool {
    tokio::time::timeout(Duration::from_millis(300), rx.next())
        .await
        .is_err()
}

#[tokio::test]
async fn sign_in_creates_then_finds_by_subject() {
    let (store, _) = common::store().await;
    let first = signed_in(&store, &github("gh|1", "ada@example.com", &["eng"])).await;
    assert_eq!(
        (first.email.as_str(), first.local, first.admin),
        ("ada@example.com", false, false)
    );
    assert_eq!(first.dex_subject.as_deref(), Some("gh|1"));
    assert_eq!(first.dex_connector.as_deref(), Some("github"));
    assert_eq!(first.idp_groups, ["eng"]);
    assert_eq!(first.local_dex_user_id, None);
    assert!(first.last_sign_in_at.is_some() && first.disabled_at.is_none());

    // The identity provider's email wins for its users.
    let again = signed_in(&store, &github("gh|1", "ada@new.example.com", &["eng"])).await;
    assert_eq!(again.id, first.id);
    assert_eq!(again.email, "ada@new.example.com");
    assert_eq!(store.list_users().await.unwrap().len(), 1);
    assert_eq!(store.user(&first.id).await.unwrap(), again);
    assert!(matches!(
        store.user(&Uuid::nil()).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn sign_in_never_links_by_email() {
    let (store, _) = common::store().await;
    let admin = store
        .create_local_user(" Ada@Example.com ", "Ada")
        .await
        .unwrap();
    assert_eq!(
        (admin.email.as_str(), admin.local),
        ("ada@example.com", true)
    );
    store.set_admin(&admin.id, true).await.unwrap();
    assert!(store.user(&admin.id).await.unwrap().admin);

    let gh = signed_in(&store, &github("gh|ada", "ada@example.com", &[])).await;
    assert_ne!(gh.id, admin.id);
    assert!(!gh.admin && !gh.local);
    assert_eq!(store.list_users().await.unwrap().len(), 2);
}

#[tokio::test]
async fn local_accounts_sign_in_by_their_dex_user_id() {
    let (store, _) = common::store().await;
    let local = store
        .create_local_user("ada@example.com", "Ada")
        .await
        .unwrap();
    let dex_user_id = local.local_dex_user_id.clone().expect("a Dex user ID");
    let through_dex = SignIn {
        subject: "CgV1LWFkYRIFbG9jYWw".into(),
        email: "ada@elsewhere.example.com".into(),
        name: "ada".into(),
        groups: vec![],
        connector_id: "local".into(),
        connector_user_id: dex_user_id,
    };
    let u = signed_in(&store, &through_dex).await;
    assert_eq!(u.id, local.id);
    // Sekisho owns a local account's email.
    assert_eq!(u.email, "ada@example.com");
    assert_eq!(u.dex_subject.as_deref(), Some("CgV1LWFkYRIFbG9jYWw"));

    let unknown = SignIn {
        subject: "other".into(),
        connector_user_id: "no-such-id".into(),
        ..through_dex
    };
    assert!(matches!(
        store.sign_in(&unknown, now()).await.unwrap(),
        SignInOutcome::UnknownLocalAccount
    ));

    // One local account per email, whatever its case.
    assert!(matches!(
        store.create_local_user(" ADA@example.com", "x").await,
        Err(StoreError::Conflict(_))
    ));
    let found = store.local_user_by_email("Ada@Example.com").await.unwrap();
    assert_eq!(found.map(|u| u.id), Some(local.id));
    signed_in(&store, &github("gh|bob", "bob@example.com", &[])).await;
    assert_eq!(
        store.local_user_by_email("bob@example.com").await.unwrap(),
        None
    );
    for bad in ["", "not-an-email", "two@@example.com", "a b@example.com"] {
        let err = store.create_local_user(bad, "").await.unwrap_err();
        assert_eq!(fields(err), ["email"], "{bad:?}");
    }
}

#[tokio::test]
async fn a_disabled_user_cannot_sign_in() {
    let (store, _) = common::store().await;
    let u = signed_in(&store, &github("gh|1", "ada@example.com", &[])).await;
    store.set_disabled(&u.id, true, now()).await.unwrap();
    assert!(store.user(&u.id).await.unwrap().disabled_at.is_some());
    assert!(matches!(
        store
            .sign_in(&github("gh|1", "ada@example.com", &[]), now())
            .await
            .unwrap(),
        SignInOutcome::Disabled
    ));

    let local = store
        .create_local_user("bob@example.com", "")
        .await
        .unwrap();
    store.set_disabled(&local.id, true, now()).await.unwrap();
    let through_dex = SignIn {
        subject: "local-bob".into(),
        email: "bob@example.com".into(),
        name: "bob".into(),
        groups: vec![],
        connector_id: "local".into(),
        connector_user_id: local.local_dex_user_id.unwrap(),
    };
    assert!(matches!(
        store.sign_in(&through_dex, now()).await.unwrap(),
        SignInOutcome::Disabled
    ));

    store.set_disabled(&u.id, false, now()).await.unwrap();
    let back = signed_in(&store, &github("gh|1", "ada@example.com", &[])).await;
    assert_eq!((back.id, back.disabled_at), (u.id, None));
    assert!(matches!(
        store.set_disabled(&Uuid::nil(), true, now()).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn sign_in_replaces_idp_groups_and_keeps_sekisho_groups() {
    let (store, _) = common::store().await;
    let u = signed_in(&store, &github("gh|1", "ada@example.com", &["eng", "ops"])).await;
    store.create_group(&group("data-team")).await.unwrap();
    store.add_member(&group("data-team"), &u.id).await.unwrap();

    let again = signed_in(&store, &github("gh|1", "ada@example.com", &["eng"])).await;
    assert_eq!(again.idp_groups, ["eng"]);
    assert_eq!(again.groups, [group("data-team")]);
    let expected: BTreeSet<String> = ["data-team", "eng"].map(String::from).into();
    assert_eq!(again.access().groups, expected);

    let mut rx = store.listen().await.unwrap();
    signed_in(&store, &github("gh|1", "ada@example.com", &["eng"])).await;
    assert!(quiet(&mut rx).await, "unchanged groups announce nothing");
    signed_in(&store, &github("gh|1", "ada@example.com", &["eng", "ml"])).await;
    assert_eq!(next_change(&mut rx).await, ChangeKind::Users);
}

#[tokio::test]
async fn signing_in_again_clears_an_idp_refusal() {
    let (store, _) = common::store().await;
    let u = signed_in(&store, &github("gh|1", "ada@example.com", &["eng"])).await;
    sqlx::query("update users set idp_refused_at = now() where id = $1")
        .bind(u.id)
        .execute(store.pool())
        .await
        .unwrap();
    let refused = store.user(&u.id).await.unwrap();
    assert!(refused.idp_refused_at.is_some() && refused.access().idp_refused);

    let mut rx = store.listen().await.unwrap();
    let back = signed_in(&store, &github("gh|1", "ada@example.com", &["eng"])).await;
    assert_eq!(back.idp_refused_at, None);
    assert_eq!(next_change(&mut rx).await, ChangeKind::Users);
}

#[tokio::test]
async fn migrated_owners_become_users() {
    let (store, _) = common::store().await;
    sqlx::query("create database plan1")
        .execute(store.pool())
        .await
        .unwrap();
    let options = store
        .pool()
        .connect_options()
        .as_ref()
        .clone()
        .database("plan1");
    let pool = sqlx::PgPool::connect_with(options).await.unwrap();
    let mut plan1 = sqlx::migrate!("./migrations");
    plan1.migrations = Cow::Owned(
        plan1
            .migrations
            .iter()
            .filter(|m| m.version <= 4)
            .cloned()
            .collect(),
    );
    plan1.run(&pool).await.unwrap();
    for sql in [
        "insert into environments (name) values ('data-team')",
        "insert into env_keys (id, environment_id, owner, name, secret_sha256) \
         select 'abcdefghijk2', id, 'ci@example.com', 'ci', sha256('a') from environments",
        "insert into env_keys (id, environment_id, owner, name, secret_sha256) \
         select 'abcdefghijk3', id, 'ci@example.com', 'ci2', sha256('b') from environments",
        "insert into budgets (name, scope, person, limit_micro_usd, window_kind, calendar) \
         values ('ada', 'person', 'ada@example.com', 5, 'day', true), \
         ('ci', 'person', 'ci@example.com', 5, 'day', true)",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let users: Vec<(Uuid, String, Option<String>, bool)> =
        sqlx::query_as("select id, email, dex_subject, local from users order by email")
            .fetch_all(&pool)
            .await
            .unwrap();
    let emails: Vec<&str> = users.iter().map(|u| u.1.as_str()).collect();
    assert_eq!(emails, ["ada@example.com", "ci@example.com"]);
    assert!(users.iter().all(|u| u.2.is_none() && !u.3));
    let (ada, ci) = (users[0].0, users[1].0);
    let owners: Vec<Uuid> = sqlx::query_scalar("select owner_id from env_keys order by id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(owners, [ci, ci]);
    let persons: Vec<(String, Uuid)> =
        sqlx::query_as("select name, person_id from budgets order by name")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(persons, [("ada".into(), ada), ("ci".into(), ci)]);
    let env_owner: Option<Uuid> = sqlx::query_scalar("select owner_id from environments")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(env_owner, None);
    // The scope check now covers person_id.
    let err = sqlx::query(
        "insert into budgets (name, scope, limit_micro_usd, window_kind, calendar) \
         values ('x', 'person', 1, 'day', true)",
    )
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        matches!(&err, sqlx::Error::Database(d) if d.constraint() == Some("budgets_check")),
        "{err:?}"
    );
    pool.close().await;
}

#[tokio::test]
async fn keys_need_an_owner_who_can_use_the_environment() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &["eng"])).await;
    let bob = signed_in(&store, &github("gh|bob", "bob@example.com", &["ops"])).await;
    store
        .upsert_environment(spec("data-team", &["eng"]), ada.id)
        .await
        .unwrap();
    let data = env("data-team");
    let (key, _token) = store.create_key(&data, ada.id, "ci", None).await.unwrap();
    assert_eq!(key.owner, ada.id);
    assert_eq!(key.state(now()), KeyState::Active);
    assert_eq!(
        store.list_user_keys(&ada.id).await.unwrap(),
        std::slice::from_ref(&key)
    );
    assert!(store.list_user_keys(&bob.id).await.unwrap().is_empty());

    let err = store
        .create_key(&data, bob.id, "ci2", None)
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["environment"]);
    let err = store
        .create_key(&data, Uuid::nil(), "ci3", None)
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["owner"]);
    store.set_disabled(&ada.id, true, now()).await.unwrap();
    let err = store
        .create_key(&data, ada.id, "ci4", None)
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["environment"]);
    assert!(matches!(
        store.create_key(&env("nowhere"), bob.id, "ci", None).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_key_is_invalid_while_its_owner_lacks_access() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &[])).await;
    store.create_group(&group("data")).await.unwrap();
    store.add_member(&group("data"), &ada.id).await.unwrap();
    store
        .upsert_environment(spec("data-team", &["data"]), ada.id)
        .await
        .unwrap();
    let data = env("data-team");
    let (key, _) = store.create_key(&data, ada.id, "ci", None).await.unwrap();
    let (paused, _) = store
        .create_key(&data, ada.id, "paused", None)
        .await
        .unwrap();
    store.set_suspended(&paused.id, true).await.unwrap();
    let states = || async {
        let listed = store.list_keys(&data).await.unwrap();
        let mine = store.list_user_keys(&ada.id).await.unwrap();
        assert_eq!(listed, mine);
        let found = store.find_key(&key.id).await.unwrap().unwrap().0;
        assert_eq!(found.state(now()), listed[0].state(now()));
        listed.iter().map(|k| k.state(now())).collect::<Vec<_>>()
    };
    assert_eq!(states().await, [KeyState::Active, KeyState::Suspended]);

    store.remove_member(&group("data"), &ada.id).await.unwrap();
    assert_eq!(states().await, [KeyState::Invalid, KeyState::Suspended]);
    store.add_member(&group("data"), &ada.id).await.unwrap();
    assert_eq!(states().await, [KeyState::Active, KeyState::Suspended]);
    store.set_disabled(&ada.id, true, now()).await.unwrap();
    assert_eq!(states().await, [KeyState::Invalid, KeyState::Suspended]);
}

#[tokio::test]
async fn environment_owner_defaults_to_the_creator() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &[])).await;
    let bob = signed_in(&store, &github("gh|bob", "bob@example.com", &[])).await;
    let carol = signed_in(&store, &github("gh|carol", "carol@example.com", &[])).await;

    let e = store
        .upsert_environment(spec("data-team", &["eng"]), ada.id)
        .await
        .unwrap();
    assert_eq!((e.owner, e.spec.owner), (Some(ada.id), Some(ada.id)));
    // An update without an owner keeps the stored one, whoever saves it.
    let e = store
        .upsert_environment(spec("data-team", &["eng"]), bob.id)
        .await
        .unwrap();
    assert_eq!(e.owner, Some(ada.id));
    // An owner named in the spec wins.
    let named = EnvironmentSpec {
        owner: Some(bob.id),
        ..spec("data-team", &["eng"])
    };
    let e = store.upsert_environment(named, ada.id).await.unwrap();
    assert_eq!(e.owner, Some(bob.id));
    assert_eq!(store.get_environment(&env("data-team")).await.unwrap(), e);
    assert_eq!(store.list_environments().await.unwrap(), [e]);

    // An environment saved before this plan takes the actor at its next save.
    sqlx::query("update environments set owner_id = null")
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(
        store
            .get_environment(&env("data-team"))
            .await
            .unwrap()
            .owner,
        None
    );
    let e = store
        .upsert_environment(spec("data-team", &["eng"]), carol.id)
        .await
        .unwrap();
    assert_eq!(e.owner, Some(carol.id));

    // The owner must be an enabled user.
    store.set_disabled(&ada.id, true, now()).await.unwrap();
    for owner in [ada.id, Uuid::nil()] {
        let named = EnvironmentSpec {
            owner: Some(owner),
            ..spec("data-team", &["eng"])
        };
        let err = store.upsert_environment(named, bob.id).await.unwrap_err();
        assert_eq!(fields(err), ["owner"]);
    }
    let err = store
        .upsert_environment(spec("ml", &[]), ada.id)
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["owner"]);
    assert_eq!(
        store
            .get_environment(&env("data-team"))
            .await
            .unwrap()
            .owner,
        Some(carol.id)
    );
}

#[tokio::test]
async fn person_budgets_name_a_user() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &["eng"])).await;
    store
        .upsert_environment(spec("data-team", &["eng"]), ada.id)
        .await
        .unwrap();
    let (key, _) = store
        .create_key(&env("data-team"), ada.id, "ci", None)
        .await
        .unwrap();
    let window = BudgetWindow {
        kind: WindowKind::Day,
        calendar: true,
    };
    let budget = |name: &str, scope| NewBudget {
        name: name.into(),
        scope,
        limit: MicroUsd(5),
        window,
    };
    let person = store
        .create_budget(budget(
            "ada",
            BudgetScope::Person {
                person: ada.id,
                environment: None,
            },
        ))
        .await
        .unwrap();
    let err = store
        .create_budget(budget(
            "nobody",
            BudgetScope::Person {
                person: Uuid::nil(),
                environment: None,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(fields(err), ["person"]);
    let keyed = store
        .create_budget(budget("ci", BudgetScope::Key(key.id.clone())))
        .await
        .unwrap();
    assert_eq!(store.get_budget(person.id).await.unwrap(), person);

    let ids =
        |budgets: Vec<sekisho_store::Budget>| budgets.into_iter().map(|b| b.id).collect::<Vec<_>>();
    let data = env("data-team");
    let with_key = store
        .applicable_budgets(&data, Some(&key.id), ada.id)
        .await
        .unwrap();
    assert_eq!(ids(with_key), [person.id, keyed.id]);
    // A session has no key.
    let without = store.applicable_budgets(&data, None, ada.id).await.unwrap();
    assert_eq!(ids(without), [person.id]);
}

#[tokio::test]
async fn offboarding_hands_over_revokes_and_ends() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &["eng"])).await;
    let bob = signed_in(&store, &github("gh|bob", "bob@example.com", &["eng"])).await;
    for name in ["data-team", "ml"] {
        store
            .upsert_environment(spec(name, &["eng"]), ada.id)
            .await
            .unwrap();
    }
    store
        .upsert_environment(spec("ops", &["eng"]), bob.id)
        .await
        .unwrap();
    let data = env("data-team");
    let (k1, _) = store.create_key(&data, ada.id, "ci", None).await.unwrap();
    let (k2, _) = store
        .create_key(&data, ada.id, "paused", None)
        .await
        .unwrap();
    store.set_suspended(&k2.id, true).await.unwrap();
    let (k3, _) = store.create_key(&data, bob.id, "bob", None).await.unwrap();
    let (s1, s2, s3) = (
        session(&store, ada.id).await,
        session(&store, ada.id).await,
        session(&store, bob.id).await,
    );

    let mut rx = store.listen().await.unwrap();
    let out = store.offboard(&ada.id, Some(bob.id), now()).await.unwrap();
    assert_eq!(out.dex_subject.as_deref(), Some("gh|ada"));
    assert_eq!(out.ended_sessions, 2);
    let mut revoked = out.revoked_keys.clone();
    revoked.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut expected = vec![k1.id.clone(), k2.id.clone()];
    expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(revoked, expected);
    assert_eq!(out.handed_over, [env("data-team"), env("ml")]);

    assert!(store.user(&ada.id).await.unwrap().disabled_at.is_some());
    let states: Vec<(String, KeyState)> = store
        .list_keys(&data)
        .await
        .unwrap()
        .into_iter()
        .map(|k| (k.name.clone(), k.state(now())))
        .collect();
    assert_eq!(
        states,
        [
            ("ci".into(), KeyState::Revoked),
            ("paused".into(), KeyState::Revoked),
            ("bob".into(), KeyState::Active),
        ]
    );
    assert_eq!(k3.owner, bob.id);
    for name in ["data-team", "ml", "ops"] {
        assert_eq!(
            store.get_environment(&env(name)).await.unwrap().owner,
            Some(bob.id)
        );
    }
    assert_eq!(ended(&store, s1).await.as_deref(), Some("offboarded"));
    assert_eq!(ended(&store, s2).await.as_deref(), Some("offboarded"));
    assert_eq!(ended(&store, s3).await, None);
    let mut seen = BTreeSet::new();
    for _ in 0..4 {
        seen.insert(format!("{:?}", next_change(&mut rx).await));
    }
    let kinds: BTreeSet<String> = ["Environments", "Keys", "Sessions", "Users"]
        .map(String::from)
        .into();
    assert_eq!(seen, kinds);

    // Someone who owns no environment needs no new owner.
    let carol = signed_in(&store, &github("gh|carol", "carol@example.com", &[])).await;
    let out = store.offboard(&carol.id, None, now()).await.unwrap();
    assert_eq!((out.ended_sessions, out.handed_over.len()), (0, 0));
    assert!(matches!(
        store.offboard(&Uuid::nil(), Some(bob.id), now()).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn offboarding_refuses_a_bad_new_owner() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &["eng"])).await;
    let carol = signed_in(&store, &github("gh|carol", "carol@example.com", &[])).await;
    store.set_disabled(&carol.id, true, now()).await.unwrap();
    store
        .upsert_environment(spec("data-team", &["eng"]), ada.id)
        .await
        .unwrap();
    let (key, _) = store
        .create_key(&env("data-team"), ada.id, "ci", None)
        .await
        .unwrap();
    let s = session(&store, ada.id).await;

    let err = store.offboard(&ada.id, None, now()).await.unwrap_err();
    assert!(message(err).contains("data-team"));
    for new_owner in [None, Some(ada.id), Some(Uuid::nil()), Some(carol.id)] {
        let err = store.offboard(&ada.id, new_owner, now()).await.unwrap_err();
        assert_eq!(fields(err), ["new_owner"], "{new_owner:?}");
    }
    // Nothing changed.
    assert_eq!(store.user(&ada.id).await.unwrap().disabled_at, None);
    assert_eq!(
        store
            .get_environment(&env("data-team"))
            .await
            .unwrap()
            .owner,
        Some(ada.id)
    );
    assert_eq!(
        store
            .find_key(&key.id)
            .await
            .unwrap()
            .unwrap()
            .0
            .state(now()),
        KeyState::Active
    );
    assert_eq!(ended(&store, s).await, None);
}

#[tokio::test]
async fn disabling_ends_sessions() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &[])).await;
    let bob = signed_in(&store, &github("gh|bob", "bob@example.com", &[])).await;
    let local = store
        .create_local_user("carol@example.com", "")
        .await
        .unwrap();
    let (a1, a2, b1, c1) = (
        session(&store, ada.id).await,
        session(&store, ada.id).await,
        session(&store, bob.id).await,
        session(&store, local.id).await,
    );

    let mut rx = store.listen().await.unwrap();
    store.set_disabled(&ada.id, true, now()).await.unwrap();
    assert_eq!(ended(&store, a1).await.as_deref(), Some("disabled"));
    assert_eq!(ended(&store, a2).await.as_deref(), Some("disabled"));
    assert_eq!(ended(&store, b1).await, None);
    let mut seen = BTreeSet::new();
    for _ in 0..2 {
        seen.insert(format!("{:?}", next_change(&mut rx).await));
    }
    assert_eq!(seen, ["Sessions", "Users"].map(String::from).into());

    // Enabling again brings no session back.
    store.set_disabled(&ada.id, false, now()).await.unwrap();
    assert_eq!(ended(&store, a1).await.as_deref(), Some("disabled"));

    assert!(store.end_session(&b1, "logout", now()).await.unwrap());
    assert!(!store.end_session(&b1, "logout", now()).await.unwrap());
    assert_eq!(ended(&store, b1).await.as_deref(), Some("logout"));

    // Every session of the users whose latest sign-in came through `github`.
    let b2 = session(&store, bob.id).await;
    let n = store
        .end_connector_sessions("github", "connector_deleted", now())
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        ended(&store, b2).await.as_deref(),
        Some("connector_deleted")
    );
    assert_eq!(ended(&store, c1).await, None);
}

#[tokio::test]
async fn groups_and_members() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &[])).await;
    let data = group("data-team");
    let mut rx = store.listen().await.unwrap();
    store.create_group(&data).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Users);
    assert!(matches!(
        store.create_group(&data).await,
        Err(StoreError::Conflict(_))
    ));
    store.add_member(&data, &ada.id).await.unwrap();
    store.add_member(&data, &ada.id).await.unwrap(); // already a member
    assert!(matches!(
        store.add_member(&group("nowhere"), &ada.id).await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        fields(store.add_member(&data, &Uuid::nil()).await.unwrap_err()),
        ["user"]
    );
    assert_eq!(
        store.list_groups().await.unwrap(),
        [(data.clone(), vec![ada.id])]
    );
    assert_eq!(
        store.user(&ada.id).await.unwrap().groups,
        std::slice::from_ref(&data)
    );

    while !quiet(&mut rx).await {}
    store.remove_member(&data, &ada.id).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Users);
    assert_eq!(store.list_groups().await.unwrap(), [(data.clone(), vec![])]);

    store.add_member(&data, &ada.id).await.unwrap();
    while !quiet(&mut rx).await {}
    store.delete_group(&data).await.unwrap();
    assert_eq!(next_change(&mut rx).await, ChangeKind::Users);
    assert!(store.user(&ada.id).await.unwrap().groups.is_empty());
    assert!(store.list_groups().await.unwrap().is_empty());
    assert!(matches!(
        store.delete_group(&data).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn auth_snapshot_lists_people() {
    let (store, _) = common::store().await;
    let ada = signed_in(&store, &github("gh|ada", "ada@example.com", &["eng"])).await;
    let bob = signed_in(&store, &github("gh|bob", "bob@example.com", &[])).await;
    let carol = signed_in(&store, &github("gh|carol", "carol@example.com", &[])).await;
    store.create_group(&group("data")).await.unwrap();
    store.add_member(&group("data"), &ada.id).await.unwrap();
    store.set_disabled(&bob.id, true, now()).await.unwrap();
    store.set_admin(&carol.id, true).await.unwrap();
    store
        .upsert_environment(spec("data-team", &["data"]), ada.id)
        .await
        .unwrap();
    let (key, _) = store
        .create_key(&env("data-team"), ada.id, "ci", None)
        .await
        .unwrap();
    let (recent, old, active) = (
        session(&store, ada.id).await,
        session(&store, ada.id).await,
        session(&store, ada.id).await,
    );
    for (id, ago) in [(recent, "10 minutes"), (old, "2 hours")] {
        sqlx::query(
            "update sessions set ended_at = now() - $2::text::interval, end_reason = 'logout' \
             where id = $1",
        )
        .bind(id)
        .bind(ago)
        .execute(store.pool())
        .await
        .unwrap();
    }

    let snap = store.auth_snapshot(now()).await.unwrap();
    let user = |id: Uuid| snap.users.iter().find(|u| u.id == id).unwrap();
    let ada_groups: BTreeSet<String> = ["data", "eng"].map(String::from).into();
    assert_eq!(user(ada.id).access.groups, ada_groups);
    assert!(!user(ada.id).admin_mark && !user(ada.id).access.disabled);
    assert!(user(bob.id).access.disabled);
    assert!(user(carol.id).admin_mark);
    assert_eq!(snap.users.len(), 3);
    assert_eq!(
        snap.environments,
        [EnvAccess {
            name: env("data-team"),
            access_groups: vec!["data".into()],
            owner: Some(ada.id),
        }]
    );
    assert_eq!(snap.keys.len(), 1);
    assert_eq!(
        (snap.keys[0].0.id.clone(), snap.keys[0].0.owner),
        (key.id, ada.id)
    );
    assert_eq!(snap.ended_sessions, [recent]);
    assert!(!snap.ended_sessions.contains(&active));
    assert!(snap.public_keys.is_empty());
}
