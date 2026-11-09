use std::future::Future;
use std::time::Duration;

/// Upper bound for every wait for something that must happen. Generous on
/// purpose: a loaded CI machine stretches the time, the bound only exists so a
/// real hang fails with a message instead of stalling the suite.
pub const WAIT: Duration = Duration::from_secs(10);

pub trait Deadline: Future + Sized {
    async fn bounded(self, file: &str, line: u32) -> Self::Output {
        tokio::time::timeout(WAIT, self)
            .await
            .unwrap_or_else(|_| panic!("await timed out at {file}:{line}"))
    }
}

impl<F: Future> Deadline for F {}

/// Re-runs `probe` until it returns `Some`, yielding to the runtime between
/// attempts, and panics with `what` once [`WAIT`] has passed. The deadline, not
/// an attempt count, decides when to give up, so slow machines get the same
/// number of seconds as fast ones.
pub async fn eventually<T, Fut>(what: &str, mut probe: impl FnMut() -> Fut) -> T
where
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {WAIT:?} waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
