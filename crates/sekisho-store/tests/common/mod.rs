//! The shared test store lives in `sekisho-testkit`.

#![allow(unused_imports)] // each test file uses a different part

pub use sekisho_testkit::people::person;
pub use sekisho_testkit::{TEST_ENCRYPTION_KEY, TestStore, store};

/// Who saves environments in these tests. `eng` is the access group the
/// tests' environments have.
#[allow(dead_code)]
pub async fn admin(store: &sekisho_store::Store) -> uuid::Uuid {
    person(store, "admin@example.com", &["eng"]).await
}
