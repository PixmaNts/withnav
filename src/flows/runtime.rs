//! Per-workspace evaluation; asynchronous effects never change an admitted event.
use super::{catalogue::catalogue, store::FlowStore};
pub use crate::agent::flow_hook::{
    PermissionCallContext, PermissionFlowAnswer, PermissionVerdict, ProtectedCheck, RuleResult,
    RunScores,
};
use instinct_flow::*;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

impl From<VerdictLike> for crate::agent::flow_hook::FlowVerdict {
    fn from(value: VerdictLike) -> Self {
        match value {
            VerdictLike::Allow => Self::Allow,
            VerdictLike::Deny { note } => Self::Deny { note },
            VerdictLike::AskCaptain { note, flow, run_id } => {
                Self::AskCaptain { note, flow, run_id }
            }
        }
    }
}

/// Keep complete Captain messages, newest last, including JSON framing in the budget.
fn bounded_captain_messages(messages: Vec<String>) -> InputValue {
    let mut bytes = 2usize;
    let mut start = messages.len();
    for (index, message) in messages.iter().enumerate().rev() {
        let size = serde_json::to_vec(message).map_or(usize::MAX, |v| v.len());
        let next = bytes
            .saturating_add(size)
            .saturating_add(usize::from(start < messages.len()));
        if next > 16 * 1024 {
            break;
        }
        bytes = next;
        start = index;
    }
    InputValue {
        value: Some(json!(&messages[start..])),
        cut: start != 0,
        redacted: false,
    }
}

/// The run id and the score of every node of `outcome` that asked Instinct.
fn run_scores(outcome: &RunOutcome) -> RunScores {
    RunScores {
        run_id: outcome.trace.run_id.clone(),
        scores: outcome
            .trace
            .nodes
            .iter()
            .filter_map(|node| match &node.answer {
                Some(instinct_flow::protocol::Answer::Noul { noul }) => {
                    Some((node.id.clone(), *noul))
                }
                _ => None,
            })
            .collect(),
    }
}

/// Whether this action is the compiled-in check's answer that the Captain asked
/// for the protected change: its allow node, reached through a "yes" answer.
fn grants_protected(outcome: &RunOutcome, action: &ProposedAction) -> bool {
    matches!(outcome.result, RunResult::Success { .. })
        && action.node_id.as_deref() == Some("protected_allow")
        && action.action == "allow"
        && action.params.get("protected_ok") == Some(&json!(true))
        && outcome
            .trace
            .nodes
            .iter()
            .any(|n| n.id == "protected_requested" && n.port.as_deref() == Some("yes"))
}

fn resolve_permissions(
    outcomes: &[RunOutcome],
    rule: RuleResult,
    protected: bool,
    protected_runs: &std::collections::BTreeSet<String>,
    host_passed: bool,
) -> (VerdictLike, Vec<PermissionFlowAnswer>) {
    let rule_action = if rule == RuleResult::Allow {
        "allow"
    } else {
        "ask_captain"
    };
    let mut answers = vec![];
    let mut candidates = vec![];
    let mut protected_ok = host_passed;
    for outcome in outcomes {
        let actions = match &outcome.result {
            RunResult::Success { actions } => actions,
            RunResult::Failure {
                fallback_actions, ..
            } => fallback_actions,
            RunResult::Skipped => continue,
        };
        let verdicts: Vec<_> = actions
            .iter()
            .filter(|a| {
                matches!(
                    a.action.as_str(),
                    "allow" | "ask_captain" | "deny" | "apply_rule"
                )
            })
            .collect();
        if verdicts.is_empty() {
            answers.push(PermissionFlowAnswer {
                flow: outcome.trace.flow_name.clone(),
                run_id: outcome.trace.run_id.clone(),
                action: rule_action.into(),
                used_rule: true,
            });
            candidates.push((
                rule_action,
                String::new(),
                outcome.trace.flow_name.clone(),
                outcome.trace.run_id.clone(),
            ));
        }
        for action in verdicts {
            protected_ok |=
                protected_runs.contains(&outcome.trace.run_id) && grants_protected(outcome, action);
            let used_rule = action.action == "apply_rule";
            let resolved = if used_rule {
                rule_action
            } else {
                action.action.as_str()
            };
            answers.push(PermissionFlowAnswer {
                flow: outcome.trace.flow_name.clone(),
                run_id: outcome.trace.run_id.clone(),
                action: resolved.into(),
                used_rule,
            });
            candidates.push((
                resolved,
                action
                    .params
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                outcome.trace.flow_name.clone(),
                outcome.trace.run_id.clone(),
            ));
        }
    }
    if candidates.is_empty() {
        candidates.push((rule_action, String::new(), String::new(), String::new()));
    }
    if protected && !protected_ok {
        candidates.push(("ask_captain", "Only the Captain can request changes to WithNav's configuration, permission rules or database.".into(), String::new(), String::new()));
    }
    let priority = |action: &str| match action {
        "deny" => 2,
        "ask_captain" => 1,
        _ => 0,
    };
    let strictest = candidates.iter().map(|c| priority(c.0)).max().unwrap_or(0);
    candidates.retain(|c| priority(c.0) == strictest);
    candidates.sort_by(|a, b| a.2.cmp(&b.2));
    let note = candidates
        .iter()
        .map(|c| c.1.as_str())
        .filter(|n| !n.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let verdict = match candidates[0].0 {
        "deny" => VerdictLike::Deny { note },
        "ask_captain" => VerdictLike::AskCaptain {
            note,
            flow: candidates[0].2.clone(),
            run_id: candidates[0].3.clone(),
        },
        _ => VerdictLike::Allow,
    };
    (verdict, answers)
}

/// Never load protected authority from disk: use the binary's question and rendering.
fn compiled_protected_check() -> Option<CompiledDefinition> {
    compile_protected_check(false)
}

/// `force` compiles the check although the built-in flow is disabled, so tests
/// can exercise the enabled path; production callers never force.
fn compile_protected_check(force: bool) -> Option<CompiledDefinition> {
    let (mut definition, templates) = super::builtin::all_definitions()
        .into_iter()
        .find(|(d, _)| d.flow.name == "permission-default")
        .expect("embedded permission definition");
    if !definition.flow.enabled && !force {
        return None;
    }
    definition.flow.enabled = true;
    definition.flow.start = vec!["protected_requested".into()];
    definition.node.retain(|n| {
        matches!(
            n.id.as_str(),
            "protected_requested" | "protected_allow" | "protected_ask"
        )
    });
    Some(
        definition
            .validate(&catalogue(), &Capabilities { images: false }, &templates)
            .expect("embedded protected check validates"),
    )
}

#[derive(Debug, Clone)]
pub struct ToolCallContextLike {
    pub call_id: String,
    pub tool: String,
    pub args: Value,
    pub agent: String,
    pub workspace: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::flow_hook::{FlowVerdict, ToolCallContext};
    use crate::flows::store::Location;

    /// A disabled global copy of the built-in permission flow shadows it, so a
    /// test sees only the flows it saves itself.
    fn without_builtin(store: &FlowStore) {
        let (mut definition, templates) = super::super::builtin::all_definitions()
            .into_iter()
            .find(|(d, _)| d.flow.name == "permission-default")
            .unwrap();
        definition.flow.enabled = false;
        store
            .save(
                "permission-default",
                Location::Global,
                None,
                super::super::store::Content::Graph {
                    definition: Box::new(definition),
                    templates,
                },
            )
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
    }

    fn permission_call(workspace: PathBuf, protected: bool) -> PermissionCallContext {
        PermissionCallContext {
            call: ToolCallContext {
                call_id: "call".into(),
                tool: "write".into(),
                args: json!({}),
                agent: Some("build".into()),
                workspace,
            },
            rule: None,
            commands: None,
            inside_project: Some(true),
            location: Some("project"),
            protected,
            skill: None,
            captain_messages: vec!["Change the configuration".into()],
            captain_messages_cut: false,
            delegation: vec![],
            delegation_cut: false,
            recent: vec![],
            recent_cut: false,
            protected_check: None,
        }
    }

    #[tokio::test]
    async fn checked_protected_call_keeps_its_protected_identity() {
        let mut call = permission_call(PathBuf::from("/project"), true);
        call.protected_check = Some(ProtectedCheck::Passed);
        let (event, _, _) =
            FlowRuntime::permission_event(call, RuleResult::None, ContextSnapshot::default());
        assert_eq!(event.payload["protected"], json!(true));
        assert_eq!(event.payload["protected_checked"], json!(true));
        assert_eq!(event.payload["location"], json!("project"));
    }

    #[tokio::test]
    async fn permission_aggregation_is_separate_from_legacy_hook() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        assert!(store
            .list()
            .unwrap()
            .iter()
            .any(|f| f.name == "permission-default" && f.enabled == Some(true)));
        without_builtin(&store);
        let runtime = FlowRuntime::new(store.clone(), Arc::new(Fake), Arc::new(NoEffects));
        assert_eq!(
            runtime
                .tool_pre_call(
                    ToolCallContextLike {
                        call_id: "legacy".into(),
                        tool: "write".into(),
                        args: json!({}),
                        agent: "build".into(),
                        workspace: dir.path().into()
                    },
                    CancellationToken::new()
                )
                .await,
            VerdictLike::Allow
        );
        for (rule, allowed) in [
            (RuleResult::Allow, true),
            (RuleResult::Ask, false),
            (RuleResult::None, false),
        ] {
            let result = runtime
                .permission_pre_call(
                    permission_call(dir.path().into(), false),
                    rule,
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(matches!(result.verdict, FlowVerdict::Allow), allowed);
            assert!(result.flows.iter().all(|f| f.used_rule));
        }
        for (name, toml) in [("allow", "[flow]\nversion=1\nname='allow'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='no_objection'\n[node.params]\n".into()), ("failed", "[flow]\nversion=1\nname='failed'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='noul'\ninstructions='test'\nthreshold=0.5\n[node.inputs]\nx='session.agent'\n".into())] {
            store.save(name, Location::Global, None, super::super::store::Content::Toml { toml, templates: BTreeMap::new() }).unwrap();
        }
        store.validate_test_documents();
        store.reload().unwrap();
        assert!(!matches!(
            runtime
                .permission_pre_call(
                    permission_call(dir.path().into(), true),
                    RuleResult::Allow,
                    CancellationToken::new()
                )
                .await
                .verdict,
            FlowVerdict::Allow
        ));
        for (rule, allowed) in [(RuleResult::Ask, false), (RuleResult::Allow, true)] {
            let result = runtime
                .permission_pre_call(
                    permission_call(dir.path().into(), false),
                    rule,
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(matches!(result.verdict, FlowVerdict::Allow), allowed);
            assert_eq!(result.flows.len(), 2, "{:?}", result.flows);
            assert!(result
                .flows
                .iter()
                .any(|f| f.flow == "failed" && f.used_rule));
        }
        assert!(matches!(
            runtime
                .tool_pre_call(
                    ToolCallContextLike {
                        call_id: "call".into(),
                        tool: "write".into(),
                        args: json!({}),
                        agent: "build".into(),
                        workspace: dir.path().into()
                    },
                    CancellationToken::new()
                )
                .await,
            VerdictLike::AskCaptain { .. }
        ));
        store
            .save(
                "deny",
                Location::Global,
                None,
                super::super::store::Content::Toml {
                    toml: text("deny", "deny"),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
        assert!(matches!(
            runtime
                .permission_pre_call(
                    permission_call(dir.path().into(), false),
                    RuleResult::Allow,
                    CancellationToken::new()
                )
                .await
                .verdict,
            FlowVerdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn disabled_embedded_permission_check_never_requests_a_decision() {
        struct NoDecision;
        impl DecisionClient for NoDecision {
            fn capabilities(&self) -> Capabilities {
                Capabilities { images: false }
            }
            fn identity(&self) -> ClientIdentity {
                ClientIdentity {
                    model: "fake/affirmative".into(),
                    variant: None,
                }
            }
            fn decide<'a>(
                &'a self,
                _: instinct_flow::protocol::DecisionRequest<'a>,
                _: &'a CancellationToken,
                _: Instant,
            ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>>
            {
                panic!("disabled built-in must not request even an affirmative decision")
            }
        }
        // The shipped flow is enabled; a build that disables it reaches this path.
        assert!(compiled_protected_check().is_some());
        assert!(compile_protected_check(false).is_some());
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        let runtime = FlowRuntime::new(store, Arc::new(NoDecision), Arc::new(NoEffects));
        assert_eq!(
            runtime
                .protected_check_with(
                    None,
                    permission_call(dir.path().into(), true),
                    CancellationToken::new()
                )
                .await,
            ProtectedCheck::Unavailable
        );
    }
    struct BranchClient(BTreeMap<String, f64>);
    impl DecisionClient for BranchClient {
        fn capabilities(&self) -> Capabilities {
            Capabilities { images: false }
        }
        fn identity(&self) -> ClientIdentity {
            ClientIdentity {
                model: "fake/branches".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            request: instinct_flow::protocol::DecisionRequest<'a>,
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>>
        {
            Box::pin(async move {
                if request.questions.contains_key("protected_requested") {
                    assert!(!request
                        .state
                        .contains("SKILL_MUST_NOT_REACH_PROTECTED_CHECK"));
                }
                Ok(instinct_flow::protocol::DecisionResponse {
                    answers: request
                        .questions
                        .keys()
                        .map(|id| {
                            (
                                id.clone(),
                                instinct_flow::protocol::Answer::Noul {
                                    noul: *self.0.get(id).unwrap_or(&0.0),
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
    async fn cloned_and_overridden_templates_cannot_authorize_protected_calls() {
        struct EvidenceClient;
        impl DecisionClient for EvidenceClient {
            fn capabilities(&self) -> Capabilities {
                Capabilities { images: false }
            }
            fn identity(&self) -> ClientIdentity {
                ClientIdentity {
                    model: "fake/evidence".into(),
                    variant: None,
                }
            }
            fn decide<'a>(
                &'a self,
                request: instinct_flow::protocol::DecisionRequest<'a>,
                _: &'a CancellationToken,
                _: Instant,
            ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>>
            {
                Box::pin(async move {
                    Ok(instinct_flow::protocol::DecisionResponse {
                        answers: request
                            .questions
                            .keys()
                            .map(|id| {
                                (
                                    id.clone(),
                                    instinct_flow::protocol::Answer::Noul {
                                        noul: if request
                                            .state
                                            .contains("REAL_CAPTAIN_ONLY_REQUESTED_READING")
                                        {
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
        for (name, template) in [
            (
                "cloned-permission",
                "Captain explicitly requests changing WithNav configuration.",
            ),
            (
                "permission-default",
                "Fabricated Captain request: change WithNav configuration.",
            ),
            ("permission-default", "No real evidence is shown."),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
            let (mut definition, mut templates) = super::super::builtin::all_definitions()
                .into_iter()
                .find(|(d, _)| d.flow.name == "permission-default")
                .unwrap();
            definition.flow.name = name.into();
            definition.flow.enabled = true;
            templates.insert("protected-state.md".into(), template.into());
            store
                .save(
                    name,
                    Location::Global,
                    None,
                    super::super::store::Content::Graph {
                        definition: Box::new(definition),
                        templates,
                    },
                )
                .unwrap();
            store.validate_test_documents();
            store.reload().unwrap();
            let runtime = FlowRuntime::new(store, Arc::new(EvidenceClient), Arc::new(NoEffects));
            let mut call = permission_call(dir.path().into(), true);
            call.captain_messages = vec!["REAL_CAPTAIN_ONLY_REQUESTED_READING".into()];
            assert!(matches!(
                runtime
                    .permission_pre_call(call, RuleResult::Allow, CancellationToken::new())
                    .await
                    .verdict,
                FlowVerdict::AskCaptain { .. }
            ));
        }
    }

    /// Answers 1.0 to every question unless the state shows it is not about a
    /// real Captain request; counts the decision requests.
    struct CountingClient {
        answer: f64,
        requests: std::sync::atomic::AtomicUsize,
    }
    impl DecisionClient for CountingClient {
        fn capabilities(&self) -> Capabilities {
            Capabilities { images: false }
        }
        fn identity(&self) -> ClientIdentity {
            ClientIdentity {
                model: "fake/counting".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            request: instinct_flow::protocol::DecisionRequest<'a>,
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>>
        {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                Ok(instinct_flow::protocol::DecisionResponse {
                    answers: request
                        .questions
                        .keys()
                        .map(|id| {
                            (
                                id.clone(),
                                instinct_flow::protocol::Answer::Noul { noul: self.answer },
                            )
                        })
                        .collect(),
                    usage: None,
                })
            })
        }
    }

    fn counting(answer: f64) -> Arc<CountingClient> {
        Arc::new(CountingClient {
            answer,
            requests: Default::default(),
        })
    }

    fn runtime_with(client: Arc<CountingClient>) -> (tempfile::TempDir, FlowRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        (dir, FlowRuntime::new(store, client, Arc::new(NoEffects)))
    }

    #[tokio::test]
    async fn protected_check_runs_only_the_compiled_node() {
        let cancel = CancellationToken::new;
        // A disabled built-in flow: Unavailable, no decision request.
        let client = counting(1.0);
        let (dir, runtime) = runtime_with(client.clone());
        let call = permission_call(dir.path().into(), true);
        assert_eq!(
            runtime.protected_check_with(None, call, cancel()).await,
            ProtectedCheck::Unavailable
        );
        assert_eq!(client.requests.load(Ordering::SeqCst), 0);
        // The shipped flow is enabled: the compiled node answers.
        let call = permission_call(dir.path().into(), true);
        assert_eq!(
            runtime.permission_protected_check(call, cancel()).await,
            ProtectedCheck::Passed
        );
        assert_eq!(client.requests.load(Ordering::SeqCst), 1);
        // Enabled: the compiled node answers from the Captain's messages.
        for (answer, expected) in [(1.0, ProtectedCheck::Passed), (0.0, ProtectedCheck::Failed)] {
            let client = counting(answer);
            let (dir, runtime) = runtime_with(client.clone());
            let call = permission_call(dir.path().into(), true);
            let result = runtime
                .protected_check_with(compile_protected_check(true), call, cancel())
                .await;
            assert_eq!(result, expected);
            assert_eq!(client.requests.load(Ordering::SeqCst), 1);
        }
        // Missing or cut Captain evidence establishes nothing and asks nothing.
        let client = counting(1.0);
        let (dir, runtime) = runtime_with(client.clone());
        let mut call = permission_call(dir.path().into(), true);
        call.captain_messages_cut = true;
        assert_eq!(
            runtime
                .protected_check_with(compile_protected_check(true), call, cancel())
                .await,
            ProtectedCheck::Unavailable
        );
        assert_eq!(client.requests.load(Ordering::SeqCst), 0);
        // A failing provider is Unavailable, never Passed.
        let (dir, runtime) = {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
            (
                dir,
                FlowRuntime::new(store, Arc::new(Fake), Arc::new(NoEffects)),
            )
        };
        let call = permission_call(dir.path().into(), true);
        assert_eq!(
            runtime
                .protected_check_with(compile_protected_check(true), call, cancel())
                .await,
            ProtectedCheck::Unavailable
        );
    }

    #[tokio::test]
    async fn a_flow_named_like_the_builtin_cannot_pass_the_protected_check() {
        // An enabled clone on disk says yes to everything; the check ignores it.
        let client = counting(1.0);
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        let (mut definition, templates) = super::super::builtin::all_definitions()
            .into_iter()
            .find(|(d, _)| d.flow.name == "permission-default")
            .unwrap();
        definition.flow.enabled = true;
        store
            .save(
                "permission-default",
                Location::Global,
                None,
                super::super::store::Content::Graph {
                    definition: Box::new(definition),
                    templates,
                },
            )
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
        let runtime = FlowRuntime::new(store, client.clone(), Arc::new(NoEffects));
        // Without the compiled node the clone supplies no authority at all.
        let call = permission_call(dir.path().into(), true);
        assert_eq!(
            runtime
                .protected_check_with(None, call, CancellationToken::new())
                .await,
            ProtectedCheck::Unavailable
        );
        assert_eq!(client.requests.load(Ordering::SeqCst), 0);
    }

    /// The adapter the agent loop will use: the decision path's two questions
    /// answered by the flow runtime.
    struct RuntimeFlows<'a> {
        runtime: &'a FlowRuntime,
        workspace: PathBuf,
        compiled: bool,
    }
    impl crate::permission::decide::PermissionFlows for RuntimeFlows<'_> {
        fn flow<'a>(
            &'a self,
            query: &'a crate::permission::decide::FlowQuery<'a>,
        ) -> BoxFuture<'a, crate::permission::decide::FlowAnswer> {
            use crate::permission::decide::{FlowAnswer, RuleResult as Rules};
            Box::pin(async move {
                let rule = match query.rule {
                    Rules::Allow => RuleResult::Allow,
                    Rules::Ask => RuleResult::Ask,
                    Rules::NoRule => RuleResult::None,
                };
                let call = permission_call(self.workspace.clone(), false);
                match self
                    .runtime
                    .permission_pre_call(call, rule, CancellationToken::new())
                    .await
                    .verdict
                {
                    FlowVerdict::Allow => FlowAnswer::Allow,
                    FlowVerdict::Deny { .. } => FlowAnswer::Deny,
                    FlowVerdict::AskCaptain { .. } => FlowAnswer::AskCaptain,
                }
            })
        }
        fn protected_check<'a>(
            &'a self,
            _: &'a crate::permission::decide::FlowQuery<'a>,
        ) -> BoxFuture<'a, ProtectedCheck> {
            Box::pin(async move {
                let call = permission_call(self.workspace.clone(), true);
                if self.compiled {
                    self.runtime
                        .protected_check_with(
                            compile_protected_check(true),
                            call,
                            CancellationToken::new(),
                        )
                        .await
                } else {
                    self.runtime
                        .protected_check_with(None, call, CancellationToken::new())
                        .await
                }
            })
        }
    }

    #[tokio::test]
    async fn the_decision_path_runs_a_protected_write_through_the_runtime() {
        use crate::permission::{
            decide::{decide, ApprovedBy, Control, Input, Verdict},
            floor::FloorContext,
            store::PermissionStore,
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["project", "config", "data"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let cx = FloorContext {
            workspace: root.join("project"),
            config_dir: Some(root.join("config")),
            data_dir: Some(root.join("data")),
            ..Default::default()
        };
        let store = PermissionStore::open(root.join("data")).unwrap();
        let target = root.join("config/config.json").display().to_string();
        let call = crate::provider::ToolCall {
            id: "c".into(),
            name: "write".into(),
            args: json!({"path": target}),
            args_valid: true,
        };
        for (compiled, answer, control, runs) in [
            // Enabled node and a yes: the write runs, under bypass without the flow.
            (true, 1.0, Control::Normal, true),
            (true, 1.0, Control::Bypass, true),
            // A no, or a disabled built-in flow: the Captain is asked.
            (true, 0.0, Control::Normal, false),
            (true, 0.0, Control::Bypass, false),
            (false, 1.0, Control::Bypass, false),
            (false, 1.0, Control::Normal, false),
        ] {
            let client = counting(answer);
            let (_keep, runtime) = runtime_with(client.clone());
            let flows = RuntimeFlows {
                runtime: &runtime,
                workspace: root.join("project"),
                compiled,
            };
            let input = Input {
                control,
                call: &call,
                commands: &[],
                cx: &cx,
                store: Some(&store),
                agent: "build",
                repeats: 1,
                validated: false,
            };
            let decision = decide(&input, &flows).await;
            match (&decision.verdict, runs, control) {
                (Verdict::Run { approved_by }, true, Control::Bypass) => {
                    assert_eq!(*approved_by, ApprovedBy::Bypass)
                }
                // The enabled built-in flow hears the Captain's request.
                (Verdict::Run { approved_by }, true, Control::Normal) => {
                    assert_eq!(*approved_by, ApprovedBy::Flow)
                }
                (Verdict::AskCaptain { always, .. }, false, _) => assert!(always.is_empty()),
                other => panic!("{compiled} {answer} {control:?}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn builtin_branches_and_protected_provenance() {
        for (protected, rule, answers, allowed) in [
            // The compiled node runs with the flows and grants it.
            (
                true,
                RuleResult::Ask,
                vec![("protected_requested", 0.9)],
                true,
            ),
            (true, RuleResult::Allow, vec![], false),
            (false, RuleResult::Ask, vec![("requested", 0.6)], true),
            (false, RuleResult::None, vec![("read_only", 0.6)], true),
            (false, RuleResult::None, vec![("recoverable", 0.7)], true),
            (
                false,
                RuleResult::None,
                vec![("read_only", 0.59), ("recoverable", 0.69)],
                false,
            ),
            (false, RuleResult::Ask, vec![], false),
            // Under an allow rule a dangerous call asks unless it was requested.
            (false, RuleResult::Allow, vec![("danger", 0.5)], false),
            (
                false,
                RuleResult::Allow,
                vec![("danger", 0.5), ("requested", 0.6)],
                true,
            ),
            // A dangerous unrequested call is never let through as a read.
            (
                false,
                RuleResult::Allow,
                vec![("danger", 0.5), ("read_only", 1.0), ("recoverable", 1.0)],
                false,
            ),
            (false, RuleResult::Allow, vec![], true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
            let (mut definition, templates) = super::super::builtin::all_definitions()
                .into_iter()
                .find(|(d, _)| d.flow.name == "permission-default")
                .unwrap();
            definition.flow.enabled = true;
            store
                .save(
                    "permission-default",
                    Location::Global,
                    None,
                    super::super::store::Content::Graph {
                        definition: Box::new(definition),
                        templates,
                    },
                )
                .unwrap();
            store.validate_test_documents();
            store.reload().unwrap();
            let client = Arc::new(BranchClient(
                answers.into_iter().map(|(n, p)| (n.into(), p)).collect(),
            ));
            let runtime = FlowRuntime::new(store.clone(), client, Arc::new(NoEffects));
            let mut call = permission_call(dir.path().into(), protected);
            call.skill = Some(crate::agent::flow_hook::ValidatedSkill {
                name: "deployment".into(),
                instructions: "SKILL_MUST_NOT_REACH_PROTECTED_CHECK".into(),
            });
            let result = runtime
                .permission_pre_call(call, rule, CancellationToken::new())
                .await;
            assert_eq!(
                matches!(result.verdict, FlowVerdict::Allow),
                allowed,
                "{rule:?} {:?}",
                result.verdict
            );
            let failed = FlowRuntime::new(store, Arc::new(Fake), Arc::new(NoEffects));
            assert!(!matches!(
                failed
                    .permission_pre_call(
                        permission_call(dir.path().into(), true),
                        RuleResult::Allow,
                        CancellationToken::new()
                    )
                    .await
                    .verdict,
                FlowVerdict::Allow
            ));
        }
    }

    /// Records the state of every question it is asked and answers `answer`.
    struct StateClient(std::sync::Mutex<Vec<(String, String)>>, f64);
    impl DecisionClient for StateClient {
        fn capabilities(&self) -> Capabilities {
            Capabilities { images: false }
        }
        fn identity(&self) -> ClientIdentity {
            ClientIdentity {
                model: "fake/state".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            request: instinct_flow::protocol::DecisionRequest<'a>,
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>>
        {
            for id in request.questions.keys() {
                self.0
                    .lock()
                    .unwrap()
                    .push((id.clone(), request.state.to_string()));
            }
            Box::pin(async move {
                Ok(instinct_flow::protocol::DecisionResponse {
                    answers: request
                        .questions
                        .keys()
                        .map(|id| {
                            (
                                id.clone(),
                                instinct_flow::protocol::Answer::Noul { noul: self.1 },
                            )
                        })
                        .collect(),
                    usage: None,
                })
            })
        }
    }

    fn proposal_then_yes(workspace: PathBuf, protected: bool) -> PermissionCallContext {
        use crate::agent::flow_hook::RecentMessage;
        let mut call = permission_call(workspace, protected);
        call.captain_messages = vec!["test the permissions".into(), "yes go".into()];
        call.recent = vec![
            RecentMessage {
                author: "agent",
                text: "PROPOSAL: say go and I delete the 2 test files".into(),
            },
            RecentMessage {
                author: "captain",
                text: "yes go".into(),
            },
        ];
        call
    }

    #[tokio::test]
    async fn the_flow_reads_a_short_answer_with_the_proposal_it_answers() {
        let client = Arc::new(StateClient(Default::default(), 1.0));
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        let runtime = FlowRuntime::new(store, client.clone(), Arc::new(NoEffects));
        let verdict = runtime
            .permission_pre_call(
                proposal_then_yes(dir.path().into(), false),
                RuleResult::None,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(verdict.verdict, FlowVerdict::Allow));
        let states = client.0.lock().unwrap().clone();
        let (_, state) = states.iter().find(|(id, _)| id == "requested").unwrap();
        assert!(state.contains("PROPOSAL: say go"), "{state}");
        assert!(state.contains(r#""author":"agent""#), "{state}");
        assert!(state.contains(r#""author":"captain","text":"yes go""#));
    }

    #[tokio::test]
    async fn the_protected_check_reads_the_recent_exchanges() {
        let client = Arc::new(StateClient(Default::default(), 1.0));
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        let runtime = FlowRuntime::new(store, client.clone(), Arc::new(NoEffects));
        let check = runtime
            .protected_check_with(
                compile_protected_check(true),
                proposal_then_yes(dir.path().into(), true),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(check, ProtectedCheck::Passed);
        let states = client.0.lock().unwrap().clone();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].0, "protected_requested");
        assert!(states[0].1.contains("PROPOSAL: say go"), "{}", states[0].1);
        assert!(states[0].1.contains("explicit acceptance"));
        // A message naming the change counts, a vague goal does not.
        assert!(states[0].1.contains("names this change"));
    }

    #[test]
    fn captain_budget_drops_oldest_and_never_splits_latest() {
        let value = bounded_captain_messages(vec!["x".repeat(16384), "latest".into()]);
        assert!(value.cut);
        assert_eq!(value.value, Some(json!(["latest"])));
        let value = bounded_captain_messages(vec!["é".repeat(16384)]);
        assert!(value.cut);
        assert_eq!(value.value, Some(json!([])));
    }

    #[tokio::test]
    async fn a_successful_blocking_flow_without_a_verdict_applies_rule() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        without_builtin(&store);
        let toml = "[flow]\nversion=1\nname='silent'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='log'\n[node.params]\ntext='audit'\n";
        store
            .save(
                "silent",
                Location::Global,
                None,
                super::super::store::Content::Toml {
                    toml: toml.into(),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
        let runtime = FlowRuntime::new(store, Arc::new(Fake), Arc::new(NoEffects));
        let result = runtime
            .permission_pre_call(
                permission_call(dir.path().into(), false),
                RuleResult::Ask,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result.verdict, FlowVerdict::AskCaptain { .. }));
        assert_eq!(result.flows.len(), 1);
        assert!(result.flows[0].used_rule);
    }
    #[tokio::test]
    async fn large_builtin_save_records_limit_failure_without_applying_effects() {
        struct NoCalls;
        impl FlowEffects for NoCalls {
            fn effect<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: &'a ProposedAction,
                _: &'a CancellationToken,
            ) -> BoxFuture<'a, Result<(), ApplyError>> {
                Box::pin(async { panic!("limit failure must not apply an effect") })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(
            root.path().into(),
            root.path().join("config"),
        ));
        let records = Arc::new(crate::flows::records::Records::new(
            root.path(),
            root.path(),
        ));
        let runtime = FlowRuntime::new(store, Arc::new(Fake), Arc::new(NoCalls))
            .with_records(records.clone());
        let mut payload = serde_json::Map::new();
        payload.extend(serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(json!({"uri":"file:///repo/test.rs", "language_id":"rust", "version":1, "diff":"edit"})).unwrap());
        for field in crate::flows::migrate::TEMPLATE_FIELDS {
            payload.insert(
                format!("template_{field}"),
                json!(if matches!(*field, "context" | "diff") {
                    "x".repeat(40 * 1024)
                } else {
                    "evidence".into()
                }),
            );
        }
        for field in crate::flows::migrate::INTEGER_FIELDS {
            payload.insert((*field).into(), json!(1));
        }
        for field in crate::flows::migrate::BOOLEAN_FIELDS {
            payload.insert((*field).into(), json!(false));
        }
        let (verdict, handles) = runtime
            .evaluate(
                Event {
                    trigger: "save".into(),
                    payload: payload.into(),
                },
                ContextSnapshot::default(),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(verdict, VerdictLike::Allow);
        assert_eq!(handles.len(), 1);
        let outcome = handles.into_iter().next().unwrap().await.unwrap();
        let RunResult::Failure {
            kind,
            message,
            fallback_actions,
        } = outcome.result
        else {
            panic!("expected limit failure")
        };
        assert_eq!(kind, FailureKind::Limit);
        assert_eq!(message, "collected inputs exceed run byte limit");
        assert!(fallback_actions.is_empty());
        assert!(outcome.trace.application.actions.is_empty());
        assert_eq!(outcome.trace.application.status, "not_applied");
        records.shutdown(Duration::from_secs(2));
        let captured = records.read().unwrap();
        assert_eq!(captured.len(), 1);
        let failure = captured[0].failure.as_ref().unwrap();
        assert_eq!(failure.kind, FailureKind::Limit);
        assert_eq!(failure.message, message);
    }
    struct Fake;
    impl DecisionClient for Fake {
        fn capabilities(&self) -> Capabilities {
            Capabilities { images: false }
        }
        fn identity(&self) -> ClientIdentity {
            ClientIdentity {
                model: "fake/instinct".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            _: instinct_flow::protocol::DecisionRequest<'a>,
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>>
        {
            Box::pin(async { Err(DecisionError::ProviderError) })
        }
    }
    struct Recovering(std::sync::atomic::AtomicUsize);
    impl DecisionClient for Recovering {
        fn capabilities(&self) -> Capabilities {
            Capabilities { images: false }
        }
        fn identity(&self) -> ClientIdentity {
            ClientIdentity {
                model: "fake/recovering".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            request: instinct_flow::protocol::DecisionRequest<'a>,
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>>
        {
            Box::pin(async move {
                let call = self.0.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    Err(DecisionError::ProviderRejected(400))
                } else if call == 3 {
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
                } else {
                    Err(DecisionError::ProviderError)
                }
            })
        }
    }
    #[tokio::test]
    async fn outage_notice_resets_only_after_provider_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        without_builtin(&store);
        let toml = "[flow]\nversion=1\nname='decision'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='noul'\ninstructions='test'\nthreshold=0.5\n[node.inputs]\nx='session.agent'\n";
        store
            .save(
                "decision",
                Location::Global,
                None,
                super::super::store::Content::Toml {
                    toml: toml.into(),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
        let effects = Arc::new(Notices(std::sync::atomic::AtomicUsize::new(0)));
        let runtime = FlowRuntime::new(
            store,
            Arc::new(Recovering(std::sync::atomic::AtomicUsize::new(0))),
            effects.clone(),
        );
        let ctx = ToolCallContextLike {
            call_id: "x".into(),
            tool: "read".into(),
            args: json!({}),
            agent: "build".into(),
            workspace: dir.path().into(),
        };
        for call in 0..5 {
            runtime
                .tool_pre_call(ctx.clone(), CancellationToken::new())
                .await;
            if call == 0 {
                assert_eq!(effects.0.load(Ordering::SeqCst), 0);
            }
        }
        assert_eq!(effects.0.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn asynchronous_effect_does_not_delay_blocking_verdict() {
        struct Slow(tokio::sync::Notify);
        impl FlowEffects for Slow {
            fn effect<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: &'a ProposedAction,
                _: &'a CancellationToken,
            ) -> BoxFuture<'a, Result<(), ApplyError>> {
                Box::pin(async move {
                    self.0.notified().await;
                    Ok(())
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        for (name, toml) in [("deny", text("deny", "deny")), ("async", "[flow]\nversion=1\nname='async'\non='tool.pre_call'\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='inject_note'\n[node.params]\ntext='note'\n".into())] {
            store.save(name, Location::Global, None, super::super::store::Content::Toml { toml, templates: BTreeMap::new() }).unwrap();
        }
        store.validate_test_documents();
        store.reload().unwrap();
        let runtime = FlowRuntime::new(
            store,
            Arc::new(Fake),
            Arc::new(Slow(tokio::sync::Notify::new())),
        );
        let (verdict, handles) = tokio::time::timeout(
            Duration::from_millis(100),
            runtime.evaluate(
                Event {
                    trigger: "tool.pre_call".into(),
                    payload: json!({"tool":"read","args":{},"call_id":"x"}),
                },
                ContextSnapshot::default(),
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(verdict, VerdictLike::Deny { .. }));
        assert_eq!(handles.len(), 1);
        handles[0].abort();
    }
    struct Notices(std::sync::atomic::AtomicUsize);
    impl FlowEffects for Notices {
        fn effect<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a ProposedAction,
            _: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<(), ApplyError>> {
            Box::pin(async { Ok(()) })
        }
        fn instinct_unavailable(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn text(name: &str, action: &str) -> String {
        format!("[flow]\nversion=1\nname='{name}'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='{action}'\n[node.params]\nnote='{name}'\n")
    }
    #[tokio::test]
    async fn context_collects_intent_and_leaves_missing_sources_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".withnav")).unwrap();
        std::fs::write(dir.path().join(".withnav/intent.md"), "## Intent\nShip it").unwrap();
        let context = ContextSnapshot {
            workspace: Some(dir.path().into()),
            ..Default::default()
        };
        let names = vec![
            "intent.record".into(),
            "workspace.root".into(),
            "conversation.digest".into(),
        ];
        let values = context
            .collect(
                &names,
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(values["intent.record"]
            .value
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("Ship it"));
        assert_eq!(values["workspace.root"].value, Some(json!(dir.path())));
        assert!(values["conversation.digest"].value.is_none());
    }
    #[tokio::test]
    async fn context_marks_nested_tokens_and_bounds_utf8() {
        let mut context = ContextSnapshot::default();
        context.values.insert(
            "conversation.digest".into(),
            InputValue {
                value: Some(json!("é".repeat(20000))),
                cut: false,
                redacted: false,
            },
        );
        context.values.insert(
            "session.model".into(),
            InputValue {
                value: Some(json!({"token":"ghp_abcdefghijklmnopqrstuvwxyz"})),
                cut: false,
                redacted: false,
            },
        );
        let names = vec!["conversation.digest".into(), "session.model".into()];
        let values = context
            .collect(
                &names,
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(values["conversation.digest"].cut);
        assert_eq!(
            values["conversation.digest"]
                .value
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap()
                .len(),
            instinct_flow::catalogue::MAX_INPUT_BYTES
        );
        assert!(values["session.model"].redacted);
    }
    #[tokio::test]
    async fn cancelled_collection_is_refused() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(ContextSnapshot::default()
            .collect(&[], &cancel, Instant::now() + Duration::from_secs(1))
            .await
            .is_err());
    }
    #[tokio::test]
    async fn deny_wins_over_ask_and_outage_is_not_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        for (name, action) in [("ask", "ask_captain"), ("deny", "deny")] {
            store
                .save(
                    name,
                    Location::Global,
                    None,
                    super::super::store::Content::Toml {
                        toml: text(name, action),
                        templates: BTreeMap::new(),
                    },
                )
                .unwrap();
        }
        store.validate_test_documents();
        store.reload().unwrap();
        let effects = Arc::new(Notices(std::sync::atomic::AtomicUsize::new(0)));
        let runtime = FlowRuntime::new(store.clone(), Arc::new(Fake), effects.clone());
        let ctx = ToolCallContextLike {
            call_id: "x".into(),
            tool: "write".into(),
            args: json!({}),
            agent: "build".into(),
            workspace: dir.path().into(),
        };
        assert!(matches!(
            runtime
                .tool_pre_call(ctx.clone(), CancellationToken::new())
                .await,
            VerdictLike::Deny { .. }
        ));
        for name in ["ask", "deny"] {
            let d = store.get(name, Location::Global).unwrap();
            store.delete(name, Location::Global, &d.revision).unwrap();
        }
        let toml="[flow]\nversion=1\nname='decision'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='noul'\ninstructions='test'\nthreshold=0.5\n[node.inputs]\nx='session.agent'\n";
        store
            .save(
                "decision",
                Location::Global,
                None,
                super::super::store::Content::Toml {
                    toml: toml.into(),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
        for _ in 0..2 {
            assert!(matches!(
                runtime
                    .tool_pre_call(ctx.clone(), CancellationToken::new())
                    .await,
                VerdictLike::AskCaptain { .. }
            ));
        }
        assert_eq!(effects.0.load(Ordering::SeqCst), 1);
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictLike {
    Allow,
    Deny {
        note: String,
    },
    AskCaptain {
        note: String,
        flow: String,
        run_id: String,
    },
}
pub trait FlowEffects: Send + Sync {
    fn effect<'a>(
        &'a self,
        flow: &'a str,
        run_id: &'a str,
        action: &'a ProposedAction,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), ApplyError>>;
    fn instinct_unavailable(&self) {}
}
pub struct NoEffects;
impl FlowEffects for NoEffects {
    fn effect<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a ProposedAction,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), ApplyError>> {
        Box::pin(async { Ok(()) })
    }
}
#[derive(Default, Clone)]
pub struct ContextSnapshot {
    pub values: BTreeMap<String, InputValue>,
    pub workspace: Option<PathBuf>,
}
impl ContextSnapshot {
    pub fn session(&mut self, session: &crate::session::SessionSummary) {
        for (name, value) in [
            ("session.id", &session.session_id),
            ("session.agent", &session.agent),
            ("session.model", &session.model),
            ("session.workspace", &session.workspace),
        ] {
            self.values.insert(
                name.into(),
                InputValue {
                    value: Some(json!(value)),
                    cut: false,
                    redacted: false,
                },
            );
        }
    }
}

/// Conservative whole-input marking; values stay available for live evaluation,
/// while the engine replaces marked evidence with its redaction marker in captures.
use super::redaction::contains_secret;
impl InputSource for ContextSnapshot {
    fn collect<'a>(
        &'a self,
        names: &'a [String],
        cancel: &'a CancellationToken,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<BTreeMap<String, InputValue>, InputError>> {
        Box::pin(async move {
            if cancel.is_cancelled() || Instant::now() >= deadline {
                return Err(InputError("collection cancelled or expired".into()));
            }
            let mut result = BTreeMap::new();
            let mut values = self.values.clone();
            if let Some(workspace) = &self.workspace {
                values.insert(
                    "workspace.root".into(),
                    InputValue {
                        value: Some(json!(workspace)),
                        cut: false,
                        redacted: false,
                    },
                );
                if names
                    .iter()
                    .any(|n| n == "workspace.branch" || n == "workspace.dirty_files")
                {
                    let status = tokio::select! { biased;
                        _ = cancel.cancelled() => return Err(InputError("collection cancelled".into())),
                        status = tokio::time::timeout_at(deadline.into(), crate::workspace::git::read(workspace)) => status.ok().flatten(),
                    };
                    if let Some(status) = status {
                        if let Some(branch) = status.branch {
                            values.insert(
                                "workspace.branch".into(),
                                InputValue {
                                    value: Some(json!(branch)),
                                    cut: false,
                                    redacted: false,
                                },
                            );
                        }
                        let files: Vec<_> = status.files.iter().map(|f| &f.path).collect();
                        values.insert(
                            "workspace.dirty_files".into(),
                            InputValue {
                                value: Some(json!(files)),
                                cut: status.dirty > files.len(),
                                redacted: false,
                            },
                        );
                    }
                }
                if names.iter().any(|n| n == "intent.record") {
                    let workspace = workspace.clone();
                    let read = tokio::task::spawn_blocking(move || {
                        let path = workspace.join(".withnav/intent.md");
                        // Refuse links and oversize files before the existing intent reader.
                        let metadata = std::fs::symlink_metadata(&path).ok()?;
                        if !metadata.is_file()
                            || metadata.len() > instinct_flow::catalogue::MAX_INPUT_BYTES as u64
                        {
                            return None;
                        }
                        let sensitive = crate::permission::floor::sensitive_read(&path)
                            || std::fs::canonicalize(&path)
                                .is_ok_and(|p| crate::permission::floor::sensitive_read(&p));
                        crate::navigator::intent::read_intent(&workspace)
                            .ok()
                            .flatten()
                            .map(|r| InputValue {
                                value: Some(json!(r.raw)),
                                cut: false,
                                redacted: sensitive,
                            })
                    });
                    let value = tokio::select! { biased;
                        _ = cancel.cancelled() => return Err(InputError("collection cancelled".into())),
                        value = tokio::time::timeout_at(deadline.into(), read) => value.ok().and_then(Result::ok).flatten(),
                    };
                    if let Some(value) = value {
                        values.insert("intent.record".into(), value);
                    }
                }
            }
            for name in names {
                let mut value = values.get(name).cloned().unwrap_or(InputValue {
                    value: None,
                    cut: false,
                    redacted: false,
                });
                value.redacted |= value.value.as_ref().is_some_and(contains_secret);
                if let Some(Value::String(text)) = &mut value.value {
                    if text.len() > instinct_flow::catalogue::MAX_INPUT_BYTES {
                        let mut end = instinct_flow::catalogue::MAX_INPUT_BYTES;
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        text.truncate(end);
                        value.cut = true;
                    }
                }
                result.insert(name.clone(), value);
            }
            Ok(result)
        })
    }
}
pub struct FlowRuntime {
    pub store: Arc<FlowStore>,
    client: Arc<dyn DecisionClient>,
    effects: std::sync::RwLock<Arc<dyn FlowEffects>>,
    outage: Arc<AtomicBool>,
    records: Option<Arc<super::records::Records>>,
}
impl FlowRuntime {
    /// Event-local editor effects share activation, client, outage and capture ownership.
    pub fn with_effects_adapter(&self, effects: Arc<dyn FlowEffects>) -> Arc<Self> {
        Arc::new(Self {
            store: self.store.clone(),
            client: self.client.clone(),
            effects: std::sync::RwLock::new(effects),
            outage: self.outage.clone(),
            records: self.records.clone(),
        })
    }
    /// Editor hosts can replace the no-op sink without replacing the workspace runtime.
    pub fn set_effects(&self, effects: Arc<dyn FlowEffects>) {
        *self.effects.write().unwrap_or_else(|e| e.into_inner()) = effects;
    }
    pub fn effects(&self) -> Arc<dyn FlowEffects> {
        self.effects
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn client(&self) -> Arc<dyn DecisionClient> {
        self.client.clone()
    }

    /// Bind the hook to a session, not the instinct provider's identity.
    pub fn tool_hook(
        self: &Arc<Self>,
        session_id: String,
        selected: Arc<std::sync::Mutex<crate::session::service::SelectedModel>>,
    ) -> Arc<dyn crate::agent::flow_hook::ToolCallFlows> {
        let effects = self
            .effects
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.tool_hook_with_effects(session_id, selected, effects)
    }
    pub fn tool_hook_with_effects(
        self: &Arc<Self>,
        session_id: String,
        selected: Arc<std::sync::Mutex<crate::session::service::SelectedModel>>,
        effects: Arc<dyn FlowEffects>,
    ) -> Arc<dyn crate::agent::flow_hook::ToolCallFlows> {
        Arc::new(SessionToolFlows {
            runtime: self.clone(),
            session_id,
            selected,
            effects,
        })
    }
    pub fn new(
        store: Arc<FlowStore>,
        client: Arc<dyn DecisionClient>,
        effects: Arc<dyn FlowEffects>,
    ) -> Self {
        Self {
            store,
            client,
            effects: std::sync::RwLock::new(effects),
            outage: Arc::new(AtomicBool::new(false)),
            records: None,
        }
    }
    pub fn with_records(mut self, records: Arc<super::records::Records>) -> Self {
        self.records = Some(records);
        self
    }
    pub async fn evaluate(
        &self,
        event: Event,
        context: ContextSnapshot,
        cancel: CancellationToken,
    ) -> (VerdictLike, Vec<tokio::task::JoinHandle<RunOutcome>>) {
        let effects = self
            .effects
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.evaluate_with_effects(event, context, cancel, effects)
            .await
    }
    pub async fn evaluate_with_effects(
        &self,
        event: Event,
        context: ContextSnapshot,
        cancel: CancellationToken,
        effects: Arc<dyn FlowEffects>,
    ) -> (VerdictLike, Vec<tokio::task::JoinHandle<RunOutcome>>) {
        let (verdict, handles, _) = self
            .evaluate_inner(event, context, cancel, effects, None, None)
            .await;
        (verdict, handles)
    }

    async fn evaluate_inner(
        &self,
        event: Event,
        context: ContextSnapshot,
        cancel: CancellationToken,
        effects: Arc<dyn FlowEffects>,
        permission: Option<(RuleResult, bool)>,
        host_check: Option<ProtectedCheck>,
    ) -> (
        VerdictLike,
        Vec<tokio::task::JoinHandle<RunOutcome>>,
        Vec<PermissionFlowAnswer>,
    ) {
        // A pending reload reads flow validations from the permission
        // database and may wait on the reload marker held by another thread:
        // it runs on the blocking pool. With nothing pending the evaluation
        // stays on this thread, so an editor trigger keeps its place among
        // the edits that follow it.
        if self.store.refresh_pending() {
            let store = self.store.clone();
            if tokio::task::spawn_blocking(move || store.refresh())
                .await
                .is_err()
            {
                tracing::warn!("flow refresh task failed; evaluating the current snapshot");
            }
        }
        let snapshot = self.store.snapshot();
        let started = Instant::now();
        let mut blocking = futures_util::stream::FuturesUnordered::new();
        let mut handles = vec![];
        let mut protected_runs = std::collections::BTreeSet::new();
        for (name, flow) in snapshot.iter() {
            let header = &flow.compiled.definition().flow;
            if !header.enabled || header.on != event.trigger {
                continue;
            }
            let budget = if header.blocking {
                header.budget_ms.unwrap_or(2000)
            } else {
                5000
            };
            let options = RunOptions {
                deadline: started + Duration::from_millis(budget),
                run_id: ulid::Ulid::generate().to_string(),
            };
            let flow = flow.clone();
            let client = self.client.clone();
            let event = event.clone();
            let context = context.clone();
            let cancel = cancel.clone();
            let effects = effects.clone();
            let outage = self.outage.clone();
            let name = name.clone();
            let records = self.records.clone();
            let is_blocking = header.blocking;
            let future = async move {
                let mut outcome = run(
                    &flow.compiled,
                    &event,
                    &context,
                    client.as_ref(),
                    options,
                    &cancel,
                )
                .await;
                // The old hook retains its original ask-on-failure default.
                if permission.is_none() && event.trigger == "tool.pre_call" {
                    if let RunResult::Failure {
                        fallback_actions,
                        message,
                        kind,
                    } = &mut outcome.result
                    {
                        let fallback = flow.compiled.definition().fallback.as_ref().and_then(|f| {
                            match kind {
                                FailureKind::Timeout => f.on_timeout.as_ref(),
                                FailureKind::ProviderError => f.on_provider_error.as_ref(),
                                FailureKind::InvalidAnswer => f.on_invalid_answer.as_ref(),
                                _ => None,
                            }
                            .or(f.on_failure.as_ref())
                        });
                        if fallback.is_none() {
                            *fallback_actions = vec![ProposedAction {
                                node_id: None,
                                action: "ask_captain".into(),
                                params: BTreeMap::from([(
                                    "note".into(),
                                    json!(format!("Flow failed: {message}")),
                                )]),
                            }];
                            outcome.trace.fallback_actions = fallback_actions.clone();
                        }
                    }
                }
                if outcome.trace.nodes.iter().any(|n| n.answer.is_some()) {
                    outage.store(false, Ordering::SeqCst);
                }
                if (matches!(
                    outcome.result,
                    RunResult::Failure {
                        kind: FailureKind::Timeout,
                        ..
                    }
                ) || outcome.provider_outage)
                    && !outage.swap(true, Ordering::SeqCst)
                {
                    effects.instinct_unavailable();
                }
                if !is_blocking && !cancel.is_cancelled() {
                    let actions = match &outcome.result {
                        RunResult::Success { actions } => actions,
                        RunResult::Failure {
                            fallback_actions, ..
                        } => fallback_actions,
                        RunResult::Skipped => return outcome,
                    };
                    let mut report = ApplyReport {
                        status: "applied".into(),
                        actions: vec![],
                    };
                    for action in actions.iter().filter(|a| {
                        catalogue()
                            .action(&a.action)
                            .is_some_and(|s| s.kind == ActionKind::Effect)
                    }) {
                        let result = if action.action == "log" {
                            Ok(())
                        } else {
                            effects
                                .effect(&name, &outcome.trace.run_id, action, &cancel)
                                .await
                        };
                        let (status, message) = application_status(&result);
                        report.actions.push(AppliedAction {
                            node_id: action.node_id.clone(),
                            action: action.action.clone(),
                            status: status.into(),
                            message,
                        });
                    }
                    report.status = if report.actions.is_empty() {
                        "not_applied"
                    } else {
                        aggregate_application_status(&report.actions)
                    }
                    .into();
                    outcome.trace.application = report;
                }
                if let Some(records) = records.filter(|_| !is_blocking) {
                    records.enqueue(outcome.trace.clone());
                }
                outcome
            };
            if header.blocking {
                blocking.push(future);
            } else {
                handles.push(tokio::spawn(future));
            }
        }
        use futures_util::StreamExt;
        let mut outcomes = vec![];
        while let Some(outcome) = blocking.next().await {
            outcomes.push(outcome);
        }
        if let Some(compiled) = permission
            .filter(|(_, protected)| *protected && host_check.is_none())
            .and_then(|_| compiled_protected_check())
        {
            // Overrides can tighten this verdict but never supply protected authority.
            // This compiled object and its template come solely from binary resources,
            // not from the active snapshot or a same-name disk definition.
            let options = RunOptions {
                deadline: started + Duration::from_millis(2000),
                run_id: ulid::Ulid::generate().to_string(),
            };
            let evidence_cut = context
                .values
                .get("conversation.captain_messages")
                .is_none_or(|v| v.cut || v.value.is_none());
            if !evidence_cut {
                protected_runs.insert(options.run_id.clone());
            }
            let outcome = run(
                &compiled,
                &event,
                &context,
                self.client.as_ref(),
                options,
                &cancel,
            )
            .await;
            outcomes.push(outcome);
        }
        outcomes.sort_by(|a, b| a.trace.flow_name.cmp(&b.trace.flow_name));
        let (permission_verdict, answers) = permission
            .map(|(rule, protected)| {
                let (verdict, answers) = resolve_permissions(
                    &outcomes,
                    rule,
                    protected,
                    &protected_runs,
                    host_check == Some(ProtectedCheck::Passed),
                );
                (Some(verdict), answers)
            })
            .unwrap_or((None, vec![]));
        let verdict = permission_verdict.unwrap_or_else(|| {
            match combine_verdicts(&outcomes, &catalogue()).ok().flatten() {
                None => VerdictLike::Allow,
                Some(action) => {
                    let note = action
                        .params
                        .get("note")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    match action.action.as_str() {
                        "deny" => VerdictLike::Deny { note },
                        "ask_captain" => {
                            let origin = outcomes.iter().find(|o| {
                                o.trace
                                    .actions
                                    .iter()
                                    .chain(&o.trace.fallback_actions)
                                    .any(|a| a.action == "ask_captain")
                            });
                            VerdictLike::AskCaptain {
                                note,
                                flow: origin
                                    .map(|o| o.trace.flow_name.clone())
                                    .unwrap_or_default(),
                                run_id: origin.map(|o| o.trace.run_id.clone()).unwrap_or_default(),
                            }
                        }
                        _ => VerdictLike::Allow,
                    }
                }
            }
        });
        for outcome in &mut outcomes {
            if !cancel.is_cancelled() {
                let actions = match &outcome.result {
                    RunResult::Success { actions } => actions,
                    RunResult::Failure {
                        fallback_actions, ..
                    } => fallback_actions,
                    RunResult::Skipped => continue,
                };
                let mut report = ApplyReport {
                    status: "applied".into(),
                    actions: vec![],
                };
                for action in actions.iter().filter(|a| {
                    catalogue()
                        .action(&a.action)
                        .is_some_and(|s| s.kind == ActionKind::Effect)
                }) {
                    let result = if action.action == "log" {
                        Ok(())
                    } else {
                        effects
                            .effect(
                                &outcome.trace.flow_name,
                                &outcome.trace.run_id,
                                action,
                                &cancel,
                            )
                            .await
                    };
                    let (status, message) = application_status(&result);
                    report.actions.push(AppliedAction {
                        node_id: action.node_id.clone(),
                        action: action.action.clone(),
                        status: status.into(),
                        message,
                    });
                }
                report.status = aggregate_application_status(&report.actions).into();
                outcome.trace.application = report;
            }
            if let Some(records) = &self.records {
                records.enqueue(outcome.trace.clone());
            }
        }
        (verdict, handles, answers)
    }

    pub async fn permission_pre_call(
        &self,
        ctx: PermissionCallContext,
        rule_result: RuleResult,
        cancel: CancellationToken,
    ) -> PermissionVerdict {
        self.permission_with_context(
            ctx,
            rule_result,
            cancel,
            ContextSnapshot::default(),
            self.effects(),
        )
        .await
    }

    /// The event and context both the permission flows and the protected check read.
    fn permission_event(
        ctx: PermissionCallContext,
        rule_result: RuleResult,
        mut context: ContextSnapshot,
    ) -> (Event, ContextSnapshot, (bool, Option<ProtectedCheck>)) {
        context.workspace = Some(ctx.call.workspace.clone());
        for (name, value) in [
            ("session.agent", json!(ctx.call.agent)),
            ("session.workspace", json!(ctx.call.workspace)),
            ("workspace.root", json!(ctx.call.workspace)),
        ] {
            context.values.entry(name.into()).or_insert(InputValue {
                value: Some(value),
                cut: false,
                redacted: false,
            });
        }
        let mut captain_evidence = bounded_captain_messages(ctx.captain_messages);
        captain_evidence.cut |= ctx.captain_messages_cut;
        context
            .values
            .insert("conversation.captain_messages".into(), captain_evidence);
        let mut delegation = bounded_captain_messages(ctx.delegation);
        delegation.cut |= ctx.delegation_cut;
        context
            .values
            .insert("conversation.delegation".into(), delegation);
        context.values.insert(
            "conversation.recent".into(),
            InputValue {
                value: Some(json!(ctx.recent)),
                cut: ctx.recent_cut,
                redacted: false,
            },
        );
        let mut payload = json!({"tool":ctx.call.tool,"args":ctx.call.args,"call_id":ctx.call.call_id,"rule_effect":rule_result,"protected":ctx.protected,"protected_checked":ctx.protected_check.is_some()});
        for (name, value) in [
            ("rule", json!(ctx.rule)),
            ("commands", json!(ctx.commands)),
            ("inside_project", json!(ctx.inside_project)),
            ("location", json!(ctx.location)),
            ("skill", json!(ctx.skill)),
        ] {
            if !value.is_null() {
                payload[name] = value;
            }
        }
        (
            Event {
                trigger: "tool.pre_call".into(),
                payload,
            },
            context,
            (ctx.protected, ctx.protected_check),
        )
    }

    async fn permission_with_context(
        &self,
        ctx: PermissionCallContext,
        rule_result: RuleResult,
        cancel: CancellationToken,
        context: ContextSnapshot,
        effects: Arc<dyn FlowEffects>,
    ) -> PermissionVerdict {
        let (event, context, (protected, host_check)) =
            Self::permission_event(ctx, rule_result, context);
        let (verdict, _, flows) = self
            .evaluate_inner(
                event,
                context,
                cancel,
                effects,
                Some((rule_result, protected)),
                host_check,
            )
            .await;
        PermissionVerdict {
            verdict: verdict.into(),
            flows,
        }
    }
    /// Run only the compiled-in protected check, the one authority for a write
    /// to WithNav's own configuration or database. Flows on disk, their names
    /// and their summaries play no part. A disabled built-in flow is
    /// `Unavailable` and no decision is requested.
    pub async fn permission_protected_check(
        &self,
        ctx: PermissionCallContext,
        cancel: CancellationToken,
    ) -> ProtectedCheck {
        self.protected_check_with(compiled_protected_check(), ctx, cancel)
            .await
    }

    /// [`Self::permission_protected_check`] with the run that answered it.
    pub async fn permission_protected_check_traced(
        &self,
        ctx: PermissionCallContext,
        cancel: CancellationToken,
    ) -> (ProtectedCheck, Option<RunScores>) {
        self.protected_check_traced_with(compiled_protected_check(), ctx, cancel)
            .await
    }

    async fn protected_check_with(
        &self,
        compiled: Option<CompiledDefinition>,
        ctx: PermissionCallContext,
        cancel: CancellationToken,
    ) -> ProtectedCheck {
        self.protected_check_traced_with(compiled, ctx, cancel)
            .await
            .0
    }

    async fn protected_check_traced_with(
        &self,
        compiled: Option<CompiledDefinition>,
        ctx: PermissionCallContext,
        cancel: CancellationToken,
    ) -> (ProtectedCheck, Option<RunScores>) {
        let Some(compiled) = compiled else {
            return (ProtectedCheck::Unavailable, None);
        };
        let (event, context, _) = Self::permission_event(
            PermissionCallContext {
                protected_check: None,
                ..ctx
            },
            RuleResult::None,
            ContextSnapshot::default(),
        );
        // Captain evidence that was cut or is missing cannot establish a request.
        if context
            .values
            .get("conversation.captain_messages")
            .is_none_or(|v| v.cut || v.value.is_none())
        {
            return (ProtectedCheck::Unavailable, None);
        }
        let outcome = run(
            &compiled,
            &event,
            &context,
            self.client.as_ref(),
            RunOptions {
                deadline: Instant::now() + Duration::from_millis(2000),
                run_id: ulid::Ulid::generate().to_string(),
            },
            &cancel,
        )
        .await;
        let result = match &outcome.result {
            RunResult::Success { actions }
                if actions.iter().any(|a| grants_protected(&outcome, a)) =>
            {
                ProtectedCheck::Passed
            }
            RunResult::Success { actions }
                if actions
                    .iter()
                    .any(|a| a.node_id.as_deref() == Some("protected_ask")) =>
            {
                ProtectedCheck::Failed
            }
            _ => ProtectedCheck::Unavailable,
        };
        let scores = run_scores(&outcome);
        if let Some(records) = &self.records {
            records.enqueue(outcome.trace.clone());
        }
        (result, Some(scores))
    }

    pub async fn tool_pre_call(
        &self,
        ctx: ToolCallContextLike,
        cancel: CancellationToken,
    ) -> VerdictLike {
        let mut context = ContextSnapshot {
            workspace: Some(ctx.workspace.clone()),
            ..Default::default()
        };
        for (name, value) in [
            ("session.agent", json!(ctx.agent)),
            ("session.workspace", json!(ctx.workspace)),
            ("workspace.root", json!(ctx.workspace)),
        ] {
            context.values.insert(
                name.into(),
                InputValue {
                    value: Some(value),
                    cut: false,
                    redacted: false,
                },
            );
        }
        self.evaluate(
            Event {
                trigger: "tool.pre_call".into(),
                payload: json!({"call_id":ctx.call_id,"tool":ctx.tool,"args":ctx.args}),
            },
            context,
            cancel,
        )
        .await
        .0
    }
}

struct SessionToolFlows {
    effects: Arc<dyn FlowEffects>,
    runtime: Arc<FlowRuntime>,
    session_id: String,
    selected: Arc<std::sync::Mutex<crate::session::service::SelectedModel>>,
}
impl std::fmt::Debug for SessionToolFlows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionToolFlows")
    }
}
impl crate::agent::flow_hook::ToolCallFlows for SessionToolFlows {
    fn permission_pre_call<'a>(
        &'a self,
        ctx: PermissionCallContext,
        rule_result: RuleResult,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, PermissionVerdict> {
        Box::pin(async move {
            let selected = self
                .selected
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut context = ContextSnapshot::default();
            for (name, value) in [
                ("session.id", json!(self.session_id)),
                ("session.agent", json!(selected.agent)),
                ("session.model", json!(selected.alias)),
            ] {
                context.values.insert(
                    name.into(),
                    InputValue {
                        value: Some(value),
                        cut: false,
                        redacted: false,
                    },
                );
            }
            self.runtime
                .permission_with_context(
                    ctx,
                    rule_result,
                    cancel.clone(),
                    context,
                    self.effects.clone(),
                )
                .await
        })
    }
    fn protected_check<'a>(
        &'a self,
        ctx: PermissionCallContext,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ProtectedCheck> {
        Box::pin(self.runtime.permission_protected_check(ctx, cancel.clone()))
    }
    fn protected_check_traced<'a>(
        &'a self,
        ctx: PermissionCallContext,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, (ProtectedCheck, Option<RunScores>)> {
        Box::pin(
            self.runtime
                .permission_protected_check_traced(ctx, cancel.clone()),
        )
    }
}

fn aggregate_application_status(actions: &[AppliedAction]) -> &'static str {
    if actions.iter().any(|action| action.status == "failed") {
        "failed"
    } else if !actions.is_empty() && actions.iter().all(|action| action.status == "suppressed") {
        "suppressed"
    } else {
        "applied"
    }
}

/// How one applied effect is recorded. A wake held back by the session's flow-wake cooldown
/// was deliberately not delivered: it is `suppressed`, not a failure, so health counters do not
/// report a working flow as broken and the Studio shows why nothing happened.
fn application_status(result: &Result<(), ApplyError>) -> (&'static str, Option<String>) {
    match result {
        Ok(()) => ("applied", None),
        Err(ApplyError(reason)) if reason == WAKE_COOLDOWN => {
            ("suppressed", Some(WAKE_COOLDOWN.to_owned()))
        }
        Err(_) => ("failed", Some("effect application failed".to_owned())),
    }
}

/// The error text the session and editor effect adapters return for a cooled-down wake.
pub(crate) const WAKE_COOLDOWN: &str = "wake cooldown";

#[cfg(test)]
mod application_status_tests {
    use super::*;

    #[test]
    fn aggregate_application_follows_action_statuses() {
        for (statuses, expected) in [
            (vec![], "applied"),
            (vec!["applied"], "applied"),
            (vec!["suppressed"], "suppressed"),
            (vec!["suppressed", "suppressed"], "suppressed"),
            (vec!["suppressed", "applied"], "applied"),
            (vec!["applied", "suppressed"], "applied"),
            (vec!["suppressed", "failed", "applied"], "failed"),
            (vec!["failed", "suppressed"], "failed"),
        ] {
            let actions: Vec<_> = statuses
                .into_iter()
                .map(|status| AppliedAction {
                    node_id: None,
                    action: "wake".into(),
                    status: status.into(),
                    message: None,
                })
                .collect();
            assert_eq!(aggregate_application_status(&actions), expected);
        }
    }

    #[test]
    fn a_cooled_down_wake_is_suppressed_not_failed() {
        assert_eq!(application_status(&Ok(())), ("applied", None));
        assert_eq!(
            application_status(&Err(ApplyError(WAKE_COOLDOWN.into()))),
            ("suppressed", Some(WAKE_COOLDOWN.to_owned()))
        );
        assert_eq!(
            application_status(&Err(ApplyError("session stopped".into()))).0,
            "failed"
        );
    }

    /// A reload marker newer than the store's view makes an evaluation reload,
    /// and that reload reads flow validations from the permission database.
    /// With the database locked and the reload failing, and with a second
    /// refresh running at the same time, an unrelated task on the same
    /// single thread must keep running.
    #[test]
    fn a_reload_on_a_locked_database_does_not_stall_the_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (config, data) = (dir.path().join("config"), dir.path().join("data"));
            let writer = FlowStore::with_data(dir.path().into(), config.clone(), data.clone());
            let store = Arc::new(FlowStore::with_data(
                dir.path().into(),
                config.clone(),
                data.clone(),
            ));
            // A saved global flow forces the reload to read its validation.
            writer
                .save(
                    "guard",
                    crate::flows::store::Location::Global,
                    None,
                    super::super::store::Content::Toml {
                        toml: "[flow]\nversion=1\nname='guard'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='no_objection'\n[node.params]\n".into(),
                        templates: BTreeMap::new(),
                    },
                )
                .unwrap();
            // Validation reads fail on a broken table: every reload fails.
            writer.reload().unwrap();
            crate::permission::store::PermissionStore::open(&data)
                .unwrap()
                .db()
                .execute_batch("DROP TABLE validations; CREATE TABLE validations (invalid TEXT)")
                .unwrap();
            writer.reload().unwrap_err();
            let runtime = Arc::new(FlowRuntime::new(
                store.clone(),
                Arc::new(crate::flows::client::UnavailableClient),
                Arc::new(NoEffects),
            ));
            let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let ticker = tokio::spawn({
                let ticks = ticks.clone();
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        ticks.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
            let (locked, is_locked) = std::sync::mpsc::channel();
            let holder = std::thread::spawn({
                let (ticks, data) = (ticks.clone(), data.clone());
                move || {
                    // Out of WAL, an exclusive lock blocks readers too, and
                    // every store open waits to switch the journal back.
                    let db = rusqlite::Connection::open(data.join("withnav.db")).unwrap();
                    db.pragma_update(None, "journal_mode", "DELETE").unwrap();
                    db.execute_batch("BEGIN EXCLUSIVE").unwrap();
                    locked.send(()).unwrap();
                    let before = ticks.load(Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(600));
                    let during = ticks.load(Ordering::SeqCst) - before;
                    db.execute_batch("COMMIT").unwrap();
                    during
                }
            });
            is_locked.recv().unwrap();
            // A concurrent refresh on another thread holds the marker while
            // it waits on the database.
            let concurrent = std::thread::spawn({
                let store = store.clone();
                move || store.refresh()
            });
            let event = || Event {
                trigger: "tool.pre_call".into(),
                payload: json!({}),
            };
            for _ in 0..2 {
                runtime
                    .evaluate(event(), ContextSnapshot::default(), CancellationToken::new())
                    .await;
            }
            let during = tokio::task::spawn_blocking(move || holder.join().unwrap())
                .await
                .unwrap();
            tokio::task::spawn_blocking(move || concurrent.join().unwrap())
                .await
                .unwrap();
            ticker.abort();
            assert!(during >= 40, "the runtime stalled: {during} ticks");
            assert!(store
                .snapshot()
                .values()
                .all(|flow| flow.location == crate::flows::store::Location::Builtin));
        });
    }

    /// An evaluation with no reload pending must not leave the runtime
    /// thread: an extra hop delays editor triggers past the edits that follow
    /// them. With the only blocking thread busy, the evaluation still ends.
    #[test]
    fn an_evaluation_without_a_pending_reload_stays_on_the_runtime_thread() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(FlowStore::with_data(
                dir.path().into(),
                dir.path().join("config"),
                dir.path().join("data"),
            ));
            assert!(!store.refresh_pending());
            let runtime = FlowRuntime::new(
                store.clone(),
                Arc::new(crate::flows::client::UnavailableClient),
                Arc::new(NoEffects),
            );
            let (release, busy) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || busy.recv());
            let evaluated = tokio::time::timeout(
                Duration::from_secs(2),
                runtime.evaluate(
                    Event {
                        trigger: "save".into(),
                        payload: json!({}),
                    },
                    ContextSnapshot::default(),
                    CancellationToken::new(),
                ),
            )
            .await;
            release.send(()).unwrap();
            occupied.await.unwrap().unwrap();
            assert!(
                evaluated.is_ok(),
                "the evaluation waited for the blocking pool"
            );
        });
    }

    /// A reload published by another store, or one in progress on another
    /// thread, is pending; a store that saw the last reload has nothing pending.
    #[test]
    fn a_reload_is_pending_after_another_store_reloads_or_while_one_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (config, data) = (dir.path().join("config"), dir.path().join("data"));
        let writer = FlowStore::with_data(dir.path().into(), config.clone(), data.clone());
        let reader = Arc::new(FlowStore::with_data(dir.path().into(), config, data));
        assert!(!reader.refresh_pending());
        writer.reload().unwrap();
        assert!(reader.refresh_pending());
        reader.refresh();
        assert!(!reader.refresh_pending());
        let held = reader.hold_reload_marker_for_test();
        assert!(
            reader.refresh_pending(),
            "a reload in progress counts as pending"
        );
        drop(held);
        assert!(!reader.refresh_pending());
    }
}
