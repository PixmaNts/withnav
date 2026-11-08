//! Part of the `withnav-engine` crate: stage contract, frozen plan and the serial runner.
use crate::core::*;
use crate::engine::authorize::ReviewStage;
use futures::future::BoxFuture;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Transform failure aborts the operation; an observer or advisory stage can be skipped instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePolicy {
    Abort,
    Skip,
}

#[derive(Debug, Clone)]
pub struct StageDescriptor {
    pub id: String,
    pub version: u32,
    pub chain: Chain,
    pub on_failure: FailurePolicy,
}

/// A question for the Captain. A stage yields it; it never waits on a UI itself.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingInteraction {
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StageHalt {
    Reject { reason: String },
    Yield(PendingInteraction),
    Cancel,
}

#[derive(Debug)]
pub enum StageStep<P> {
    Continue(P),
    Halt(StageHalt),
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StageError(pub String);

#[derive(Debug, thiserror::Error)]
#[error("stage {stage} failed: {source}")]
pub struct ChainError {
    pub stage: String,
    pub source: StageError,
}

pub struct StageContext {
    pub call: Option<ToolCallId>,
}

/// What a stage returns is data: the runner decides how it lands on the input.
/// `Default` is the no-op patch used when a skippable stage fails.
pub trait Patch<I>: Default {
    fn apply(self, input: &mut I);
}

pub trait Stage<I, P>: Send + Sync {
    fn descriptor(&self) -> &StageDescriptor;
    fn run<'a>(
        &'a self,
        input: &'a I,
        cx: &'a StageContext,
    ) -> BoxFuture<'a, Result<StageStep<P>, StageError>>;
}

pub type SharedStage<I, P> = Arc<dyn Stage<I, P>>;

/// `tool.prepare` patch: may rewrite the name and arguments, nothing else.
#[derive(Debug, Default)]
pub struct ToolPatch {
    pub name: Option<ToolName>,
    pub args: Option<ToolArguments>,
}

impl Patch<ToolCall> for ToolPatch {
    fn apply(self, call: &mut ToolCall) {
        if let Some(name) = self.name {
            call.name = name;
        }
        if let Some(args) = self.args {
            call.args = args;
        }
    }
}

/// What `tool.result` stages see. Stages get `&ResultDraft`, so they cannot edit it directly.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultDraft {
    pub call: ToolCallId,
    pub output: ToolOutput,
}

/// `tool.result` patch: only the output text exists, so ids and status cannot be touched.
#[derive(Debug, Default)]
pub struct ResultPatch {
    pub text: Option<String>,
}

impl Patch<ResultDraft> for ResultPatch {
    fn apply(self, draft: &mut ResultDraft) {
        if let Some(text) = self.text {
            draft.output.text = text;
        }
    }
}

/// Ordered stage lists, frozen when a turn starts and shared by `Arc`.
/// A request waiting on the Captain keeps the plan it was decided under.
#[derive(Default)]
pub struct PipelinePlan {
    pub tool_prepare: Vec<SharedStage<ToolCall, ToolPatch>>,
    pub review: Vec<ReviewStage>,
    pub tool_result: Vec<SharedStage<ResultDraft, ResultPatch>>,
}

impl std::fmt::Debug for PipelinePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelinePlan")
            .field("tool_prepare", &self.tool_prepare.len())
            .field("review", &self.review.len())
            .field("tool_result", &self.tool_result.len())
            .finish()
    }
}

/// A stage registered in the wrong slot is refused when the plan is built, not when it runs.
#[derive(Debug, thiserror::Error)]
#[error("stage {stage} belongs to {actual:?}, not {slot:?}")]
pub struct PlanError {
    pub stage: String,
    pub actual: Chain,
    pub slot: Chain,
}

fn check<I, P>(stage: &SharedStage<I, P>, slot: Chain) -> Result<(), PlanError> {
    let d = stage.descriptor();
    if d.chain == slot {
        return Ok(());
    }
    Err(PlanError { stage: d.id.clone(), actual: d.chain, slot })
}

impl PipelinePlan {
    pub fn prepare(mut self, s: SharedStage<ToolCall, ToolPatch>) -> Result<Self, PlanError> {
        check(&s, Chain::ToolPrepare)?;
        self.tool_prepare.push(s);
        Ok(self)
    }

    pub fn result(mut self, s: SharedStage<ResultDraft, ResultPatch>) -> Result<Self, PlanError> {
        check(&s, Chain::ToolResult)?;
        self.tool_result.push(s);
        Ok(self)
    }

    pub fn review(mut self, s: ReviewStage) -> Result<Self, PlanError> {
        if let ReviewStage::Reviewer(r) = &s {
            check(r, Chain::ToolAuthorize)?;
        }
        self.review.push(s);
        Ok(self)
    }
}

/// Holder of the plan the next turn will use. A turn takes one snapshot and keeps it.
pub struct PlanStore(Mutex<Arc<PipelinePlan>>);

impl PlanStore {
    pub fn new(plan: PipelinePlan) -> Self {
        Self(Mutex::new(Arc::new(plan)))
    }

    pub fn snapshot(&self) -> Arc<PipelinePlan> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn swap(&self, plan: PipelinePlan) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(plan);
    }
}

pub enum ChainResult<I> {
    Completed(I),
    Halted(StageHalt),
}

/// Serial runner: one stage at a time, patches applied between stages, one observation per run.
pub struct Runner {
    telemetry: Arc<dyn Telemetry>,
}

impl Runner {
    pub fn new(telemetry: Arc<dyn Telemetry>) -> Self {
        Self { telemetry }
    }

    /// Run one stage and record it. A failing stage whose policy is `Skip` yields the no-op patch.
    pub async fn step<I, P: Default>(
        &self,
        stage: &dyn Stage<I, P>,
        input: &I,
        cx: &StageContext,
    ) -> Result<StageStep<P>, ChainError> {
        let d = stage.descriptor();
        let started = Instant::now();
        let result = stage.run(input, cx).await;
        let outcome = match &result {
            Ok(StageStep::Continue(_)) => StageOutcome::Continued,
            Ok(StageStep::Halt(_)) => StageOutcome::Halted,
            Err(_) => StageOutcome::Failed,
        };
        self.telemetry.record(StageObservation {
            stage: d.id.clone(),
            version: d.version,
            chain: d.chain,
            duration: started.elapsed(),
            outcome,
            call: cx.call.clone(),
        });
        match result {
            Ok(step) => Ok(step),
            Err(_) if d.on_failure == FailurePolicy::Skip => Ok(StageStep::Continue(P::default())),
            Err(source) => Err(ChainError { stage: d.id.clone(), source }),
        }
    }

    pub async fn run_chain<I, P: Patch<I>>(
        &self,
        stages: &[SharedStage<I, P>],
        mut input: I,
        cx: &StageContext,
    ) -> Result<ChainResult<I>, ChainError> {
        for stage in stages {
            match self.step(stage.as_ref(), &input, cx).await? {
                StageStep::Continue(patch) => patch.apply(&mut input),
                StageStep::Halt(halt) => return Ok(ChainResult::Halted(halt)),
            }
        }
        Ok(ChainResult::Completed(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::*;
    use serde_json::json;

    fn draft() -> ToolCall {
        tool_call("c1", "echo", json!({"text": "a"}))
    }

    fn rename(id: &str, log: &Log, to: &'static str) -> SharedStage<ToolCall, ToolPatch> {
        let log = log.clone();
        let id = id.to_owned();
        FnStage::arc(&id.clone(), Chain::ToolPrepare, move |_: &ToolCall| {
            log.lock().unwrap().push(id.clone());
            Ok(StageStep::Continue(ToolPatch { name: Some(to.into()), args: None }))
        })
    }

    #[tokio::test]
    async fn stages_run_in_plan_order_and_apply_patches() {
        let log = Log::default();
        let stages = [rename("a", &log, "one"), rename("b", &log, "two")];
        let runner = Runner::new(Arc::new(RecordingTelemetry::default()));
        let out = runner.run_chain(&stages, draft(), &StageContext { call: None }).await.unwrap();
        assert!(matches!(out, ChainResult::Completed(c) if c.name == "two".into()));
        assert_eq!(*log.lock().unwrap(), ["a", "b"]);
    }

    #[tokio::test]
    async fn a_halt_short_circuits_the_rest_of_the_chain() {
        let log = Log::default();
        let halt = FnStage::arc("stop", Chain::ToolPrepare, |_: &ToolCall| {
            Ok(StageStep::Halt(StageHalt::Reject { reason: "no".into() }))
        });
        let stages = [rename("a", &log, "one"), halt, rename("c", &log, "three")];
        let runner = Runner::new(Arc::new(RecordingTelemetry::default()));
        let out = runner.run_chain(&stages, draft(), &StageContext { call: None }).await.unwrap();
        assert!(matches!(out, ChainResult::Halted(StageHalt::Reject { .. })));
        assert_eq!(*log.lock().unwrap(), ["a"]);
    }

    #[tokio::test]
    async fn one_observation_per_stage_run_reaches_telemetry() {
        let telemetry = Arc::new(RecordingTelemetry::default());
        let runner = Runner::new(telemetry.clone());
        let ok = rename("a", &Log::default(), "one");
        let failing = FnStage::arc("boom", Chain::ToolPrepare, |_: &ToolCall| Err(StageError("x".into())));
        let skipped =
            FnStage::skippable("soft", Chain::ToolPrepare, |_: &ToolCall| Err(StageError("y".into())));
        let stages = [ok, skipped, failing];
        let cx = StageContext { call: Some("c1".into()) };
        let err = runner.run_chain(&stages, draft(), &cx).await.err().unwrap();
        assert_eq!(err.stage, "boom");
        let seen = telemetry.observations();
        let got: Vec<_> = seen.iter().map(|o| (o.stage.as_str(), o.chain, o.outcome)).collect();
        assert_eq!(
            got,
            [
                ("a", Chain::ToolPrepare, StageOutcome::Continued),
                ("soft", Chain::ToolPrepare, StageOutcome::Failed),
                ("boom", Chain::ToolPrepare, StageOutcome::Failed),
            ]
        );
        assert!(seen.iter().all(|o| o.call == Some("c1".into()) && o.version == 1));
    }

    #[test]
    fn a_stage_in_the_wrong_slot_is_refused_when_the_plan_is_built() {
        let wrong = FnStage::arc("r", Chain::ToolResult, |_: &ToolCall| {
            Ok(StageStep::Continue(ToolPatch::default()))
        });
        assert!(PipelinePlan::default().prepare(wrong).is_err());
    }
}
