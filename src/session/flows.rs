//! Root-only flow event adapters. Effects bypass command admission entirely.
use crate::flows::runtime::{ContextSnapshot, FlowEffects, FlowRuntime};
use instinct_flow::{ApplyError, BoxFuture, Event, InputValue, ProposedAction};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct Effects {
    pub runtime: std::sync::Weak<FlowRuntime>,
    pub status: Arc<crate::bus::runtime::RuntimeStatus>,
    pub sequencer: crate::bus::sequencer::Sequencer,
    pub turn_end: bool,
}
impl FlowEffects for Effects {
    fn effect<'a>(
        &'a self,
        flow: &'a str,
        run_id: &'a str,
        action: &'a ProposedAction,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), ApplyError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(ApplyError("session stopped".into()));
            }
            let text = match action.action.as_str() {
                "wake" => action.params.get("note"),
                "inject_note" => action.params.get("text"),
                // Remark delivery belongs to the replaceable editor sink.
                "remark" => {
                    return match self.runtime.upgrade() {
                        Some(runtime) => {
                            runtime.effects().effect(flow, run_id, action, cancel).await
                        }
                        None => Err(ApplyError("workspace stopped".into())),
                    }
                }
                "log" => return Ok(()),
                _ => return Err(ApplyError("unknown flow effect".into())),
            }
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ApplyError("missing flow text".into()))?;
            self.status
                .queue_flow_note_report(
                    text.to_string(),
                    flow.to_string(),
                    run_id.to_string(),
                    action.action == "wake",
                    self.turn_end,
                )
                .map_err(ApplyError)
        })
    }
    fn instinct_unavailable(&self) {
        let _ = self.sequencer.publish(crate::bus::sequencer::Candidate::new(crate::bus::event::EventKind::CommandResult,
            json!({"operation_id":ulid::Ulid::new(),"name":"flow_notice","ok":false,"data":{"warning":"instinct unavailable"}})));
    }
}

pub(crate) fn context(handle: &super::service::SessionHandle, session_id: &str) -> ContextSnapshot {
    let selected = handle.selected();
    let mut context = ContextSnapshot {
        workspace: Some(handle.workspace.clone()),
        ..Default::default()
    };
    for (name, value) in [
        ("session.id", json!(session_id)),
        ("session.agent", json!(selected.agent)),
        ("session.model", json!(selected.alias)),
        ("session.workspace", json!(handle.workspace)),
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
    context
}

pub(crate) async fn received(
    handle: &super::service::SessionHandle,
    command: &crate::bus::command::Command,
) {
    if command.from_driver() {
        handle.status.flow_driver_message();
    }
    let Some(runtime) = &handle.flows else {
        return;
    };
    let author = if command.from_driver() {
        "driver"
    } else if command.source_actor_id.starts_with("agent:") {
        "subagent"
    } else {
        "system"
    };
    let mut payload = json!({"command_id":command.command_id.to_string(),"author":author,"text":command.text_content()});
    if let Some(name) = command.content.iter().find_map(|block| match block {
        crate::bus::command::ContentBlock::LoadedSkill { name, .. } => Some(name),
        _ => None,
    }) {
        payload["selected_skill"] = json!({"name":name,"origin":"driver"});
    }
    runtime
        .evaluate_with_effects(
            Event {
                trigger: "message.received".into(),
                payload,
            },
            context(handle, &command.session_id.to_string()),
            handle.session_cancel.clone(),
            Arc::new(Effects {
                runtime: Arc::downgrade(runtime),
                status: handle.status.clone(),
                sequencer: handle.sequencer.clone(),
                turn_end: false,
            }),
        )
        .await;
}

pub(crate) fn hook(
    runtime: &Arc<FlowRuntime>,
    session_id: String,
    selected: Arc<std::sync::Mutex<super::service::SelectedModel>>,
    status: Arc<crate::bus::runtime::RuntimeStatus>,
    sequencer: crate::bus::sequencer::Sequencer,
) -> Arc<dyn crate::agent::flow_hook::ToolCallFlows> {
    runtime.tool_hook_with_effects(
        session_id,
        selected,
        Arc::new(Effects {
            runtime: Arc::downgrade(runtime),
            status,
            sequencer,
            turn_end: false,
        }),
    )
}

pub(crate) async fn settled(handle: super::service::SessionHandle, session_id: String) {
    let Some(runtime) = handle.flows.clone() else {
        return;
    };
    let Ok(subscription) = handle.sequencer.subscribe_live().await else {
        return;
    };
    tokio::spawn(async move {
        let mut receiver = subscription.receiver;
        let mut texts = std::collections::BTreeMap::new();
        loop {
            let event = tokio::select! { biased;
                _ = handle.session_cancel.cancelled() => break,
                event = receiver.recv() => match event { Some(event) => event, None => break },
            };
            let Some(turn_id) = event.payload["turn_id"].as_str() else {
                continue;
            };
            if event.kind == "llm.response" {
                if let Some(text) = event.payload["message"]["text"].as_str() {
                    texts.insert(turn_id.to_string(), text.to_string());
                }
            }
            if event.kind != "turn.end" {
                continue;
            }
            let status = match event.payload["outcome"].as_str() {
                Some("done") => "completed",
                Some("interrupted") => "interrupted",
                Some("error") => "failed",
                _ => {
                    texts.remove(turn_id);
                    continue;
                }
            };
            let payload = json!({"turn_id":turn_id,"text":texts.remove(turn_id).unwrap_or_default(),"status":status});
            let runtime = runtime.clone();
            let context = context(&handle, &session_id);
            let cancel = handle.session_cancel.clone();
            let effects = Arc::new(Effects {
                runtime: Arc::downgrade(&runtime),
                status: handle.status.clone(),
                sequencer: handle.sequencer.clone(),
                turn_end: true,
            });
            tokio::spawn(async move {
                runtime
                    .evaluate_with_effects(
                        Event {
                            trigger: "turn.end".into(),
                            payload,
                        },
                        context,
                        cancel,
                        effects,
                    )
                    .await;
            });
        }
    });
}
