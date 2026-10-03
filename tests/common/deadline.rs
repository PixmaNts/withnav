use std::future::Future;
use std::time::Duration;

pub trait Deadline: Future + Sized {
    async fn bounded(self, file: &str, line: u32) -> Self::Output {
        tokio::time::timeout(Duration::from_secs(10), self)
            .await
            .unwrap_or_else(|_| panic!("await timed out at {file}:{line}"))
    }
}

impl<F: Future> Deadline for F {}
