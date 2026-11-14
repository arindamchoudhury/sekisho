//! How Sekisho connects to PostgreSQL.

mod common;

use sekisho_store::StoreOptions;

async fn show(store: &sekisho_store::Store, setting: &str) -> String {
    sqlx::query_scalar("select current_setting($1)")
        .bind(setting)
        .fetch_one(store.pool())
        .await
        .unwrap()
}

/// A transaction left idle, or a lock waited on, too long ends in an error
/// instead of holding a connection: a backstop for a wait that passes
/// through Sekisho, which PostgreSQL can't see as a deadlock.
#[tokio::test]
async fn connections_bound_idle_transactions_and_lock_waits() {
    let (store, _) = common::store().await;
    assert_eq!(
        show(&store, "idle_in_transaction_session_timeout").await,
        "1min"
    );
    assert_eq!(show(&store, "lock_timeout").await, "30s");
    assert_eq!(
        store.pool().options().get_max_connections(),
        StoreOptions::default().max_connections
    );
    assert_eq!(StoreOptions::default().max_connections, 20);
}
