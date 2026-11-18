//! What `sekisho serve` waits for when it stops (D189).
//!
//! agentgateway relays each answer through the stream guard
//! (`fullDuplexStreamed`), so a stream the guard ends is an answer cut
//! short. On shutdown, running streams get a grace to finish; any still
//! running then are ended, as a revocation ends one, and new ones are
//! refused. Ending a stream finishes its tally, so the rate-limit service
//! charges its estimate; the drain then waits, for a bounded time, for the
//! charges still being recorded, which would otherwise be lost with the
//! process.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// How long ended streams' guards get to return, so their tallies finish.
const ENDING: Duration = Duration::from_secs(1);

/// The stream guard's running streams and the charger's charges, shared
/// by both and by `sekisho serve`.
#[derive(Clone, Default)]
pub struct Drain {
    streams: Running,
    charges: Running,
    /// Set once running streams are to be ended.
    end_streams: Arc<watch::Sender<bool>>,
}

/// What a drain left behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drained {
    /// Streams still running when the grace ran out, which it ended.
    pub streams_ended: usize,
    /// Charges still running when the wait for them ran out.
    pub charges_left: usize,
}

impl Drain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits up to `grace` for running streams to end, then ends the rest
    /// (their clients' answers stop there) and refuses new ones; then
    /// waits up to `charges` for the spend still being recorded.
    pub async fn run(&self, grace: Duration, charges: Duration) -> Drained {
        let mut drained = Drained::default();
        if tokio::time::timeout(grace, self.streams.idle())
            .await
            .is_err()
        {
            drained.streams_ended = self.streams.count();
            self.end_streams.send_replace(true);
            // Each guard answers and returns at once.
            let _ = tokio::time::timeout(ENDING, self.streams.idle()).await;
        } else {
            // None left to end; refuse any that arrive from here on.
            self.end_streams.send_replace(true);
        }
        if tokio::time::timeout(charges, self.charges.idle())
            .await
            .is_err()
        {
            drained.charges_left = self.charges.count();
        }
        drained
    }

    /// Runs a stream's guard, counted until it returns.
    pub(crate) fn spawn_stream(&self, guard: impl Future<Output = ()> + Send + 'static) {
        self.streams.spawn(guard);
    }

    /// Runs a charge, counted until it returns.
    pub(crate) fn spawn_charge(&self, charge: impl Future<Output = ()> + Send + 'static) {
        self.charges.spawn(charge);
    }

    /// Resolves once running streams are to be ended (at once if they
    /// already are).
    pub(crate) fn streams_ending(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut ending = self.end_streams.subscribe();
        async move {
            let _ = ending.wait_for(|end| *end).await;
        }
    }
}

/// A count of running tasks.
#[derive(Clone, Default)]
struct Running(Arc<watch::Sender<usize>>);

impl Running {
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        // Counted before the task starts, so a drain never misses it.
        self.0.send_modify(|n| *n += 1);
        let done = Done(Arc::clone(&self.0));
        tokio::spawn(async move {
            let _done = done; // counted out however the task ends
            task.await;
        });
    }

    fn count(&self) -> usize {
        *self.0.borrow()
    }

    async fn idle(&self) {
        let _ = self.0.subscribe().wait_for(|n| *n == 0).await;
    }
}

struct Done(Arc<watch::Sender<usize>>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_idle_drain_returns_at_once() {
        let drain = Drain::new();
        let drained = tokio::time::timeout(
            Duration::from_millis(100),
            drain.run(Duration::from_secs(30), Duration::from_secs(30)),
        )
        .await
        .expect("nothing to wait for");
        assert_eq!(drained, Drained::default());
    }

    #[tokio::test]
    async fn streams_are_told_to_end_only_after_the_grace() {
        let drain = Drain::new();
        let ending = drain.streams_ending();
        drain.spawn_stream(ending);
        let run = tokio::spawn({
            let drain = drain.clone();
            async move { drain.run(Duration::from_millis(200), Duration::ZERO).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!run.is_finished(), "the grace isn't over");
        assert_eq!(
            run.await.unwrap(),
            Drained {
                streams_ended: 1,
                charges_left: 0
            }
        );
    }

    #[tokio::test]
    async fn the_drain_waits_for_charges() {
        let drain = Drain::new();
        drain.spawn_charge(tokio::time::sleep(Duration::from_millis(200)));
        let started = tokio::time::Instant::now();
        let drained = drain.run(Duration::ZERO, Duration::from_secs(5)).await;
        assert_eq!(drained.charges_left, 0);
        assert!(started.elapsed() >= Duration::from_millis(200));
        drain.spawn_charge(std::future::pending());
        let drained = drain.run(Duration::ZERO, Duration::from_millis(50)).await;
        assert_eq!(drained.charges_left, 1);
    }
}
