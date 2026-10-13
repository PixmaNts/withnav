//! The agent loop on the rule-based permission path, end to end: scripted
//! model responses, scripted permission flows or the built-in flow over a
//! fake instinct, and the decision table.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::tests::{
    calls_chunk, make_actor, registry_gated, spawn_mock, text_chunk, tool_result_of, MockServer,
};
use super::*;
use crate::agent::flow_hook::{
    FlowVerdict, PermissionCallContext, PermissionVerdict, ProtectedCheck, RuleResult,
    ToolCallFlows,
};
use crate::agent::permissions::PermissionOrigin;
use crate::permission::decisions::{DecisionFilter, Outcome};
use crate::permission::rules::{Effect, RuleFilter, RuleOrigin, RuleScope};
use crate::provider::test_support::{http_sse, sse_body};

/// Scripted permission flows that count what they are asked.
#[derive(Debug)]
pub(super) struct FakeFlows {
    mode: &'static str,
    check: ProtectedCheck,
    asked: AtomicUsize,
    checked: AtomicUsize,
}

impl FakeFlows {
    pub(super) fn new(mode: &'static str) -> Self {
        Self::with_check(mode, ProtectedCheck::Unavailable)
    }

    fn with_check(mode: &'static str, check: ProtectedCheck) -> Self {
        Self {
            mode,
            check,
            asked: AtomicUsize::new(0),
            checked: AtomicUsize::new(0),
        }
    }

    pub(super) fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }

    fn checked(&self) -> usize {
        self.checked.load(Ordering::SeqCst)
    }
}

impl ToolCallFlows for FakeFlows {
    fn permission_pre_call<'a>(
        &'a self,
        ctx: PermissionCallContext,
        rule: RuleResult,
        _: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, PermissionVerdict> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let ask = FlowVerdict::AskCaptain {
                note: "flow asks".into(),
                flow: "test".into(),
                run_id: "run".into(),
            };
            let verdict = match self.mode {
                "allow" => FlowVerdict::Allow,
                "deny" => FlowVerdict::Deny {
                    note: "flow denied".into(),
                },
                "ask" => ask,
                "timeout" => std::future::pending().await,
                // The flows had nothing to say: the rule's result applies.
                _ if rule == RuleResult::Allow && !ctx.protected => FlowVerdict::Allow,
                _ => ask,
            };
            // A scripted verdict is the flow's own; the default applies the rule.
            let own = matches!(self.mode, "allow" | "deny" | "ask");
            PermissionVerdict {
                verdict,
                flows: vec![crate::agent::flow_hook::PermissionFlowAnswer {
                    flow: "test".into(),
                    run_id: "run".into(),
                    action: String::new(),
                    used_rule: !own,
                }],
            }
        })
    }

    fn protected_check<'a>(
        &'a self,
        _: PermissionCallContext,
        _: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, ProtectedCheck> {
        self.checked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { self.check })
    }
}

/// A build actor in a canonical temporary workspace, with its own config and
/// data directories for the fixed floor.
fn actor_with(flows: Option<Arc<FakeFlows>>) -> AgentActor {
    let mut actor = make_actor(
        "http://127.0.0.1:1/v1".into(),
        registry_gated(),
        CancellationToken::new(),
    );
    actor.role = Some(
        crate::agent::definitions::Agents::builtin()
            .get("build")
            .unwrap()
            .clone(),
    );
    let ws = actor.tool_context.workspace.canonicalize().unwrap();
    actor.tool_context.workspace = ws.clone();
    actor.tool_context.writable_roots = vec![ws.clone()];
    let home = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
    std::fs::create_dir_all(home.join("config")).unwrap();
    actor.permission_dirs = crate::agent::actor::PermissionDirs {
        config: Some(home.join("config")),
        data: Some(home.join("data")),
    };
    actor.permission_session = "session".into();
    actor.tool_flows = flows.map(|flows| flows as Arc<dyn ToolCallFlows>);
    actor
}

async fn script(actor: &mut AgentActor, responses: Vec<Vec<u8>>) -> MockServer {
    let server = spawn_mock(responses).await;
    actor.provider.base_url = server.url.clone();
    server
}

fn calls(calls: &[(&str, &str, serde_json::Value)]) -> Vec<u8> {
    http_sse(&sse_body(&[&calls_chunk(calls)]))
}

fn done(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

async fn run(actor: &mut AgentActor) -> RunOutcome {
    actor.push_user("go");
    run_agent(actor, &mut |_| {}).await.unwrap()
}

/// The `approved_by` of every tool post-call, in order.
async fn run_observed(actor: &mut AgentActor) -> (RunOutcome, Vec<serde_json::Value>) {
    actor.push_user("go");
    let mut approved = Vec::new();
    let outcome = run_agent(actor, &mut |event| {
        if let AgentEvent::PostCall { result, .. } = event {
            approved.push(result.metadata["approved_by"].clone());
        }
    })
    .await
    .unwrap();
    (outcome, approved)
}

fn decisions(actor: &AgentActor) -> Vec<crate::permission::decisions::Decision> {
    let mut rows = actor
        .permission_store
        .as_ref()
        .unwrap()
        .list_decisions(&DecisionFilter::default(), 100)
        .unwrap();
    rows.reverse();
    rows
}

fn add_rule(actor: &AgentActor, action: &str, resource: &str, effect: Effect) {
    actor
        .permission_store
        .as_ref()
        .unwrap()
        .add(
            action,
            resource,
            effect,
            RuleScope::Global,
            RuleOrigin::Manual,
        )
        .unwrap();
}

fn bash(id: &str, command: &str) -> (String, String, serde_json::Value) {
    (id.into(), "bash".into(), json!({ "command": command }))
}

fn owned(calls: &[(String, String, serde_json::Value)]) -> Vec<u8> {
    let borrowed: Vec<_> = calls
        .iter()
        .map(|(id, name, args)| (id.as_str(), name.as_str(), args.clone()))
        .collect();
    self::calls(&borrowed)
}

#[tokio::test]
async fn no_rule_and_a_flow_allow_runs_once_with_flow_provenance() {
    let flows = Arc::new(FakeFlows::new("allow"));
    let mut actor = actor_with(Some(flows.clone()));
    script(
        &mut actor,
        vec![owned(&[bash("b1", "echo hi")]), done("ok")],
    )
    .await;
    let (outcome, approved) = run_observed(&mut actor).await;
    assert!(matches!(outcome, RunOutcome::Done(_)), "{outcome:?}");
    assert_eq!(approved, [json!("flow")]);
    assert_eq!(flows.asked(), 1, "one decision, one flow run");
    let rows = decisions(&actor);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, Outcome::Allowed);
    assert_eq!(rows[0].approved_by.as_deref(), Some("flow"));
    assert_eq!(rows[0].flow_verdict.as_deref(), Some("allow"));
}

#[tokio::test(start_paused = true)]
async fn no_rule_and_a_flow_that_never_answers_asks_the_captain() {
    let flows = Arc::new(FakeFlows::new("timeout"));
    let mut actor = actor_with(Some(flows.clone()));
    script(&mut actor, vec![owned(&[bash("b1", "echo hi")])]).await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("a silent flow applies the rule, and no rule asks");
    };
    assert!(!request.requires_captain, "a flow question is bypassable");
    assert_eq!(
        request.always_label.as_deref(),
        Some("Always (project): bash echo *")
    );
    let rows = decisions(&actor);
    assert_eq!(rows[0].outcome, Outcome::Pending);
    assert_eq!(rows[0].flow_verdict.as_deref(), Some("apply_rule"));
}

#[tokio::test]
async fn a_deny_rule_denies_under_bypass_without_asking_any_flow() {
    let flows = Arc::new(FakeFlows::new("allow"));
    let mut actor = actor_with(Some(flows.clone()));
    actor.bypass.lock().unwrap().enabled = true;
    add_rule(&actor, "bash", "rm *", Effect::Deny);
    let target = actor.tool_context.workspace.join("keep");
    std::fs::write(&target, "x").unwrap();
    script(
        &mut actor,
        vec![owned(&[bash("b1", "rm -f keep")]), done("ok")],
    )
    .await;
    assert!(matches!(run(&mut actor).await, RunOutcome::Done(_)));
    assert!(target.exists(), "a denied call never runs");
    let result: serde_json::Value =
        serde_json::from_str(&tool_result_of(&actor, "b1").unwrap()).unwrap();
    assert_eq!(result["error"]["code"], "permission_denied");
    assert_eq!(result["metadata"]["denied_by"], "rule");
    assert_eq!(flows.asked(), 0);
    let rows = decisions(&actor);
    assert_eq!(rows[0].outcome, Outcome::Denied);
    assert_eq!(rows[0].effect, Some(Effect::Deny));
}

#[tokio::test]
async fn a_credential_read_asks_under_bypass_and_bypass_cannot_answer_it() {
    let flows = Arc::new(FakeFlows::new("allow"));
    let mut actor = actor_with(Some(flows.clone()));
    actor.bypass.lock().unwrap().enabled = true;
    add_rule(&actor, "*", "*", Effect::Allow);
    std::fs::write(actor.tool_context.workspace.join(".env"), "TOKEN=1").unwrap();
    script(
        &mut actor,
        vec![owned(&[(
            "r1".into(),
            "read".into(),
            json!({"path": ".env"}),
        )])],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("a credential always asks");
    };
    assert!(request.requires_captain);
    assert_eq!(request.always_label, None, "never saved as a rule");
    assert!(!pending_bypass_eligible(&actor));
    assert!(resolve_permission_with_origin(
        &mut actor,
        request.id,
        PermissionDecision::AllowOnce,
        PermissionOrigin::Bypass,
        &mut |_| {}
    )
    .await
    .is_err());
    assert!(actor.has_pending());
    assert_eq!(flows.asked(), 0);
    assert!(decisions(&actor)[0]
        .command_matches
        .as_array()
        .is_none_or(|m| m.is_empty()));
}

#[tokio::test]
async fn a_config_write_runs_only_on_a_passed_protected_check() {
    for (check, runs) in [
        (ProtectedCheck::Passed, true),
        (ProtectedCheck::Failed, false),
        (ProtectedCheck::Unavailable, false),
    ] {
        let flows = Arc::new(FakeFlows::with_check("allow", check));
        let mut actor = actor_with(Some(flows.clone()));
        let config = actor
            .permission_dirs
            .config
            .clone()
            .unwrap()
            .join("config.json");
        let write = (
            "w1".to_owned(),
            "write".to_owned(),
            json!({"path": config, "content": "{}"}),
        );
        script(&mut actor, vec![owned(&[write]), done("ok")]).await;
        let outcome = run(&mut actor).await;
        assert_eq!(flows.checked(), 1, "{check:?}");
        if runs {
            assert!(matches!(outcome, RunOutcome::Done(_)), "{check:?}");
            assert_eq!(std::fs::read_to_string(&config).unwrap(), "{}");
        } else {
            let RunOutcome::PermissionRequired(request) = outcome else {
                panic!("{check:?} asks");
            };
            assert!(request.requires_captain, "bypass never answers it");
            assert_eq!(request.always_label, None, "Always never covers it");
            assert!(!config.exists());
        }
    }
}

#[tokio::test]
async fn always_on_a_push_saves_the_prefix_and_releases_the_held_sibling() {
    let flows = Arc::new(FakeFlows::new("apply"));
    let mut actor = actor_with(Some(flows.clone()));
    script(
        &mut actor,
        vec![
            owned(&[
                bash("p1", "git push origin x"),
                bash("p2", "git push origin y"),
            ]),
            done("pushed"),
        ],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("no rule asks");
    };
    assert_eq!(request.tool_call_id, "p1");
    assert_eq!(
        request.always_label.as_deref(),
        Some("Always (project): bash git push *")
    );
    let mut approved = Vec::new();
    let outcome = resolve_permission(
        &mut actor,
        request.id,
        PermissionDecision::AllowAlways,
        &mut |event| {
            if let AgentEvent::PostCall { result, .. } = event {
                approved.push(result.metadata["approved_by"].clone());
            }
        },
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "pushed"));
    // The rule exists before the call runs, and the sibling runs under it.
    assert_eq!(approved, [json!("captain"), json!("rule")]);
    let rules = actor
        .permission_store
        .as_ref()
        .unwrap()
        .list(&RuleFilter::default())
        .unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(
        (rules[0].action.as_str(), rules[0].resource.as_str()),
        ("bash", "git push *")
    );
    assert_eq!(
        rules[0].scope,
        RuleScope::Project(actor.tool_context.workspace.clone())
    );
    assert_eq!(rules[0].origin, RuleOrigin::Always);
    let rows = decisions(&actor);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].outcome, rows[0].approved_by.as_deref()),
        (Outcome::Allowed, Some("captain"))
    );
    assert_eq!(
        rows[0].captain_answer.as_ref().unwrap()["decision"],
        "allow_always"
    );
    assert_eq!(
        (rows[1].outcome, rows[1].approved_by.as_deref()),
        (Outcome::Allowed, Some("rule"))
    );
}

#[tokio::test]
async fn reject_settles_every_held_sibling_and_the_model_reads_the_message() {
    let flows = Arc::new(FakeFlows::new("apply"));
    let mut actor = actor_with(Some(flows.clone()));
    let ws = actor.tool_context.workspace.clone();
    let server = script(
        &mut actor,
        vec![
            owned(&[
                (
                    "w1".into(),
                    "write".into(),
                    json!({"path": ws.join("a.txt"), "content": "A"}),
                ),
                (
                    "w2".into(),
                    "write".into(),
                    json!({"path": ws.join("b.txt"), "content": "B"}),
                ),
            ]),
            done("understood"),
        ],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("no rule asks");
    };
    let outcome = resolve_permission(
        &mut actor,
        request.id,
        PermissionDecision::Reject {
            message: Some("not in this repo".into()),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "understood"));
    assert!(!ws.join("a.txt").exists() && !ws.join("b.txt").exists());
    assert_eq!(flows.asked(), 1, "the sibling was never decided");
    for id in ["w1", "w2"] {
        let result: serde_json::Value =
            serde_json::from_str(&tool_result_of(&actor, id).unwrap()).unwrap();
        assert_eq!(result["status"], "denied", "{id}");
        assert_eq!(result["error"]["message"], "not in this repo", "{id}");
    }
    let requests = server.requests.lock().await;
    assert!(requests[1].contains("not in this repo"));
    let rows = decisions(&actor);
    assert_eq!(rows[0].outcome, Outcome::Denied);
}

/// A tool the registry knows nothing special about, like an MCP tool.
struct Echo;

impl crate::tools::Tool for Echo {
    fn name(&self) -> &str {
        "mcp__fake__echo"
    }

    fn schema(&self) -> serde_json::Value {
        json!({"description":"echo", "parameters":{"type":"object"}})
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        _: CancellationToken,
        _: &'a crate::tools::ToolContext,
    ) -> futures::future::BoxFuture<'a, Result<crate::tools::ToolResult, crate::error::ToolError>>
    {
        Box::pin(async move { Ok(crate::tools::ToolResult::ok(args.to_string())) })
    }
}

#[tokio::test]
async fn an_unknown_tool_asks_and_always_saves_a_global_rule() {
    let flows = Arc::new(FakeFlows::new("apply"));
    let mut actor = actor_with(Some(flows.clone()));
    actor.role = None;
    actor.registry.register(Box::new(Echo));
    let echo = |id: &str| {
        owned(&[(
            id.to_owned(),
            "mcp__fake__echo".to_owned(),
            json!({"text": "hi"}),
        )])
    };
    script(
        &mut actor,
        vec![echo("e1"), done("one"), echo("e2"), done("two")],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("no rule asks, with or without a role");
    };
    assert_eq!(
        request.always_label.as_deref(),
        Some("Always (global): mcp__fake__echo *")
    );
    let outcome = resolve_permission(
        &mut actor,
        request.id,
        PermissionDecision::AllowAlways,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Done(_)));
    assert!(tool_result_of(&actor, "e1").unwrap().contains("hi"));
    let rules = actor
        .permission_store
        .as_ref()
        .unwrap()
        .list(&RuleFilter::default())
        .unwrap();
    assert_eq!(rules[0].scope, RuleScope::Global);
    // The next turn runs it under the saved rule.
    let (outcome, approved) = run_observed(&mut actor).await;
    assert!(matches!(outcome, RunOutcome::Done(_)));
    assert_eq!(approved, [json!("rule")]);
}

#[tokio::test]
async fn an_opaque_command_asks_and_never_offers_always() {
    let flows = Arc::new(FakeFlows::new("apply"));
    let mut actor = actor_with(Some(flows.clone()));
    add_rule(&actor, "bash", "*", Effect::Allow);
    script(
        &mut actor,
        vec![owned(&[bash("o1", "if true; then echo x; fi")])],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("an opaque command matches no allow rule");
    };
    assert_eq!(request.always_label, None);
    assert!(request.always.is_empty());
    assert!(matches!(
        resolve_permission(
            &mut actor,
            request.id,
            PermissionDecision::AllowAlways,
            &mut |_| {}
        )
        .await,
        Err(ResolveError::Rejected(PermissionError::Stale))
    ));
    assert!(actor.has_pending(), "a refused answer changes nothing");
}

#[tokio::test]
async fn yolo_skips_rules_flows_and_floor_and_says_so() {
    let flows = Arc::new(FakeFlows::with_check("deny", ProtectedCheck::Failed));
    let mut actor = actor_with(Some(flows.clone()));
    actor.yolo = true;
    add_rule(&actor, "*", "*", Effect::Deny);
    let config = actor
        .permission_dirs
        .config
        .clone()
        .unwrap()
        .join("config.json");
    script(
        &mut actor,
        vec![
            owned(&[
                bash("b1", "echo yolo"),
                (
                    "w1".into(),
                    "write".into(),
                    json!({"path": config, "content": "{}"}),
                ),
            ]),
            done("ok"),
        ],
    )
    .await;
    let (outcome, approved) = run_observed(&mut actor).await;
    assert!(matches!(outcome, RunOutcome::Done(_)));
    assert_eq!(approved, [json!("yolo"), json!("yolo")]);
    assert_eq!((flows.asked(), flows.checked()), (0, 0));
    assert!(decisions(&actor)
        .iter()
        .all(|row| row.approved_by.as_deref() == Some("yolo")));
}

#[tokio::test]
async fn bypass_answers_a_flow_question_by_deciding_again() {
    let flows = Arc::new(FakeFlows::new("ask"));
    let mut actor = actor_with(Some(flows.clone()));
    script(
        &mut actor,
        vec![owned(&[bash("b1", "echo hi")]), done("ok")],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("the flow asks");
    };
    actor.bypass.lock().unwrap().enabled = true;
    assert!(pending_bypass_eligible(&actor));
    let mut approved = Vec::new();
    let outcome = resolve_permission_with_origin(
        &mut actor,
        request.id,
        PermissionDecision::AllowOnce,
        PermissionOrigin::Bypass,
        &mut |event| {
            if let AgentEvent::PostCall { result, .. } = event {
                approved.push(result.metadata["approved_by"].clone());
            }
        },
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Done(_)));
    assert_eq!(approved, [json!("bypass")]);
    let rows = decisions(&actor);
    assert_eq!(rows.len(), 1, "the pending row is settled, not duplicated");
    assert_eq!(
        (rows[0].outcome, rows[0].approved_by.as_deref()),
        (Outcome::Allowed, Some("bypass"))
    );
    // Bypass never answers "Always".
    let flows = Arc::new(FakeFlows::new("ask"));
    let mut actor = actor_with(Some(flows));
    script(&mut actor, vec![owned(&[bash("b1", "echo hi")])]).await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("the flow asks");
    };
    assert!(resolve_permission_with_origin(
        &mut actor,
        request.id,
        PermissionDecision::AllowAlways,
        PermissionOrigin::Bypass,
        &mut |_| {}
    )
    .await
    .is_err());
}

#[tokio::test]
async fn without_a_flow_runtime_the_rule_applies() {
    let mut actor = actor_with(None);
    add_rule(&actor, "bash", "echo *", Effect::Allow);
    script(
        &mut actor,
        vec![
            owned(&[bash("b1", "echo hi"), bash("b2", "touch made")]),
            done("ok"),
        ],
    )
    .await;
    let mut approved = Vec::new();
    actor.push_user("go");
    let outcome = run_agent(&mut actor, &mut |event| {
        if let AgentEvent::PostCall { result, .. } = event {
            approved.push(result.metadata["approved_by"].clone());
        }
    })
    .await
    .unwrap();
    assert_eq!(approved, [json!("rule")]);
    let RunOutcome::PermissionRequired(request) = outcome else {
        panic!("no rule asks");
    };
    assert_eq!(request.tool_call_id, "b2");
}

#[tokio::test]
async fn a_stop_cancels_the_pending_decision() {
    let flows = Arc::new(FakeFlows::new("ask"));
    let mut actor = actor_with(Some(flows));
    script(&mut actor, vec![owned(&[bash("b1", "echo hi")])]).await;
    assert!(matches!(
        run(&mut actor).await,
        RunOutcome::PermissionRequired(_)
    ));
    actor.interrupt_pending_permission();
    assert_eq!(decisions(&actor)[0].outcome, Outcome::Cancelled);
}

#[tokio::test]
async fn the_same_call_three_times_in_a_turn_asks_and_a_new_turn_resets() {
    let flows = Arc::new(FakeFlows::new("allow"));
    let mut actor = actor_with(Some(flows));
    let three = owned(&[
        bash("b1", "echo same"),
        bash("b2", "echo same"),
        bash("b3", "echo same"),
    ]);
    script(&mut actor, vec![three.clone(), three.clone()]).await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("the third identical call asks");
    };
    assert_eq!(request.tool_call_id, "b3");
    assert!(
        request.requires_captain,
        "bypass skips the loop check itself"
    );
    actor.interrupt_pending_permission();
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("again only at the third call of the new turn");
    };
    assert_eq!(request.tool_call_id, "b3");
}

#[cfg(unix)]
#[tokio::test]
async fn a_target_swapped_after_the_decision_is_decided_again_floor_included() {
    let flows = Arc::new(FakeFlows::with_check("allow", ProtectedCheck::Failed));
    let mut actor = actor_with(Some(flows));
    add_rule(&actor, "write", "*", Effect::Allow);
    let ws = actor.tool_context.workspace.clone();
    let config = actor
        .permission_dirs
        .config
        .clone()
        .unwrap()
        .join("config.json");
    std::fs::write(&config, "original").unwrap();
    let target = ws.join("notes.txt");
    std::fs::write(&target, "old").unwrap();
    super::tests::on_next_dispatch({
        let (target, config) = (target.clone(), config.clone());
        move || {
            std::fs::remove_file(&target).unwrap();
            std::os::unix::fs::symlink(&config, &target).unwrap();
        }
    });
    script(
        &mut actor,
        vec![owned(&[(
            "w1".into(),
            "write".into(),
            json!({"path": target, "content": "new"}),
        )])],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("the moved target now names the configuration");
    };
    assert!(request.requires_captain);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "original");
}

/// Answers every instinct question yes and counts the protected ones.
struct CountingInstinct {
    protected: AtomicUsize,
}

impl instinct_flow::DecisionClient for CountingInstinct {
    fn capabilities(&self) -> instinct_flow::Capabilities {
        instinct_flow::Capabilities { images: false }
    }
    fn identity(&self) -> instinct_flow::ClientIdentity {
        instinct_flow::ClientIdentity {
            model: "fake/counting".into(),
            variant: None,
        }
    }
    fn decide<'a>(
        &'a self,
        request: instinct_flow::protocol::DecisionRequest<'a>,
        _: &'a CancellationToken,
        _: std::time::Instant,
    ) -> futures::future::BoxFuture<
        'a,
        Result<instinct_flow::protocol::DecisionResponse, instinct_flow::DecisionError>,
    > {
        if request.questions.contains_key("protected_requested") {
            self.protected.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async move {
            Ok(instinct_flow::protocol::DecisionResponse {
                answers: request
                    .questions
                    .keys()
                    .map(|id| {
                        (
                            id.clone(),
                            instinct_flow::protocol::Answer::Noul { noul: 1.0 },
                        )
                    })
                    .collect(),
                usage: None,
            })
        })
    }
}

/// The session's adapter, reduced to the two permission questions.
struct RuntimeFlows(Arc<crate::flows::runtime::FlowRuntime>);

impl std::fmt::Debug for RuntimeFlows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RuntimeFlows")
    }
}

impl ToolCallFlows for RuntimeFlows {
    fn permission_pre_call<'a>(
        &'a self,
        ctx: PermissionCallContext,
        rule: RuleResult,
        cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, PermissionVerdict> {
        Box::pin(self.0.permission_pre_call(ctx, rule, cancel.clone()))
    }
    fn protected_check<'a>(
        &'a self,
        ctx: PermissionCallContext,
        cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, ProtectedCheck> {
        Box::pin(self.0.permission_protected_check(ctx, cancel.clone()))
    }
}

#[tokio::test]
async fn a_protected_write_asks_the_protected_question_exactly_once() {
    let mut actor = actor_with(None);
    let client = Arc::new(CountingInstinct {
        protected: AtomicUsize::new(0),
    });
    let ws = actor.tool_context.workspace.clone();
    let store = Arc::new(crate::flows::store::FlowStore::new(
        ws.clone(),
        actor.permission_dirs.config.clone().unwrap(),
    ));
    let runtime = Arc::new(crate::flows::runtime::FlowRuntime::new(
        store,
        client.clone(),
        Arc::new(crate::flows::runtime::NoEffects),
    ));
    actor.tool_flows = Some(Arc::new(RuntimeFlows(runtime)));
    let config = actor
        .permission_dirs
        .config
        .clone()
        .unwrap()
        .join("config.json");
    script(
        &mut actor,
        vec![
            owned(&[(
                "w1".into(),
                "write".into(),
                json!({"path": config, "content": "{}"}),
            )]),
            done("ok"),
        ],
    )
    .await;
    actor.push_user("set my config.json to an empty object");
    let mut approved = Vec::new();
    let outcome = run_agent(&mut actor, &mut |event| {
        if let AgentEvent::PostCall { result, .. } = event {
            approved.push(result.metadata["approved_by"].clone());
        }
    })
    .await
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Done(_)), "{outcome:?}");
    assert_eq!(approved, [json!("flow")]);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "{}");
    assert_eq!(client.protected.load(Ordering::SeqCst), 1);
}

/// An actor whose workspace holds the skill `helper`, not yet validated.
fn actor_with_skill() -> AgentActor {
    actor_with_skill_and_flow("allow")
}

fn actor_with_skill_and_flow(mode: &'static str) -> AgentActor {
    let mut actor = actor_with(Some(Arc::new(FakeFlows::new(mode))));
    let ws = actor.tool_context.workspace.clone();
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    let dir = ws.join(".agents/skills/helper");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: helper\ndescription: Helps.\n---\nHELPER BODY\n",
    )
    .unwrap();
    let mut skills =
        crate::agent::skills::Skills::load(&crate::agent::instructions::RulesRoots::default(), &ws);
    skills.validation = Arc::new(crate::agent::skills::validation::SkillValidation {
        data_dir: actor.permission_dirs.data.clone(),
        ..Default::default()
    });
    actor.set_skills(skills);
    actor
}

#[tokio::test]
async fn rejecting_a_skill_validation_settles_every_held_sibling() {
    let mut actor = actor_with_skill();
    let ws = actor.tool_context.workspace.clone();
    script(
        &mut actor,
        vec![
            owned(&[
                ("s1".into(), "skill".into(), json!({"name": "helper"})),
                (
                    "w1".into(),
                    "write".into(),
                    json!({"path": ws.join("a.txt"), "content": "A"}),
                ),
            ]),
            done("understood"),
        ],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("an unvalidated skill asks");
    };
    assert_eq!(request.tool, "skill");
    let outcome = resolve_permission(
        &mut actor,
        request.id,
        PermissionDecision::Reject {
            message: Some("not that skill".into()),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Done(_)), "{outcome:?}");
    assert!(!ws.join("a.txt").exists(), "the held write never ran");
    for id in ["s1", "w1"] {
        let text = tool_result_of(&actor, id).unwrap();
        assert!(text.contains("not that skill"), "{id}: {text}");
    }
}

#[tokio::test]
async fn a_cancelled_skill_validation_settles_every_held_sibling() {
    let mut actor = actor_with_skill();
    let ws = actor.tool_context.workspace.clone();
    script(
        &mut actor,
        vec![owned(&[
            ("s1".into(), "skill".into(), json!({"name": "helper"})),
            (
                "w1".into(),
                "write".into(),
                json!({"path": ws.join("a.txt"), "content": "A"}),
            ),
        ])],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("an unvalidated skill asks");
    };
    actor.cancel.cancel();
    let outcome = resolve_permission(
        &mut actor,
        request.id,
        PermissionDecision::AllowOnce,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome, RunOutcome::Interrupted);
    assert!(tool_result_of(&actor, "s1").is_some());
    assert!(
        tool_result_of(&actor, "w1").is_some(),
        "the sibling is settled"
    );
}

#[tokio::test]
async fn the_decision_history_keeps_no_secret_and_stays_bounded() {
    const TOKEN: &str = "ghp_RECOGNISABLEsecretTOKEN0123456789";
    let mut actor = actor_with(Some(Arc::new(FakeFlows::new("apply"))));
    // A long line of many commands, one of them carrying a token near the end.
    let mut line = format!("export T={TOKEN} && ");
    line += &(0..400)
        .map(|n| format!("echo filler-{n}-{}", "x".repeat(40)))
        .collect::<Vec<_>>()
        .join(" && ");
    line.push_str(&format!(
        " && curl -H 'Authorization: token {TOKEN}' https://api"
    ));
    script(&mut actor, vec![owned(&[bash("b1", &line)]), done("ok")]).await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("no rule asks");
    };
    resolve_permission(
        &mut actor,
        request.id,
        PermissionDecision::Reject {
            message: Some(format!("never send {TOKEN} {}", "y".repeat(8000))),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    let db = actor.permission_store.as_ref().unwrap().db();
    let mut statement = db
        .prepare(
            "SELECT call_snapshot, command_matches, captain_answer, flow_verdict
             FROM permission_decisions",
        )
        .unwrap();
    let rows: Vec<Vec<Option<String>>> = statement
        .query_map([], |row| Ok((0..4).map(|i| row.get(i).unwrap()).collect()))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 1);
    for field in rows[0].iter().flatten() {
        assert!(!field.contains(TOKEN), "{}", &field[..field.len().min(200)]);
        assert!(field.len() <= 8 * 1024, "{} bytes", field.len());
    }
    assert!(rows[0][0].as_deref().unwrap().contains("<redacted>"));
    assert!(rows[0][1].as_deref().unwrap().contains("<redacted>"));
    assert!(rows[0][2]
        .as_deref()
        .unwrap()
        .contains("never send <redacted>"));
}

/// A validated `helper` skill under an `ask` rule: the model's call waits on a
/// rule question (not a validation). Returns the request.
async fn waiting_on_a_skill_rule(
    actor: &mut AgentActor,
) -> crate::agent::permissions::PermissionRequest {
    let snapshot = actor.skills.snapshot("helper").unwrap();
    actor.skills.validation.approve(&snapshot, true).unwrap();
    add_rule(actor, "skill", "helper", Effect::Ask);
    script(
        actor,
        vec![
            owned(&[("s1".into(), "skill".into(), json!({"name": "helper"}))]),
            done("ok"),
        ],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(actor).await else {
        panic!("the ask rule asks");
    };
    assert!(request.skill_rule);
    request
}

fn skill_rows(actor: &AgentActor) -> Vec<crate::permission::decisions::Decision> {
    decisions(actor)
        .into_iter()
        .filter(|row| row.tool == "skill")
        .collect()
}

#[tokio::test]
async fn bypass_answering_a_skill_rule_question_settles_its_pending_row() {
    let mut actor = actor_with_skill_and_flow("apply");
    let request = waiting_on_a_skill_rule(&mut actor).await;
    actor.bypass.lock().unwrap().enabled = true;
    assert!(pending_bypass_eligible(&actor));
    let outcome = resolve_permission_with_origin(
        &mut actor,
        request.id,
        PermissionDecision::AllowOnce,
        PermissionOrigin::Bypass,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Done(_)), "{outcome:?}");
    assert!(tool_result_of(&actor, "s1")
        .unwrap()
        .contains("HELPER BODY"));
    let rows = skill_rows(&actor);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].request_id, Some(request.id.to_string()));
    assert_eq!(
        (rows[0].outcome, rows[0].approved_by.as_deref()),
        (Outcome::Allowed, Some("bypass"))
    );
}

#[tokio::test]
async fn a_deny_rule_written_while_a_skill_question_waits_settles_it_as_denied() {
    let mut actor = actor_with_skill_and_flow("apply");
    let request = waiting_on_a_skill_rule(&mut actor).await;
    add_rule(&actor, "skill", "helper", Effect::Deny);
    actor.bypass.lock().unwrap().enabled = true;
    resolve_permission_with_origin(
        &mut actor,
        request.id,
        PermissionDecision::AllowOnce,
        PermissionOrigin::Bypass,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(!tool_result_of(&actor, "s1")
        .unwrap()
        .contains("HELPER BODY"));
    let rows = skill_rows(&actor);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].outcome, Outcome::Denied);
}

#[tokio::test]
async fn unavailable_rules_disable_bypass_always_and_history_but_allow_flows() {
    for (mode, bypass, yolo, runs) in [
        ("apply", false, false, false),
        ("apply", true, false, false),
        ("allow", true, false, true),
        ("deny", false, true, true),
    ] {
        let mut actor = actor_with(Some(Arc::new(FakeFlows::new(mode))));
        let original = actor.permission_store.take().unwrap();
        actor.permissions_unavailable = Some("database unavailable".into());
        actor.yolo = yolo;
        actor.bypass.lock().unwrap().enabled = bypass;
        script(
            &mut actor,
            vec![owned(&[bash("b1", "echo hi")]), done("ok")],
        )
        .await;
        let outcome = run(&mut actor).await;
        if runs {
            assert!(
                matches!(outcome, RunOutcome::Done(_)),
                "{mode}: {outcome:?}"
            );
        } else {
            let RunOutcome::PermissionRequired(request) = outcome else {
                panic!("must ask")
            };
            assert!(request.requires_captain);
            assert!(request.always.is_empty() && request.always_label.is_none());
            assert!(!pending_bypass_eligible(&actor));
            assert!(resolve_permission(
                &mut actor,
                request.id,
                PermissionDecision::AllowAlways,
                &mut |_| {}
            )
            .await
            .is_err());
            assert!(resolve_permission_with_origin(
                &mut actor,
                request.id,
                PermissionDecision::AllowOnce,
                PermissionOrigin::Bypass,
                &mut |_| {}
            )
            .await
            .is_err());
        }
        assert!(original
            .list_decisions(&DecisionFilter::default(), 10)
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn unavailable_rules_keep_the_fixed_floor() {
    for tool in ["read", "write"] {
        let mut actor = actor_with(Some(Arc::new(FakeFlows::new("allow"))));
        actor.permission_store = None;
        actor.bypass.lock().unwrap().enabled = true;
        let path = if tool == "read" {
            actor.tool_context.workspace.join(".env")
        } else {
            actor
                .permission_dirs
                .config
                .as_ref()
                .unwrap()
                .join("config.json")
        };
        script(
            &mut actor,
            vec![owned(&[(
                "c1".into(),
                tool.into(),
                json!({"path":path,"content":"{}"}),
            )])],
        )
        .await;
        let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
            panic!("floor asks")
        };
        assert!(request.requires_captain && request.always_label.is_none());
    }
}

struct DelegationClient;
impl instinct_flow::DecisionClient for DelegationClient {
    fn capabilities(&self) -> instinct_flow::Capabilities {
        instinct_flow::Capabilities { images: false }
    }
    fn identity(&self) -> instinct_flow::ClientIdentity {
        instinct_flow::ClientIdentity {
            model: "fake/delegation".into(),
            variant: None,
        }
    }
    fn decide<'a>(
        &'a self,
        request: instinct_flow::protocol::DecisionRequest<'a>,
        _: &'a CancellationToken,
        _: std::time::Instant,
    ) -> futures::future::BoxFuture<
        'a,
        Result<instinct_flow::protocol::DecisionResponse, instinct_flow::DecisionError>,
    > {
        Box::pin(async move {
            if request.questions.contains_key("protected_requested") {
                assert!(
                    !request.state.contains("DELEGATED_ACTION"),
                    "{}",
                    request.state
                );
            } else {
                assert!(
                    request.state.contains("DELEGATED_ACTION"),
                    "{}",
                    request.state
                );
                assert!(request.state.contains("Delegation from the parent agent"));
            }
            Ok(instinct_flow::protocol::DecisionResponse {
                answers: request
                    .questions
                    .keys()
                    .map(|id| {
                        (
                            id.clone(),
                            instinct_flow::protocol::Answer::Noul {
                                noul: if id == "protected_requested" {
                                    0.0
                                } else {
                                    1.0
                                },
                            },
                        )
                    })
                    .collect(),
                usage: None,
            })
        })
    }
}

#[tokio::test]
async fn delegation_allows_an_ordinary_action_but_never_a_protected_write() {
    let mut actor = actor_with(None);
    let runtime = Arc::new(crate::flows::runtime::FlowRuntime::new(
        Arc::new(crate::flows::store::FlowStore::new(
            actor.tool_context.workspace.clone(),
            actor.permission_dirs.config.clone().unwrap(),
        )),
        Arc::new(DelegationClient),
        Arc::new(crate::flows::runtime::NoEffects),
    ));
    actor.tool_flows = Some(Arc::new(RuntimeFlows(runtime)));
    actor.push_user_with_provenance(
        "DELEGATED_ACTION run echo",
        crate::provider::openai::UserProvenance::Parent,
    );
    script(
        &mut actor,
        vec![owned(&[bash("b1", "echo hi")]), done("ok")],
    )
    .await;
    assert!(matches!(run(&mut actor).await, RunOutcome::Done(_)));
    let path = actor
        .permission_dirs
        .config
        .as_ref()
        .unwrap()
        .join("config.json");
    script(
        &mut actor,
        vec![owned(&[(
            "w1".into(),
            "write".into(),
            json!({"path":path,"content":"{}"}),
        )])],
    )
    .await;
    assert!(matches!(
        run(&mut actor).await,
        RunOutcome::PermissionRequired(_)
    ));
}

#[tokio::test]
async fn unavailable_rules_still_stop_identical_calls_under_bypass() {
    let mut actor = actor_with(Some(Arc::new(FakeFlows::new("allow"))));
    actor.permission_store = None;
    actor.bypass.lock().unwrap().enabled = true;
    script(
        &mut actor,
        vec![
            owned(&[
                bash("b1", "echo same"),
                bash("b2", "echo same"),
                bash("b3", "echo same"),
            ]),
            done("ok"),
        ],
    )
    .await;
    let RunOutcome::PermissionRequired(request) = run(&mut actor).await else {
        panic!("the third call must ask")
    };
    assert_eq!(request.tool_call_id, "b3");
    assert!(request.requires_captain && request.always_label.is_none());
    assert!(request.reason.contains("3 times"));
}
