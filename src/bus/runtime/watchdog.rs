//! A progress doubt holds the next provider boundary, not a human interaction.
pub use crate::bus::event::PauseToken;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

#[derive(Debug, Clone)]
pub struct RootHold {
    pub token: PauseToken,
    pub evidence: serde_json::Value,
    pub published: bool,
    pub turn_id: Option<Ulid>,
}

#[derive(Debug, Default)]
pub struct WatchdogPark {
    pub natural_retained: Notify,
    state: Mutex<(u64, bool)>,
    terminal_owner: Mutex<TerminalState>,
    pub supervisor_cancel: CancellationToken,
    pub stopped: AtomicBool,
    doubt: AtomicBool,
    resumed: Notify,
    boundary_stop: AtomicBool,
    root: Mutex<Option<RootHold>>,
    incarnation: Mutex<Ulid>,
}

pub type NaturalOutcome = (crate::provider::AssistantMessage, super::CompletionOutcome);
#[derive(Debug, Default)]
struct TerminalState {
    owner: Option<TerminalOwner>,
    pending: Option<NaturalOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOwner {
    Natural,
    Watchdog,
}

impl WatchdogPark {
    pub fn install_incarnation(&self, incarnation: Ulid) {
        *self.incarnation.lock().unwrap_or_else(|e| e.into_inner()) = incarnation;
    }
    pub fn request_root_pause(&self, generation: u64, evidence: serde_json::Value) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 != generation || !state.1 || self.is_doubt() {
            return false;
        }
        *self.root.lock().unwrap_or_else(|e| e.into_inner()) = Some(RootHold {
            token: PauseToken {
                incarnation: *self.incarnation.lock().unwrap_or_else(|e| e.into_inner()),
                hold_id: Ulid::generate(),
            },
            evidence,
            published: false,
            turn_id: None,
        });
        self.doubt.store(true, Ordering::Release);
        true
    }
    pub fn root_hold(&self) -> Option<RootHold> {
        self.root.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn publish_root_hold(&self) -> Option<RootHold> {
        let mut root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        let hold = root.as_mut()?;
        if hold.published {
            return None;
        }
        hold.published = true;
        Some(hold.clone())
    }
    pub fn bind_root_turn(&self, turn_id: Ulid) {
        if let Some(hold) = self.root.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            hold.turn_id = Some(turn_id);
        }
    }
    pub fn discard_unpublished_root(&self) {
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        if root.as_ref().is_some_and(|hold| !hold.published) {
            root.take();
            self.doubt.store(false, Ordering::Release);
        }
    }
    pub fn resolve_root(&self, token: PauseToken, apply: impl FnOnce()) -> bool {
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        if !root
            .as_ref()
            .is_some_and(|hold| hold.token == token && hold.published)
        {
            return false;
        }
        apply();
        root.take();
        self.doubt.store(false, Ordering::Release);
        self.resumed.notify_waiters();
        true
    }
    pub fn take_root_on_exit(&self) -> Option<RootHold> {
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let hold = self.root.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.doubt.store(false, Ordering::Release);
        hold
    }
    /// Root stops settle the complete tool batch rather than cancelling a call.
    pub fn stop_at_boundary(&self, generation: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 != generation || !state.1 || self.is_doubt() {
            return false;
        }
        self.boundary_stop.store(true, Ordering::Release);
        self.resumed.notify_waiters();
        true
    }
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
        self.boundary_stop.store(false, Ordering::Release);
        if running && self.root_hold().is_none() {
            self.doubt.store(false, Ordering::Release);
        }
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
    pub fn rollback_stop(&self, doubt: bool) -> Option<NaturalOutcome> {
        let mut terminal = self
            .terminal_owner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = terminal.pending.take();
        terminal.owner = pending.as_ref().map(|_| TerminalOwner::Natural);
        self.doubt.store(doubt, Ordering::Release);
        self.stopped.store(false, Ordering::Release);
        self.resumed.notify_waiters();
        pending
    }
    pub fn natural_outcome(&self, outcome: NaturalOutcome) -> Option<NaturalOutcome> {
        let mut terminal = self
            .terminal_owner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match terminal.owner {
            Some(TerminalOwner::Watchdog) => {
                terminal.pending = Some(outcome);
                self.natural_retained.notify_one();
                None
            }
            Some(TerminalOwner::Natural) => None,
            None => {
                terminal.owner = Some(TerminalOwner::Natural);
                Some(outcome)
            }
        }
    }
    pub fn claim_terminal(&self, owner: TerminalOwner) -> bool {
        let mut claimed = self
            .terminal_owner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if claimed.owner.is_some() {
            return false;
        }
        claimed.owner = Some(owner);
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
        if self.stopped.load(Ordering::Acquire) || self.root_hold().is_some() {
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
    pub(super) async fn boundary(&self, cancel: &CancellationToken) -> bool {
        loop {
            let notified = self.resumed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.root_hold().is_some() {
                return false;
            }
            if self.boundary_stop.swap(false, Ordering::AcqRel) {
                return false;
            }
            if !self.is_doubt() && !self.stopped.load(Ordering::Acquire) {
                break;
            }
            tokio::select! { biased; _ = cancel.cancelled() => break, _ = notified => {} }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_tokens_are_consumed_once_and_cannot_release_a_new_hold() {
        let park = WatchdogPark::default();
        park.transition(true);
        assert!(park.request_root_pause(park.generation().unwrap(), serde_json::json!({})));
        let first = park.publish_root_hold().unwrap().token;
        assert!(!park.resolve_root(
            PauseToken {
                incarnation: Ulid::generate(),
                ..first
            },
            || {}
        ));
        assert!(park.resolve_root(first, || {}));
        assert!(park.request_root_pause(park.generation().unwrap(), serde_json::json!({})));
        let second = park.publish_root_hold().unwrap().token;
        assert_ne!(first, second);
        assert!(!park.resolve_root(first, || {}));
        assert!(park.is_doubt());
        assert!(park.resolve_root(second, || {}));
        assert!(!park.resolve_root(second, || {}));
    }
    #[tokio::test]
    async fn root_stop_is_consumed_at_a_safe_boundary_and_does_not_cross_turns() {
        let park = WatchdogPark::default();
        park.transition(true);
        let generation = park.generation().unwrap();
        assert!(park.stop_at_boundary(generation));
        assert!(!park.boundary(&CancellationToken::new()).await);
        assert!(park.boundary(&CancellationToken::new()).await);
        assert!(park.stop_at_boundary(generation));
        park.transition(false);
        park.transition(true);
        assert!(!park.stop_at_boundary(generation));
        assert!(park.boundary(&CancellationToken::new()).await);
    }
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
        park.transition(false);
        park.transition(true);
        assert!(
            !park.is_doubt(),
            "a replacement turn cannot inherit an old hold"
        );
    }
}
