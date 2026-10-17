//! Part of the `withnav-engine` crate: the fixed authorization skeleton and the review slot.
//!
//! The order below mirrors the permission engine in use today: yolo, written deny, floor,
//! doom loop, free read / temp write, bypass, review slot, rule. Only this module can build an
//! [`ExecutionPermit`], so no stage, plugin patch or JSON argument can forge one.
use crate::core::*;
use crate::engine::pipeline::*;
use futures::future::BoxFuture;
use std::sync::Arc;

pub const DOOM_LOOP_THRESHOLD: u32 = 3;

/// Proof that the host authorized exactly this call. The private field keeps construction
/// inside this module, and the call is frozen in it: dispatch runs `call()`, nothing else.
///
/// ```compile_fail
/// use withnav_sketch::core::*;
/// use withnav_sketch::engine::authorize::ExecutionPermit;
/// let call = ToolCall { id: "c".into(), name: "rm".into(), args: ToolArguments(serde_json::json!({})) };
/// let _forged = ExecutionPermit { call };
/// ```
#[derive(Debug)]
pub struct ExecutionPermit {
    call: ToolCall,
}

impl ExecutionPermit {
    fn issue(call: &ToolCall) -> Self {
        Self { call: call.clone() }
    }

    pub fn call(&self) -> &ToolCall {
        &self.call
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub call: ToolCallId,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Denial {
    pub rule: Option<String>,
    pub message: String,
}

#[derive(Debug)]
pub enum Authorization {
    Run(ExecutionPermit),
    Ask(PermissionRequest),
    Deny(Denial),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Floor {
    #[default]
    None,
    /// A credential: always asks, nothing can allow it.
    Credential(String),
    /// A path that cannot be resolved the way execution would resolve it: same treatment.
    Unresolved(String),
    /// WithNav's own configuration: only the protected check can let it through.
    Protected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuleResult {
    Allow,
    Ask,
    #[default]
    NoRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DoomRule {
    #[default]
    None,
    Allow,
    Deny,
}

/// Everything read from the rule store for one call, read once and before any decision.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub floor: Floor,
    pub free_read: bool,
    pub temp_write: bool,
    /// Name of the written deny rule that matches, if any.
    pub deny: Option<String>,
    pub result: RuleResult,
    pub doom: DoomRule,
}

#[derive(Debug, thiserror::Error)]
#[error("the permission rules could not be read: {0}")]
pub struct RuleError(pub String);

pub trait RuleRepository: Send + Sync {
    fn lookup(&self, call: &ToolCall, class: ToolClass) -> Result<Facts, RuleError>;
}

/// What stages and flows see: the call as rewritten by `tool.prepare`, never the original.
#[derive(Debug, Clone)]
pub struct PermissionView {
    pub call: ToolCall,
    pub class: ToolClass,
    pub control: Control,
    pub rule: RuleResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowAnswer {
    Allow,
    Deny,
    AskCaptain,
    ApplyRule,
}

/// Built-in review: may allow, as the permission flows do today.
pub trait PermissionFlow: Send + Sync {
    fn answer<'a>(&'a self, view: &'a PermissionView) -> BoxFuture<'a, FlowAnswer>;
}

/// The protected check: a generic allow never stands in for it.
pub trait ProtectedCheck: Send + Sync {
    fn passes<'a>(&'a self, view: &'a PermissionView) -> BoxFuture<'a, bool>;
}

/// What a plugin reviewer may say. There is no `Allow`: a reviewer can only escalate, so the
/// type system keeps plugins from widening what the Captain allowed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Restriction {
    #[default]
    NoOpinion,
    Ask {
        reason: String,
    },
    Deny {
        reason: String,
    },
}

/// One entry of the configurable review slot.
#[derive(Clone)]
pub enum ReviewStage {
    Flow(Arc<dyn PermissionFlow>),
    Reviewer(SharedStage<PermissionView, Restriction>),
}

pub struct Authorizer {
    pub runner: Arc<Runner>,
    pub rules: Arc<dyn RuleRepository>,
    pub protected: Arc<dyn ProtectedCheck>,
}

impl Authorizer {
    pub async fn authorize(
        &self,
        plan: &PipelinePlan,
        call: &ToolCall,
        class: ToolClass,
        control: Control,
        repeats: u32,
    ) -> Authorization {
        let run = || Authorization::Run(ExecutionPermit::issue(call));
        let ask = |reason: String| Authorization::Ask(PermissionRequest { call: call.id.clone(), reason });
        let deny = |rule: Option<String>, message: String| Authorization::Deny(Denial { rule, message });

        // 1. Yolo checks nothing: no rule, no flow, no reviewer.
        if control == Control::Yolo {
            return run();
        }
        let facts = match self.rules.lookup(call, class) {
            Ok(facts) => facts,
            Err(e) => return ask(e.to_string()),
        };
        let view = PermissionView { call: call.clone(), class, control, rule: facts.result };

        // 2. A written deny is final: it comes before anything that could ask or allow.
        if let Some(rule) = facts.deny {
            return deny(Some(rule.clone()), format!("denied by the permission rule {rule}"));
        }
        // 3. The floor.
        match &facts.floor {
            Floor::Credential(t) => {
                return ask(format!("{t} is a credential; WithNav always asks"));
            }
            Floor::Unresolved(t) => return ask(format!("{t} cannot be resolved")),
            Floor::Protected(t) if !self.protected.passes(&view).await => {
                return ask(format!("{t} is WithNav's own configuration"));
            }
            Floor::Protected(_) | Floor::None => {}
        }
        // 4. Doom loop, after the deny so a question never hides a deny; bypass skips it.
        if repeats >= DOOM_LOOP_THRESHOLD {
            let reason = format!("{} called {repeats} times in a row with the same arguments", call.name.0);
            match facts.doom {
                DoomRule::Deny => return deny(None, reason),
                DoomRule::Allow => {}
                DoomRule::None if control == Control::Bypass => {}
                DoomRule::None => return ask(reason),
            }
        }
        // 5. Free read and temporary write need no flow; a written ask rule still applies.
        if facts.free_read || (facts.temp_write && facts.result != RuleResult::Ask) {
            return run();
        }
        // 6. Bypass skips the review slot.
        if control == Control::Bypass {
            return run();
        }
        // 7. Review slot: flows may allow, reviewers may only escalate. Order does not matter
        // for safety: a reviewer placed before the flows still cannot turn an ask into a run.
        let mut answer = FlowAnswer::ApplyRule;
        let mut escalation: Option<String> = None;
        let cx = StageContext { call: Some(call.id.clone()) };
        for stage in &plan.review {
            match stage {
                ReviewStage::Flow(flow) => {
                    let a = flow.answer(&view).await;
                    if answer == FlowAnswer::ApplyRule {
                        answer = a;
                    }
                }
                ReviewStage::Reviewer(reviewer) => {
                    match self.runner.step(reviewer.as_ref(), &view, &cx).await {
                        Ok(StageStep::Continue(Restriction::NoOpinion)) => {}
                        Ok(StageStep::Continue(Restriction::Ask { reason })) => {
                            escalation.get_or_insert(reason);
                        }
                        Ok(StageStep::Continue(Restriction::Deny { reason }))
                        | Ok(StageStep::Halt(StageHalt::Reject { reason })) => {
                            return deny(None, reason);
                        }
                        Ok(StageStep::Halt(_)) => {
                            escalation.get_or_insert("a reviewer wants your decision".into());
                        }
                        // A reviewer that fails closed toward the Captain, never toward a run.
                        Err(e) => {
                            escalation.get_or_insert(e.to_string());
                        }
                    }
                }
            }
        }
        if let Some(reason) = escalation {
            return ask(reason);
        }
        // 8. The flow's answer, then the rule.
        match (answer, facts.result) {
            (FlowAnswer::Allow, _) | (FlowAnswer::ApplyRule, RuleResult::Allow) => run(),
            (FlowAnswer::Deny, _) => deny(None, "denied by the permission flow".into()),
            (FlowAnswer::AskCaptain, _) => ask("the permission flow wants your decision".into()),
            (FlowAnswer::ApplyRule, RuleResult::Ask) => ask("a permission rule asks for this call".into()),
            (FlowAnswer::ApplyRule, RuleResult::NoRule) => ask("no permission rule covers this call".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::*;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        tool_call("c1", name, json!({}))
    }

    struct Rig {
        auth: Authorizer,
        telemetry: Arc<RecordingTelemetry>,
    }

    fn build(rules: InMemoryRules) -> Rig {
        let telemetry = Arc::new(RecordingTelemetry::default());
        let auth = Authorizer {
            runner: Arc::new(Runner::new(telemetry.clone())),
            rules: Arc::new(rules),
            protected: Arc::new(FixedProtected(true)),
        };
        Rig { auth, telemetry }
    }

    fn plan(review: Vec<ReviewStage>) -> PipelinePlan {
        review.into_iter().fold(PipelinePlan::default(), |p, s| p.review(s).unwrap())
    }

    fn reviewer(id: &str, r: Restriction) -> ReviewStage {
        ReviewStage::Reviewer(FnStage::arc(id, Chain::ToolAuthorize, move |_: &PermissionView| {
            Ok(StageStep::Continue(r.clone()))
        }))
    }

    async fn decide(rig: &Rig, plan: &PipelinePlan, name: &str, control: Control) -> Authorization {
        rig.auth.authorize(plan, &call(name), ToolClass::Write, control, 0).await
    }

    #[tokio::test]
    async fn a_written_deny_is_final_even_if_a_flow_allows() {
        let rig = build(InMemoryRules::default().deny("rm"));
        let p = plan(vec![ReviewStage::Flow(Arc::new(FixedFlow(FlowAnswer::Allow)))]);
        assert!(matches!(decide(&rig, &p, "rm", Control::Normal).await, Authorization::Deny(_)));
        // Bypass does not lift it either; only yolo, which checks nothing, runs it.
        assert!(matches!(decide(&rig, &p, "rm", Control::Bypass).await, Authorization::Deny(_)));
        assert!(matches!(decide(&rig, &p, "rm", Control::Yolo).await, Authorization::Run(_)));
    }

    #[tokio::test]
    async fn a_reviewer_can_escalate_but_never_allow() {
        let flows = || ReviewStage::Flow(Arc::new(FixedFlow(FlowAnswer::ApplyRule)));
        // Allow rule + Ask reviewer -> Ask. Allow rule + NoOpinion -> Run (the rule decides).
        let rig_allow = build(InMemoryRules::default().allow("echo"));
        let p = plan(vec![flows(), reviewer("scan", Restriction::Ask { reason: "secret".into() })]);
        assert!(matches!(decide(&rig_allow, &p, "echo", Control::Normal).await, Authorization::Ask(_)));
        let p = plan(vec![flows(), reviewer("scan", Restriction::NoOpinion)]);
        assert!(matches!(decide(&rig_allow, &p, "echo", Control::Normal).await, Authorization::Run(_)));
        let p = plan(vec![reviewer("scan", Restriction::Deny { reason: "x".into() }), flows()]);
        assert!(matches!(decide(&rig_allow, &p, "echo", Control::Normal).await, Authorization::Deny(_)));

        // Ask rule: no restriction, in any position, ever yields Run.
        let rig_ask = build(InMemoryRules::default().ask("echo"));
        for r in [
            Restriction::NoOpinion,
            Restriction::Ask { reason: "a".into() },
            Restriction::Deny { reason: "d".into() },
        ] {
            for p in
                [plan(vec![flows(), reviewer("r", r.clone())]), plan(vec![reviewer("r", r.clone()), flows()])]
            {
                assert!(!matches!(
                    decide(&rig_ask, &p, "echo", Control::Normal).await,
                    Authorization::Run(_)
                ));
            }
        }
    }

    #[tokio::test]
    async fn yolo_skips_the_authorize_chain_and_the_review_stages() {
        let seen = Log::default();
        let log = seen.clone();
        let spy =
            ReviewStage::Reviewer(FnStage::arc("spy", Chain::ToolAuthorize, move |_: &PermissionView| {
                log.lock().unwrap().push("spy".into());
                Ok(StageStep::Continue(Restriction::Deny { reason: "never reached".into() }))
            }));
        let rig = build(InMemoryRules::default().ask("echo"));
        let p = plan(vec![spy]);
        assert!(matches!(decide(&rig, &p, "echo", Control::Yolo).await, Authorization::Run(_)));
        assert!(seen.lock().unwrap().is_empty());
        assert!(rig.telemetry.observations().is_empty());
        // Control: normal does consult the reviewer, and it is observed.
        assert!(matches!(decide(&rig, &p, "echo", Control::Normal).await, Authorization::Deny(_)));
        assert_eq!(rig.telemetry.observations().len(), 1);
    }

    #[tokio::test]
    async fn floor_and_doom_loop_ask_before_any_rule_can_allow() {
        let floored =
            build(InMemoryRules::default().allow("cat").floor("cat", Floor::Credential(".env".into())));
        let p = plan(vec![]);
        assert!(matches!(decide(&floored, &p, "cat", Control::Bypass).await, Authorization::Ask(_)));
        let looped = build(InMemoryRules::default().allow("echo"));
        let c = call("echo");
        let at = |n| looped.auth.authorize(&p, &c, ToolClass::Write, Control::Normal, n);
        assert!(matches!(at(2).await, Authorization::Run(_)));
        assert!(matches!(at(DOOM_LOOP_THRESHOLD).await, Authorization::Ask(_)));
    }
}
