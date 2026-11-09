use std::sync::Arc;
use std::time::Duration;

use sekisho_core::keys::{EnvKeyId, hash_secret, parse_token};
use sekisho_core::names::EnvName;
use sekisho_hooks::cache::{AuthCache, CachedKey};
use sekisho_store::{EnvironmentSpec, Store};
use time::OffsetDateTime;
use tokio::time::{Instant, sleep};

async fn env(store: &Store, name: &str) -> EnvName {
    let spec = EnvironmentSpec {
        name: EnvName::parse(name).unwrap(),
        description: String::new(),
        notice: String::new(),
        access_groups: Vec::new(),
        models: Vec::new(),
    };
    store.upsert_environment(spec).await.unwrap().spec.name
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
        .create_key(&env, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let a = AuthCache::load(&store).await.unwrap();
    a.spawn_refresh(store.clone());
    let b = AuthCache::load(&store).await.unwrap();
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
        .create_key(&data, "alice@example.com", "ci", Some(expires))
        .await
        .unwrap();
    let (old, _) = store
        .create_key(&data, "bob@example.com", "old", None)
        .await
        .unwrap();
    store.set_suspended(&ci.id, true).await.unwrap();
    store.revoke_key(&old.id).await.unwrap();

    let cache = AuthCache::load(&store).await.unwrap();
    let (_, secret) = parse_token(&token).unwrap();
    assert_eq!(
        cache.key(&ci.id),
        Some(CachedKey {
            id: ci.id.clone(),
            environment: data.clone(),
            owner: "alice@example.com".into(),
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
    let cache = AuthCache::load(&store).await.unwrap();
    cache.spawn_refresh(store.clone());
    let second = Duration::from_secs(1);

    let ml = env(&store, "ml-team").await;
    eventually(second, "new environment", || cache.environment_exists(&ml)).await;
    let (key, _) = store
        .create_key(&ml, "alice@example.com", "ci", None)
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
        .create_key(&data, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let cache = AuthCache::load(&store).await.unwrap();
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
    let cache = AuthCache::load(&store).await.unwrap();
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
