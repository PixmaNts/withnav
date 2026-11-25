//! Deterministic test seams for [`Service`], compiled only when the
//! `test-support` feature (or a test build of this crate) enables them.
//!
//! Production code calls the same registry methods in every configuration:
//! with the feature off, each seam compiles to a no-op that immediately
//! returns the neutral outcome, so a release build carries no hook and no
//! seam state. Integration tests arm the seams through the public `Service`
//! methods (kept `#[doc(hidden)]`); the registry itself stays crate-private.

#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, Mutex};

#[cfg(any(test, feature = "test-support"))]
use super::subagents::root_watchdog::TestRootWatchdog;
#[cfg(any(test, feature = "test-support"))]
use super::subagents::supervisor::TestWatchdog;
#[cfg(any(test, feature = "test-support"))]
use super::subagents::watchdog::Clock;

/// A park point: a place where the service can be paused from an
/// integration test so the test can deterministically interleave its own
/// actions (inspect or modify state, then release the service).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(any(test, feature = "test-support")), allow(dead_code))]
pub enum ParkPoint {
    /// A child parks before its preparation pass.
    ChildPreparation,
    /// A child parks after its preparation pass, before its publication pass.
    ChildPublication,
    /// A session parks just before it is registered in the session map.
    ChildRegistration,
    /// A session parks while the bypass gate is held, before a bypass change
    /// is committed.
    ChildBypass,
    /// A child parks inside its cleanup path.
    ChildCleanup,
    /// A session parks while its skills are still loading, before it is
    /// marked ready.
    SkillLoading,
    /// A resume parks after the runtime is prepared, before re-registration.
    ResumeRegistration,
    /// A root operation parks before its result is published.
    RootPublication,
    /// A watchdog stop parks while child operations are being handed back.
    WatchdogOwnership,
    /// A watchdog stop parks just before its terminal write is claimed.
    WatchdogClaimWrite,
    /// A watchdog stop parks during teardown, before the child is dropped.
    WatchdogTeardown,
    /// A lifecycle claim parks before the session's lifecycle moves on.
    LifecycleClaim,
    /// A shutdown parks while its snapshot is taken.
    ShutdownSnapshot,
}

/// A deterministic pause point inside a child admission flow.
///
/// The child notifies `reached` before performing its first (preparation)
/// pass and waits on `release` before performing its second (publication)
/// pass; the test decides when each pass may proceed.
#[doc(hidden)]
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Default)]
pub struct ChildPreparationBarrier {
    pub reached: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
}

/// Owned handle for the bypass-commit seam: parks the bypass write before it
/// commits and can make the commit fail after a configurable number of
/// successes.
#[derive(Clone)]
pub(crate) struct BypassCommitSeam {
    #[cfg(any(test, feature = "test-support"))]
    barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(any(test, feature = "test-support"))]
    countdown: Arc<std::sync::atomic::AtomicUsize>,
}

impl BypassCommitSeam {
    /// Parks until the test releases the bypass-commit barrier (when armed)
    /// and then consumes one shot of the injected failure countdown,
    /// returning whether this commit must fail.
    pub(crate) async fn before_commit(&self) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        {
            let barrier = self
                .barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(barrier) = barrier {
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
            // Test seam: one writer at a time, so load/store is enough.
            let left = self.countdown.load(std::sync::atomic::Ordering::Acquire);
            if left > 0 {
                self.countdown
                    .store(left - 1, std::sync::atomic::Ordering::Release);
            }
            if left == 1 {
                return true;
            }
        }
        false
    }
}

/// Owned handle for the delivery-commit seam: counts down injected delivery
/// commit failures. A stored `usize::MAX` fails permanently (the test armed
/// a model switch that must keep failing until the scenario is over).
#[derive(Clone)]
pub(crate) struct DeliverySeam {
    #[cfg(any(test, feature = "test-support"))]
    countdown: Arc<std::sync::atomic::AtomicUsize>,
}

impl DeliverySeam {
    /// Consumes one shot of the injected delivery failure countdown,
    /// returning whether this commit must fail.
    pub(crate) fn should_fail_commit(&self) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        {
            // `fetch_update` is deprecated in Rust 1.99 in favour of `try_update`, which
            // does not exist on the minimum supported Rust (1.89, Cargo.toml); keep the old
            // name until the minimum moves past the rename.
            #[allow(deprecated)]
            let previous = self.countdown.fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| {
                    if n == usize::MAX {
                        Some(n)
                    } else {
                        n.checked_sub(1)
                    }
                },
            );
            if previous == Ok(1) || previous == Ok(usize::MAX) {
                self.countdown
                    .store(usize::MAX, std::sync::atomic::Ordering::Release);
                return true;
            }
        }
        false
    }
}

/// The registry of test seams owned by a [`Service`].
///
/// With the feature off the struct is empty and every method below returns
/// the neutral outcome; production code still compiles and runs unchanged.
#[derive(Default, Clone, Debug)]
pub(crate) struct TestPoints {
    // The struct and its fields are `pub(super)`: the whole session module
    // tree consumes the registry; only integration tests arm it, through
    // `Service`.
    /// Arming slot for the skill-loading park.
    #[cfg(any(test, feature = "test-support"))]
    skill_loading_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Remaining delivery-commit failures before (and after) the permanent
    /// mode kicks in.
    #[cfg(any(test, feature = "test-support"))]
    delivery_failure_countdown: Arc<std::sync::atomic::AtomicUsize>,
    /// Arming slot for the shutdown-snapshot park.
    #[cfg(any(test, feature = "test-support"))]
    shutdown_snapshot_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the watchdog claim-write park.
    #[cfg(any(test, feature = "test-support"))]
    watchdog_claim_write_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the watchdog ownership park.
    #[cfg(any(test, feature = "test-support"))]
    watchdog_ownership_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the budget-notice park.
    #[cfg(any(test, feature = "test-support"))]
    budget_notice_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the lifecycle-claim park.
    #[cfg(any(test, feature = "test-support"))]
    lifecycle_claim_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Injected watchdog stop-write failure, one shot.
    #[cfg(any(test, feature = "test-support"))]
    watchdog_stop_failure: Arc<std::sync::atomic::AtomicBool>,
    /// Injected budget-notice enqueue failure, latched until the service
    /// observes it.
    #[cfg(any(test, feature = "test-support"))]
    budget_notice_failure: Arc<std::sync::atomic::AtomicBool>,
    /// Arming slot for the watchdog teardown park.
    #[cfg(any(test, feature = "test-support"))]
    watchdog_teardown_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Watchdog double installed by an integration test.
    #[cfg(any(test, feature = "test-support"))]
    watchdog_test: Arc<Mutex<Option<TestWatchdog>>>,
    /// Root-watchdog double installed by an integration test.
    #[cfg(any(test, feature = "test-support"))]
    root_watchdog_test: Arc<Mutex<Option<TestRootWatchdog>>>,
    /// Arming slot for the child-registration park.
    #[cfg(any(test, feature = "test-support"))]
    child_registration_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the child-cleanup park.
    #[cfg(any(test, feature = "test-support"))]
    child_cleanup_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the child-preparation park.
    #[cfg(any(test, feature = "test-support"))]
    child_preparation_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the child-publication park.
    #[cfg(any(test, feature = "test-support"))]
    child_publication_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the resume-registration park.
    #[cfg(any(test, feature = "test-support"))]
    resume_registration_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the child bypass park.
    #[cfg(any(test, feature = "test-support"))]
    child_bypass_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Arming slot for the bypass-commit park.
    #[cfg(any(test, feature = "test-support"))]
    bypass_commit_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Remaining bypass-commit failures; the commit fails when the count
    /// transitions from 1.
    #[cfg(any(test, feature = "test-support"))]
    bypass_commit_failure: Arc<std::sync::atomic::AtomicUsize>,
    /// Arming slot for the root-publication park.
    #[cfg(any(test, feature = "test-support"))]
    root_publication_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    /// Injected root-publication failure, consumed on the next publication.
    #[cfg(any(test, feature = "test-support"))]
    root_publication_failure: Arc<std::sync::atomic::AtomicBool>,
    /// Injected child start/publication failure: `Some(true)` fails the
    /// publication pass, `Some(false)` fails the start pass; each is
    /// consumed once.
    #[cfg(any(test, feature = "test-support"))]
    child_start_failure: Arc<Mutex<Option<bool>>>,
}

#[cfg(any(test, feature = "test-support"))]
impl TestPoints {
    /// Arms the skill-loading park: the next session to reach the point
    /// parks until the barrier is released.
    pub(super) fn pause_skill_loading(&self, barrier: ChildPreparationBarrier) {
        *self
            .skill_loading_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the shutdown-snapshot park.
    pub(super) fn pause_shutdown_snapshot(&self, barrier: ChildPreparationBarrier) {
        *self
            .shutdown_snapshot_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the watchdog claim-write park.
    pub(super) fn pause_watchdog_claim_write(&self, barrier: ChildPreparationBarrier) {
        *self
            .watchdog_claim_write_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the budget-notice park.
    pub(super) fn pause_budget_notice(&self, barrier: ChildPreparationBarrier) {
        *self
            .budget_notice_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the watchdog ownership park.
    pub(super) fn pause_watchdog_ownership(&self, barrier: ChildPreparationBarrier) {
        *self
            .watchdog_ownership_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the lifecycle-claim park.
    pub(super) fn pause_lifecycle_claim(&self, barrier: ChildPreparationBarrier) {
        *self
            .lifecycle_claim_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Injects a watchdog stop-write failure: the next stop write fails once.
    pub(super) fn fail_watchdog_stop_write(&self) {
        self.watchdog_stop_failure
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Injects a budget-notice enqueue failure.
    pub(super) fn fail_budget_notice_enqueue(&self) {
        self.budget_notice_failure
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether a budget-notice failure is still armed.
    pub(super) fn budget_notice_failure_armed(&self) -> bool {
        self.budget_notice_failure
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Borrows the budget-notice seam: parks before the notice is enqueued
    /// and re-notifies the parked test once the enqueue attempt is over.
    pub(super) fn budget_notice_seam(&self) -> BudgetNoticeSeam {
        BudgetNoticeSeam {
            reached: self
                .budget_notice_barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
            failure: self.budget_notice_failure.clone(),
        }
    }

    /// Arms the watchdog teardown park.
    pub(super) fn pause_watchdog_teardown(&self, barrier: ChildPreparationBarrier) {
        *self
            .watchdog_teardown_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the child-registration park.
    pub(super) fn pause_child_registration(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_registration_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the child bypass park.
    pub(super) fn pause_child_bypass_gate(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_bypass_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the bypass-commit park: the next durable bypass write waits
    /// before it is recorded.
    pub(super) fn pause_bypass_commit(&self, barrier: ChildPreparationBarrier) {
        *self
            .bypass_commit_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Makes the next bypass commit fail.
    pub(super) fn fail_bypass_commit(&self) {
        self.fail_bypass_commit_after(0);
    }

    /// Makes the bypass commit fail after `skip` successful commits.
    pub(super) fn fail_bypass_commit_after(&self, skip: usize) {
        self.bypass_commit_failure
            .store(skip + 1, std::sync::atomic::Ordering::Release);
    }

    /// Arms the root-publication park.
    pub(super) fn pause_root_publication(&self, barrier: ChildPreparationBarrier) {
        *self
            .root_publication_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Injects a root-publication failure: the next publication fails once.
    pub(super) fn fail_root_publication(&self) {
        self.root_publication_failure
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Arms the child-cleanup park.
    pub(super) fn pause_child_cleanup(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_cleanup_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Injects a child start (`false`) or publication (`true`) failure.
    pub(super) fn fail_child_start(&self, publication: bool) {
        *self
            .child_start_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(publication);
    }

    /// Arms the child-preparation park.
    pub(super) fn pause_child_preparation(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_preparation_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the child-publication park.
    pub(super) fn pause_child_publication(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_publication_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Arms the resume-registration park.
    pub(super) fn pause_resume_registration(&self, barrier: ChildPreparationBarrier) {
        *self
            .resume_registration_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Injects `ordinal` delivery-commit failures before a permanent
    /// failure mode (model switches keep failing once the countdown is
    /// exhausted).
    pub(super) fn fail_delivery_commit(&self, ordinal: usize) {
        self.delivery_failure_countdown
            .store(ordinal, std::sync::atomic::Ordering::Release);
    }

    /// Installs the watchdog double used by integration tests.
    pub(super) fn install_watchdog_test(
        &self,
        clock: Arc<dyn Clock>,
        judge: Arc<dyn crate::provider::systemone::DecisionModel>,
    ) {
        *self.watchdog_test.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(TestWatchdog { clock, judge });
    }

    /// Clones the installed watchdog double, if any.
    pub(super) fn watchdog_test(&self) -> Option<TestWatchdog> {
        self.watchdog_test
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Clones the installed root-watchdog double, if any.
    pub(super) fn root_watchdog_test(&self) -> Option<TestRootWatchdog> {
        self.root_watchdog_test
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Installs the root-watchdog double used by integration tests.
    pub(super) fn install_root_watchdog_test(
        &self,
        clock: Arc<dyn Clock>,
        judge: Arc<dyn crate::provider::systemone::DecisionModel>,
        reviewer: Option<Arc<dyn super::subagents::root_watchdog::ProgressReviewer>>,
    ) {
        *self
            .root_watchdog_test
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(TestRootWatchdog {
            clock,
            judge,
            reviewer,
        });
    }

    /// Parks at `point` when an integration test has armed it.
    pub(super) async fn park(&self, point: ParkPoint) {
        let slot = match point {
            ParkPoint::ChildPreparation => &self.child_preparation_barrier,
            ParkPoint::ChildPublication => &self.child_publication_barrier,
            ParkPoint::ChildRegistration => &self.child_registration_barrier,
            ParkPoint::ChildBypass => &self.child_bypass_barrier,
            ParkPoint::ChildCleanup => &self.child_cleanup_barrier,
            ParkPoint::SkillLoading => &self.skill_loading_barrier,
            ParkPoint::ResumeRegistration => &self.resume_registration_barrier,
            ParkPoint::RootPublication => &self.root_publication_barrier,
            ParkPoint::WatchdogOwnership => &self.watchdog_ownership_barrier,
            ParkPoint::WatchdogClaimWrite => &self.watchdog_claim_write_barrier,
            ParkPoint::WatchdogTeardown => &self.watchdog_teardown_barrier,
            ParkPoint::LifecycleClaim => &self.lifecycle_claim_barrier,
            ParkPoint::ShutdownSnapshot => &self.shutdown_snapshot_barrier,
        };
        let barrier = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(barrier) = barrier {
            barrier.reached.notify_one();
            barrier.release.notified().await;
        }
    }

    /// Consumes an armed watchdog stop-write failure.
    pub(super) fn take_watchdog_stop_failure(&self) -> bool {
        self.watchdog_stop_failure
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Consumes an armed root-publication failure.
    pub(super) fn take_root_publication_failure(&self) -> bool {
        self.root_publication_failure
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Consumes an armed child-publication failure, leaving a
    /// child-start failure (`Some(false)`) untouched for the start path.
    pub(super) fn child_start_publication_failure(&self) -> bool {
        let mut slot = self
            .child_start_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *slot == Some(true) {
            *slot = None;
            true
        } else {
            false
        }
    }

    /// Consumes an armed child-start failure.
    pub(super) fn take_child_start_failure(&self) -> Option<bool> {
        self.child_start_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Borrows the bypass-commit seam for the current commit.
    pub(super) fn bypass_commit_seam(&self) -> BypassCommitSeam {
        BypassCommitSeam {
            barrier: self.bypass_commit_barrier.clone(),
            countdown: self.bypass_commit_failure.clone(),
        }
    }

    /// Borrows the delivery-commit seam for the current stream.
    pub(super) fn delivery_seam(&self) -> DeliverySeam {
        DeliverySeam {
            countdown: self.delivery_failure_countdown.clone(),
        }
    }
}

#[cfg(not(any(test, feature = "test-support")))]
impl TestPoints {
    /// No-op: the feature is off, nothing can park.
    pub(super) async fn park(&self, _point: ParkPoint) {}

    /// No-op: the feature is off, commits never fail.
    pub(super) fn bypass_commit_seam(&self) -> BypassCommitSeam {
        BypassCommitSeam {}
    }

    /// No-op: the feature is off, commits never fail.
    pub(super) fn delivery_seam(&self) -> DeliverySeam {
        DeliverySeam {}
    }

    // Mirrors the feature-on API so both builds expose the same call surface;
    // the feature-off build never calls it, which is fine.
    #[allow(dead_code)]
    /// No-op: the feature is off, no failure is armed.
    pub(super) fn budget_notice_failure_armed(&self) -> bool {
        false
    }

    /// No-op: the feature is off, nothing parks and nothing fails.
    pub(super) fn budget_notice_seam(&self) -> BudgetNoticeSeam {
        BudgetNoticeSeam {}
    }

    /// No-op: the feature is off, no failure is armed.
    pub(super) fn take_watchdog_stop_failure(&self) -> bool {
        false
    }

    /// No-op: the feature is off, no failure is armed.
    pub(super) fn take_root_publication_failure(&self) -> bool {
        false
    }

    // Mirrors the feature-on API so both builds expose the same call surface;
    // the feature-off build never calls it, which is fine.
    #[allow(dead_code)]
    /// No-op: the feature is off, no failure is armed.
    pub(super) fn child_start_publication_failure(&self) -> bool {
        false
    }

    /// No-op: the feature is off, no failure is armed.
    pub(super) fn take_child_start_failure(&self) -> Option<bool> {
        None
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug)]
pub(crate) struct BudgetNoticeSeam {
    reached: Option<ChildPreparationBarrier>,
    failure: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(any(test, feature = "test-support"))]
impl BudgetNoticeSeam {
    /// Parks the test before the notice is enqueued and reports whether a
    /// failure was armed (consuming it).
    pub(super) async fn park(&self) -> bool {
        if let Some(barrier) = &self.reached {
            barrier.reached.notify_one();
            barrier.release.notified().await;
        }
        self.failure
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Re-notifies the parked test once the enqueue attempt is over.
    pub(super) fn renotify(&self) {
        if let Some(barrier) = &self.reached {
            barrier.reached.notify_one();
        }
    }
}

#[cfg(not(any(test, feature = "test-support")))]
#[derive(Clone, Debug)]
pub(crate) struct BudgetNoticeSeam {}

#[cfg(not(any(test, feature = "test-support")))]
impl BudgetNoticeSeam {
    /// No-op: the feature is off, nothing parks and nothing fails.
    pub(super) async fn park(&self) -> bool {
        false
    }

    /// No-op: the feature is off.
    pub(super) fn renotify(&self) {}
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn park_returns_immediately_when_nothing_is_armed() {
        let points = TestPoints::default();
        tokio::time::timeout(Duration::from_secs(1), async {
            points.park(ParkPoint::ChildPreparation).await;
            points.park(ParkPoint::ShutdownSnapshot).await;
        })
        .await
        .expect("an unarmed park point must not wait");
    }

    #[tokio::test]
    async fn park_waits_for_release_and_is_one_shot() {
        let points = TestPoints::default();
        let barrier = ChildPreparationBarrier::default();
        points.pause_child_preparation(barrier.clone());
        let parked = {
            let points = points.clone();
            tokio::spawn(async move { points.park(ParkPoint::ChildPreparation).await })
        };
        barrier.reached.notified().await;
        assert!(!parked.is_finished(), "park must wait for release");
        barrier.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), parked)
            .await
            .expect("park must finish after release")
            .unwrap();
        // The barrier was taken: the next pass does not wait.
        tokio::time::timeout(
            Duration::from_secs(1),
            points.park(ParkPoint::ChildPreparation),
        )
        .await
        .expect("a consumed barrier must not park again");
    }

    #[tokio::test]
    async fn pausing_the_bypass_commit_does_not_arm_a_failure() {
        let points = TestPoints::default();
        let barrier = ChildPreparationBarrier::default();
        points.pause_bypass_commit(barrier.clone());
        let seam = points.bypass_commit_seam();
        let commit = tokio::spawn(async move { seam.before_commit().await });
        barrier.reached.notified().await;
        barrier.release.notify_one();
        assert!(!commit.await.unwrap(), "a paused commit must still succeed");
    }

    #[tokio::test]
    async fn bypass_commit_fails_exactly_after_the_armed_skip() {
        let points = TestPoints::default();
        let seam = points.bypass_commit_seam();
        points.fail_bypass_commit_after(2);
        assert!(!seam.before_commit().await);
        assert!(!seam.before_commit().await);
        assert!(seam.before_commit().await, "the third commit fails");
        assert!(!seam.before_commit().await, "the failure is one-shot");
    }

    #[test]
    fn delivery_commit_fails_from_the_armed_ordinal_on() {
        let points = TestPoints::default();
        let seam = points.delivery_seam();
        assert!(!seam.should_fail_commit(), "unarmed seam never fails");
        points.fail_delivery_commit(2);
        assert!(!seam.should_fail_commit());
        assert!(seam.should_fail_commit(), "the second commit fails");
        assert!(seam.should_fail_commit(), "and every later one");
    }

    #[test]
    fn failure_flags_are_consumed_once() {
        let points = TestPoints::default();
        points.fail_root_publication();
        assert!(points.take_root_publication_failure());
        assert!(!points.take_root_publication_failure());
        points.fail_watchdog_stop_write();
        assert!(points.take_watchdog_stop_failure());
        assert!(!points.take_watchdog_stop_failure());
        points.fail_child_start(true);
        assert!(points.child_start_publication_failure());
        assert_eq!(points.take_child_start_failure(), None);
        points.fail_child_start(false);
        assert!(
            !points.child_start_publication_failure(),
            "a start failure is left for the start path"
        );
        assert_eq!(points.take_child_start_failure(), Some(false));
    }
}
