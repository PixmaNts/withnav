//! A progress doubt holds the next provider boundary, not a human interaction.
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
pub struct WatchdogPark {
    pub supervisor_cancel: CancellationToken,
    pub stopped: AtomicBool,
    doubt: AtomicBool,
    resumed: Notify,
}
impl WatchdogPark {
    pub fn is_doubt(&self) -> bool {
        self.doubt.load(Ordering::Acquire)
    }
    pub fn park(&self) -> bool {
        self.doubt
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub fn resume(&self) -> bool {
        if !self.doubt.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.resumed.notify_waiters();
        true
    }
    pub(super) async fn boundary(&self, cancel: &CancellationToken) {
        loop {
            let notified = self.resumed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_doubt() {
                break;
            }
            tokio::select! { biased; _ = cancel.cancelled() => break, _ = notified => {} }
        }
    }
}
