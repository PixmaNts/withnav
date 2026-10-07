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
                if self.0.fetch_add(1, Ordering::SeqCst) == 2 {
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
        let toml = "[flow]\nversion=1\nname='decision'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='noul'\ninstructions='test'\nthreshold=0.5\n";
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
        for _ in 0..4 {
            runtime
                .tool_pre_call(ctx.clone(), CancellationToken::new())
                .await;
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
        std::fs::create_dir(dir.path().join(".navigator")).unwrap();
        std::fs::write(
            dir.path().join(".navigator/intent.md"),
            "## Intent\nShip it",
        )
        .unwrap();
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
                value: Some(json!("é".repeat(10000))),
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
            16384
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
                        let path = workspace.join(".navigator/intent.md");
                        // Refuse links and oversize files before the existing intent reader.
                        let metadata = std::fs::symlink_metadata(&path).ok()?;
                        if !metadata.is_file() || metadata.len() > 16384 {
                            return None;
                        }
                        let sensitive = crate::agent::permission_gate::sensitive_read(&path)
                            || std::fs::canonicalize(&path)
                                .is_ok_and(|p| crate::agent::permission_gate::sensitive_read(&p));
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
    effects: std::sync::RwLock<Arc<dyn FlowEffects>>,
    outage: Arc<AtomicBool>,
    records: Option<Arc<super::records::Records>>,
}
impl FlowRuntime {
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
                    let failed = records.append(&outcome.trace).is_err();
                    if records.update_health(&outcome.trace, failed).is_err() {
                        tracing::warn!("private flow health update failed");
                    }
                    if failed {
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
                        effects
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
                let failed = records.append(&outcome.trace).is_err();
                if records.update_health(&outcome.trace, failed).is_err() {
                    tracing::warn!("private flow health update failed");
                }
                if failed {
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
    fn pre_call<'a>(
        &'a self,
        call: &'a crate::agent::flow_hook::ToolCallContext,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, crate::agent::flow_hook::FlowVerdict> {
        Box::pin(async move {
            let selected = self
                .selected
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut context = ContextSnapshot {
                workspace: Some(call.workspace.clone()),
                ..Default::default()
            };
            for (name, value) in [
                ("session.id", json!(self.session_id)),
                ("session.agent", json!(selected.agent)),
                ("session.model", json!(selected.alias)),
                ("session.workspace", json!(call.workspace)),
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
            let verdict = self
                .runtime
                .evaluate_with_effects(
                    Event {
                        trigger: "tool.pre_call".into(),
                        payload: json!({"tool":call.tool,"args":call.args,"call_id":call.call_id}),
                    },
                    context,
                    cancel.clone(),
                    self.effects.clone(),
                )
                .await
                .0;
            use crate::agent::flow_hook::FlowVerdict;
            match verdict {
                VerdictLike::NoObjection => FlowVerdict::NoObjection,
                VerdictLike::Deny { note } => FlowVerdict::Deny { note },
                VerdictLike::AskDriver { note, flow, run_id } => {
                    FlowVerdict::AskDriver { note, flow, run_id }
                }
            }
        })
    }
}
