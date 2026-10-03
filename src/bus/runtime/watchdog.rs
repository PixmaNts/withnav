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
    pub fn claim_stop(&self) -> bool {
        if self
            .doubt
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.stopped.store(true, Ordering::Release);
        true
    }
    pub(super) async fn boundary(&self, cancel: &CancellationToken) {
        loop {
            let notified = self.resumed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_doubt() && !self.stopped.load(Ordering::Acquire) {
                break;
            }
            tokio::select! { biased; _ = cancel.cancelled() => break, _ = notified => {} }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_turn_unwinds_a_doubt_boundary_without_resuming_it() {
        let park = WatchdogPark::default();
        assert!(park.park());
        let cancel = CancellationToken::new();
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), park.boundary(&cancel))
            .await
            .unwrap();
        assert!(park.is_doubt());
        assert!(park.resume());
        assert!(!park.resume());
    }
}
