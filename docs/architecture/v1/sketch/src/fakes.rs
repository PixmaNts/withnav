//! Test doubles for the ports: scripted model, echo tool, recording telemetry, in-memory rules.
use crate::core::*;
use crate::engine::authorize::*;
use crate::engine::pipeline::*;
use futures::StreamExt;
use futures::future::{BoxFuture, FutureExt};
use futures::stream::BoxStream;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub type Log = Arc<Mutex<Vec<String>>>;

pub fn tool_call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), args: ToolArguments(args) }
}

/// Plays one prepared event list per `chat` call; running out of script is a protocol error.
pub struct ScriptedProvider {
    script: Mutex<VecDeque<Vec<ModelEvent>>>,
}

impl ScriptedProvider {
    pub fn new(script: Vec<Vec<ModelEvent>>) -> Self {
        Self { script: Mutex::new(script.into()) }
    }
}

impl ModelProvider for ScriptedProvider {
    fn chat<'a>(
        &'a self,
        _req: ChatRequest,
    ) -> BoxFuture<'a, Result<BoxStream<'a, Result<ModelEvent, ModelError>>, ModelError>> {
        async move {
            let events = self.script.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            let events = events.ok_or_else(|| ModelError::Protocol("script exhausted".into()))?;
            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
        .boxed()
    }
}

pub fn text_script(text: &str) -> Vec<ModelEvent> {
    vec![ModelEvent::Text(text.into()), ModelEvent::Done { finish: FinishReason::Stop }]
}

pub fn call_script(text: &str, id: &str, name: &str, args: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Text(text.into()),
        ModelEvent::ToolCallStarted { index: 0, id: id.into(), name: name.into() },
        ModelEvent::ToolCallArgs { index: 0, fragment: args.into() },
        ModelEvent::Done { finish: FinishReason::ToolCalls },
    ]
}

pub fn two_call_script() -> Vec<ModelEvent> {
    let mut events = call_script("", "c1", "echo", r#"{"text":"a"}"#);
    events.pop();
    events.extend([
        ModelEvent::ToolCallStarted { index: 1, id: "c2".into(), name: "echo".into() },
        ModelEvent::ToolCallArgs { index: 1, fragment: r#"{"text":"b"}"#.into() },
        ModelEvent::Done { finish: FinishReason::ToolCalls },
    ]);
    events
}

/// Returns its `text` argument.
pub struct EchoTool {
    descriptor: ToolDescriptor,
}

impl EchoTool {
    pub fn new(name: &str) -> Self {
        Self { descriptor: ToolDescriptor { name: name.into(), class: ToolClass::Write } }
    }
}

impl Tool for EchoTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    fn execute<'a>(&'a self, args: ToolArguments) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        let text = args.0["text"].as_str().unwrap_or_default().to_owned();
        async move { Ok(ToolOutput { status: ToolStatus::Ok, text }) }.boxed()
    }
}

#[derive(Default)]
pub struct RecordingTelemetry(Mutex<Vec<StageObservation>>);

impl RecordingTelemetry {
    pub fn observations(&self) -> Vec<StageObservation> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Telemetry for RecordingTelemetry {
    fn record(&self, observation: StageObservation) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push(observation);
    }
}

/// Rules keyed by tool name; an unlisted tool has no rule at all.
#[derive(Default)]
pub struct InMemoryRules(HashMap<String, Facts>);

impl InMemoryRules {
    fn entry(mut self, name: &str, edit: impl FnOnce(&mut Facts)) -> Self {
        let facts = self.0.entry(name.to_owned()).or_default();
        edit(facts);
        self
    }

    pub fn allow(self, name: &str) -> Self {
        self.entry(name, |f| f.result = RuleResult::Allow)
    }

    pub fn ask(self, name: &str) -> Self {
        self.entry(name, |f| f.result = RuleResult::Ask)
    }

    pub fn deny(self, name: &str) -> Self {
        let rule = format!("deny {name}");
        self.entry(name, |f| f.deny = Some(rule))
    }

    pub fn floor(self, name: &str, floor: Floor) -> Self {
        self.entry(name, |f| f.floor = floor)
    }
}

impl RuleRepository for InMemoryRules {
    fn lookup(&self, call: &ToolCall, _class: ToolClass) -> Result<Facts, RuleError> {
        Ok(self.0.get(&call.name.0).cloned().unwrap_or_default())
    }
}

pub struct FixedProtected(pub bool);

impl ProtectedCheck for FixedProtected {
    fn passes<'a>(&'a self, _view: &'a PermissionView) -> BoxFuture<'a, bool> {
        let pass = self.0;
        async move { pass }.boxed()
    }
}

pub struct FixedFlow(pub FlowAnswer);

impl PermissionFlow for FixedFlow {
    fn answer<'a>(&'a self, _view: &'a PermissionView) -> BoxFuture<'a, FlowAnswer> {
        let answer = self.0;
        async move { answer }.boxed()
    }
}

/// A flow that defers to the rule and remembers every view it was shown.
#[derive(Default)]
pub struct RecordingFlow(Mutex<Vec<PermissionView>>);

impl RecordingFlow {
    pub fn seen(&self) -> Vec<PermissionView> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl PermissionFlow for RecordingFlow {
    fn answer<'a>(&'a self, view: &'a PermissionView) -> BoxFuture<'a, FlowAnswer> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push(view.clone());
        async { FlowAnswer::ApplyRule }.boxed()
    }
}

/// A stage made from a closure; enough for tests that only look at the stage contract.
type StageFn<I, P> = Box<dyn Fn(&I) -> Result<StageStep<P>, StageError> + Send + Sync>;

pub struct FnStage<I, P> {
    descriptor: StageDescriptor,
    f: StageFn<I, P>,
}

impl<I: Send + Sync + 'static, P: Send + 'static> FnStage<I, P> {
    fn build(
        id: &str,
        chain: Chain,
        on_failure: FailurePolicy,
        f: impl Fn(&I) -> Result<StageStep<P>, StageError> + Send + Sync + 'static,
    ) -> SharedStage<I, P> {
        Arc::new(Self {
            descriptor: StageDescriptor { id: id.into(), version: 1, chain, on_failure },
            f: Box::new(f),
        })
    }

    pub fn arc(
        id: &str,
        chain: Chain,
        f: impl Fn(&I) -> Result<StageStep<P>, StageError> + Send + Sync + 'static,
    ) -> SharedStage<I, P> {
        Self::build(id, chain, FailurePolicy::Abort, f)
    }

    pub fn skippable(
        id: &str,
        chain: Chain,
        f: impl Fn(&I) -> Result<StageStep<P>, StageError> + Send + Sync + 'static,
    ) -> SharedStage<I, P> {
        Self::build(id, chain, FailurePolicy::Skip, f)
    }
}

impl<I: Send + Sync, P: Send> Stage<I, P> for FnStage<I, P> {
    fn descriptor(&self) -> &StageDescriptor {
        &self.descriptor
    }

    fn run<'a>(
        &'a self,
        input: &'a I,
        _cx: &'a StageContext,
    ) -> BoxFuture<'a, Result<StageStep<P>, StageError>> {
        let result = (self.f)(input);
        async move { result }.boxed()
    }
}
