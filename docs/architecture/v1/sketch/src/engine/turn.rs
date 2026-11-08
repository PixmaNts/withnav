//! Part of the `withnav-engine` crate: a minimal turn loop over the ports and the chains.
//!
//! Tool calls run serially, in the order the model emitted them:
//! `tool.prepare` → preflight → authorize → dispatch → `tool.result` → transcript.
use crate::core::*;
use crate::engine::authorize::*;
use crate::engine::pipeline::*;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;

/// Per-turn values fixed at the start. The plan is a snapshot: a config reload swaps the
/// [`PlanStore`] for the next turn and leaves this one alone.
pub struct TurnContext {
    pub plan: Arc<PipelinePlan>,
    pub control: Control,
}

#[derive(Debug, PartialEq)]
pub enum TurnOutcome {
    Done,
    /// The turn yielded: the Captain must answer before the held call can continue.
    Waiting(PermissionRequest),
}

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error(transparent)]
    Model(#[from] ModelError),
}

pub struct Engine {
    pub provider: Arc<dyn ModelProvider>,
    pub tools: HashMap<ToolName, Arc<dyn Tool>>,
    pub authorizer: Authorizer,
}

enum CallEnd {
    Settled(ToolOutput),
    Waiting(PermissionRequest),
}

fn failed(status: ToolStatus, text: impl Into<String>) -> CallEnd {
    CallEnd::Settled(ToolOutput { status, text: text.into() })
}

impl Engine {
    pub async fn run_turn(
        &self,
        cx: &TurnContext,
        transcript: &mut Vec<Message>,
    ) -> Result<TurnOutcome, TurnError> {
        loop {
            let calls = self.model_step(transcript).await?;
            if calls.is_empty() {
                return Ok(TurnOutcome::Done);
            }
            for call in calls {
                let id = call.id.clone();
                match self.run_call(cx, transcript, call).await {
                    CallEnd::Settled(output) => {
                        let block = ContentBlock::ToolResult { call: id.clone(), output };
                        transcript.push(message(&id.0, Role::Tool, Origin::Host, block));
                    }
                    CallEnd::Waiting(request) => return Ok(TurnOutcome::Waiting(request)),
                }
            }
        }
    }

    /// One model call: collect the text and the tool calls, append the assistant message.
    async fn model_step(&self, transcript: &mut Vec<Message>) -> Result<Vec<ToolCall>, ModelError> {
        let mut stream = self.provider.chat(ChatRequest { messages: transcript.clone() }).await?;
        let (mut text, mut calls, mut done) =
            (String::new(), Vec::<(ToolCallId, ToolName, String)>::new(), false);
        while let Some(event) = stream.next().await {
            match event? {
                ModelEvent::Text(t) => text.push_str(&t),
                ModelEvent::ToolCallStarted { id, name, .. } => calls.push((id, name, String::new())),
                ModelEvent::ToolCallArgs { index, fragment } => match calls.get_mut(index as usize) {
                    Some(call) => call.2.push_str(&fragment),
                    None => {
                        return Err(ModelError::Protocol(format!("args for unknown call {index}")));
                    }
                },
                ModelEvent::Done { .. } => done = true,
            }
        }
        if !done {
            return Err(ModelError::Truncated);
        }
        let calls: Vec<ToolCall> = calls
            .into_iter()
            .map(|(id, name, raw)| {
                let args = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
                ToolCall { id, name, args: ToolArguments(args) }
            })
            .collect();
        let mut content = vec![ContentBlock::Text(text)];
        content.extend(calls.iter().cloned().map(ContentBlock::ToolCall));
        transcript.push(Message {
            id: "assistant".into(),
            role: Role::Assistant,
            origin: Origin::Host,
            content,
        });
        Ok(calls)
    }

    async fn run_call(&self, cx: &TurnContext, transcript: &[Message], proposed: ToolCall) -> CallEnd {
        let runner = &self.authorizer.runner;
        let stage_cx = StageContext { call: Some(proposed.id.clone()) };
        let plan = &cx.plan;

        // Rewrites happen here, before authorization; the decision only ever sees the result.
        let call = match runner.run_chain(&plan.tool_prepare, proposed, &stage_cx).await {
            Ok(ChainResult::Completed(call)) => call,
            Ok(ChainResult::Halted(StageHalt::Reject { reason })) => {
                return failed(ToolStatus::Denied, reason);
            }
            Ok(ChainResult::Halted(_)) => return failed(ToolStatus::Error, "interrupted"),
            Err(e) => return failed(ToolStatus::Error, e.to_string()),
        };
        // Preflight: the rewritten call must name a tool that exists.
        let Some(tool) = self.tools.get(&call.name) else {
            return failed(ToolStatus::Error, format!("unknown tool {}", call.name.0));
        };
        let class = tool.descriptor().class;
        let repeats = identical_run(transcript, &call);

        let permit = match self.authorizer.authorize(plan, &call, class, cx.control, repeats).await {
            Authorization::Run(permit) => permit,
            Authorization::Ask(request) => return CallEnd::Waiting(request),
            Authorization::Deny(denial) => return failed(ToolStatus::Denied, denial.message),
        };
        // Dispatch runs what the permit froze, not a copy a stage could have touched.
        let output = match tool.execute(permit.call().args.clone()).await {
            Ok(output) => output,
            Err(e) => ToolOutput { status: ToolStatus::Error, text: e.to_string() },
        };
        let draft = ResultDraft { call: call.id.clone(), output };
        match runner.run_chain(&plan.tool_result, draft, &stage_cx).await {
            Ok(ChainResult::Completed(draft)) => CallEnd::Settled(draft.output),
            Ok(ChainResult::Halted(_)) => failed(ToolStatus::Error, "result withheld"),
            Err(e) => failed(ToolStatus::Error, e.to_string()),
        }
    }
}

fn message(id: &str, role: Role, origin: Origin, block: ContentBlock) -> Message {
    Message { id: id.into(), role, origin, content: vec![block] }
}

/// How many times in a row this exact call (name and arguments) was proposed, this one included.
/// The assistant message holding `call` is already in the transcript.
fn identical_run(transcript: &[Message], call: &ToolCall) -> u32 {
    let calls = transcript.iter().flat_map(|m| &m.content).filter_map(|b| match b {
        ContentBlock::ToolCall(c) => Some(c),
        _ => None,
    });
    let same = |c: &&ToolCall| c.name == call.name && c.args == call.args;
    let mut n = 0;
    for c in calls.collect::<Vec<_>>().iter().rev() {
        if !same(c) {
            break;
        }
        n += 1;
    }
    n.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::*;
    use serde_json::json;

    struct Rig {
        engine: Engine,
        telemetry: Arc<RecordingTelemetry>,
    }

    fn build(provider: ScriptedProvider, rules: InMemoryRules) -> Rig {
        let telemetry = Arc::new(RecordingTelemetry::default());
        let engine = Engine {
            provider: Arc::new(provider),
            tools: HashMap::from([(
                ToolName::from("echo"),
                Arc::new(EchoTool::new("echo")) as Arc<dyn Tool>,
            )]),
            authorizer: Authorizer {
                runner: Arc::new(Runner::new(telemetry.clone())),
                rules: Arc::new(rules),
                protected: Arc::new(FixedProtected(true)),
            },
        };
        Rig { engine, telemetry }
    }

    fn user(text: &str) -> Vec<Message> {
        vec![message("u1", Role::User, Origin::Captain, ContentBlock::Text(text.into()))]
    }

    fn results(t: &[Message]) -> Vec<(ToolCallId, ToolOutput)> {
        t.iter()
            .flat_map(|m| m.content.clone())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { call, output } => Some((call, output)),
                _ => None,
            })
            .collect()
    }

    fn ctx(plan: PipelinePlan, control: Control) -> TurnContext {
        TurnContext { plan: Arc::new(plan), control }
    }

    #[tokio::test]
    async fn end_to_end_text_tool_call_result_then_second_model_call() {
        let provider = ScriptedProvider::new(vec![
            call_script("let me echo", "c1", "echo", r#"{"text":"hello"}"#),
            text_script("all done"),
        ]);
        let rig = build(provider, InMemoryRules::default().allow("echo"));
        let mut t = user("go");
        let out = rig.engine.run_turn(&ctx(PipelinePlan::default(), Control::Normal), &mut t).await.unwrap();
        assert_eq!(out, TurnOutcome::Done);
        assert_eq!(results(&t), [("c1".into(), ToolOutput { status: ToolStatus::Ok, text: "hello".into() })]);
        // user, assistant(call), tool result, assistant(final).
        assert_eq!(t.len(), 4);
        assert_eq!(t[3].content, [ContentBlock::Text("all done".into())]);
    }

    #[tokio::test]
    async fn an_unruled_call_yields_instead_of_running() {
        let provider = ScriptedProvider::new(vec![call_script("", "c1", "echo", r#"{"text":"x"}"#)]);
        let rig = build(provider, InMemoryRules::default());
        let mut t = user("go");
        let out = rig.engine.run_turn(&ctx(PipelinePlan::default(), Control::Normal), &mut t).await.unwrap();
        assert!(matches!(out, TurnOutcome::Waiting(r) if r.call == "c1".into()));
        assert!(results(&t).is_empty());
    }

    #[tokio::test]
    async fn a_prepare_patch_rewrites_the_call_before_authorization() {
        let provider = ScriptedProvider::new(vec![
            call_script("", "c1", "echo", r#"{"text":"raw"}"#),
            text_script("ok"),
        ]);
        let rig = build(provider, InMemoryRules::default().allow("echo"));
        let rewrite = FnStage::arc("rewrite", Chain::ToolPrepare, |_: &ToolCall| {
            Ok(StageStep::Continue(ToolPatch {
                name: None,
                args: Some(ToolArguments(json!({"text": "rewritten"}))),
            }))
        });
        let flow = Arc::new(RecordingFlow::default());
        let plan = PipelinePlan::default()
            .prepare(rewrite)
            .unwrap()
            .review(ReviewStage::Flow(flow.clone()))
            .unwrap();
        let mut t = user("go");
        rig.engine.run_turn(&ctx(plan, Control::Normal), &mut t).await.unwrap();
        assert_eq!(flow.seen()[0].call.args, ToolArguments(json!({"text": "rewritten"})));
        assert_eq!(results(&t)[0].1.text, "rewritten");
        // A rewrite into a denied tool is judged under the new name.
        let provider = ScriptedProvider::new(vec![call_script("", "c2", "echo", "{}"), text_script("ok")]);
        let rig2 = build(provider, InMemoryRules::default().allow("echo").deny("echo2"));
        let rename = FnStage::arc("rename", Chain::ToolPrepare, |_: &ToolCall| {
            Ok(StageStep::Continue(ToolPatch { name: Some("echo2".into()), args: None }))
        });
        let mut t = user("go");
        let plan = PipelinePlan::default().prepare(rename).unwrap();
        rig2.engine.run_turn(&ctx(plan, Control::Normal), &mut t).await.unwrap();
        // Preflight refuses the unknown rewritten tool before the deny rule is even needed.
        assert_eq!(results(&t)[0].1.status, ToolStatus::Error);
    }

    #[tokio::test]
    async fn a_result_patch_redacts_the_text_and_leaves_id_and_status_alone() {
        let provider = ScriptedProvider::new(vec![
            call_script("", "c1", "echo", r#"{"text":"key=SECRET"}"#),
            text_script("ok"),
        ]);
        let rig = build(provider, InMemoryRules::default().allow("echo"));
        let redact = FnStage::arc("redact", Chain::ToolResult, |d: &ResultDraft| {
            Ok(StageStep::Continue(ResultPatch { text: Some(d.output.text.replace("SECRET", "[redacted]")) }))
        });
        let mut t = user("go");
        rig.engine
            .run_turn(&ctx(PipelinePlan::default().result(redact).unwrap(), Control::Normal), &mut t)
            .await
            .unwrap();
        let (id, out) = results(&t).remove(0);
        assert_eq!((id, out.status, out.text.as_str()), ("c1".into(), ToolStatus::Ok, "key=[redacted]"));
        assert_eq!(rig.telemetry.observations().len(), 1);
    }

    #[tokio::test]
    async fn a_plan_swapped_mid_turn_does_not_affect_the_running_turn() {
        let provider = ScriptedProvider::new(vec![two_call_script(), text_script("ok")]);
        let rig = build(provider, InMemoryRules::default().allow("echo"));
        let store = Arc::new(crate::engine::pipeline::PlanStore::new(PipelinePlan::default()));
        let runs = Log::default();
        let (swap_store, log) = (store.clone(), runs.clone());
        let swapper = FnStage::arc("swapper", Chain::ToolPrepare, move |_: &ToolCall| {
            log.lock().unwrap().push("ran".into());
            swap_store.swap(PipelinePlan::default());
            Ok(StageStep::Continue(ToolPatch::default()))
        });
        store.swap(PipelinePlan::default().prepare(swapper).unwrap());
        let cx = TurnContext { plan: store.snapshot(), control: Control::Normal };
        let mut t = user("go");
        rig.engine.run_turn(&cx, &mut t).await.unwrap();
        // The swap happened during the first call; the second call still ran the old stage.
        assert_eq!(runs.lock().unwrap().len(), 2);
        assert_eq!(store.snapshot().tool_prepare.len(), 0);
        assert_eq!(cx.plan.tool_prepare.len(), 1);
    }
}
