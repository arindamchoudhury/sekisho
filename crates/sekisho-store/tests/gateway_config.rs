mod common;

use std::collections::HashSet;

use sekisho_core::secret_box::SecretBox;
use sekisho_store::StoreError;

#[tokio::test]
async fn versions_change_only_with_content() {
    let (store, sb) = common::store().await;
    assert_eq!(store.gateway_config_after(&sb, 0).await.unwrap(), None);
    let v1 = store.save_gateway_config(&sb, "binds: []\n").await.unwrap();
    assert!(v1 > 0);
    assert_eq!(
        store.save_gateway_config(&sb, "binds: []\n").await.unwrap(),
        v1
    ); // same content
    let v2 = store
        .save_gateway_config(&sb, "binds: [] # changed\n")
        .await
        .unwrap();
    assert!(v2 > v1);
    assert_eq!(
        store.save_gateway_config(&sb, "binds: []\n").await.unwrap(),
        v2 + 1
    ); // back again is a change
    let latest = v2 + 1;

    assert_eq!(
        store.gateway_config_after(&sb, 0).await.unwrap(),
        Some((latest, "binds: []\n".into()))
    );
    assert_eq!(
        store
            .gateway_config_after(&sb, v1)
            .await
            .unwrap()
            .unwrap()
            .0,
        latest
    ); // newest, not next
    assert_eq!(store.gateway_config_after(&sb, latest).await.unwrap(), None);
    // An agent ahead of the database (re-created since) gets the newest too.
    assert_eq!(
        store.gateway_config_after(&sb, latest + 5).await.unwrap(),
        Some((latest, "binds: []\n".into()))
    );
}

/// Rendered configs carry decrypted provider keys, so they are sealed like
/// the keys themselves.
#[tokio::test]
async fn configs_are_sealed_at_rest() {
    let (store, sb) = common::store().await;
    let yaml = "binds: []\n# backendAuth: sk-provider-secret-123\n";
    let version = store.save_gateway_config(&sb, yaml).await.unwrap();
    let rows: Vec<String> = sqlx::query_scalar("select row_to_json(g)::text from gateway_config g")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        !rows[0].contains("sk-provider-secret-123") && !rows[0].contains("binds"),
        "{}",
        rows[0]
    );
    assert_eq!(
        store.gateway_config_after(&sb, 0).await.unwrap(),
        Some((version, yaml.into()))
    );

    // Another encryption key can't read it.
    let other = SecretBox::from_base64_key("CAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAg=").unwrap();
    assert!(matches!(
        store.gateway_config_after(&other, 0).await,
        Err(StoreError::Secret(_))
    ));
}

#[tokio::test]
async fn only_recent_versions_are_kept() {
    let (store, sb) = common::store().await;
    let mut latest = 0;
    for n in 0..25 {
        latest = store
            .save_gateway_config(&sb, &format!("binds: [] # {n}\n"))
            .await
            .unwrap();
    }
    let versions: Vec<i64> =
        sqlx::query_scalar("select version from gateway_config order by version")
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert_eq!(versions.len(), 10);
    assert_eq!(*versions.last().unwrap(), latest);
    assert_eq!(
        store.gateway_config_after(&sb, 0).await.unwrap(),
        Some((latest, "binds: [] # 24\n".into()))
    );
}

#[tokio::test]
async fn concurrent_saves_of_one_config_make_one_version() {
    let (store, sb) = common::store().await;
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let s = store.clone();
            let sb = sb.clone();
            tokio::spawn(async move { s.save_gateway_config(&sb, "binds: [] # replicas\n").await })
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
