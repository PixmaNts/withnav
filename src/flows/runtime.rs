//! Per-workspace evaluation; asynchronous effects never change an admitted event.
use super::{
    catalogue::catalogue,
    store::{FlowStore, Location},
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
    async fn deny_wins_over_ask_and_outage_is_not_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        for (name, action) in [("ask", "ask_driver"), ("deny", "deny")] {
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
        let toml="[flow]\nversion=1\nname='decision'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='noul'\ninstructions='test'\nthreshold=0.5\n";
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
        store.reload().unwrap();
        for _ in 0..2 {
            assert!(matches!(
                runtime
                    .tool_pre_call(ctx.clone(), CancellationToken::new())
                    .await,
                VerdictLike::AskDriver { .. }
            ));
        }
        assert_eq!(effects.0.load(Ordering::SeqCst), 1);
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictLike {
    NoObjection,
    Deny {
        note: String,
    },
    AskDriver {
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
        Box::pin(async { Err(ApplyError("flow effects are not connected".into())) })
    }
}
#[derive(Default, Clone)]
pub struct ContextSnapshot {
    pub values: BTreeMap<String, InputValue>,
}
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
            for name in names {
                let mut value = self.values.get(name).cloned().unwrap_or(InputValue {
                    value: None,
                    cut: false,
                    redacted: false,
                });
                if let Some(Value::String(text)) = &mut value.value {
                    if text.len() > 16384 {
                        let mut end = 16384;
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
    effects: Arc<dyn FlowEffects>,
    outage: Arc<AtomicBool>,
    records: Option<Arc<super::records::Records>>,
}
impl FlowRuntime {
    pub fn new(
        store: Arc<FlowStore>,
        client: Arc<dyn DecisionClient>,
        effects: Arc<dyn FlowEffects>,
    ) -> Self {
        Self {
            store,
            client,
            effects,
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
        let snapshot = self.store.snapshot();
        let trusted = self.store.trusted();
        let started = Instant::now();
        let mut blocking = futures_util::stream::FuturesUnordered::new();
        let mut handles = vec![];
        for (name, flow) in snapshot.iter() {
            let header = &flow.compiled.definition().flow;
            if !header.enabled
                || header.on != event.trigger
                || (flow.location == Location::Workspace && !trusted)
            {
                continue;
            }
            let budget = if header.blocking {
                header.budget_ms.unwrap_or(2000)
            } else {
                5000
            };
            let options = RunOptions {
                deadline: started + Duration::from_millis(budget),
                run_id: ulid::Ulid::new().to_string(),
            };
            let flow = flow.clone();
            let client = self.client.clone();
            let event = event.clone();
            let context = context.clone();
            let cancel = cancel.clone();
            let effects = self.effects.clone();
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
                if outcome.trace.nodes.iter().any(|n| n.answer.is_some()) {
                    outage.store(false, Ordering::SeqCst);
                }
                if matches!(
                    outcome.result,
                    RunResult::Failure {
                        kind: FailureKind::ProviderError | FailureKind::Timeout,
                        ..
                    }
                ) && !outage.swap(true, Ordering::SeqCst)
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
                        let status = if result.is_ok() {
                            "applied"
                        } else {
                            report.status = "failed".into();
                            "failed"
                        };
                        report.actions.push(AppliedAction {
                            node_id: action.node_id.clone(),
                            action: action.action.clone(),
                            status: status.into(),
                            message: result.err().map(|_| "effect application failed".into()),
                        });
                    }
                    outcome.trace.application = report;
                }
                if let Some(records) = records.filter(|_| !is_blocking) {
                    if records.append(&outcome.trace).is_err() {
                        tracing::warn!("private flow capture failed");
                    }
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
        outcomes.sort_by(|a, b| a.trace.flow_name.cmp(&b.trace.flow_name));
        let verdict = match combine_verdicts(&outcomes, &catalogue()).ok().flatten() {
            None => VerdictLike::NoObjection,
            Some(action) => {
                let note = action
                    .params
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                match action.action.as_str() {
                    "deny" => VerdictLike::Deny { note },
                    "ask_driver" => {
                        let origin = outcomes.iter().find(|o| {
                            o.trace
                                .actions
                                .iter()
                                .chain(&o.trace.fallback_actions)
                                .any(|a| a.action == "ask_driver")
                        });
                        VerdictLike::AskDriver {
                            note,
                            flow: origin
                                .map(|o| o.trace.flow_name.clone())
                                .unwrap_or_default(),
                            run_id: origin.map(|o| o.trace.run_id.clone()).unwrap_or_default(),
                        }
                    }
                    _ => VerdictLike::NoObjection,
                }
            }
        };
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
                        self.effects
                            .effect(
                                &outcome.trace.flow_name,
                                &outcome.trace.run_id,
                                action,
                                &cancel,
                            )
                            .await
                    };
                    let status = if result.is_ok() {
                        "applied"
                    } else {
                        report.status = "failed".into();
                        "failed"
                    };
                    report.actions.push(AppliedAction {
                        node_id: action.node_id.clone(),
                        action: action.action.clone(),
                        status: status.into(),
                        message: result.err().map(|_| "effect application failed".into()),
                    });
                }
                outcome.trace.application = report;
            }
            if let Some(records) = &self.records {
                if records.append(&outcome.trace).is_err() {
                    tracing::warn!("private flow capture failed");
                }
            }
        }
        (verdict, handles)
    }
    pub async fn tool_pre_call(
        &self,
        ctx: ToolCallContextLike,
        cancel: CancellationToken,
    ) -> VerdictLike {
        let mut context = ContextSnapshot::default();
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
        context.values.insert(
            "session.model".into(),
            InputValue {
                value: Some(json!(self.client.identity().model)),
                cut: false,
                redacted: false,
            },
        );
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
