//! Remarks use anchored metacognition; flow notes use typed safe-boundary steering.
use super::{trigger::TriggerContext, Runtime};
use crate::{
    flows::runtime::{ContextSnapshot, FlowEffects},
    navigator::{log::PendingTrigger, TriggerKind},
};
use instinct_flow::{ApplyError, BoxFuture, Event, ProposedAction};
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Weak,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct EditorEffects {
    runtime: Weak<Runtime>,
    context: Mutex<Option<(TriggerContext, PendingTrigger)>>,
    delivered: AtomicBool,
}
impl FlowEffects for EditorEffects {
    fn effect<'a>(
        &'a self,
        flow: &'a str,
        run_id: &'a str,
        action: &'a ProposedAction,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), ApplyError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(ApplyError("editor event cancelled".into()));
            }
            let runtime = self
                .runtime
                .upgrade()
                .ok_or_else(|| ApplyError("editor closed".into()))?;
            if !matches!(action.action.as_str(), "remark" | "wake" | "inject_note") {
                return Err(ApplyError("unsupported editor effect".into()));
            }
            let (ctx, mut pending) = self
                .context
                .lock()
                .await
                .clone()
                .ok_or_else(|| ApplyError("editor event already delivered".into()))?;
            let uri = ctx.uri.clone();
            if self.delivered.swap(true, Ordering::SeqCst) {
                pending.trigger_id = crate::navigator::new_ulid_string();
            }
            match action.action.as_str() {
                "remark" => {
                    let prompt = action
                        .params
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ApplyError("missing remark prompt".into()))?;
                    pending.requested_action = Some("remark");
                    runtime
                        .deliver_remark(uri, ctx, pending, prompt.into(), cancel.clone())
                        .await;
                }
                "wake" | "inject_note" => {
                    let wake = action.action == "wake";
                    let text = action
                        .params
                        .get(if wake { "note" } else { "text" })
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| ApplyError("missing flow text".into()))?;
                    let result = runtime
                        .conversation
                        .as_ref()
                        .ok_or_else(|| ApplyError("no editor conversation".into()))?
                        .post_flow_note(text.into(), flow.into(), run_id.into(), wake);
                    if let Err(error) = result {
                        if error == "wake cooldown" {
                            pending.requested_action = Some(if wake {
                                "wake_cognition"
                            } else {
                                "inject_note"
                            });
                            pending.finish(
                                &runtime.log,
                                crate::navigator::log::DeliveryOutcome::None {
                                    reason: "wake cooldown",
                                },
                            );
                        }
                        return Err(ApplyError(error));
                    }
                    if wake {
                        if let Some(doc) = runtime.documents.lock().await.get_mut(&uri) {
                            doc.last_wake = Some(tokio::time::Instant::now());
                            doc.rebase_to(&ctx.text);
                        }
                    }
                    pending.requested_action = Some(if wake {
                        "wake_cognition"
                    } else {
                        "inject_note"
                    });
                    pending.finish(
                        &runtime.log,
                        crate::navigator::log::DeliveryOutcome::Silent { reason: None },
                    );
                }
                _ => unreachable!(),
            }
            Ok(())
        })
    }
}

impl Runtime {
    /// Returns true once an eligible flow owns the trigger, including skips/failures.
    pub(super) async fn evaluate_flows(self: &Arc<Self>, uri: &str, event: TriggerKind) -> bool {
        let Some(factory) = &self.flows else {
            return false;
        };
        let effects = Arc::new(EditorEffects {
            runtime: Arc::downgrade(self),
            context: Mutex::new(None),
            delivered: AtomicBool::new(false),
        });
        let flows = factory(effects.clone());
        let trusted = flows.store.trusted();
        if !flows.store.snapshot().values().any(|f| {
            let header = &f.compiled.definition().flow;
            header.enabled
                && header.on == event.as_str()
                && (f.location != crate::flows::store::Location::Workspace || trusted)
        }) {
            return false;
        }
        let Some(mut ctx) = self.gather(uri, event, tokio::time::Instant::now()).await else {
            return true;
        };
        ctx.event = event.as_str();
        if ctx.help_in_flight || ctx.remark_in_flight {
            return true;
        }
        let cancel = CancellationToken::new();
        {
            let mut docs = self.documents.lock().await;
            let Some(doc) = docs.get_mut(uri) else {
                return true;
            };
            if doc.in_flight_version.is_some() {
                return true;
            }
            doc.in_flight_version = Some(ctx.version);
            doc.in_flight_cancel = Some(cancel.clone());
        }
        let workspace = self.workspace.lock().await.clone();
        let mut pending = PendingTrigger::new(
            &self.run_id,
            workspace.as_ref().map(|p| p.display().to_string()),
            uri,
            ctx.version,
            ctx.event,
            ctx.prefilter_inputs(),
        );
        pending.prefilter_outcome = "pass";
        let mut payload = serde_json::Map::new();
        match event {
            TriggerKind::Conversation => {
                payload.insert("turn_id".into(), json!(crate::navigator::new_ulid_string()));
                payload.insert("text".into(), json!(ctx.conversation_text));
                payload.insert("uri".into(), json!(uri));
            }
            _ => {
                payload.extend([
                    ("uri".into(), json!(uri)),
                    ("language_id".into(), json!(ctx.language_id)),
                    ("version".into(), json!(ctx.version)),
                    ("diff".into(), json!(ctx.diff)),
                ]);
                if event == TriggerKind::Idle {
                    payload.insert("idle_ms".into(), json!(self.config.lock().await.idle_ms));
                }
                if event == TriggerKind::Diagnostics {
                    payload.insert("diagnostics".into(), ctx.diagnostics_payload.clone());
                }
            }
        }
        payload.extend(ctx.prefilter_inputs());
        payload.extend(
            ctx.template_inputs()
                .into_iter()
                .map(|(k, v)| (format!("template_{k}"), json!(v))),
        );
        *effects.context.lock().await = Some((ctx, pending));
        self.set_status("thinking", None).await;
        let mut context = ContextSnapshot {
            workspace,
            ..Default::default()
        };
        let captured = effects.context.lock().await;
        if let Some((ctx, _)) = captured.as_ref() {
            for (name, value) in [
                ("conversation.digest", json!(ctx.conversation_text)),
                ("intent.record", json!(ctx.intent_text)),
                (
                    "editor.open_files",
                    json!(self
                        .documents
                        .lock()
                        .await
                        .open_buffers()
                        .keys()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()),
                ),
            ] {
                context.values.insert(
                    name.into(),
                    instinct_flow::InputValue {
                        value: Some(value),
                        cut: false,
                        redacted: false,
                    },
                );
            }
        }
        drop(captured);
        if let Some(session_id) = self
            .conversation
            .as_ref()
            .and_then(|feed| feed.active_session())
        {
            context.values.insert(
                "session.id".into(),
                instinct_flow::InputValue {
                    value: Some(json!(session_id.to_string())),
                    cut: false,
                    redacted: false,
                },
            );
        }
        let (_, handles) = flows
            .evaluate(
                Event {
                    trigger: event.as_str().into(),
                    payload: payload.into(),
                },
                context,
                cancel.clone(),
            )
            .await;
        let mut failed = false;
        let mut skipped = true;
        for handle in handles {
            match handle.await {
                Ok(outcome) => {
                    failed |= matches!(outcome.result, instinct_flow::RunResult::Failure { .. });
                    skipped &= matches!(outcome.result, instinct_flow::RunResult::Skipped);
                }
                Err(error) => {
                    failed = true;
                    tracing::warn!(%error, "editor flow task failed");
                }
            }
        }
        if let Some((_, mut pending)) = effects
            .context
            .lock()
            .await
            .take()
            .filter(|_| !effects.delivered.load(Ordering::SeqCst))
        {
            pending.requested_action = if failed || skipped {
                None
            } else {
                Some("silent")
            };
            let outcome = if cancel.is_cancelled() {
                crate::navigator::log::DeliveryOutcome::Cancelled { reason: None }
            } else if failed {
                crate::navigator::log::DeliveryOutcome::ProviderError {
                    reason: Some("flow"),
                }
            } else if skipped {
                crate::navigator::log::DeliveryOutcome::None {
                    reason: "flow_skipped",
                }
            } else {
                crate::navigator::log::DeliveryOutcome::Silent { reason: None }
            };
            pending.finish(&self.log, outcome);
            if !failed && !skipped && !cancel.is_cancelled() && event != TriggerKind::Conversation {
                if let Some(doc) = self.documents.lock().await.get_mut(uri) {
                    doc.rebase();
                }
            }
        }
        self.clear_in_flight(uri).await;
        self.set_status("idle", None).await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::{
        builtin,
        store::{Content, FlowStore, Location},
    };
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn editor_wake_and_inject_note_use_the_typed_flow_poster_not_message_admission() {
        struct Silent;
        struct Server;
        struct Stuck;
        impl instinct_flow::DecisionClient for Stuck {
            fn capabilities(&self) -> instinct_flow::Capabilities {
                instinct_flow::Capabilities { images: false }
            }
            fn identity(&self) -> instinct_flow::ClientIdentity {
                instinct_flow::ClientIdentity {
                    model: "test/stuck".into(),
                    variant: None,
                }
            }
            fn decide<'a>(
                &'a self,
                request: instinct_flow::protocol::DecisionRequest<'a>,
                _: &'a CancellationToken,
                _: std::time::Instant,
            ) -> BoxFuture<
                'a,
                Result<instinct_flow::protocol::DecisionResponse, instinct_flow::DecisionError>,
            > {
                Box::pin(async move {
                    use instinct_flow::protocol::{Answer, Question};
                    Ok(instinct_flow::protocol::DecisionResponse {
                        answers: request
                            .questions
                            .iter()
                            .map(|(id, question)| {
                                let answer = match question {
                                    Question::Noul { .. } => Answer::Noul {
                                        noul: if id == "stuck" { 1.0 } else { 0.0 },
                                    },
                                    Question::Choice { .. } => Answer::Choice {
                                        choice: "cognition".into(),
                                        probabilities: BTreeMap::from([
                                            ("cognition".into(), 1.0),
                                            ("metacognition".into(), 0.0),
                                            ("silent".into(), 0.0),
                                        ]),
                                        confidence: 1.0,
                                    },
                                    Question::Score { .. } => {
                                        panic!("editor defaults do not score")
                                    }
                                };
                                (id.clone(), answer)
                            })
                            .collect(),
                        usage: None,
                    })
                })
            }
        }
        impl tower_lsp_server::LanguageServer for Server {
            async fn initialize(
                &self,
                _: tower_lsp_server::ls_types::InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<tower_lsp_server::ls_types::InitializeResult>
            {
                Ok(Default::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }
        impl crate::navigator::Metacognition for Silent {
            fn remark<'a>(
                &'a self,
                _: &'a crate::navigator::metacognition::RemarkContext,
                _: &'a CancellationToken,
            ) -> futures::future::BoxFuture<
                'a,
                Result<Option<crate::navigator::Remark>, crate::navigator::ModelError>,
            > {
                Box::pin(async { panic!("flow note must not call metacognition") })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let feed = crate::navigator::ConversationFeed::new();
        let id = ulid::Ulid::new();
        feed.register_session(
            id,
            Arc::new(|_| Box::pin(async { panic!("flow note must not enter message admission") })),
        );
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = observed.clone();
        let suppress = Arc::new(AtomicBool::new(false));
        let suppress_post = suppress.clone();
        feed.register_flow_poster(
            id,
            Arc::new(move |text, flow, run_id, wake| {
                if suppress_post.load(Ordering::SeqCst) {
                    return Err("wake cooldown".into());
                }
                sink.lock().unwrap().push((text, flow, run_id, wake));
                Ok(())
            }),
        );
        feed.observe(&crate::bus::event::EventEnvelope::new(
            id,
            0,
            crate::bus::event::EventKind::MessageReceived,
            json!({"source":"user:test", "content":"driver task"}),
        ))
        .await;
        let store = Arc::new(FlowStore::new(dir.path().into(), dir.path().join("config")));
        let factory: Arc<crate::navigator::FlowFactory> = Arc::new(move |effects| {
            Arc::new(crate::flows::runtime::FlowRuntime::new(
                store.clone(),
                Arc::new(Stuck),
                effects,
            ))
        });
        let mut runtime = None;
        let (_service, _socket) = tower_lsp_server::LspService::new(|client| {
            let rt = Runtime::new(
                client,
                crate::navigator::NavigatorDeps::new(
                    Arc::new(Silent),
                    dir.path().join("log.jsonl"),
                )
                .with_conversation(feed)
                .with_flows(factory),
            );
            runtime = Some(rt);
            Server
        });
        let runtime = runtime.unwrap();
        let uri = "file:///workspace/test.rs";
        runtime
            .documents
            .lock()
            .await
            .open(uri, "rust".into(), "fn main() {}".into(), 1);
        let ctx = runtime
            .gather(uri, TriggerKind::Save, tokio::time::Instant::now())
            .await
            .unwrap();
        let pending = PendingTrigger::new("runtime", None, uri, 1, "save", ctx.prefilter_inputs());
        let effects = EditorEffects {
            runtime: Arc::downgrade(&runtime),
            context: Mutex::new(Some((ctx, pending))),
            delivered: AtomicBool::new(false),
        };
        for (action, param) in [("wake", "note"), ("inject_note", "text")] {
            effects
                .effect(
                    "editor-review",
                    "capture",
                    &ProposedAction {
                        node_id: None,
                        action: action.into(),
                        params: BTreeMap::from([(param.into(), json!("observation"))]),
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                (
                    "observation".into(),
                    "editor-review".into(),
                    "capture".into(),
                    true
                ),
                (
                    "observation".into(),
                    "editor-review".into(),
                    "capture".into(),
                    false
                ),
            ]
        );
        observed.lock().unwrap().clear();
        {
            let mut documents = runtime.documents.lock().await;
            let doc = documents.get_mut(uri).unwrap();
            doc.last_wake = None;
            documents.change(uri, "fn main() { todo!() }".into(), 2);
        }
        assert!(runtime.evaluate_flows(uri, TriggerKind::Save).await);
        assert_eq!(observed.lock().unwrap().len(), 1);
        assert!(observed.lock().unwrap()[0].3);
        let idle = runtime
            .gather(uri, TriggerKind::Idle, tokio::time::Instant::now())
            .await
            .unwrap();
        assert_eq!(idle.changed_lines, 0);
        assert!(idle.diff.is_empty());
        assert!(idle.since_last_wake_ms < 1000);
        assert!(runtime.evaluate_flows(uri, TriggerKind::Idle).await);
        assert_eq!(observed.lock().unwrap().len(), 1);
        suppress.store(true, Ordering::SeqCst);
        runtime
            .documents
            .lock()
            .await
            .change(uri, "fn main() { panic!() }".into(), 3);
        let ctx = runtime
            .gather(uri, TriggerKind::Save, tokio::time::Instant::now())
            .await
            .unwrap();
        let last_wake = runtime
            .documents
            .lock()
            .await
            .get_mut(uri)
            .unwrap()
            .last_wake;
        let pending = PendingTrigger::new("runtime", None, uri, 3, "save", ctx.prefilter_inputs());
        let effects = EditorEffects {
            runtime: Arc::downgrade(&runtime),
            context: Mutex::new(Some((ctx, pending))),
            delivered: AtomicBool::new(false),
        };
        assert_eq!(
            effects
                .effect(
                    "review",
                    "suppressed",
                    &ProposedAction {
                        node_id: None,
                        action: "wake".into(),
                        params: BTreeMap::from([("note".into(), json!("review"))]),
                    },
                    &CancellationToken::new()
                )
                .await
                .unwrap_err()
                .0,
            "wake cooldown"
        );
        let ctx = runtime
            .gather(uri, TriggerKind::Idle, tokio::time::Instant::now())
            .await
            .unwrap();
        assert!(ctx.changed_lines > 0);
        assert_eq!(
            runtime
                .documents
                .lock()
                .await
                .get_mut(uri)
                .unwrap()
                .last_wake,
            last_wake
        );
    }

    #[test]
    fn disabled_override_and_revoked_workspace_do_not_create_a_second_producer() {
        let root = tempfile::tempdir().unwrap();
        let store = FlowStore::new(root.path().into(), root.path().join("config"));
        let owns = |trigger: &str| {
            store.snapshot().values().any(|f| {
                let h = &f.compiled.definition().flow;
                h.enabled
                    && h.on == trigger
                    && (f.location != Location::Workspace || store.trusted())
            })
        };
        assert!(owns("save"));
        assert!(!owns("diagnostics"));
        let (mut definition, templates) = builtin::definitions()
            .into_iter()
            .find(|(d, _)| d.flow.on == "save")
            .unwrap();
        definition.flow.enabled = false;
        store
            .save(
                "navigator-default-save",
                Location::Global,
                None,
                Content::Graph {
                    definition: Box::new(definition),
                    templates,
                },
            )
            .unwrap();
        store.reload().unwrap();
        assert!(!owns("save"));
        let toml = "[flow]\nversion=1\nname='custom-save'\non='save'\nstart=['note']\n[[node]]\nid='note'\ntype='action'\naction='wake'\n[node.params]\nnote='Review this'\n";
        store
            .save(
                "custom-save",
                Location::Workspace,
                None,
                Content::Toml {
                    toml: toml.into(),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        store.set_trust(true).unwrap();
        store.reload().unwrap();
        assert!(owns("save"));
        store.set_trust(false).unwrap();
        assert!(!owns("save"));
    }
}
