use std::future::Future;
use std::time::Duration;

/// How long any single wait in an integration test may take before it fails.
/// Generous enough for a loaded CI runner; a missed wake fails instead of hanging.
pub const DEADLINE: Duration = Duration::from_secs(10);

pub trait Deadline: Future + Sized {
    async fn bounded(self, file: &str, line: u32) -> Self::Output {
        tokio::time::timeout(DEADLINE, self)
            .await
            .unwrap_or_else(|_| panic!("await timed out at {file}:{line}"))
    }
}

impl<F: Future> Deadline for F {}
