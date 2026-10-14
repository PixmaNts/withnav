//! The order in which yolo, the fixed floor, rules, bypass and the permission flow decide a tool call.
//!
//! [`decide`] reads rules and asks the permission flows through
//! [`PermissionFlows`]; it runs no tool and writes no decision. The flows are
//! a trait so that the caller wires in the real flow runtime and this module
//! knows nothing about flows.

use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use serde_json::{json, Value};

use super::{
    floor::{self, resolve_path, Floor, FloorContext},
    rules::{literal, Effect, Rule, RuleFilter, RuleScope},
    shell::ShellCommand,
    store::{Error, PermissionStore},
};
pub use crate::agent::flow_hook::ProtectedCheck;
use crate::provider::ToolCall;

/// Identical consecutive calls in one turn that count as a loop.
pub const DOOM_LOOP_THRESHOLD: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Yolo,
    Bypass,
    Normal,
}

/// The rules' answer for a call that no `deny` stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuleResult {
    Allow,
    Ask,
    #[default]
    NoRule,
}

/// What the permission flows are asked about.
#[derive(Debug)]
pub struct FlowQuery<'a> {
    pub rule: RuleResult,
    pub matched: Option<&'a Rule>,
    /// `Protected` when the call writes WithNav's own configuration or database.
    pub floor: &'a Floor,
    /// The protected check's answer once it was asked; the flow never asks it again.
    pub protected_check: Option<ProtectedCheck>,
}

/// The aggregate answer of the permission flows on the `tool.pre_call` trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowAnswer {
    Allow,
    AskCaptain,
    Deny,
    /// The flows failed or had nothing to say: the rule's result applies.
    ApplyRule,
}

impl FlowAnswer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AskCaptain => "ask_captain",
            Self::Deny => "deny",
            Self::ApplyRule => "apply_rule",
        }
    }
}

/// The two questions the decision path puts to the flow side. They are kept
/// apart: a flow's `Allow` never stands in for the protected check, which only
/// the compiled-in node of the built-in flow can answer.
pub trait PermissionFlows: Send + Sync {
    /// The aggregate verdict of the permission flows (`permission_pre_call`).
    fn flow<'a>(&'a self, query: &'a FlowQuery<'a>) -> BoxFuture<'a, FlowAnswer>;
    /// Whether the Captain's own messages ask for a protected write. `Unavailable`
    /// when the built-in flow is disabled or the check failed.
    fn protected_check<'a>(&'a self, query: &'a FlowQuery<'a>) -> BoxFuture<'a, ProtectedCheck>;
    /// [`Self::protected_check`] with the run that answered it, kept in the
    /// decision so that a refused protected write can be explained.
    fn protected_check_traced<'a>(
        &'a self,
        query: &'a FlowQuery<'a>,
    ) -> BoxFuture<'a, (ProtectedCheck, Option<crate::agent::flow_hook::RunScores>)> {
        Box::pin(async move { (self.protected_check(query).await, None) })
    }
}

pub struct Input<'a> {
    pub control: Control,
    pub call: &'a ToolCall,
    /// The split commands of a `bash` call; empty for other tools.
    pub commands: &'a [ShellCommand],
    pub cx: &'a FloorContext,
    /// None when the session cannot open its permission database. No rule is
    /// consulted, bypass is disabled, and no reusable approval is proposed.
    pub store: Option<&'a PermissionStore>,
    pub agent: &'a str,
    /// Identical consecutive calls in this turn, this one included; see [`DoomTracker`].
    pub repeats: usize,
    /// The Captain validated the exact content this call admits (a skill):
    /// with no rule it counts as allowed; `deny` and `ask` rules still apply.
    pub validated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovedBy {
    Yolo,
    Rule,
    Bypass,
    Flow,
    /// The Captain validated this exact content and no rule applies to it.
    Validation,
}

impl ApprovedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yolo => "yolo",
            Self::Rule => "rule",
            Self::Bypass => "bypass",
            Self::Flow => "flow",
            Self::Validation => "validation",
        }
    }
}

/// The rule an "Always" answer would save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleProposal {
    pub action: String,
    pub resource: String,
    pub scope: RuleScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Run {
        approved_by: ApprovedBy,
    },
    Deny {
        /// `None` when the permission flow denied.
        rule: Option<Rule>,
        message: String,
    },
    AskCaptain {
        reason: String,
        /// Empty when "Always" must not be offered.
        always: Vec<RuleProposal>,
    },
}

/// What the decision table records besides the verdict.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trace {
    pub rule_id: Option<i64>,
    pub effect: Option<Effect>,
    pub flow_verdict: Option<&'static str>,
    pub protected_check: Option<ProtectedCheck>,
    /// The run that answered the protected check and its score, when one ran.
    pub protected_run: Option<crate::agent::flow_hook::RunScores>,
    pub command_matches: Value,
    pub floor: Option<String>,
    pub doom_loop: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub verdict: Verdict,
    pub trace: Trace,
}

/// Counts identical consecutive calls (same tool, identical arguments) in one turn.
#[derive(Debug, Default)]
pub struct DoomTracker {
    last: Option<(String, Value)>,
    count: usize,
}

impl DoomTracker {
    /// Record the next call of the turn and return how many identical calls end here.
    pub fn observe(&mut self, call: &ToolCall) -> usize {
        let same = self
            .last
            .as_ref()
            .is_some_and(|(name, args)| *name == call.name && *args == call.args);
        self.count = if same { self.count + 1 } else { 1 };
        if !same {
            self.last = Some((call.name.clone(), call.args.clone()));
        }
        self.count
    }

    pub fn new_turn(&mut self) {
        *self = Self::default();
    }
}

/// Everything a decision reads from the permission database and the file
/// system, gathered by [`lookup`] before any flow is asked.
pub struct Lookup {
    floor: Floor,
    free_read: bool,
    /// A native write inside a temporary directory: it runs once no rule
    /// stops or asks about it.
    temp_write: bool,
    rules: Evaluation,
    /// The rule for a loop, read only when the call repeats enough and no
    /// written deny stops it. Its failure is kept apart: it only matters at
    /// the loop check, after the deny and the floor.
    doom: Result<Option<Rule>, Error>,
}

/// Reads what deciding `input` needs. Blocking: the database may wait up to
/// its busy timeout on another connection's write, so async callers run this
/// on the blocking pool.
pub fn lookup(input: &Input<'_>) -> Result<Lookup, Error> {
    if input.control == Control::Yolo {
        return Ok(Lookup {
            floor: Floor::None,
            free_read: false,
            temp_write: false,
            rules: Evaluation::default(),
            doom: Ok(None),
        });
    }
    let floor = floor::check(input.call, input.commands, input.cx);
    let free_read = input.store.is_some() && floor == Floor::None && reads_inside_roots(input);
    let temp_write = input.store.is_some() && floor == Floor::None && writes_inside_temp(input);
    let rules = if free_read {
        Evaluation::default()
    } else {
        evaluate(input)?
    };
    let doom = if input.repeats >= DOOM_LOOP_THRESHOLD && rules.deny.is_none() {
        input
            .store
            .map(|store| {
                store.matching(
                    "doom_loop",
                    &input.call.name,
                    &input.cx.workspace,
                    input.agent,
                )
            })
            .transpose()
            .map(Option::flatten)
    } else {
        Ok(None)
    };
    Ok(Lookup {
        floor,
        free_read,
        temp_write,
        rules,
        doom,
    })
}

/// Decide one tool call, reading the rules on the calling thread.
#[cfg(test)]
pub async fn decide(input: &Input<'_>, flows: &dyn PermissionFlows) -> Decision {
    decide_with(input, lookup(input), flows).await
}

/// Decide one tool call from what [`lookup`] read; nothing else is touched.
/// A rule lookup that failed asks the Captain, whatever the control level but yolo.
pub async fn decide_with(
    input: &Input<'_>,
    lookup: Result<Lookup, Error>,
    flows: &dyn PermissionFlows,
) -> Decision {
    match lookup {
        Ok(lookup) => run(input, lookup, flows).await,
        Err(error) => unreadable(error),
    }
}

fn unreadable(error: Error) -> Decision {
    Decision {
        verdict: Verdict::AskCaptain {
            reason: format!("the permission rules could not be read: {error}"),
            always: Vec::new(),
        },
        trace: Trace::default(),
    }
}

async fn run(input: &Input<'_>, lookup: Lookup, flows: &dyn PermissionFlows) -> Decision {
    let Lookup {
        floor,
        free_read,
        temp_write,
        rules,
        doom,
    } = lookup;
    let mut trace = Trace::default();
    let run = |approved_by, trace| Decision {
        verdict: Verdict::Run { approved_by },
        trace,
    };
    let ask = |reason: String, always, trace| Decision {
        verdict: Verdict::AskCaptain { reason, always },
        trace,
    };

    // 1. Yolo checks nothing.
    if input.control == Control::Yolo {
        return run(ApprovedBy::Yolo, trace);
    }

    // 2. The fixed floor (`floor`): a credential, or a path that cannot be
    // resolved the way execution would resolve it, asks and nothing can allow it.
    // 3. Reads inside the readable roots (`free_read`) need no rule and no
    // flow. They still go through the loop check below, so a model re-reading
    // the same file forever is stopped. A native write inside a temporary
    // directory (`temp_write`) needs no flow either: what it changes is
    // temporary by definition. A written `deny` or `ask` rule still applies.
    // 4. Rules (`rules`), read by `lookup`.
    trace.command_matches = rules.matches.clone();
    trace.rule_id = rules.rule.as_ref().map(|rule| rule.id);
    trace.effect = rules.rule.as_ref().map(|rule| rule.effect);

    // A written deny stops the call before anything can ask about it.
    if let Some(rule) = rules.deny {
        trace.rule_id = Some(rule.id);
        trace.effect = Some(Effect::Deny);
        let message = format!(
            "denied by the permission rule {} {}",
            rule.action, rule.resource
        );
        return Decision {
            verdict: Verdict::Deny {
                rule: Some(rule),
                message,
            },
            trace,
        };
    }

    match &floor {
        Floor::Credential(target) => {
            trace.floor = Some(format!("credential: {target}"));
            return ask(
                format!("{target} is a credential; WithNav always asks before touching one"),
                Vec::new(),
                trace,
            );
        }
        Floor::Unresolved(target) => {
            trace.floor = Some(format!("unresolved: {target}"));
            return ask(
                format!("{target} cannot be resolved, so WithNav cannot tell what it names"),
                Vec::new(),
                trace,
            );
        }
        Floor::Protected(target) => trace.floor = Some(format!("protected: {target}")),
        Floor::NamesProtected(target) => trace.floor = Some(format!("names protected: {target}")),
        Floor::None => {}
    }

    // A protected write needs the protected check, and only it: rules, bypass
    // and a generic flow allow never stand in for it, and it is never saved as
    // a rule. It is asked after the deny above and before anything can run it.
    // A command that only names a protected file goes through the same check:
    // its question also accepts a call that only reads WithNav's files.
    if let Floor::Protected(target) | Floor::NamesProtected(target) = &floor {
        let query = FlowQuery {
            rule: rules.result,
            matched: rules.rule.as_ref(),
            floor: &floor,
            protected_check: None,
        };
        let (check, run) = flows.protected_check_traced(&query).await;
        trace.protected_check = Some(check);
        trace.protected_run = run;
        if check != ProtectedCheck::Passed {
            return ask(
                match &floor {
                    Floor::NamesProtected(_) => format!("{target} is WithNav's own configuration or database and this command may change it; only your explicit request can change it"),
                    _ => format!("{target} is WithNav's own configuration; only your explicit request can change it"),
                },
                Vec::new(),
                trace,
            );
        }
    }

    // A loop is checked after the rules so that a `deny` is never hidden by a
    // question; bypass skips it.
    if input.repeats >= DOOM_LOOP_THRESHOLD {
        trace.doom_loop = true;
        let doom = match doom {
            Ok(rule) => rule,
            Err(error) => return unreadable(error),
        };
        match doom {
            Some(rule) if rule.effect == Effect::Deny => {
                trace.rule_id = Some(rule.id);
                trace.effect = Some(Effect::Deny);
                let message = format!(
                    "{} was called {} times in a row with the same arguments",
                    input.call.name, input.repeats
                );
                return Decision {
                    verdict: Verdict::Deny {
                        rule: Some(rule),
                        message,
                    },
                    trace,
                };
            }
            Some(rule) if rule.effect == Effect::Allow => {}
            _ if input.control == Control::Bypass && input.store.is_some() => {}
            _ => {
                return ask(
                    format!(
                        "{} was called {} times in a row with the same arguments",
                        input.call.name, input.repeats
                    ),
                    Vec::new(),
                    trace,
                )
            }
        }
    }

    if free_read || (temp_write && rules.result != RuleResult::Ask) {
        return run(ApprovedBy::Rule, trace);
    }

    // 5. Bypass.
    if input.control == Control::Bypass && input.store.is_some() {
        return run(ApprovedBy::Bypass, trace);
    }

    // 6. The permission flow, with the rules' result.
    let answer = flows
        .flow(&FlowQuery {
            rule: rules.result,
            matched: rules.rule.as_ref(),
            floor: &floor,
            protected_check: trace.protected_check,
        })
        .await;
    trace.flow_verdict = Some(answer.as_str());
    let proposals = if input.store.is_none()
        || matches!(floor, Floor::Protected(_) | Floor::NamesProtected(_))
    {
        Vec::new()
    } else {
        rules.proposals
    };
    let reason = || match rules.result {
        RuleResult::Ask => "a permission rule asks for this call".to_owned(),
        _ => "no permission rule covers this call".to_owned(),
    };
    match answer {
        FlowAnswer::Allow => run(ApprovedBy::Flow, trace),
        FlowAnswer::Deny => Decision {
            verdict: Verdict::Deny {
                rule: None,
                message: "denied by the permission flow".into(),
            },
            trace,
        },
        FlowAnswer::AskCaptain => ask(
            "the permission flow wants your decision".into(),
            proposals,
            trace,
        ),
        // 7. The Captain.
        FlowAnswer::ApplyRule if rules.result == RuleResult::Allow && rules.rule.is_none() => {
            run(ApprovedBy::Validation, trace)
        }
        FlowAnswer::ApplyRule if rules.result == RuleResult::Allow => run(ApprovedBy::Rule, trace),
        FlowAnswer::ApplyRule => ask(reason(), proposals, trace),
    }
}

fn project_roots(cx: &FloorContext) -> Vec<PathBuf> {
    std::iter::once(&cx.workspace)
        .chain(&cx.writable_roots)
        .filter_map(|root| resolve_path(&root.to_string_lossy(), cx))
        .collect()
}

fn inside(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn path_argument(call: &ToolCall) -> Option<&str> {
    call.args.get("path").and_then(Value::as_str)
}

fn reads_inside_roots(input: &Input<'_>) -> bool {
    if !matches!(input.call.name.as_str(), "read" | "grep" | "glob") {
        return false;
    }
    let Some(path) = path_argument(input.call) else {
        // `grep` and `glob` default to the workspace.
        return input.call.name != "read";
    };
    let mut roots = project_roots(input.cx);
    roots.extend(
        input
            .cx
            .readable_roots
            .iter()
            .filter_map(|root| resolve_path(&root.to_string_lossy(), input.cx)),
    );
    resolve_path(path, input.cx).is_some_and(|path| inside(&path, &roots))
}

/// Whether a native `write` or `edit` lands inside a temporary directory:
/// the system temp directory or a scratch directory, after every link is
/// followed, so a link in `/tmp` to a project file is not a temp write. A
/// project that itself lives in the temp directory is judged as a project.
fn writes_inside_temp(input: &Input<'_>) -> bool {
    if !matches!(input.call.name.as_str(), "write" | "edit") {
        return false;
    }
    let Some(target) = path_argument(input.call).and_then(|path| resolve_path(path, input.cx))
    else {
        return false;
    };
    let roots: Vec<PathBuf> = input
        .cx
        .temp_roots
        .iter()
        .filter_map(|root| resolve_path(&root.to_string_lossy(), input.cx))
        .collect();
    let workspace = resolve_path(&input.cx.workspace.to_string_lossy(), input.cx);
    inside(&target, &roots) && !workspace.is_some_and(|workspace| target.starts_with(workspace))
}

struct Item {
    result: RuleResult,
    rule: Option<Rule>,
    proposal: Option<RuleProposal>,
    label: String,
}

#[derive(Default)]
struct Evaluation {
    result: RuleResult,
    /// The rule behind `result`.
    rule: Option<Rule>,
    deny: Option<Rule>,
    /// "Always" proposals for the parts of the call no `allow` rule covers.
    proposals: Vec<RuleProposal>,
    matches: Value,
}

fn classify(rule: Option<Rule>, proposal: Option<RuleProposal>, label: String) -> Item {
    Item {
        result: match rule.as_ref().map(|rule| rule.effect) {
            None => RuleResult::NoRule,
            Some(Effect::Allow) => RuleResult::Allow,
            // A deny is split out by the caller.
            Some(Effect::Ask | Effect::Deny) => RuleResult::Ask,
        },
        rule,
        proposal,
        label,
    }
}

fn items(input: &Input<'_>) -> Result<Vec<Item>, Error> {
    let call = input.call;
    let find = |action: &str, resource: &str| {
        input.store.map_or(Ok(None), |store| {
            store.matching(action, resource, &input.cx.workspace, input.agent)
        })
    };
    let project = |action: &str, resource: String| RuleProposal {
        action: action.to_owned(),
        resource,
        scope: RuleScope::Project(input.cx.workspace.clone()),
    };
    let tool = call.name.as_str();
    match tool {
        "bash" => {
            let mut items = Vec::new();
            for command in input.commands {
                let Some(text) = command.rule_text.as_deref().filter(|_| !command.opaque) else {
                    // Syntax the splitter does not understand: no allow or ask
                    // rule matches, and "Always" is not offered.
                    let deny = opaque_deny(input)?;
                    let mut item = classify(None, None, command.raw.clone());
                    if let Some(rule) = deny {
                        item.result = RuleResult::Ask;
                        item.rule = Some(rule);
                    }
                    items.push(item);
                    continue;
                };
                let proposal = command
                    .save_pattern()
                    .map(|pattern| project("bash", pattern));
                items.push(classify(
                    find("bash", text)?,
                    proposal,
                    command.normalized.clone(),
                ));
            }
            if items.is_empty() {
                items.push(classify(None, None, String::new()));
            }
            Ok(items)
        }
        "read" | "grep" | "glob" | "write" | "edit" => {
            let Some(path) = path_argument(call) else {
                return Ok(vec![classify(find(tool, "*")?, None, "*".into())]);
            };
            let Some(resolved) = resolve_path(path, input.cx) else {
                return Ok(vec![classify(None, None, path.to_owned())]);
            };
            let text = resolved.to_string_lossy().into_owned();
            let proposal = project(tool, literal(&text));
            let mut items = vec![classify(find(tool, &text)?, Some(proposal), text.clone())];
            if !inside(&resolved, &project_roots(input.cx)) {
                let parent = resolved.parent().map_or(String::new(), |p| {
                    p.to_string_lossy().trim_end_matches('/').to_owned()
                });
                let proposal = project("external_directory", format!("{}/*", literal(&parent)));
                items.push(classify(
                    find("external_directory", &text)?,
                    Some(proposal),
                    format!("external_directory {text}"),
                ));
            }
            Ok(items)
        }
        "skill" => {
            let name = call.args.get("name").and_then(Value::as_str).unwrap_or("*");
            let resource = literal(name);
            let proposal = RuleProposal {
                action: "skill".into(),
                resource: resource.clone(),
                scope: RuleScope::Global,
            };
            Ok(vec![classify(
                find("skill", name)?,
                Some(proposal),
                name.to_owned(),
            )])
        }
        "fetch" => {
            let raw = call.args.get("url").and_then(Value::as_str).unwrap_or("*");
            // The rule lookup and the proposal use one representation, the one
            // the fetch tool would request. A URL the tool refuses gets no
            // proposal: "Always" must not save a rule for a call that cannot run.
            let (resource, proposal) = match fetch_url(raw) {
                Some(url) => {
                    let origin = literal(&url.origin().ascii_serialization());
                    (
                        canonical_url(&url),
                        Some(project("fetch", format!("{origin}/*"))),
                    )
                }
                None => (raw.trim().to_owned(), None),
            };
            Ok(vec![classify(
                find("fetch", &resource)?,
                proposal,
                resource.clone(),
            )])
        }
        _ => {
            let proposal = RuleProposal {
                action: tool.to_owned(),
                resource: "*".into(),
                scope: RuleScope::Global,
            };
            Ok(vec![classify(find(tool, "*")?, Some(proposal), "*".into())])
        }
    }
}

/// The URL as the fetch tool validates it: http or https, a host, no
/// credentials.
fn fetch_url(raw: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(raw.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// Lowercase scheme and host, punycode host, default port dropped, `/` for an
/// empty path, no fragment (it is never sent).
fn canonical_url(url: &reqwest::Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    url.to_string()
}

fn evaluate(input: &Input<'_>) -> Result<Evaluation, Error> {
    let items = items(input)?;
    let matches = Value::Array(
        items
            .iter()
            .map(|item| {
                json!({
                    "command": item.label,
                    "rule_id": item.rule.as_ref().map(|rule| rule.id),
                    "effect": item.rule.as_ref().map(|rule| rule.effect),
                })
            })
            .collect(),
    );
    let deny = items
        .iter()
        .filter_map(|item| item.rule.as_ref())
        .find(|rule| rule.effect == Effect::Deny)
        .cloned();
    let strictest = |wanted| items.iter().find(|item| item.result == wanted);
    let (result, rule) = if let Some(item) = strictest(RuleResult::Ask) {
        (RuleResult::Ask, item.rule.clone())
    } else if strictest(RuleResult::NoRule).is_some() && input.validated && input.store.is_some() {
        (RuleResult::Allow, None)
    } else if strictest(RuleResult::NoRule).is_some() {
        (RuleResult::NoRule, None)
    } else {
        (
            RuleResult::Allow,
            items.first().and_then(|item| item.rule.clone()),
        )
    };
    // One opaque command makes a line impossible to save as a rule.
    let any_unsavable =
        items.iter().any(|item| item.proposal.is_none()) && input.call.name == "bash";
    let mut proposals: Vec<RuleProposal> = Vec::new();
    if !any_unsavable && items.iter().all(|item| item.rule.is_none()) {
        for item in &items {
            if item.result == RuleResult::NoRule {
                if let Some(proposal) = &item.proposal {
                    if !proposals.contains(proposal) {
                        proposals.push(proposal.clone());
                    }
                }
            }
        }
    }
    Ok(Evaluation {
        result,
        rule,
        deny,
        proposals,
        matches,
    })
}

/// A `deny` rule whose resource is exactly `*` is the only rule an opaque
/// command can match, because it has no rule text to match anything narrower.
fn opaque_deny(input: &Input<'_>) -> Result<Option<Rule>, Error> {
    let Some(store) = input.store else {
        return Ok(None);
    };
    let workspace = input.cx.workspace.canonicalize()?;
    Ok(store
        .list(&RuleFilter::default())?
        .into_iter()
        .filter(|rule| rule.effect == Effect::Deny && rule.resource == "*")
        .filter(|rule| pattern_matches(&rule.action, "bash"))
        .find(|rule| match &rule.scope {
            RuleScope::Global => true,
            RuleScope::Project(path) => *path == workspace,
            RuleScope::Agent(name) => name == input.agent,
        }))
}

/// The permission pattern language: `*` matches any text, a backslash quotes a
/// star or another backslash.
fn pattern_matches(pattern: &str, text: &str) -> bool {
    let mut tokens: Vec<Option<char>> = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => tokens.push(None),
            '\\' if matches!(chars.peek(), Some('*' | '\\')) => tokens.push(chars.next()),
            _ => tokens.push(Some(c)),
        }
    }
    let text: Vec<char> = text.chars().collect();
    let (mut t, mut p, mut star, mut mark) = (0, 0, None, 0);
    while t < text.len() {
        match tokens.get(p) {
            Some(None) => {
                star = Some(p);
                mark = t;
                p += 1;
            }
            Some(Some(c)) if *c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some(s) => {
                    p = s + 1;
                    mark += 1;
                    t = mark;
                }
                None => return false,
            },
        }
    }
    tokens[p..].iter().all(Option::is_none)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{rules::RuleOrigin, shell::split};

    struct Fixture {
        dir: tempfile::TempDir,
        store: PermissionStore,
        cx: FloorContext,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in [
            "project",
            "config/flows",
            "data",
            "home/.ssh",
            "outside",
            "tmp",
        ] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let store = PermissionStore::open(root.join("data")).unwrap();
        let cx = FloorContext {
            workspace: root.join("project"),
            writable_roots: Vec::new(),
            readable_roots: vec![root.join("skills")],
            temp_roots: vec![root.join("tmp")],
            config_dir: Some(root.join("config")),
            data_dir: Some(root.join("data")),
            home: Some(root.join("home")),
        };
        Fixture { dir, store, cx }
    }

    impl Fixture {
        fn root(&self) -> PathBuf {
            self.dir.path().canonicalize().unwrap()
        }

        fn add(&self, action: &str, resource: &str, effect: Effect) -> Rule {
            self.store
                .add(
                    action,
                    resource,
                    effect,
                    RuleScope::Global,
                    RuleOrigin::Manual,
                )
                .unwrap()
        }

        fn decide_full(
            &self,
            control: Control,
            call: &ToolCall,
            repeats: usize,
            flows: &Script,
        ) -> Decision {
            let commands = match call.name.as_str() {
                "bash" => split(call.args["command"].as_str().unwrap()),
                _ => Vec::new(),
            };
            let input = Input {
                control,
                call,
                commands: &commands,
                cx: &self.cx,
                store: Some(&self.store),
                agent: "build",
                repeats,
                validated: false,
            };
            futures::executor::block_on(decide(&input, flows))
        }

        /// The flow answers `answer`; the protected check is unavailable.
        fn decide_with(
            &self,
            control: Control,
            call: &ToolCall,
            repeats: usize,
            answer: FlowAnswer,
        ) -> (Decision, usize) {
            let flows = Script::new(answer, ProtectedCheck::Unavailable);
            let decision = self.decide_full(control, call, repeats, &flows);
            (decision, flows.flow_calls.get())
        }

        fn decide(&self, control: Control, call: &ToolCall) -> Decision {
            self.decide_with(control, call, 1, FlowAnswer::ApplyRule).0
        }
    }

    /// A scripted flow side that counts what it is asked.
    struct Script {
        answer: FlowAnswer,
        check: ProtectedCheck,
        flow_calls: std::cell::Cell<usize>,
        check_calls: std::cell::Cell<usize>,
        seen: std::cell::Cell<Option<(RuleResult, Option<i64>)>>,
    }

    // The scripted side is only used from one thread inside `block_on`.
    unsafe impl Sync for Script {}

    impl Script {
        fn new(answer: FlowAnswer, check: ProtectedCheck) -> Self {
            Self {
                answer,
                check,
                flow_calls: Default::default(),
                check_calls: Default::default(),
                seen: Default::default(),
            }
        }
    }

    impl PermissionFlows for Script {
        fn flow<'a>(&'a self, query: &'a FlowQuery<'a>) -> BoxFuture<'a, FlowAnswer> {
            self.seen
                .set(Some((query.rule, query.matched.map(|rule| rule.id))));
            self.flow_calls.set(self.flow_calls.get() + 1);
            Box::pin(async move { self.answer })
        }

        fn protected_check<'a>(&'a self, _: &'a FlowQuery<'a>) -> BoxFuture<'a, ProtectedCheck> {
            self.check_calls.set(self.check_calls.get() + 1);
            Box::pin(async move { self.check })
        }
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            args,
            args_valid: true,
        }
    }

    fn bash(line: &str) -> ToolCall {
        call("bash", json!({"command": line}))
    }

    fn file(name: &str, path: &str) -> ToolCall {
        call(name, json!({"path": path}))
    }

    fn allow(verdict: FlowAnswer) -> FlowAnswer {
        verdict
    }

    fn run_by(decision: &Decision) -> Option<ApprovedBy> {
        match decision.verdict {
            Verdict::Run { approved_by } => Some(approved_by),
            _ => None,
        }
    }

    fn proposals(decision: &Decision) -> Vec<RuleProposal> {
        match &decision.verdict {
            Verdict::AskCaptain { always, .. } => always.clone(),
            other => panic!("not a question: {other:?}"),
        }
    }

    #[test]
    fn yolo_runs_everything_without_looking() {
        let f = fixture();
        f.add("*", "*", Effect::Deny);
        for call in [bash("rm -rf /"), file("write", ".env")] {
            let (decision, asked) = f.decide_with(Control::Yolo, &call, 9, allow(FlowAnswer::Deny));
            assert_eq!(run_by(&decision), Some(ApprovedBy::Yolo));
            assert_eq!(asked, 0);
        }
    }

    #[test]
    fn credential_asks_under_bypass_and_allow_rules_with_no_proposal() {
        let f = fixture();
        f.add("*", "*", Effect::Allow);
        for control in [Control::Normal, Control::Bypass] {
            for call in [
                file("read", ".env"),
                file("write", "~/.ssh/id_ed25519"),
                bash("cat .env"),
            ] {
                let (decision, asked) = f.decide_with(control, &call, 1, allow(FlowAnswer::Allow));
                assert!(
                    matches!(&decision.verdict, Verdict::AskCaptain { always, .. } if always.is_empty()),
                    "{call:?} {decision:?}"
                );
                assert_eq!(asked, 0);
                assert!(decision
                    .trace
                    .floor
                    .as_deref()
                    .unwrap()
                    .starts_with("credential"));
            }
        }
    }

    #[test]
    fn protected_write_needs_a_passed_protected_check() {
        let f = fixture();
        let target = f.root().join("config/config.json").display().to_string();
        let write = file("write", &target);
        f.add("write", "*", Effect::Allow);
        // A generic flow allow, an allow rule and bypass do not satisfy it.
        for check in [ProtectedCheck::Failed, ProtectedCheck::Unavailable] {
            for control in [Control::Normal, Control::Bypass] {
                let flows = Script::new(FlowAnswer::Allow, check);
                let decision = f.decide_full(control, &write, 1, &flows);
                assert!(
                    matches!(&decision.verdict, Verdict::AskCaptain { always, .. } if always.is_empty()),
                    "{check:?} {decision:?}"
                );
                assert_eq!(flows.flow_calls.get(), 0);
                assert_eq!(flows.check_calls.get(), 1);
                assert_eq!(decision.trace.protected_check, Some(check));
            }
        }
        // Passed plus the flow's allow runs.
        let flows = Script::new(FlowAnswer::Allow, ProtectedCheck::Passed);
        let decision = f.decide_full(Control::Normal, &write, 1, &flows);
        assert_eq!(run_by(&decision), Some(ApprovedBy::Flow));
        // Passed does not replace the flow: its question still applies and
        // never saves a rule for the protected file.
        let flows = Script::new(FlowAnswer::AskCaptain, ProtectedCheck::Passed);
        let decision = f.decide_full(Control::Normal, &write, 1, &flows);
        assert!(proposals(&decision).is_empty());
        // Under bypass only the protected check runs, never the flow.
        let flows = Script::new(FlowAnswer::Deny, ProtectedCheck::Passed);
        let decision = f.decide_full(Control::Bypass, &write, 1, &flows);
        assert_eq!(run_by(&decision), Some(ApprovedBy::Bypass));
        assert_eq!((flows.check_calls.get(), flows.flow_calls.get()), (1, 0));
    }

    #[test]
    fn written_ask_has_no_always_proposal() {
        let f = fixture();
        f.add("bash", "*", Effect::Ask);
        let decision = f.decide(Control::Normal, &bash("git push origin feature"));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        assert!(proposals(&decision).is_empty(), "{decision:?}");
    }

    #[test]
    fn credential_deny_precedes_floor_question() {
        let f = fixture();
        f.add("read", "*", Effect::Deny);
        let target = f.root().join("home/.ssh/id_ed25519").display().to_string();
        for control in [Control::Normal, Control::Bypass] {
            let decision = f.decide(control, &file("read", &target));
            assert!(
                matches!(decision.verdict, Verdict::Deny { .. }),
                "{decision:?}"
            );
        }
    }

    #[test]
    fn protected_write_still_obeys_a_deny() {
        let f = fixture();
        let target = f.root().join("data/withnav.db").display().to_string();
        f.add("write", "*", Effect::Deny);
        let flows = Script::new(FlowAnswer::Allow, ProtectedCheck::Passed);
        let decision = f.decide_full(Control::Normal, &file("write", &target), 1, &flows);
        assert!(matches!(decision.verdict, Verdict::Deny { .. }));
        assert_eq!(flows.check_calls.get(), 0);
    }

    #[test]
    fn permission_writers_require_protected_check_under_bypass() {
        let f = fixture();
        for command in ["add", "revoke", "validate", "revoke-validation"] {
            let decision = f.decide(
                Control::Bypass,
                &bash(&format!("nav permissions {command} flow")),
            );
            assert!(
                matches!(decision.verdict, Verdict::AskCaptain { .. }),
                "{command}: {decision:?}"
            );
            assert!(proposals(&decision).is_empty());
        }
        for command in ["list", "history", "validations"] {
            let decision = f.decide(
                Control::Bypass,
                &bash(&format!("nav permissions {command}")),
            );
            assert_eq!(run_by(&decision), Some(ApprovedBy::Bypass), "{command}");
        }
    }

    #[test]
    fn a_command_naming_the_database_goes_through_the_protected_check() {
        let f = fixture();
        let db = f.root().join("data/withnav.db").display().to_string();
        for line in [
            format!("sqlite3 -readonly {db} 'SELECT 1'"),
            format!("sqlite3 {db} 'DELETE FROM permission_rules'"),
        ] {
            let flows = Script::new(FlowAnswer::Allow, ProtectedCheck::Passed);
            let decision = f.decide_full(Control::Normal, &bash(&line), 1, &flows);
            assert_eq!(run_by(&decision), Some(ApprovedBy::Flow), "{line}");
            assert_eq!(flows.check_calls.get(), 1);
            let flows = Script::new(FlowAnswer::Allow, ProtectedCheck::Failed);
            let decision = f.decide_full(Control::Bypass, &bash(&line), 1, &flows);
            assert!(
                matches!(decision.verdict, Verdict::AskCaptain { .. }),
                "{line}"
            );
            assert!(proposals(&decision).is_empty());
        }
    }

    #[test]
    fn bash_touching_protected_state_asks() {
        let f = fixture();
        f.add("bash", "nav permissions *", Effect::Allow);
        let decision = f.decide(Control::Normal, &bash("nav permissions add bash '*' allow"));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        assert!(proposals(&decision).is_empty());
    }

    #[test]
    fn reads_inside_roots_run_without_rule_or_flow() {
        let f = fixture();
        let skill = f.root().join("skills/a/SKILL.md").display().to_string();
        f.add("read", "*", Effect::Ask);
        for call in [
            file("read", "src/a.rs"),
            file("grep", "."),
            call("glob", json!({"pattern": "*.rs"})),
            file("read", &skill),
        ] {
            let (decision, asked) =
                f.decide_with(Control::Normal, &call, 1, allow(FlowAnswer::Deny));
            assert_eq!(run_by(&decision), Some(ApprovedBy::Rule), "{call:?}");
            assert_eq!(asked, 0);
        }
        // Outside the roots a read goes through the rules.
        let outside = f.root().join("outside/x").display().to_string();
        let decision = f.decide(Control::Normal, &file("read", &outside));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
    }

    #[test]
    fn writes_inside_a_temp_directory_run_without_the_flow() {
        let f = fixture();
        let temp = |name: &str| f.root().join("tmp").join(name).display().to_string();
        for call in [
            file("write", &temp("x")),
            file("edit", &temp("deep/new/y.txt")),
        ] {
            let (decision, asked) =
                f.decide_with(Control::Normal, &call, 1, allow(FlowAnswer::Deny));
            assert_eq!(run_by(&decision), Some(ApprovedBy::Rule), "{call:?}");
            assert_eq!(asked, 0);
        }
        // The real system temp directory too.
        let cx = FloorContext {
            temp_roots: floor::system_temp_roots(),
            ..f.cx.clone()
        };
        let fixture = Fixture { cx, ..f };
        let (decision, asked) = fixture.decide_with(
            Control::Normal,
            &file("write", "/tmp/withnav-temp-write-test/x"),
            1,
            FlowAnswer::Deny,
        );
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
        assert_eq!(asked, 0);
    }

    #[test]
    fn a_temp_write_still_meets_rules_the_floor_and_links() {
        let f = fixture();
        let target = f.root().join("tmp/x").display().to_string();
        // A written deny on the path denies.
        f.add("write", &literal(&target), Effect::Deny);
        let decision = f.decide(Control::Normal, &file("write", &target));
        assert!(matches!(decision.verdict, Verdict::Deny { .. }));
        // A credential-looking path asks.
        let key = f.root().join("tmp/.env").display().to_string();
        let (decision, asked) =
            f.decide_with(Control::Normal, &file("write", &key), 1, FlowAnswer::Allow);
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        assert_eq!(asked, 0);
        // A link in the temp directory to a project file is a project write.
        let link = f.root().join("tmp/link");
        std::os::unix::fs::symlink(f.cx.workspace.join("a.rs"), &link).unwrap();
        let (decision, asked) = f.decide_with(
            Control::Normal,
            &file("write", &link.display().to_string()),
            1,
            FlowAnswer::AskCaptain,
        );
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        assert_eq!(asked, 1);
        // An ask rule still asks.
        let other = f.root().join("tmp/y").display().to_string();
        f.add("write", &literal(&other), Effect::Ask);
        let decision = f.decide(Control::Normal, &file("write", &other));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
    }

    #[test]
    fn an_edit_outside_temp_still_goes_to_the_flow() {
        let f = fixture();
        for path in [
            "src/a.rs".to_owned(),
            f.root().join("outside/b").display().to_string(),
        ] {
            let (decision, asked) = f.decide_with(
                Control::Normal,
                &file("edit", &path),
                1,
                FlowAnswer::AskCaptain,
            );
            assert!(
                matches!(decision.verdict, Verdict::AskCaptain { .. }),
                "{path}"
            );
            assert_eq!(asked, 1, "{path}");
        }
        // A project that lives in the temp directory is judged as a project.
        let cx = FloorContext {
            temp_roots: vec![f.root()],
            ..f.cx.clone()
        };
        let inner = Fixture { cx, ..f };
        let (_, asked) = inner.decide_with(
            Control::Normal,
            &file("write", "src/a.rs"),
            1,
            FlowAnswer::AskCaptain,
        );
        assert_eq!(asked, 1);
        let f = inner;
        // A bash write to the temp directory is the flow's to judge.
        let line = format!("echo x > {}", f.root().join("tmp/x").display());
        let (_, asked) = f.decide_with(Control::Normal, &bash(&line), 1, FlowAnswer::Allow);
        assert_eq!(asked, 1);
    }

    #[test]
    fn deny_comes_before_bypass_and_flow() {
        let f = fixture();
        let rule = f.add("bash", "rm *", Effect::Deny);
        for control in [Control::Normal, Control::Bypass] {
            let (decision, asked) =
                f.decide_with(control, &bash("rm -rf build"), 1, allow(FlowAnswer::Allow));
            assert!(
                matches!(&decision.verdict, Verdict::Deny { rule: Some(r), .. } if r.id == rule.id)
            );
            assert_eq!(asked, 0);
            assert_eq!(decision.trace.rule_id, Some(rule.id));
        }
    }

    #[test]
    fn bash_is_judged_per_command_and_the_strictest_wins() {
        let f = fixture();
        f.add("bash", "git status *", Effect::Allow);
        f.add("bash", "cargo test *", Effect::Allow);
        let (decision, asked) = f.decide_with(
            Control::Normal,
            &bash("git status && cargo test -p a"),
            1,
            FlowAnswer::ApplyRule,
        );
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
        assert_eq!(asked, 1);
        // A partially matched line asks without proposing reusable approval.
        let decision = f.decide(Control::Normal, &bash("git status && rm -rf x"));
        let always = proposals(&decision);
        assert!(always.is_empty());
        // One denied command denies the line.
        f.add("bash", "rm *", Effect::Deny);
        let decision = f.decide(Control::Bypass, &bash("git status && rm -rf x"));
        assert!(matches!(decision.verdict, Verdict::Deny { .. }));
        // An ask rule on one command makes the line ask even when the flow is silent.
        f.add("bash", "git push *", Effect::Ask);
        let decision = f.decide(Control::Normal, &bash("git status && git push origin main"));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
    }

    #[test]
    fn bash_rules_match_rule_text_and_not_the_display_text() {
        let f = fixture();
        f.add("bash", "git push *", Effect::Deny);
        let decision = f.decide(Control::Bypass, &bash("git -C ../x push origin main"));
        assert!(matches!(decision.verdict, Verdict::Deny { .. }));
        f.add("bash", "git status *", Effect::Allow);
        let decision = f.decide(Control::Normal, &bash("git status"));
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
    }

    #[test]
    fn opaque_commands_only_meet_a_deny_on_star() {
        let f = fixture();
        let line = "if true; then ls; fi";
        assert!(split(line)[0].opaque);
        f.add("bash", "*", Effect::Allow);
        f.add("bash", "if *", Effect::Allow);
        f.add("bash", "if *", Effect::Deny);
        let (decision, asked) =
            f.decide_with(Control::Normal, &bash(line), 1, FlowAnswer::ApplyRule);
        assert!(proposals(&decision).is_empty());
        assert_eq!(asked, 1);
        // Even with a flow allow, "Always" is not offered for the question the flow raises.
        let (decision, _) = f.decide_with(
            Control::Normal,
            &bash(line),
            1,
            allow(FlowAnswer::AskCaptain),
        );
        assert!(proposals(&decision).is_empty());
        let (decision, _) = f.decide_with(Control::Bypass, &bash(line), 1, FlowAnswer::ApplyRule);
        assert_eq!(run_by(&decision), Some(ApprovedBy::Bypass));
        let star = f.add("bash", "*", Effect::Deny);
        let decision = f.decide(Control::Bypass, &bash(line));
        assert!(
            matches!(&decision.verdict, Verdict::Deny { rule: Some(r), .. } if r.id == star.id)
        );
    }

    #[test]
    fn destructive_or_writing_commands_offer_no_always() {
        let f = fixture();
        for line in [
            "rm -v f",
            "echo x > f",
            "git status && rm -v f",
            "sqlite3 x.db .tables",
        ] {
            let decision = f.decide(Control::Normal, &bash(line));
            assert!(proposals(&decision).is_empty(), "{line}");
        }
        for (line, pattern) in [
            ("sqlite3 -readonly x.db 'SELECT 1'", "sqlite3 *"),
            ("git status", "git status *"),
        ] {
            let always = proposals(&f.decide(Control::Normal, &bash(line)));
            assert_eq!(always.len(), 1, "{line}");
            assert_eq!(always[0].resource, pattern);
        }
    }

    #[test]
    fn a_line_with_an_opaque_part_offers_no_always() {
        let f = fixture();
        let decision = f.decide(
            Control::Normal,
            &bash("git status && for i in 1; do ls; done"),
        );
        assert!(proposals(&decision).is_empty());
    }

    #[test]
    fn outside_project_write_asks_and_needs_both_rules() {
        let f = fixture();
        let target = f.root().join("outside/note.txt").display().to_string();
        let write = file("write", &target);
        let decision = f.decide(Control::Normal, &write);
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        let always = proposals(&decision);
        assert_eq!(always.len(), 2);
        assert_eq!(always[0].action, "write");
        assert_eq!(always[0].resource, literal(&target));
        assert_eq!(always[1].action, "external_directory");
        assert_eq!(
            always[1].resource,
            format!(
                "{}/*",
                literal(&f.root().join("outside").display().to_string())
            )
        );
        // The file rule alone is not enough.
        f.add("write", &literal(&target), Effect::Allow);
        let decision = f.decide(Control::Normal, &write);
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        assert!(proposals(&decision).is_empty());
        // Both together allow it.
        f.add(
            "external_directory",
            &format!(
                "{}/*",
                literal(&f.root().join("outside").display().to_string())
            ),
            Effect::Allow,
        );
        let decision = f.decide(Control::Normal, &write);
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
        // A deny on the directory wins over the allowed file.
        f.add(
            "external_directory",
            &format!(
                "{}/*",
                literal(&f.root().join("outside").display().to_string())
            ),
            Effect::Deny,
        );
        let decision = f.decide(Control::Normal, &write);
        assert!(matches!(decision.verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn inside_project_file_rules_use_the_canonical_literal_path() {
        let f = fixture();
        let target = f.cx.workspace.join("a*b.txt");
        let resource = literal(&target.display().to_string());
        f.add("write", &resource, Effect::Allow);
        let decision = f.decide(Control::Normal, &file("write", "a*b.txt"));
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
        // A star in the name is not a wildcard.
        let decision = f.decide(Control::Normal, &file("write", "aXb.txt"));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        let always = proposals(&decision);
        assert_eq!(always.len(), 1);
        assert_eq!(always[0].scope, RuleScope::Project(f.cx.workspace.clone()));
        assert_eq!(
            always[0].resource,
            literal(&f.cx.workspace.join("aXb.txt").display().to_string())
        );
        // A relative path and `..` fold to the same resource.
        let decision = f.decide(Control::Normal, &file("write", "src/../a*b.txt"));
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
    }

    fn fetch(url: &str) -> ToolCall {
        call("fetch", json!({ "url": url }))
    }

    #[test]
    fn fetch_uses_the_url_and_proposes_the_origin() {
        let f = fixture();
        let decision = f.decide(Control::Normal, &fetch("https://docs.rs/x/y?q=1"));
        let always = proposals(&decision);
        assert_eq!(always.len(), 1);
        assert_eq!(always[0].action, "fetch");
        assert_eq!(always[0].resource, "https://docs.rs/*");
        assert_eq!(always[0].scope, RuleScope::Project(f.cx.workspace.clone()));
        f.add("fetch", "https://docs.rs/*", Effect::Allow);
        let decision = f.decide(Control::Normal, &fetch("https://docs.rs/x"));
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
        let decision = f.decide(Control::Normal, &fetch("https://evil.example/x"));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
    }

    #[test]
    fn fetch_urls_have_one_canonical_form_for_lookup_and_proposal() {
        let f = fixture();
        // Whatever the spelling, "Always" saves a rule the same call then matches.
        for (url, origin) in [
            ("https://docs.rs", "https://docs.rs"),
            ("HTTPS://Docs.RS/A?b#frag", "https://docs.rs"),
            ("https://docs.rs:443/x", "https://docs.rs"),
            ("http://docs.rs:80/x", "http://docs.rs"),
            ("https://docs.rs:8443/x", "https://docs.rs:8443"),
            ("http://localhost:3000/", "http://localhost:3000"),
            ("https://bücher.example/x", "https://xn--bcher-kva.example"),
            ("http://[::1]:8080/x", "http://[::1]:8080"),
            ("https://[2001:DB8::1]/", "https://[2001:db8::1]"),
            ("  https://docs.rs/x  ", "https://docs.rs"),
        ] {
            let always = proposals(&f.decide(Control::Normal, &fetch(url)));
            assert_eq!(always.len(), 1, "{url}");
            assert_eq!(always[0].resource, format!("{origin}/*"), "{url}");
            let rule = f.add("fetch", &always[0].resource, Effect::Allow);
            let decision = f.decide(Control::Normal, &fetch(url));
            assert_eq!(run_by(&decision), Some(ApprovedBy::Rule), "{url}");
            f.store.revoke(rule.id).unwrap();
        }
        // A rule on the exact canonical URL matches the other spellings of it.
        f.add("fetch", "https://docs.rs/a", Effect::Deny);
        for url in ["HTTPS://DOCS.rs:443/a", "https://docs.rs/a#top"] {
            let decision = f.decide(Control::Bypass, &fetch(url));
            assert!(matches!(decision.verdict, Verdict::Deny { .. }), "{url}");
        }
    }

    #[test]
    fn fetch_urls_the_tool_refuses_get_no_proposal() {
        let f = fixture();
        for url in [
            "https://user@docs.rs/x",
            "https://user:pw@docs.rs/x",
            "ftp://docs.rs/x",
            "file:///etc/passwd",
            "not a url",
            "https://",
        ] {
            let decision = f.decide(Control::Normal, &fetch(url));
            assert!(proposals(&decision).is_empty(), "{url}");
        }
        // A rule on the origin does not cover credentials in the URL.
        f.add("fetch", "https://docs.rs/*", Effect::Allow);
        let decision = f.decide(Control::Normal, &fetch("https://user@docs.rs/x"));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
    }

    #[test]
    fn output_options_and_read_write_redirections_ask_under_bypass_and_allow_rules() {
        let f = fixture();
        let config = f.root().join("config/config.json").display().to_string();
        f.add("bash", "*", Effect::Allow);
        f.add("bash", "sort *", Effect::Allow);
        for line in [
            format!("sort -o {config} in"),
            format!("sort --output={config} in"),
            format!("echo '{{}}' 1<> {config}"),
            format!("sort -o{config} in"),
            format!("if true; then sort -o{config} input; fi"),
        ] {
            for control in [Control::Normal, Control::Bypass] {
                let flows = Script::new(FlowAnswer::Allow, ProtectedCheck::Failed);
                let decision = f.decide_full(control, &bash(&line), 1, &flows);
                assert!(
                    matches!(&decision.verdict, Verdict::AskCaptain { always, .. } if always.is_empty()),
                    "{line} {decision:?}"
                );
                assert_eq!(flows.flow_calls.get(), 0, "{line}");
            }
        }
        // A plain read of it stays free of the floor.
        let decision = f.decide(Control::Bypass, &bash(&format!("sort {config}")));
        assert_eq!(run_by(&decision), Some(ApprovedBy::Bypass));
    }

    #[test]
    fn attached_output_to_a_credential_asks_under_bypass() {
        let f = fixture();
        f.add("bash", "*", Effect::Allow);
        for line in ["sort -o.env input", "if true; then sort -o.env input; fi"] {
            for control in [Control::Normal, Control::Bypass] {
                let decision = f.decide(control, &bash(line));
                assert!(
                    matches!(&decision.verdict, Verdict::AskCaptain { always, .. } if always.is_empty()),
                    "{line}"
                );
                assert!(decision
                    .trace
                    .floor
                    .as_deref()
                    .unwrap()
                    .starts_with("credential"));
            }
        }
    }

    #[test]
    fn a_link_below_the_flows_directory_needs_the_protected_check() {
        let f = fixture();
        let root = f.root();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        let inside = root.join("config/flows/custom.toml");
        std::os::unix::fs::symlink(root.join("elsewhere/custom.toml"), &inside).unwrap();
        f.add("write", "*", Effect::Allow);
        let write = file("write", &inside.display().to_string());
        for control in [Control::Normal, Control::Bypass] {
            let flows = Script::new(FlowAnswer::Allow, ProtectedCheck::Failed);
            let decision = f.decide_full(control, &write, 1, &flows);
            assert!(
                matches!(&decision.verdict, Verdict::AskCaptain { always, .. } if always.is_empty())
            );
            assert_eq!(flows.check_calls.get(), 1);
        }
    }

    #[test]
    fn a_path_that_cannot_be_resolved_asks_and_is_never_saved() {
        let f = fixture();
        let project = &f.cx.workspace;
        std::os::unix::fs::symlink(project.join("b"), project.join("a")).unwrap();
        std::os::unix::fs::symlink(project.join("a"), project.join("b")).unwrap();
        f.add("*", "*", Effect::Allow);
        for control in [Control::Normal, Control::Bypass] {
            for call in [file("write", "a/x"), file("read", "a/x"), bash("touch a/x")] {
                let decision = f.decide(control, &call);
                assert!(
                    matches!(&decision.verdict, Verdict::AskCaptain { always, .. } if always.is_empty()),
                    "{call:?}"
                );
                assert!(decision
                    .trace
                    .floor
                    .as_deref()
                    .unwrap()
                    .starts_with("unresolved"));
            }
        }
        // Yolo skips the floor.
        let decision = f.decide(Control::Yolo, &file("write", "a/x"));
        assert_eq!(run_by(&decision), Some(ApprovedBy::Yolo));
    }

    #[test]
    fn mcp_and_other_tools_use_star_with_a_global_proposal() {
        let f = fixture();
        let tool = call("mcp__github__create_issue", json!({"title": "x"}));
        let always = proposals(&f.decide(Control::Normal, &tool));
        assert_eq!(
            always,
            vec![RuleProposal {
                action: "mcp__github__create_issue".into(),
                resource: "*".into(),
                scope: RuleScope::Global,
            }]
        );
        f.add("mcp__github__create_issue", "*", Effect::Allow);
        assert_eq!(
            run_by(&f.decide(Control::Normal, &tool)),
            Some(ApprovedBy::Rule)
        );
    }

    #[test]
    fn bypass_runs_what_no_rule_denies() {
        let f = fixture();
        f.add("bash", "git push *", Effect::Ask);
        let (decision, asked) = f.decide_with(
            Control::Bypass,
            &bash("git push origin main"),
            1,
            allow(FlowAnswer::Deny),
        );
        assert_eq!(run_by(&decision), Some(ApprovedBy::Bypass));
        assert_eq!(asked, 0);
    }

    #[test]
    fn flow_verdicts_and_the_rule_fallback() {
        let f = fixture();
        let call = bash("make check");
        let (decision, _) = f.decide_with(Control::Normal, &call, 1, allow(FlowAnswer::Allow));
        assert_eq!(run_by(&decision), Some(ApprovedBy::Flow));
        assert_eq!(decision.trace.flow_verdict, Some("allow"));
        let (decision, _) = f.decide_with(Control::Normal, &call, 1, allow(FlowAnswer::AskCaptain));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        let (decision, _) = f.decide_with(Control::Normal, &call, 1, allow(FlowAnswer::Deny));
        assert!(matches!(
            &decision.verdict,
            Verdict::Deny { rule: None, .. }
        ));
        // No verdict: no rule asks, an ask rule asks, an allow rule runs.
        let (decision, _) = f.decide_with(Control::Normal, &call, 1, FlowAnswer::ApplyRule);
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        let ask = f.add("bash", "make *", Effect::Ask);
        let (decision, _) = f.decide_with(Control::Normal, &call, 1, FlowAnswer::ApplyRule);
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        assert_eq!(decision.trace.rule_id, Some(ask.id));
        let allow_rule = f.add("bash", "make check *", Effect::Allow);
        let (decision, _) = f.decide_with(Control::Normal, &call, 1, FlowAnswer::ApplyRule);
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
        assert_eq!(decision.trace.rule_id, Some(allow_rule.id));
        assert_eq!(decision.trace.effect, Some(Effect::Allow));
    }

    #[test]
    fn the_flow_sees_the_rule_result() {
        let f = fixture();
        let rule = f.add("bash", "make *", Effect::Allow);
        let flows = Script::new(FlowAnswer::AskCaptain, ProtectedCheck::Unavailable);
        f.decide_full(Control::Normal, &bash("make all"), 1, &flows);
        assert_eq!(flows.seen.get(), Some((RuleResult::Allow, Some(rule.id))));
    }

    #[test]
    fn doom_loop_counts_resets_and_bypass_skips_it() {
        let f = fixture();
        f.add("bash", "ls *", Effect::Allow);
        let call = bash("ls -la");
        let (decision, _) = f.decide_with(Control::Normal, &call, 2, FlowAnswer::ApplyRule);
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
        let (decision, _) = f.decide_with(Control::Normal, &call, 3, FlowAnswer::ApplyRule);
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        assert!(decision.trace.doom_loop);
        let (decision, _) = f.decide_with(Control::Bypass, &call, 3, FlowAnswer::ApplyRule);
        assert_eq!(run_by(&decision), Some(ApprovedBy::Bypass));
        // A free read is stopped too.
        let read = file("read", "a.rs");
        let (decision, _) = f.decide_with(Control::Normal, &read, 3, FlowAnswer::ApplyRule);
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
        // A rule on the loop decides it.
        f.add("doom_loop", "bash", Effect::Allow);
        let (decision, _) = f.decide_with(Control::Normal, &call, 5, FlowAnswer::ApplyRule);
        assert_eq!(run_by(&decision), Some(ApprovedBy::Rule));
        f.add("doom_loop", "bash", Effect::Deny);
        let (decision, _) = f.decide_with(Control::Bypass, &call, 5, FlowAnswer::ApplyRule);
        assert!(matches!(decision.verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn a_written_deny_wins_over_a_failing_loop_lookup() {
        let f = fixture();
        f.add("bash", "rm *", Effect::Deny);
        *f.store.fail_action.lock().unwrap() = Some("doom_loop".into());
        let call = bash("rm -rf target");
        let (decision, asked) = f.decide_with(
            Control::Normal,
            &call,
            DOOM_LOOP_THRESHOLD,
            FlowAnswer::Allow,
        );
        assert!(
            matches!(decision.verdict, Verdict::Deny { rule: Some(_), .. }),
            "{decision:?}"
        );
        assert_eq!(asked, 0);
        // Without a deny the failed loop lookup still asks, in its place.
        let call = bash("ls -la");
        let (decision, _) = f.decide_with(
            Control::Normal,
            &call,
            DOOM_LOOP_THRESHOLD,
            FlowAnswer::Allow,
        );
        assert!(
            matches!(decision.verdict, Verdict::AskCaptain { .. }),
            "{decision:?}"
        );
    }

    #[test]
    fn the_tracker_counts_identical_calls_and_resets() {
        let mut tracker = DoomTracker::default();
        let a = bash("ls");
        let b = bash("ls -a");
        assert_eq!(tracker.observe(&a), 1);
        assert_eq!(tracker.observe(&a), 2);
        assert_eq!(tracker.observe(&a), 3);
        assert_eq!(tracker.observe(&b), 1);
        assert_eq!(tracker.observe(&a), 1);
        assert_eq!(tracker.observe(&file("read", "x")), 1);
        // Same arguments under another tool is a different call.
        let same_args = call("write", a.args.clone());
        assert_eq!(tracker.observe(&a), 1);
        assert_eq!(tracker.observe(&same_args), 1);
        assert_eq!(tracker.observe(&a), 1);
        assert_eq!(tracker.observe(&a), 2);
        tracker.new_turn();
        assert_eq!(tracker.observe(&a), 1);
    }

    #[test]
    fn a_rule_database_error_asks_the_captain() {
        let f = fixture();
        f.store
            .db()
            .execute_batch("DROP TABLE permission_rules")
            .unwrap();
        let decision = f.decide(Control::Bypass, &bash("ls"));
        assert!(matches!(decision.verdict, Verdict::AskCaptain { .. }));
    }

    #[test]
    fn the_trace_records_per_command_matches() {
        let f = fixture();
        let rule = f.add("bash", "git status *", Effect::Allow);
        let decision = f.decide(Control::Normal, &bash("git status && ls"));
        let matches = decision.trace.command_matches.as_array().unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0]["rule_id"], rule.id);
        assert!(matches[1]["rule_id"].is_null());
    }

    #[test]
    fn pattern_matching_follows_the_rule_language() {
        assert!(pattern_matches("*", "bash"));
        assert!(pattern_matches("ba*", "bash"));
        assert!(pattern_matches("a\\*b", "a*b"));
        assert!(!pattern_matches("a\\*b", "axb"));
        assert!(!pattern_matches("bash", "bashx"));
    }
}
