//! Test containers a killed test run left behind.
//!
//! testcontainers removes a container when its handle is dropped, so a test
//! process that is stopped (Ctrl+C, `SIGTERM`, `SIGKILL`, a WSL restart)
//! leaves every container it had running. Each container the testkit starts
//! carries [`STARTED_LABEL`], the Unix time it was started, and the first
//! container a test process starts removes the labelled ones older than
//! [`STALE_AFTER`].
//!
//! testcontainers 0.28's `watchdog` feature is meant to remove them on
//! `SIGINT` and `SIGTERM`, but on Linux it caught both signals and did
//! nothing: the run went on to the end and removed nothing. So it stays off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;

/// The label on every test container: the Unix time, in seconds, at which
/// it was started.
pub const STARTED_LABEL: &str = "sekisho.test.started";

/// A test container started this long ago is a leftover: no test run takes
/// this long.
pub const STALE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);

/// The label to put on a test container started now.
pub fn started_label() -> (&'static str, String) {
    (STARTED_LABEL, now().to_string())
}

/// Removes leftover test containers, once per test process; later calls
/// return at once. Never fails: without a `docker` command it does nothing.
pub async fn remove_leftovers_once() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if !DONE.swap(true, Ordering::SeqCst) {
        remove_leftovers().await;
    }
}

/// Removes the labelled containers started more than [`STALE_AFTER`] ago,
/// with their anonymous volumes, and returns their IDs. Never fails: a
/// `docker` command that is missing or refuses removes nothing.
pub async fn remove_leftovers() -> Vec<String> {
    let listing = Command::new("docker")
        .args(["ps", "--all", "--no-trunc", "--filter"])
        .arg(format!("label={STARTED_LABEL}"))
        .arg("--format")
        .arg(format!("{{{{.ID}}}} {{{{.Label \"{STARTED_LABEL}\"}}}}"))
        .output()
        .await;
    let Ok(listing) = listing else {
        return Vec::new();
    };
    if !listing.status.success() {
        return Vec::new();
    }
    let stale = stale_ids(&String::from_utf8_lossy(&listing.stdout), now());
    if stale.is_empty() {
        return stale;
    }
    let removed = Command::new("docker")
        .args(["rm", "--force", "--volumes"])
        .args(&stale)
        .output()
        .await;
    match removed {
        Ok(out) if out.status.success() => {
            eprintln!(
                "removed {} leftover test container(s) started over {:?} ago",
                stale.len(),
                STALE_AFTER
            );
            stale
        }
        _ => Vec::new(),
    }
}

/// The IDs in `docker ps` lines of `<id> <started>` whose start is more
/// than [`STALE_AFTER`] before `now`. Lines that don't parse are skipped.
fn stale_ids(listing: &str, now: u64) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let (id, started) = line.trim().split_once(' ')?;
            let started: u64 = started.trim().parse().ok()?;
            (now.saturating_sub(started) > STALE_AFTER.as_secs()).then(|| id.to_owned())
        })
        .collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn only_containers_older_than_two_hours_are_stale() {
        let old = NOW - STALE_AFTER.as_secs() - 1;
        let edge = NOW - STALE_AFTER.as_secs();
        let listing = format!("aaa {old}\nbbb {edge}\nccc {NOW}\n");
        assert_eq!(stale_ids(&listing, NOW), ["aaa"]);
    }

    #[test]
    fn lines_that_dont_parse_are_skipped() {
        let listing = "aaa\nbbb \nccc soon\nddd 12\n\n";
        assert_eq!(stale_ids(listing, NOW), ["ddd"]);
    }

    #[test]
    fn a_start_in_the_future_is_not_stale() {
        assert!(stale_ids(&format!("aaa {}", NOW + 60), NOW).is_empty());
    }

    async fn docker(args: &[&str]) -> String {
        let out = Command::new("docker").args(args).output().await.unwrap();
        assert!(out.status.success(), "docker {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Needs Docker, like [`crate::store`]. `docker create` makes a container
    /// without starting it, so the test needs no time to start one.
    #[tokio::test]
    async fn a_leftover_is_removed_and_a_new_container_kept() {
        let old = (now() - STALE_AFTER.as_secs() - 60).to_string();
        let (_, new) = started_label();
        let old_label = format!("{STARTED_LABEL}={old}");
        let new_label = format!("{STARTED_LABEL}={new}");
        let old_id = docker(&["create", "--label", &old_label, "postgres:18"]).await;
        let new_id = docker(&["create", "--label", &new_label, "postgres:18"]).await;

        let removed = remove_leftovers().await;

        let left = |id: String| async move {
            docker(&["ps", "--all", "--quiet", "--no-trunc", "--filter", &format!("id={id}")]).await
        };
        let new_left = left(new_id.clone()).await;
        docker(&["rm", "--force", "--volumes", &new_id]).await;
        assert!(removed.contains(&old_id), "{removed:?} lacks {old_id}");
        assert!(!removed.contains(&new_id));
        assert!(left(old_id).await.is_empty());
        assert_eq!(new_left, new_id);
    }
}
