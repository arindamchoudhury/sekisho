mod common;

use std::collections::HashSet;

#[tokio::test]
async fn versions_change_only_with_content() {
    let (store, _sb) = common::store().await;
    assert_eq!(store.gateway_config_after(0).await.unwrap(), None);
    let v1 = store.save_gateway_config("binds: []\n").await.unwrap();
    assert!(v1 > 0);
    assert_eq!(store.save_gateway_config("binds: []\n").await.unwrap(), v1); // same content
    let v2 = store
        .save_gateway_config("binds: [] # changed\n")
        .await
        .unwrap();
    assert!(v2 > v1);
    assert_eq!(
        store.save_gateway_config("binds: []\n").await.unwrap(),
        v2 + 1
    ); // back again is a change
    let latest = v2 + 1;

    assert_eq!(
        store.gateway_config_after(0).await.unwrap(),
        Some((latest, "binds: []\n".into()))
    );
    assert_eq!(
        store.gateway_config_after(v1).await.unwrap().unwrap().0,
        latest
    ); // newest, not next
    assert_eq!(store.gateway_config_after(latest).await.unwrap(), None);
    let sha: Vec<u8> = sqlx::query_scalar("select sha256 from gateway_config where version = $1")
        .bind(v1)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(sha.len(), 32);
}

#[tokio::test]
async fn concurrent_saves_of_one_config_make_one_version() {
    let (store, _sb) = common::store().await;
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let s = store.clone();
            tokio::spawn(async move { s.save_gateway_config("binds: [] # replicas\n").await })
        })
        .collect();
    let mut versions = HashSet::new();
    for t in tasks {
        versions.insert(t.await.unwrap().unwrap());
    }
    assert_eq!(versions.len(), 1);
    let count: i64 = sqlx::query_scalar("select count(*) from gateway_config")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}
