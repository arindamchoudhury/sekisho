//! Test support for Sekisho's crates.
//!
//! [`store`] gives a migrated `Store` on a throwaway PostgreSQL 18 container.
//! It needs Docker. [`people`] signs people in to it, and [`FakeDex`] stands
//! in for Dex. [`git_http`], [`registry`] and [`agents`] stand in for the Git
//! hosts, container registries and A2A agents catalogs reach.

pub mod agents;
pub mod eventstream;
pub mod fake_dex;
pub mod git_http;
pub mod github;
pub mod leftovers;
pub mod people;
pub mod registry;

pub use fake_dex::FakeDex;

use std::ops::Deref;
use std::time::Duration;

use sekisho_core::secret_box::SecretBox;
use sekisho_store::{Store, StoreOptions};
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

/// How many fresh containers [`retry_unreachable`] starts before it gives up.
pub const CONTAINER_TRIES: usize = 3;

/// How long a new container's published port has to answer. A healthy one
/// answers within a second of its server being ready.
pub const REACHABLE_WITHIN: Duration = Duration::from_secs(10);

/// Calls `attempt` until it succeeds, at most `tries` times, and panics with
/// every reason if none does. Each attempt starts its own containers and
/// checks that they answer on their published ports; returning `Err` drops
/// them, which removes them.
///
/// A container can be up while its published port isn't: Docker Desktop on
/// WSL 2 now and then publishes a port that refuses every connection (about
/// one container in 200), and Docker can report no port at all. A fresh
/// container gets a fresh port.
pub async fn retry_unreachable<T, F, Fut>(what: &str, tries: usize, mut attempt: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut reasons = Vec::new();
    for n in 1..=tries {
        match attempt().await {
            Ok(value) => return value,
            Err(reason) => {
                eprintln!("{what}, try {n} of {tries}: {reason}; starting another");
                reasons.push(reason);
            }
        }
    }
    panic!(
        "{what} unreachable in {tries} tries: {}",
        reasons.join("; ")
    );
}

/// Starts `postgres:18`, migrates it, and returns the store with a test
/// `SecretBox` keyed by [`TEST_ENCRYPTION_KEY`]. A container whose port
/// doesn't answer is replaced ([`retry_unreachable`]).
pub async fn store() -> (TestStore, SecretBox) {
    leftovers::remove_leftovers_once().await;
    let (store, postgres) = retry_unreachable("postgres:18", CONTAINER_TRIES, || async {
        let (label, started) = leftovers::started_label();
        // The entrypoint's temporary initdb server logs its ready line to
        // stdout; only the real server logs it to stderr.
        let postgres = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_label(label, started)
            .with_env_var("POSTGRES_PASSWORD", "sekisho")
            .with_cmd(["postgres", "-c", "fsync=off"])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await
            .expect("start postgres:18 (is Docker running?)");
        let port = postgres
            .get_host_port_ipv4(5432)
            .await
            .map_err(|e| format!("no published port: {e}"))?;
        // 127.0.0.1, not localhost: on Windows, localhost tries ::1 first.
        let url = format!("postgres://postgres:sekisho@127.0.0.1:{port}/postgres");
        match tokio::time::timeout(
            REACHABLE_WITHIN,
            Store::connect(&url, StoreOptions::default()),
        )
        .await
        {
            Ok(Ok(store)) => Ok((store, postgres)),
            Ok(Err(e)) => Err(format!("127.0.0.1:{port}: {e}")),
            Err(_) => Err(format!(
                "127.0.0.1:{port} didn't answer within {REACHABLE_WITHIN:?}"
            )),
        }
    })
    .await;
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn a_reachable_first_try_starts_once() {
        let calls = AtomicUsize::new(0);
        let got = retry_unreachable("thing", 3, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(7)
        })
        .await;
        assert_eq!(got, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unreachable_try_is_started_again() {
        let calls = AtomicUsize::new(0);
        let got = retry_unreachable("thing", 3, || async {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 | 1 => Err("port refused".to_owned()),
                n => Ok(n),
            }
        })
        .await;
        assert_eq!(got, 2, "the third try's value");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    #[should_panic(expected = "thing unreachable in 3 tries: one; two; three")]
    async fn it_gives_up_after_its_tries_with_every_reason() {
        let calls = AtomicUsize::new(0);
        retry_unreachable("thing", 3, || async {
            Err::<(), _>(["one", "two", "three"][calls.fetch_add(1, Ordering::SeqCst)].to_owned())
        })
        .await;
    }
}
