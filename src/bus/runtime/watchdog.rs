//! A progress doubt holds the next provider boundary, not a human interaction.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
pub struct WatchdogPark {
    state: Mutex<(u64, bool)>,
    terminal_owner: Mutex<Option<TerminalOwner>>,
    pub supervisor_cancel: CancellationToken,
    pub stopped: AtomicBool,
    doubt: AtomicBool,
    resumed: Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOwner {
    Natural,
    Watchdog,
}

impl WatchdogPark {
    pub fn claim_stuck(&self, generation: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 != generation || !state.1 || self.is_doubt() {
            return false;
        }
        self.claim_terminal(TerminalOwner::Watchdog)
    }
    pub fn transition(&self, running: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 = state.0.wrapping_add(1);
        state.1 = running;
    }
    pub fn generation(&self) -> Option<u64> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.1 && !self.is_doubt() && !self.stopped.load(Ordering::Acquire)).then_some(state.0)
    }
    pub fn accept_verdict(&self, generation: u64, doubt: bool) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 != generation
            || !state.1
            || self.is_doubt()
            || self.stopped.load(Ordering::Acquire)
        {
            return false;
        }
        if doubt {
            self.doubt.store(true, Ordering::Release);
        }
        true
    }
    pub fn rollback_stop(&self, doubt: bool) {
        *self
            .terminal_owner
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.doubt.store(doubt, Ordering::Release);
        self.stopped.store(false, Ordering::Release);
        self.resumed.notify_waiters();
    }
    pub fn claim_terminal(&self, owner: TerminalOwner) -> bool {
        let mut claimed = self
            .terminal_owner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if claimed.is_some() {
            return false;
        }
        *claimed = Some(owner);
        if owner == TerminalOwner::Watchdog {
            self.stopped.store(true, Ordering::Release);
        }
        true
    }
    pub fn is_doubt(&self) -> bool {
        self.doubt.load(Ordering::Acquire)
    }
    pub fn park(&self) -> bool {
        self.doubt
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub fn resume(&self) -> bool {
        self.resume_with(|| {})
    }
    pub fn resume_with(&self, apply: impl FnOnce()) -> bool {
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        if !self.doubt.swap(false, Ordering::AcqRel) {
            return false;
        }
        apply();
        self.resumed.notify_waiters();
        true
    }
    pub fn claim_stop(&self) -> bool {
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.is_doubt() || self.stopped.swap(true, Ordering::AcqRel) {
            return false;
        }
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

    #[test]
    fn verdict_generation_does_not_cross_a_park_or_replacement_turn() {
        let park = WatchdogPark::default();
        park.transition(true);
        let old = park.generation().unwrap();
        park.transition(false);
        assert!(!park.accept_verdict(old, true));
        park.transition(true);
        assert!(!park.accept_verdict(old, true));
        assert!(park.accept_verdict(park.generation().unwrap(), true));
    }
}
