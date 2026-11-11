//! The harness for tests that check a locked `withnav.db` does not stall a current-thread runtime:
//! a ticker task counts 5 ms sleeps while another thread holds the database locked, and the test
//! compares the count at the end of the hold with a baseline it reads right before the operation.
//! Reading the baseline any earlier (when the lock is taken) would also count the free runtime
//! before the operation starts, so an operation that then blocks the runtime could still pass.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How long the holder keeps the database locked.
const HOLD: Duration = Duration::from_millis(600);

/// A free runtime ticks about 120 times in the hold on Linux, but only about 38 on Windows, where
/// the default timer resolution (about 15.6 ms) stretches each 5 ms sleep. A blocked runtime shows
/// almost none, so 10 tells them apart on both.
const FREE_RUNTIME_TICKS: usize = 10;

/// Counts the 5 ms sleeps a ticker task completes on the current runtime.
pub(crate) struct StallProbe {
    ticks: Arc<AtomicUsize>,
    ticker: tokio::task::JoinHandle<()>,
}

impl StallProbe {
    /// Starts the ticker on the current runtime.
    pub(crate) fn start() -> Self {
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticker = tokio::spawn({
            let ticks = ticks.clone();
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        Self { ticks, ticker }
    }

    /// The ticks so far.
    fn now(&self) -> usize {
        self.ticks.load(Ordering::SeqCst)
    }

    /// Opens the database at `path` on its own thread, locks it with `lock`, and returns once it
    /// is locked. The hold starts at [`Hold::start`]: the thread then keeps the lock for [`HOLD`],
    /// reads the ticks, and commits.
    pub(crate) fn hold(
        &self,
        path: PathBuf,
        lock: impl FnOnce(&rusqlite::Connection) + Send + 'static,
    ) -> Hold {
        let (locked, is_locked) = std::sync::mpsc::channel();
        let (start, started) = std::sync::mpsc::channel::<()>();
        let ticks = self.ticks.clone();
        let holder = std::thread::spawn(move || {
            let db = rusqlite::Connection::open(path).unwrap();
            lock(&db);
            locked.send(()).unwrap();
            // A test that ends before starting the hold drops the sender: release at once.
            if started.recv().is_ok() {
                std::thread::sleep(HOLD);
            }
            let released = ticks.load(Ordering::SeqCst);
            db.execute_batch("COMMIT").unwrap();
            released
        });
        is_locked.recv().unwrap();
        Hold {
            ticks: self.ticks.clone(),
            start,
            holder,
        }
    }
}

impl Drop for StallProbe {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}

/// A database held locked by [`StallProbe::hold`].
pub(crate) struct Hold {
    ticks: Arc<AtomicUsize>,
    start: std::sync::mpsc::Sender<()>,
    holder: std::thread::JoinHandle<usize>,
}

impl Hold {
    /// Starts the hold and returns the baseline: call it right before the operation under test,
    /// so the free runtime before the operation is neither counted nor taken out of the hold.
    pub(crate) fn start(&self) -> usize {
        let baseline = self.ticks.load(Ordering::SeqCst);
        self.start.send(()).unwrap();
        baseline
    }

    /// Waits for the hold to end and asserts the runtime kept ticking from `baseline` until the
    /// release. The join runs on a blocking thread, so waiting never stalls the ticker itself.
    pub(crate) async fn assert_not_stalled(self, baseline: usize) {
        let holder = self.holder;
        let released = tokio::task::spawn_blocking(move || holder.join().unwrap())
            .await
            .unwrap();
        let during = released - baseline;
        assert!(
            during >= FREE_RUNTIME_TICKS,
            "the runtime stalled: {during} ticks"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The harness must catch an operation that blocks the runtime thread for the hold, even when
    /// the runtime ran freely between taking the lock and starting the operation: those free ticks
    /// come before the baseline and do not count.
    #[test]
    #[should_panic(expected = "the runtime stalled")]
    fn an_operation_that_blocks_the_runtime_is_reported_as_a_stall() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("withnav.db");
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch("CREATE TABLE t (x INTEGER)")
                .unwrap();
            let probe = StallProbe::start();
            let hold = probe.hold(path.clone(), |db| {
                db.execute_batch("BEGIN IMMEDIATE").unwrap();
            });
            // Enough free ticks to pass if the baseline were read when the lock was taken.
            let locked = probe.now();
            while probe.now() - locked < FREE_RUNTIME_TICKS {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let before = hold.start();
            // The operation waits out the lock on the runtime thread.
            let db = rusqlite::Connection::open(&path).unwrap();
            db.busy_timeout(Duration::from_secs(5)).unwrap();
            db.execute_batch("BEGIN IMMEDIATE; COMMIT").unwrap();
            hold.assert_not_stalled(before).await;
        });
    }
}
