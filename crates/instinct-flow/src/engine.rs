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
#[serde(deny_unknown_fields)]
pub struct InputValue {
    pub value: Option<serde_json::Value>,
    pub cut: bool,
    #[serde(default)]
    pub redacted: bool,
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
    collected_bytes: usize,
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
                selected_inputs: None,
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
            collected_bytes: 0,
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
            redacted: false,
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
                redacted: false,
            };
        }
        if let Some(name) = reference.strip_prefix("constant.") {
            return InputValue {
                value: self.compiled.definition.constants.get(name).cloned(),
                cut: false,
                redacted: false,
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
            return InputValue {
                value,
                cut: false,
                redacted: record
                    .selected_inputs
                    .as_ref()
                    .is_some_and(|m| m.values().any(|v| v.captured.redacted)),
            };
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
        self.accept_inputs(names, collected, false)
    }
    fn needed(&self, map: &BTreeMap<String, String>) -> Vec<String> {
        map.values()
            .filter(|r| {
                !r.starts_with("event.")
                    && !r.starts_with("constant.")
                    && crate::validation::upstream(r).is_none()
                    && !self.cache.contains_key(*r)
            })
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    fn accept_inputs(
        &mut self,
        names: Vec<String>,
        collected: BTreeMap<String, InputValue>,
        replay: bool,
    ) -> Result<(), Failure> {
        for name in names {
            if self.cache.contains_key(&name) {
                continue;
            }
            let mut value = collected.get(&name).cloned().unwrap_or(InputValue {
                value: None,
                cut: false,
                redacted: false,
            });
            let spec = self
                .compiled
                .catalogue
                .inputs
                .iter()
                .find(|s| s.name == name)
                .expect("validated host reference");
            if value.value.as_ref().is_some_and(|v| {
                !v.is_null()
                    && !spec.value_type.matches(v)
                    && !(replay && value.cut && v.is_string())
            }) {
                return Err((FailureKind::Input, "host input has invalid type".into()));
            }
            if value.value.as_ref().is_some_and(|v| v.is_null()) {
                value.value = None;
            }
            self.bound(&mut value, &name)?;
            self.trace
                .inputs
                .insert(name.clone(), stored_input(value.clone()));
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
        let cap = self.compiled.catalogue.limits.input_bytes.min(16384);
        let serialized;
        let mut structured_cut = false;
        let text = if let Some(s) = v.as_str() {
            s
        } else {
            (serialized, structured_cut) =
                crate::rendering::json_prefix(v, cap).map_err(render_failure)?;
            &serialized
        };
        let mut bytes = text.len().min(cap);
        if text.len() > cap || structured_cut {
            if expression_reads {
                return Err((
                    FailureKind::Limit,
                    "expression scalar cannot be truncated".into(),
                ));
            }
            let mut boundary = cap.min(text.len());
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            bytes = boundary;
            value.value = Some(text[..boundary].into());
            value.cut = true;
        }
        self.collected_bytes = self.collected_bytes.saturating_add(bytes);
        if self.collected_bytes > self.compiled.catalogue.limits.run_input_bytes.min(65536) {
            return Err((
                FailureKind::Limit,
                "collected inputs exceed run byte limit".into(),
            ));
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
        let state = crate::rendering::state_bounded(
            &values,
            template,
            self.compiled.catalogue.limits.state_bytes.min(65536),
        )
        .map_err(render_failure)?;
        if state.len() > self.compiled.catalogue.limits.state_bytes.min(65536) {
            return Err((
                FailureKind::Limit,
                "rendered state exceeds byte limit".into(),
            ));
        }
        Ok(state)
    }
    fn deterministic(&mut self, i: usize) -> Result<(), Failure> {
        self.capture_selected(i);
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
                let params = crate::rendering::params(params, &inputs).map_err(render_failure)?;
                self.validate_params(action, &params)?;
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
    fn selected(&self, map: &BTreeMap<String, String>) -> BTreeMap<String, SelectedInput> {
        map.iter()
            .map(|(alias, reference)| {
                (
                    alias.clone(),
                    SelectedInput {
                        reference: reference.clone(),
                        captured: self.resolve(reference),
                    },
                )
            })
            .collect()
    }
    fn capture_selected(&mut self, i: usize) {
        self.trace.nodes[i].selected_inputs = Some(
            self.selected(&self.compiled.definition.node[i].inputs)
                .into_iter()
                .map(|(alias, mut input)| {
                    input.captured = stored_input(input.captured);
                    (alias, input)
                })
                .collect(),
        );
    }
    fn missing_capture(&self, map: &BTreeMap<String, String>, record: &RunRecord) -> bool {
        map.values().any(|r| {
            self.resolve(r).redacted
                || record.inputs.get(r).is_some_and(|v| v.redacted)
                || (!r.starts_with("event.")
                    && !r.starts_with("constant.")
                    && crate::validation::upstream(r).is_none()
                    && !record.inputs.contains_key(r))
        })
    }
    fn prepare_replay(
        &mut self,
        map: &BTreeMap<String, String>,
        record: &RunRecord,
    ) -> Result<bool, Failure> {
        self.prepare_local(map)?;
        let names = self.needed(map);
        self.accept_inputs(names.clone(), record.inputs.clone(), true)?;
        Ok(names
            .iter()
            .all(|name| self.cache.get(name) == record.inputs.get(name)))
    }
    fn stored_state(&self, i: usize) -> Result<String, Failure> {
        let node = &self.compiled.definition.node[i];
        let values = self
            .aliases(&node.inputs)
            .into_iter()
            .map(|(k, mut v)| {
                if v.redacted {
                    v.value = Some("<redacted>".into());
                    v.cut = false;
                }
                (k, v)
            })
            .collect();
        crate::rendering::state_bounded(
            &values,
            node.kind
                .template()
                .and_then(|n| self.compiled.templates.get(n))
                .map(String::as_str),
            self.compiled.catalogue.limits.state_bytes.min(65536),
        )
        .map_err(render_failure)
    }
    fn validate_params(
        &self,
        action: &str,
        params: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), Failure> {
        let spec = self
            .compiled
            .catalogue
            .action(action)
            .expect("validated action");
        if spec.params.iter().any(|p| match params.get(&p.name) {
            Some(value) => !p.value_type.matches(value),
            None => p.required,
        }) || params
            .keys()
            .any(|name| !spec.params.iter().any(|p| &p.name == name))
        {
            return Err((
                FailureKind::Input,
                "resolved action parameter has invalid type".into(),
            ));
        }
        Ok(())
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
                redacted: false,
            },
        );
        values.insert(
            "failure_message".into(),
            InputValue {
                value: Some(message.into()),
                cut: false,
                redacted: false,
            },
        );
        let mut actions = Vec::new();
        let mut next = Some(&chain);
        while let Some(item) = next {
            let Ok(params) = crate::rendering::params(&item.params, &values) else {
                return;
            };
            if self.validate_params(&item.action, &params).is_err() {
                return;
            }
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
        for node in &mut self.trace.nodes {
            if node
                .selected_inputs
                .as_ref()
                .is_some_and(|m| m.values().any(|v| v.captured.redacted))
            {
                node.answer = None;
                node.variables.clear();
                for action in &mut self.trace.actions {
                    if action.node_id.as_ref() == Some(&node.id) {
                        for value in action.params.values_mut() {
                            *value = "<redacted>".into();
                        }
                    }
                }
            }
        }
        if self
            .compiled
            .definition
            .fallback
            .as_ref()
            .is_some_and(|f| self.aliases(&f.inputs).values().any(|v| v.redacted))
        {
            for action in &mut self.trace.fallback_actions {
                for value in action.params.values_mut() {
                    *value = "<redacted>".into();
                }
            }
        }
        RunOutcome {
            result,
            trace: self.trace,
        }
    }
}
fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}
fn stored_input(mut value: InputValue) -> InputValue {
    if value.redacted {
        value.value = None;
    }
    value
}
fn render_failure(d: Diagnostic) -> Failure {
    (
        if d.code == "limit" {
            FailureKind::Limit
        } else {
            FailureKind::Input
        },
        d.message,
    )
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
    type Collected = (
        Vec<String>,
        Result<BTreeMap<String, InputValue>, InputError>,
    );
    let mut collections: FuturesUnordered<BoxFuture<'_, Collected>> = FuturesUnordered::new();
    let mut collecting = BTreeSet::new();
    let mut pending_references = BTreeSet::new();
    let mut decision_count = 0;
    loop {
        e.check(cancel)?;
        for i in e.ready() {
            if collecting.contains(&i) {
                continue;
            }
            let map = e.compiled.definition.node[i].inputs.clone();
            e.prepare_local(&map)?;
            let names = e.needed(&map);
            if !names.is_empty() {
                collecting.insert(i);
                let names: Vec<_> = names
                    .into_iter()
                    .filter(|name| pending_references.insert(name.clone()))
                    .collect();
                if names.is_empty() {
                    continue;
                }
                let deadline = e.deadline;
                collections.push(Box::pin(async move {
                    let result = inputs.collect(&names, cancel, deadline).await;
                    (names, result)
                }));
            }
        }
        loop {
            let ready = e.ready();
            let deterministic: Vec<_> = ready
                .into_iter()
                .filter(|i| {
                    !collecting.contains(i)
                        && e.needed(&e.compiled.definition.node[*i].inputs).is_empty()
                        && e.compiled.definition.node[*i].kind.question().is_none()
                })
                .collect();
            if deterministic.is_empty() {
                break;
            }
            for i in deterministic {
                let map = e.compiled.definition.node[i].inputs.clone();
                e.prepare_local(&map)?;
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
            if collecting.contains(&i)
                || !e.needed(&e.compiled.definition.node[i].inputs).is_empty()
            {
                continue;
            }
            if e.compiled.definition.node[i].kind.question().is_none() {
                continue;
            }
            let map = e.compiled.definition.node[i].inputs.clone();
            e.prepare_local(&map)?;
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
                e.capture_selected(*i);
                e.status[*i] = Status::Running;
                e.trace.nodes[*i].batch_id = Some(id.clone());
            }
            let stored = e.stored_state(
                *nodes
                    .iter()
                    .find(|i| {
                        e.aliases(&e.compiled.definition.node[**i].inputs)
                            .values()
                            .any(|v| v.redacted)
                    })
                    .unwrap_or(&nodes[0]),
            );
            let omitted = stored.is_err();
            e.trace.batches.push(BatchRecord {
                id,
                node_ids: questions.keys().cloned().collect(),
                state: stored.unwrap_or_else(|_| "<state omitted: redacted>".into()),
                redacted: omitted,
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
        if e.ready().iter().any(|i| !collecting.contains(i)) {
            continue;
        }
        if calls.is_empty() && collections.is_empty() {
            // Newly activated nodes may still need host collection on the next iteration.
            if !e.ready().is_empty() {
                continue;
            }
            e.check(cancel)?;
            if e.status.contains(&Status::Pending) {
                return Err((FailureKind::Limit, "scheduler made no progress".into()));
            }
            return Ok(true);
        }
        let (batch, nodes, response, duration) = tokio::select! {biased;
            _=cancel.cancelled()=>return Err((FailureKind::Cancelled,"run cancelled".into())),
            _=tokio::time::sleep_until(e.deadline.into())=>return Err((FailureKind::Timeout,"run deadline exhausted".into())),
            result=collections.next(), if !collections.is_empty()=> {
                let (names, result) = result.expect("nonempty collection set");
                e.check(cancel)?;
                for name in &names {
                    pending_references.remove(name);
                }
                e.accept_inputs(names, result.map_err(|_| (FailureKind::Input, "host input collection failed".into()))?, false)?;
                collecting.retain(|i| !e.needed(&e.compiled.definition.node[*i].inputs).is_empty());
                continue;
            },
            result=calls.next(), if !calls.is_empty()=>result.expect("nonempty call set"),
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
    e.trace.mode = "replay".into();
    e.trace.started_at = record.started_at.clone();
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
        let flow_inputs = &definition.definition.flow.inputs;
        if e.missing_capture(flow_inputs, record)
            || !e.prepare_replay(flow_inputs, record).unwrap_or(false)
        {
            comparable = false;
            return Ok(false);
        }
        if let Some(when) = definition.expressions.get("flow.when") {
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
                    let node = &definition.definition.node[i];
                    if e.missing_capture(&node.inputs, record) {
                        nodes.get_mut(&node.id).expect("replay node").status =
                            "missing_capture".into();
                        comparable = false;
                        return Ok(false);
                    }
                    if !e.prepare_replay(&node.inputs, record).unwrap_or(false) {
                        nodes.get_mut(&node.id).expect("replay node").status =
                            "question_changed".into();
                        comparable = false;
                        return Ok(false);
                    }
                    e.deterministic(i)?;
                }
                continue;
            }
            for i in ready {
                let node = &definition.definition.node[i];
                let capture = record.nodes.iter().find(|n| n.id == node.id);
                let batch = capture
                    .and_then(|n| n.batch_id.as_ref())
                    .and_then(|id| record.batches.iter().find(|b| b.id == *id));
                let missing = e.missing_capture(&node.inputs, record)
                    || batch.is_some_and(|b| b.redacted)
                    || capture
                        .and_then(|n| n.selected_inputs.as_ref())
                        .is_some_and(|m| m.values().any(|v| v.captured.redacted));
                if missing || capture.is_none() || batch.is_none() {
                    nodes.get_mut(&node.id).expect("replay node").status = "missing_capture".into();
                    comparable = false;
                    return Ok(false);
                }
                if !e.prepare_replay(&node.inputs, record).unwrap_or(false) {
                    nodes.get_mut(&node.id).expect("replay node").status =
                        "question_changed".into();
                    comparable = false;
                    return Ok(false);
                }
                let state = e.state(i)?;
                let question = node.kind.question().expect("decision frontier");
                let inputs_equal = capture
                    .and_then(|n| n.selected_inputs.as_ref())
                    .map_or_else(
                        || record.definition_digest == definition.digest,
                        |original| *original == e.selected(&node.inputs),
                    );
                let classification = if missing || capture.is_none() || batch.is_none() {
                    "missing_capture"
                } else if record.identity != *identity
                    || !inputs_equal
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
                        && !missing
                        && inputs_equal
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
                    redacted: false,
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
        if f.fallback
            .as_ref()
            .is_some_and(|f| e.missing_capture(&f.inputs, record))
        {
            comparable = false;
        }
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
        let fallback_inputs = f
            .fallback
            .as_ref()
            .map(|f| f.inputs.clone())
            .unwrap_or_default();
        if !e.prepare_replay(&fallback_inputs, record).unwrap_or(false) {
            comparable = false;
        }
        let mut values = e.aliases(&fallback_inputs);
        values.insert(
            "failure_kind".into(),
            InputValue {
                value: Some(kind.name().into()),
                cut: false,
                redacted: false,
            },
        );
        values.insert(
            "failure_message".into(),
            InputValue {
                value: Some(message.clone().into()),
                cut: false,
                redacted: false,
            },
        );
        let mut chain = chain;
        while let Some(item) = chain {
            match crate::rendering::params(&item.params, &values) {
                Ok(params) if e.validate_params(&item.action, &params).is_ok() => {
                    e.trace.fallback_actions.push(ProposedAction {
                        node_id: None,
                        action: item.action.clone(),
                        params,
                    })
                }
                _ => {
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
