use std::sync::Arc;
use std::time::Duration;

use sekisho_core::keys::{EnvKeyId, hash_secret, parse_token};
use sekisho_core::names::EnvName;
use sekisho_hooks::cache::{AuthCache, CachedKey};
use sekisho_store::{EnvironmentSpec, Store};
use time::OffsetDateTime;
use tokio::time::{Instant, sleep};

const ISSUER: &str = "https://sekisho.test";

async fn env(store: &Store, name: &str) -> EnvName {
    let spec = EnvironmentSpec {
        name: EnvName::parse(name).unwrap(),
        description: String::new(),
        notice: String::new(),
        access_groups: vec!["eng".into()],
        models: Vec::new(),
        owner: None,
    };
    store
        .upsert_environment(
            spec,
            sekisho_testkit::people::person(store, "admin@example.com", &["eng"]).await,
        )
        .await
        .unwrap()
        .spec
        .name
}

/// Waits up to `within` for `done`, polling every 20 ms.
async fn eventually(within: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < within,
            "{what} not seen within {within:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn revocation_reaches_other_replica_within_one_second() {
    let (store, _) = sekisho_testkit::store().await;
    let env = env(&store, "data-team").await;
    let (key, _) = store
        .create_key(
            &env,
            sekisho_testkit::people::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
        .await
        .unwrap();
    let a = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    a.spawn_refresh(store.clone());
    let b = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    b.spawn_refresh(store.clone());
    // Let both refresh tasks subscribe and do their first reload, so the
    // revocation below can only reach them by notification.
    sleep(Duration::from_millis(500)).await;
    store.revoke_key(&key.id).await.unwrap();
    let start = Instant::now();
    while !(a.key(&key.id).unwrap().revoked && b.key(&key.id).unwrap().revoked) {
        assert!(start.elapsed() < Duration::from_secs(1));
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn load_reflects_the_store() {
    let (store, _) = sekisho_testkit::store().await;
    let data = env(&store, "data-team").await;
    let expires = OffsetDateTime::now_utc() + time::Duration::days(30);
    let (ci, token) = store
        .create_key(
            &data,
            sekisho_testkit::people::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            Some(expires),
        )
        .await
        .unwrap();
    let (old, _) = store
        .create_key(
            &data,
            sekisho_testkit::people::person(&store, "bob@example.com", &["eng"]).await,
            "old",
            None,
        )
        .await
        .unwrap();
    store.set_suspended(&ci.id, true).await.unwrap();
    store.revoke_key(&old.id).await.unwrap();

    let cache = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    let (_, secret) = parse_token(&token).unwrap();
    assert_eq!(
        cache.key(&ci.id),
        Some(CachedKey {
            id: ci.id.clone(),
            environment: data.clone(),
            owner: ci.owner,
            secret_sha256: hash_secret(&secret),
            expires_at: ci.expires_at,
            suspended: true,
            revoked: false,
        })
    );
    assert!(cache.key(&old.id).unwrap().revoked); // revoked keys stay, so the hooks can say so
    assert_eq!(cache.key(&EnvKeyId::parse("aaaaaaaaaaaa").unwrap()), None);
    assert!(cache.environment_exists(&data));
    assert!(!cache.environment_exists(&EnvName::parse("ml-team").unwrap()));
}

#[tokio::test]
async fn changes_reach_the_cache() {
    let (store, _) = sekisho_testkit::store().await;
    let cache = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    cache.spawn_refresh(store.clone());
    let second = Duration::from_secs(1);

    let ml = env(&store, "ml-team").await;
    eventually(second, "new environment", || cache.environment_exists(&ml)).await;
    let (key, _) = store
        .create_key(
            &ml,
            sekisho_testkit::people::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
        .await
        .unwrap();
    eventually(second, "new key", || cache.key(&key.id).is_some()).await;
    store.set_suspended(&key.id, true).await.unwrap();
    eventually(second, "suspension", || {
        cache.key(&key.id).is_some_and(|k| k.suspended)
    })
    .await;
    store.set_suspended(&key.id, false).await.unwrap();
    eventually(second, "resumption", || {
        cache.key(&key.id).is_some_and(|k| !k.suspended)
    })
    .await;
    store.delete_environment(&ml).await.unwrap();
    eventually(second, "deletion", || {
        !cache.environment_exists(&ml) && cache.key(&key.id).is_none()
    })
    .await;
}

#[tokio::test]
async fn safety_net_catches_unannounced_changes() {
    let (store, _) = sekisho_testkit::store().await;
    let data = env(&store, "data-team").await;
    let (key, _) = store
        .create_key(
            &data,
            sekisho_testkit::people::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            None,
        )
        .await
        .unwrap();
    let cache = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    cache.spawn_refresh_every(store.clone(), Duration::from_millis(200));
    // A change written without a notification, as if the NOTIFY were lost.
    sqlx::query("update env_keys set suspended = true where id = $1")
        .bind(key.id.as_str())
        .execute(store.pool())
        .await
        .unwrap();
    eventually(Duration::from_secs(2), "unannounced suspension", || {
        cache.key(&key.id).is_some_and(|k| k.suspended)
    })
    .await;
}

#[tokio::test]
async fn refresh_stops_when_the_cache_is_dropped() {
    let (store, _) = sekisho_testkit::store().await;
    let cache = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    let task = cache.spawn_refresh_every(store.clone(), Duration::from_millis(100));
    let weak = Arc::downgrade(&cache);
    drop(cache);
    assert!(
        weak.upgrade().is_none(),
        "the task must not keep the cache alive"
    );
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the refresh task ends")
        .unwrap();
}

#[tokio::test]
async fn an_ended_session_reaches_other_replicas_within_one_second() {
    let (store, sb) = sekisho_testkit::store().await;
    let alice = sekisho_testkit::people::person(&store, "alice@example.com", &["eng"]).await;
    let (_, session) = sekisho_testkit::people::session_token(&store, &sb, ISSUER, alice).await;
    let a = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    a.spawn_refresh(store.clone());
    let b = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    b.spawn_refresh(store.clone());
    assert!(!a.session_ended(&session) && !b.session_ended(&session));
    // Let both refresh tasks subscribe before the change.
    sleep(Duration::from_millis(300)).await;
    store
        .end_session(&session, "logout", OffsetDateTime::now_utc())
        .await
        .unwrap();
    eventually(Duration::from_secs(1), "the ended session", || {
        a.session_ended(&session) && b.session_ended(&session)
    })
    .await;
}

#[tokio::test]
async fn ended_sessions_leave_the_cache_after_65_minutes() {
    let (store, sb) = sekisho_testkit::store().await;
    let alice = sekisho_testkit::people::person(&store, "alice@example.com", &["eng"]).await;
    let (_, session) = sekisho_testkit::people::session_token(&store, &sb, ISSUER, alice).await;
    let ended_ago = |minutes: i64| {
        let store = store.clone();
        async move {
            sqlx::query(
                "update sessions set end_reason = 'logout',                  ended_at = now() - make_interval(mins => $2) where id = $1",
            )
            .bind(session)
            .bind(i32::try_from(minutes).unwrap())
            .execute(store.pool())
            .await
            .unwrap();
        }
    };
    let cache = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    ended_ago(64).await;
    cache.reload(&store).await.unwrap();
    assert!(cache.session_ended(&session));
    // Any token of a session ended 65 minutes ago has expired anyway.
    ended_ago(66).await;
    cache.reload(&store).await.unwrap();
    assert!(!cache.session_ended(&session));
}

#[tokio::test]
async fn the_cache_holds_people_environments_and_published_keys() {
    let (store, sb) = sekisho_testkit::store().await;
    let data = env(&store, "data-team").await;
    let alice = sekisho_testkit::people::person(&store, "alice@example.com", &["eng"]).await;
    let boss = sekisho_testkit::people::admin(&store, "boss@example.com").await;
    sekisho_testkit::people::session_token(&store, &sb, ISSUER, alice).await;
    let cache = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    assert_eq!(cache.issuer(), ISSUER);
    let user = cache.user(&alice).expect("alice");
    assert!(user.access.groups.contains("eng") && !user.admin_mark);
    assert!(cache.user(&boss).unwrap().admin_mark);
    let environment = cache.environment(&data).expect("data-team");
    assert_eq!(environment.access_groups, ["eng"]);
    assert!(environment.owner.is_some());
    let published = store
        .published_keys(OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(
        cache.public_key(&published[0].kid),
        Some(published[0].clone())
    );
    assert_eq!(cache.public_key("0000000000000000"), None);
}
