//! Test support: a migrated `Store` on a throwaway PostgreSQL 18 container.

#![allow(dead_code)] // each test file uses a different part of this module

use std::ops::Deref;
use std::time::Duration;

use sekisho_core::secret_box::SecretBox;
use sekisho_store::Store;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// base64 of 32 bytes of 7.
pub const TEST_ENCRYPTION_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";

/// A `Store` that owns its database container, which is removed when the
/// test ends. Derefs to `Store`, so `store.clone()` gives a plain `Store`.
pub struct TestStore {
    store: Store,
    _postgres: ContainerAsync<GenericImage>,
}

impl Deref for TestStore {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.store
    }
}

pub async fn store() -> (TestStore, SecretBox) {
    // The entrypoint's temporary initdb server logs its ready line to stdout;
    // only the real server logs it to stderr.
    let postgres = GenericImage::new("postgres", "18")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "sekisho")
        .with_cmd(["postgres", "-c", "fsync=off"])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .await
        .expect("start postgres:18 (is Docker running?)");
    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    // 127.0.0.1, not localhost: on Windows, localhost tries ::1 first.
    let url = format!("postgres://postgres:sekisho@127.0.0.1:{port}/postgres");
    let store = Store::connect(&url)
        .await
        .expect("connect to test postgres");
    store.migrate().await.expect("migrate");
    let secret_box = SecretBox::from_base64_key(TEST_ENCRYPTION_KEY).unwrap();
    (
        TestStore {
            store,
            _postgres: postgres,
        },
        secret_box,
    )
}
