//! OR-activated concurrent scheduling with whole-run action selection.
use crate::{protocol::*, *};
use futures_util::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputValue {
    pub value: Option<serde_json::Value>,
    pub cut: bool,
}
#[derive(Debug, Clone, thiserror::Error)]
#[error("input collection failed: {0}")]
pub struct InputError(pub String);
pub trait InputSource: Send + Sync {
    fn collect<'a>(
        &'a self,
        names: &'a [String],
        cancel: &'a CancellationToken,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<BTreeMap<String, InputValue>, InputError>>;
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub trigger: String,
    pub payload: serde_json::Value,
}
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub deadline: Instant,
    pub run_id: String,
}
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub result: RunResult,
    pub trace: RunRecord,
}
#[derive(Debug, Clone)]
pub enum RunResult {
    Success {
        actions: Vec<ProposedAction>,
    },
    Skipped,
    Failure {
        kind: FailureKind,
        message: String,
        fallback_actions: Vec<ProposedAction>,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Timeout,
    ProviderError,
    InvalidAnswer,
    Expression,
    Input,
    Cancelled,
    Limit,
}
impl FailureKind {
    fn name(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ProviderError => "provider_error",
            Self::InvalidAnswer => "invalid_answer",
            Self::Expression => "expression",
            Self::Input => "input",
            Self::Cancelled => "cancelled",
            Self::Limit => "limit",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedAction {
    pub node_id: Option<String>,
    pub action: String,
    pub params: BTreeMap<String, serde_json::Value>,
}
pub trait ActionExecutor: Send + Sync {
    fn apply<'a>(
        &'a self,
        event: &'a Event,
        actions: &'a [ProposedAction],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ApplyReport, ApplyError>>;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Running,
    Completed,
    Unreachable,
}
type Failure = (FailureKind, String);
struct Evaluation<'a> {
    compiled: &'a CompiledDefinition,
    trace: RunRecord,
    status: Vec<Status>,
    cache: BTreeMap<String, InputValue>,
    bytes: usize,
    deadline: Instant,
}
impl<'a> Evaluation<'a> {
    fn new(
        compiled: &'a CompiledDefinition,
        event: &Event,
        identity: ClientIdentity,
        options: RunOptions,
    ) -> Self {
        let budget = if compiled.definition.flow.blocking {
            compiled.definition.flow.budget_ms.unwrap_or(2000)
        } else {
            compiled.catalogue.limits.async_deadline_ms.min(5000)
        };
        let deadline = options
            .deadline
            .min(Instant::now() + Duration::from_millis(budget));
        let nodes = compiled
            .definition
            .node
            .iter()
            .map(|n| NodeRecord {
                id: n.id.clone(),
                status: "not_reached".into(),
                port: None,
                duration_ms: 0,
                answer: None,
                result: None,
                variables: BTreeMap::new(),
                batch_id: None,
                error: None,
            })
            .collect();
        Self {
            compiled,
            trace: RunRecord {
                version: 1,
                run_id: options.run_id,
                flow_name: compiled.definition.flow.name.clone(),
                definition_digest: compiled.digest.clone(),
                mode: "live".into(),
                started_at: crate::record::timestamp(),
                duration_ms: 0,
                budget_ms: budget,
                event: event.clone(),
                identity,
                inputs: BTreeMap::new(),
                nodes,
                batches: Vec::new(),
                status: "success".into(),
                failure: None,
                actions: Vec::new(),
                fallback_actions: Vec::new(),
                usage: Some(DecisionUsage::default()),
                cost_usd: None,
                application: ApplyReport::default(),
            },
            status: vec![Status::Pending; compiled.definition.node.len()],
            cache: BTreeMap::new(),
            bytes: 0,
            deadline,
        }
    }
    fn check(&self, cancel: &CancellationToken) -> Result<(), Failure> {
        if cancel.is_cancelled() {
            Err((FailureKind::Cancelled, "run cancelled".into()))
        } else if Instant::now() >= self.deadline {
            Err((FailureKind::Timeout, "run deadline exhausted".into()))
        } else {
            Ok(())
        }
    }
    fn ready(&mut self) -> Vec<usize> {
        let mut ready = Vec::new();
        for &i in &self.compiled.order {
            if self.status[i] != Status::Pending {
                continue;
            }
            let node = &self.compiled.definition.node[i];
            let start = self.compiled.definition.flow.start.contains(&node.id);
            let active = start
                || self.compiled.predecessors[i].iter().any(|(p, port)| {
                    self.status[*p] == Status::Completed
                        && self.trace.nodes[*p].port.as_ref() == Some(port)
                });
            let incoming_settled = self.compiled.predecessors[i]
                .iter()
                .all(|(p, _)| matches!(self.status[*p], Status::Completed | Status::Unreachable));
            if !active && incoming_settled {
                self.status[i] = Status::Unreachable;
                continue;
            }
            let data_settled = node
                .inputs
                .values()
                .filter_map(|r| crate::validation::upstream(r))
                .all(|id| {
                    self.compiled
                        .definition
                        .node
                        .iter()
                        .position(|n| n.id == id)
                        .is_some_and(|j| {
                            matches!(self.status[j], Status::Completed | Status::Unreachable)
                        })
                });
            if active && data_settled {
                ready.push(i);
            }
        }
        ready
    }
    fn resolve(&self, reference: &str) -> InputValue {
        if let Some(value) = self.cache.get(reference) {
            return value.clone();
        }
        let absent = || InputValue {
            value: None,
            cut: false,
        };
        if let Some(name) = reference.strip_prefix("event.") {
            return InputValue {
                value: self
                    .trace
                    .event
                    .payload
                    .get(name)
                    .filter(|v| !v.is_null())
                    .cloned(),
                cut: false,
            };
        }
        if let Some(name) = reference.strip_prefix("constant.") {
            return InputValue {
                value: self.compiled.definition.constants.get(name).cloned(),
                cut: false,
            };
        }
        if let Some(id) = crate::validation::upstream(reference) {
            let Some(index) = self
                .compiled
                .definition
                .node
                .iter()
                .position(|n| n.id == id)
            else {
                return absent();
            };
            let record = &self.trace.nodes[index];
            if self.status[index] != Status::Completed {
                return absent();
            }
            let value = if reference.starts_with("result.") {
                record.result.map(serde_json::Value::Bool)
            } else if reference.starts_with("variable.") {
                reference
                    .split('.')
                    .nth(2)
                    .and_then(|name| record.variables.get(name))
                    .cloned()
            } else {
                record.answer.as_ref().and_then(|answer| {
                    let suffix = reference
                        .strip_prefix(&format!("answer.{id}"))
                        .unwrap_or_default();
                    match (suffix, answer) {
                        ("", Answer::Noul { noul }) => Some(serde_json::json!(noul)),
                        ("", Answer::Choice { choice, .. }) => Some(choice.clone().into()),
                        ("", Answer::Score { score, .. }) => Some(serde_json::json!(score)),
                        (
                            ".confidence",
                            Answer::Choice { confidence, .. } | Answer::Score { confidence, .. },
                        ) => Some(serde_json::json!(confidence)),
                        (
                            suffix,
                            Answer::Choice { probabilities, .. }
                            | Answer::Score { probabilities, .. },
                        ) => suffix
                            .strip_prefix(".probabilities.")
                            .and_then(|key| probabilities.get(key))
                            .map(|p| serde_json::json!(p)),
                        _ => None,
                    }
                })
            };
            return InputValue { value, cut: false };
        }
        self.cache.get(reference).cloned().unwrap_or_else(absent)
    }
    fn aliases(&self, map: &BTreeMap<String, String>) -> BTreeMap<String, InputValue> {
        map.iter()
            .map(|(alias, r)| (alias.clone(), self.resolve(r)))
            .collect()
    }
    async fn collect(
        &mut self,
        map: &BTreeMap<String, String>,
        source: &dyn InputSource,
        cancel: &CancellationToken,
    ) -> Result<(), Failure> {
        self.check(cancel)?;
        self.prepare_local(map)?;
        let names: Vec<_> = map
            .values()
            .filter(|r| {
                !r.starts_with("event.")
                    && !r.starts_with("constant.")
                    && crate::validation::upstream(r).is_none()
                    && !self.cache.contains_key(*r)
            })
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if names.is_empty() {
            return Ok(());
        }
        let collected = tokio::select! {biased;
            _=cancel.cancelled()=>return Err((FailureKind::Cancelled,"run cancelled".into())),
            _=tokio::time::sleep_until(self.deadline.into())=>return Err((FailureKind::Timeout,"input deadline exhausted".into())),
            result=source.collect(&names,cancel,self.deadline)=>result.map_err(|_|(FailureKind::Input,"host input collection failed".into()))?,
        };
        self.check(cancel)?;
        for name in names {
            let mut value = collected.get(&name).cloned().unwrap_or(InputValue {
                value: None,
                cut: false,
            });
            let spec = self
                .compiled
                .catalogue
                .inputs
                .iter()
                .find(|s| s.name == name)
                .expect("validated host reference");
            if value
                .value
                .as_ref()
                .is_some_and(|v| !v.is_null() && !spec.value_type.matches(v))
            {
                return Err((FailureKind::Input, "host input has invalid type".into()));
            }
            if value.value.as_ref().is_some_and(|v| v.is_null()) {
                value.value = None;
            }
            self.bound(&mut value, &name)?;
            self.trace.inputs.insert(name.clone(), value.clone());
            self.cache.insert(name, value);
        }
        Ok(())
    }
    fn prepare_local(&mut self, map: &BTreeMap<String, String>) -> Result<(), Failure> {
        let references: BTreeSet<_> = map
            .values()
            .filter(|r| {
                (r.starts_with("event.") || r.starts_with("constant."))
                    && !self.cache.contains_key(*r)
            })
            .cloned()
            .collect();
        for reference in references {
            let mut value = self.resolve(&reference);
            self.bound(&mut value, &reference)?;
            self.cache.insert(reference, value);
        }
        Ok(())
    }
    fn bound(&mut self, value: &mut InputValue, reference: &str) -> Result<(), Failure> {
        let expression_reads = self.compiled.definition.node.iter().any(|n| {
            matches!(n.kind, NodeKind::Expression { .. })
                && n.inputs.values().any(|r| r == reference)
        }) || self
            .compiled
            .definition
            .flow
            .inputs
            .values()
            .any(|r| r == reference);
        if value.cut && expression_reads {
            return Err((
                FailureKind::Limit,
                "expression input was already truncated".into(),
            ));
        }
        let Some(v) = &value.value else {
            return Ok(());
        };
        let text = v
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string());
        let cap = self.compiled.catalogue.limits.input_bytes.min(16384).min(
            self.compiled
                .catalogue
                .limits
                .run_input_bytes
                .min(65536)
                .saturating_sub(self.bytes),
        );
        if text.len() > cap {
            if expression_reads {
                return Err((
                    FailureKind::Limit,
                    "expression scalar cannot be truncated".into(),
                ));
            }
            let mut boundary = cap;
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            value.value = Some(text[..boundary].into());
            value.cut = true;
            self.bytes += boundary;
        } else {
            self.bytes += text.len();
        }
        Ok(())
    }
    fn state(&self, i: usize) -> Result<String, Failure> {
        let node = &self.compiled.definition.node[i];
        let values = self.aliases(&node.inputs);
        let template = node
            .kind
            .template()
            .and_then(|name| self.compiled.templates.get(name))
            .map(String::as_str);
        let state = crate::rendering::state(&values, template)
            .map_err(|_| (FailureKind::Input, "state rendering failed".into()))?;
        if state.len() > self.compiled.catalogue.limits.state_bytes.min(65536) {
            return Err((
                FailureKind::Limit,
                "rendered state exceeds byte limit".into(),
            ));
        }
        Ok(state)
    }
    fn deterministic(&mut self, i: usize) -> Result<(), Failure> {
        let node = &self.compiled.definition.node[i];
        let inputs = self.aliases(&node.inputs);
        let start = Instant::now();
        let port = match &node.kind {
            NodeKind::Expression { assign, .. } => {
                let mut values = inputs
                    .iter()
                    .map(|(alias, v)| {
                        (
                            alias.clone(),
                            crate::expression::from_json(v.value.as_ref()),
                        )
                    })
                    .collect();
                for name in assign.keys() {
                    let expression =
                        &self.compiled.expressions[&format!("node[{}].assign.{name}", node.id)];
                    let value = expression
                        .evaluate(&values)
                        .map_err(|e| (FailureKind::Expression, e.message))?;
                    self.trace.nodes[i]
                        .variables
                        .insert(name.clone(), crate::expression::to_json(value.clone()));
                    values.insert(name.clone(), value);
                }
                let result = self.compiled.expressions[&format!("node[{}].condition", node.id)]
                    .boolean(&values)
                    .map_err(|e| (FailureKind::Expression, e.message))?;
                self.trace.nodes[i].result = Some(result);
                if result { "true" } else { "false" }.into()
            }
            NodeKind::Action { action, params } => {
                let params = crate::rendering::params(params, &inputs)
                    .map_err(|e| (FailureKind::Input, e.message))?;
                self.trace.actions.push(ProposedAction {
                    node_id: Some(node.id.clone()),
                    action: action.clone(),
                    params,
                });
                "then".into()
            }
            _ => {
                return Err((
                    FailureKind::Limit,
                    "scheduler selected nondeterministic node".into(),
                ))
            }
        };
        self.complete(i, port);
        self.trace.nodes[i].duration_ms = elapsed(start);
        Ok(())
    }
    fn complete(&mut self, i: usize, port: String) {
        self.status[i] = Status::Completed;
        self.trace.nodes[i].status = "completed".into();
        self.trace.nodes[i].port = Some(port);
    }
    fn answer(&mut self, i: usize, answer: Answer) -> Result<(), Failure> {
        let port = match (&self.compiled.definition.node[i].kind, &answer) {
            (NodeKind::Noul { threshold, .. }, Answer::Noul { noul }) => if *noul
                >= crate::validation::threshold(threshold, &self.compiled.definition)
                    .expect("validated threshold")
            {
                "yes"
            } else {
                "no"
            }
            .into(),
            (NodeKind::Choice { .. }, Answer::Choice { choice, .. }) => choice.clone(),
            (
                NodeKind::Score {
                    levels, at_least, ..
                },
                Answer::Score { score, .. },
            ) => if *score
                >= levels
                    .iter()
                    .position(|l| l == at_least)
                    .expect("validated level") as f64
            {
                "at_least"
            } else {
                "below"
            }
            .into(),
            _ => return Err((FailureKind::InvalidAnswer, "answer type mismatch".into())),
        };
        self.trace.nodes[i].answer = Some(answer);
        self.complete(i, port);
        Ok(())
    }
    async fn fallback(
        &mut self,
        kind: FailureKind,
        message: &str,
        source: &dyn InputSource,
        cancel: &CancellationToken,
    ) {
        let f = &self.compiled.definition;
        let chain = f
            .fallback
            .as_ref()
            .and_then(|fallback| {
                match kind {
                    FailureKind::Timeout => fallback.on_timeout.as_ref(),
                    FailureKind::ProviderError => fallback.on_provider_error.as_ref(),
                    FailureKind::InvalidAnswer => fallback.on_invalid_answer.as_ref(),
                    _ => None,
                }
                .or(fallback.on_failure.as_ref())
            })
            .or_else(|| {
                if f.flow.blocking {
                    self.compiled
                        .catalogue
                        .trigger(&f.flow.on)
                        .and_then(|t| t.default_fallback.as_ref())
                } else {
                    None
                }
            })
            .cloned();
        let Some(chain) = chain else {
            return;
        };
        let map = f
            .fallback
            .as_ref()
            .map(|f| f.inputs.clone())
            .unwrap_or_default();
        if self.prepare_local(&map).is_err() {
            return;
        }
        // Failure selection must not reset the run deadline; exhausted inputs remain absent.
        let _ = self.collect(&map, source, cancel).await;
        let mut values = self.aliases(&map);
        values.insert(
            "failure_kind".into(),
            InputValue {
                value: Some(kind.name().into()),
                cut: false,
            },
        );
        values.insert(
            "failure_message".into(),
            InputValue {
                value: Some(message.into()),
                cut: false,
            },
        );
        let mut actions = Vec::new();
        let mut next = Some(&chain);
        while let Some(item) = next {
            let Ok(params) = crate::rendering::params(&item.params, &values) else {
                return;
            };
            actions.push(ProposedAction {
                node_id: None,
                action: item.action.clone(),
                params,
            });
            next = item.then.as_deref();
        }
        self.trace.fallback_actions = actions;
    }
    fn finish(mut self, status: Result<bool, Failure>, start: Instant) -> RunOutcome {
        self.trace.duration_ms = elapsed(start);
        self.trace.usage =
            self.trace
                .batches
                .iter()
                .try_fold(DecisionUsage::default(), |sum, batch| {
                    batch.usage.map(|usage| DecisionUsage {
                        input_tokens: sum.input_tokens.saturating_add(usage.input_tokens),
                        output_tokens: sum.output_tokens.saturating_add(usage.output_tokens),
                    })
                });
        self.trace.actions.sort_by_key(|a| {
            a.node_id
                .as_ref()
                .and_then(|id| {
                    self.compiled
                        .order
                        .iter()
                        .position(|i| self.compiled.definition.node[*i].id == *id)
                })
                .unwrap_or(usize::MAX)
        });
        self.trace.batches.sort_by(|a, b| a.id.cmp(&b.id));
        let result = match status {
            Ok(true) => RunResult::Success {
                actions: self.trace.actions.clone(),
            },
            Ok(false) => {
                self.trace.status = "skipped".into();
                RunResult::Skipped
            }
            Err((kind, message)) => {
                self.trace.status = "failure".into();
                self.trace.actions.clear();
                self.trace.failure = Some(FailureRecord {
                    kind,
                    message: message.clone(),
                });
                for (i, status) in self.status.iter().enumerate() {
                    if *status == Status::Running {
                        self.trace.nodes[i].status = "failed".into();
                        self.trace.nodes[i].error = Some(message.clone());
                    }
                }
                RunResult::Failure {
                    kind,
                    message,
                    fallback_actions: self.trace.fallback_actions.clone(),
                }
            }
        };
        RunOutcome {
            result,
            trace: self.trace,
        }
    }
}
fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}
fn decision_failure(error: DecisionError) -> Failure {
    let kind = match error {
        DecisionError::Timeout => FailureKind::Timeout,
        DecisionError::Cancelled => FailureKind::Cancelled,
        DecisionError::ProviderError => FailureKind::ProviderError,
        DecisionError::InvalidAnswer(_) => FailureKind::InvalidAnswer,
        DecisionError::Limit => FailureKind::Limit,
    };
    (kind, error.to_string())
}
pub async fn run(
    definition: &CompiledDefinition,
    event: &Event,
    inputs: &dyn InputSource,
    client: &dyn DecisionClient,
    options: RunOptions,
    cancel: &CancellationToken,
) -> RunOutcome {
    let start = Instant::now();
    let mut evaluation = Evaluation::new(definition, event, client.identity(), options);
    let child = cancel.child_token();
    let result = execute(&mut evaluation, inputs, client, &child).await;
    child.cancel();
    if let Err((kind, message)) = &result {
        evaluation.fallback(*kind, message, inputs, cancel).await;
    }
    evaluation.finish(result, start)
}
async fn execute(
    e: &mut Evaluation<'_>,
    inputs: &dyn InputSource,
    client: &dyn DecisionClient,
    cancel: &CancellationToken,
) -> Result<bool, Failure> {
    let definition = &e.compiled.definition;
    if !definition.flow.enabled || e.trace.event.trigger != definition.flow.on {
        return Ok(false);
    }
    e.check(cancel)?;
    let trigger = e
        .compiled
        .catalogue
        .trigger(&definition.flow.on)
        .expect("validated trigger");
    let Some(payload) = e.trace.event.payload.as_object() else {
        return Err((FailureKind::Input, "event payload must be object".into()));
    };
    for (name, value) in payload {
        if !trigger.payload_fields.iter().any(|field| {
            field.name == *name
                && (field.value_type.matches(value) || (!field.required && value.is_null()))
        }) {
            return Err((FailureKind::Input, "unknown or invalid event field".into()));
        }
    }
    for field in &trigger.payload_fields {
        if field.required
            && !payload
                .get(&field.name)
                .is_some_and(|v| field.value_type.matches(v))
        {
            return Err((FailureKind::Input, "missing required event field".into()));
        }
    }
    let map = definition.flow.inputs.clone();
    e.collect(&map, inputs, cancel).await?;
    if let Some(when) = e.compiled.expressions.get("flow.when") {
        let values = e
            .aliases(&map)
            .iter()
            .map(|(alias, v)| {
                (
                    alias.clone(),
                    crate::expression::from_json(v.value.as_ref()),
                )
            })
            .collect();
        if !when
            .boolean(&values)
            .map_err(|d| (FailureKind::Expression, d.message))?
        {
            return Ok(false);
        }
    }
    type Completed = (
        usize,
        Vec<usize>,
        Result<DecisionResponse, DecisionError>,
        u64,
    );
    let semaphore =
        tokio::sync::Semaphore::new(e.compiled.catalogue.limits.concurrent_calls.min(4));
    let mut calls: FuturesUnordered<BoxFuture<'_, Completed>> = FuturesUnordered::new();
    let mut decision_count = 0;
    loop {
        e.check(cancel)?;
        loop {
            let ready = e.ready();
            let deterministic: Vec<_> = ready
                .into_iter()
                .filter(|i| e.compiled.definition.node[*i].kind.question().is_none())
                .collect();
            if deterministic.is_empty() {
                break;
            }
            for i in deterministic {
                let map = e.compiled.definition.node[i].inputs.clone();
                e.collect(&map, inputs, cancel).await?;
                e.check(cancel)?;
                if let Err(error) = e.deterministic(i) {
                    e.trace.nodes[i].status = "failed".into();
                    e.trace.nodes[i].error = Some(error.1.clone());
                    return Err(error);
                }
            }
        }
        let ready = e.ready();
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for i in ready {
            if e.compiled.definition.node[i].kind.question().is_none() {
                continue;
            }
            let map = e.compiled.definition.node[i].inputs.clone();
            e.collect(&map, inputs, cancel).await?;
            let state = e.state(i)?;
            groups.entry(state).or_default().push(i);
        }
        for (state, nodes) in groups {
            decision_count += nodes.len();
            if decision_count > e.compiled.catalogue.limits.decisions_per_run.min(16) {
                return Err((FailureKind::Limit, "decision budget exhausted".into()));
            }
            let questions: Questions = nodes
                .iter()
                .map(|i| {
                    let n = &e.compiled.definition.node[*i];
                    (n.id.clone(), n.kind.question().expect("decision node"))
                })
                .collect();
            let batch = e.trace.batches.len();
            let id = format!("b{}", batch + 1);
            for i in &nodes {
                e.status[*i] = Status::Running;
                e.trace.nodes[*i].batch_id = Some(id.clone());
            }
            e.trace.batches.push(BatchRecord {
                id,
                node_ids: questions.keys().cloned().collect(),
                state: state.clone(),
                questions: questions.clone(),
                duration_ms: 0,
                usage: None,
                cost_usd: None,
            });
            let semaphore = &semaphore;
            let deadline = e.deadline;
            let call_ms = e.compiled.catalogue.limits.call_deadline_ms.min(2000);
            calls.push(Box::pin(async move {
                let start=Instant::now();
                let work=async {
                    let _permit=semaphore.acquire().await.map_err(|_|DecisionError::Cancelled)?;
                    let call_deadline=deadline.min(Instant::now()+Duration::from_millis(call_ms));
                    let response=tokio::time::timeout_at(call_deadline.into(),client.decide(DecisionRequest {state:&state,questions:&questions},cancel,call_deadline)).await.map_err(|_|DecisionError::Timeout)??;
                    if cancel.is_cancelled(){return Err(DecisionError::Cancelled);}
                    if Instant::now()>=call_deadline {return Err(DecisionError::Timeout);}
                    validate_answers(&questions,&response.answers)?;Ok(response)
                };
                let response=tokio::select! {biased;_=cancel.cancelled()=>Err(DecisionError::Cancelled),_=tokio::time::sleep_until(deadline.into())=>Err(DecisionError::Timeout),r=work=>r};
                (batch,nodes,response,elapsed(start))
            }));
        }
        if calls.is_empty() {
            if e.status.contains(&Status::Pending) {
                return Err((FailureKind::Limit, "scheduler made no progress".into()));
            }
            return Ok(true);
        }
        let (batch, nodes, response, duration) = tokio::select! {biased;
            _=cancel.cancelled()=>return Err((FailureKind::Cancelled,"run cancelled".into())),
            _=tokio::time::sleep_until(e.deadline.into())=>return Err((FailureKind::Timeout,"run deadline exhausted".into())),
            result=calls.next()=>result.expect("nonempty call set"),
        };
        e.check(cancel)?;
        e.trace.batches[batch].duration_ms = duration;
        let response = response.map_err(decision_failure)?;
        if serde_json::to_vec(&response).map_or(true, |bytes| {
            bytes.len() > e.compiled.catalogue.limits.response_bytes.min(262144)
        }) {
            return Err((
                FailureKind::Limit,
                "provider response exceeds host ceiling".into(),
            ));
        }
        e.trace.batches[batch].usage = response.usage;
        e.trace.usage = match (e.trace.usage, response.usage) {
            (Some(a), Some(b)) => Some(DecisionUsage {
                input_tokens: a.input_tokens.saturating_add(b.input_tokens),
                output_tokens: a.output_tokens.saturating_add(b.output_tokens),
            }),
            _ => None,
        };
        for i in nodes {
            let id = &e.compiled.definition.node[i].id;
            e.answer(i, response.answers[id].clone())?;
            e.trace.nodes[i].duration_ms = duration;
        }
    }
}
pub fn combine_verdicts(
    outcomes: &[RunOutcome],
    catalogue: &Catalogue,
) -> Result<Option<ProposedAction>, Vec<Diagnostic>> {
    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    for outcome in outcomes {
        let actions = match &outcome.result {
            RunResult::Success { actions } => actions,
            RunResult::Failure {
                fallback_actions, ..
            } => fallback_actions,
            RunResult::Skipped => continue,
        };
        for action in actions {
            match catalogue.action(&action.action) {
                Some(spec) if spec.kind == ActionKind::Verdict => {
                    candidates.push((spec.priority.unwrap_or(0), &outcome.trace.flow_name, action))
                }
                Some(_) => {}
                None => errors.push(Diagnostic::new(
                    "unknown_action",
                    "outcome proposes unknown action",
                    "actions",
                )),
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let Some(priority) = candidates.iter().map(|(p, _, _)| *p).max() else {
        return Ok(None);
    };
    candidates.retain(|(p, _, _)| *p == priority);
    candidates.sort_by(|a, b| a.1.cmp(b.1));
    let mut action = candidates[0].2.clone();
    let notes: Vec<_> = candidates
        .iter()
        .filter_map(|(_, _, a)| a.params.get("note").and_then(|v| v.as_str()))
        .collect();
    if !notes.is_empty() {
        action.params.insert("note".into(), notes.join("\n").into());
    }
    Ok(Some(action))
}
pub fn replay(
    definition: &CompiledDefinition,
    record: &RunRecord,
    identity: &ClientIdentity,
) -> ReplayResult {
    let start = Instant::now();
    let mut e = Evaluation::new(
        definition,
        &record.event,
        identity.clone(),
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(5),
            run_id: record.run_id.clone(),
        },
    );
    let mut captured = Evaluation::new(
        definition,
        &record.event,
        record.identity.clone(),
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(5),
            run_id: record.run_id.clone(),
        },
    );
    captured.cache = record.inputs.clone();
    for (i, node) in definition.definition.node.iter().enumerate() {
        if let Some(original) = record.nodes.iter().find(|n| n.id == node.id) {
            captured.trace.nodes[i] = original.clone();
            captured.status[i] = if original.status == "completed" {
                Status::Completed
            } else {
                Status::Unreachable
            };
        }
    }
    e.trace.mode = "replay".into();
    e.trace.started_at = record.started_at.clone();
    e.cache = record.inputs.clone();
    e.trace.inputs = record.inputs.clone();
    let mut nodes: BTreeMap<String, ReplayNode> = definition
        .definition
        .node
        .iter()
        .map(|n| {
            (
                n.id.clone(),
                ReplayNode {
                    id: n.id.clone(),
                    status: "not_reached".into(),
                },
            )
        })
        .collect();
    let mut comparable = true;
    let status = (|| -> Result<bool, Failure> {
        if !definition.definition.flow.enabled
            || record.event.trigger != definition.definition.flow.on
        {
            return Ok(false);
        }
        if let Some(when) = definition.expressions.get("flow.when") {
            e.prepare_local(&definition.definition.flow.inputs)?;
            captured.prepare_local(&definition.definition.flow.inputs)?;
            let values = e
                .aliases(&definition.definition.flow.inputs)
                .iter()
                .map(|(k, v)| (k.clone(), crate::expression::from_json(v.value.as_ref())))
                .collect();
            if !when
                .boolean(&values)
                .map_err(|d| (FailureKind::Expression, d.message))?
            {
                return Ok(false);
            }
        }
        loop {
            let ready = e.ready();
            if ready.is_empty() {
                return Ok(true);
            }
            let deterministic: Vec<_> = ready
                .iter()
                .copied()
                .filter(|i| definition.definition.node[*i].kind.question().is_none())
                .collect();
            if !deterministic.is_empty() {
                for i in deterministic {
                    e.prepare_local(&definition.definition.node[i].inputs)?;
                    captured.prepare_local(&definition.definition.node[i].inputs)?;
                    e.deterministic(i)?;
                }
                continue;
            }
            for i in ready {
                let node = &definition.definition.node[i];
                e.prepare_local(&node.inputs)?;
                captured.prepare_local(&node.inputs)?;
                let state = e.state(i)?;
                let question = node.kind.question().expect("decision frontier");
                let capture = record.nodes.iter().find(|n| n.id == node.id);
                let batch = capture
                    .and_then(|n| n.batch_id.as_ref())
                    .and_then(|id| record.batches.iter().find(|b| b.id == *id));
                let missing = node.inputs.values().any(|reference| {
                    !reference.starts_with("event.")
                        && !reference.starts_with("constant.")
                        && crate::validation::upstream(reference).is_none()
                        && !record.inputs.contains_key(reference)
                });
                let classification = if missing || capture.is_none() || batch.is_none() {
                    "missing_capture"
                } else if record.identity != *identity
                    || e.aliases(&node.inputs) != captured.aliases(&node.inputs)
                    || batch.is_some_and(|b| {
                        b.state != state || b.questions.get(&node.id) != Some(&question)
                    })
                {
                    "question_changed"
                } else if capture.and_then(|n| n.answer.as_ref()).is_none() {
                    "missing_capture"
                } else {
                    "comparable"
                };
                nodes.get_mut(&node.id).expect("replay node").status = classification.into();
                if classification != "comparable" {
                    if classification == "missing_capture"
                        && capture.is_some_and(|n| n.status == "failed")
                        && batch.is_some()
                        && record.identity == *identity
                        && batch.is_some_and(|b| {
                            b.state == state && b.questions.get(&node.id) == Some(&question)
                        })
                    {
                        if let Some(failure) = &record.failure {
                            return Err((failure.kind, failure.message.clone()));
                        }
                    }
                    comparable = false;
                    return Ok(false);
                }
                let answer = capture
                    .and_then(|n| n.answer.clone())
                    .expect("comparable answer");
                validate_answers(
                    &Questions::from([(node.id.clone(), question)]),
                    &Answers::from([(node.id.clone(), answer.clone())]),
                )
                .map_err(decision_failure)?;
                let id = format!("b{}", e.trace.batches.len() + 1);
                e.trace.nodes[i].batch_id = Some(id.clone());
                e.trace.batches.push(BatchRecord {
                    id,
                    node_ids: vec![node.id.clone()],
                    state,
                    questions: Questions::from([(
                        node.id.clone(),
                        node.kind.question().expect("decision"),
                    )]),
                    duration_ms: 0,
                    usage: None,
                    cost_usd: None,
                });
                e.answer(i, answer)?;
            }
        }
    })();
    if let Err((kind, message)) = &status {
        let f = &definition.definition;
        let chain = f
            .fallback
            .as_ref()
            .and_then(|fallback| {
                match kind {
                    FailureKind::Timeout => fallback.on_timeout.as_ref(),
                    FailureKind::ProviderError => fallback.on_provider_error.as_ref(),
                    FailureKind::InvalidAnswer => fallback.on_invalid_answer.as_ref(),
                    _ => None,
                }
                .or(fallback.on_failure.as_ref())
            })
            .or_else(|| {
                if f.flow.blocking {
                    definition
                        .catalogue
                        .trigger(&f.flow.on)
                        .and_then(|t| t.default_fallback.as_ref())
                } else {
                    None
                }
            });
        let mut values = e.aliases(
            &f.fallback
                .as_ref()
                .map(|f| f.inputs.clone())
                .unwrap_or_default(),
        );
        values.insert(
            "failure_kind".into(),
            InputValue {
                value: Some(kind.name().into()),
                cut: false,
            },
        );
        values.insert(
            "failure_message".into(),
            InputValue {
                value: Some(message.clone().into()),
                cut: false,
            },
        );
        let mut chain = chain;
        while let Some(item) = chain {
            match crate::rendering::params(&item.params, &values) {
                Ok(params) => e.trace.fallback_actions.push(ProposedAction {
                    node_id: None,
                    action: item.action.clone(),
                    params,
                }),
                Err(_) => {
                    comparable = false;
                    break;
                }
            }
            chain = item.then.as_deref();
        }
    }
    ReplayResult {
        comparable,
        nodes: definition
            .order
            .iter()
            .map(|i| {
                nodes
                    .remove(&definition.definition.node[*i].id)
                    .expect("replay node exists")
            })
            .collect(),
        outcome: comparable.then(|| e.finish(status, start)),
    }
}
