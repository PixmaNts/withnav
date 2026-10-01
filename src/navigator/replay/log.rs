//! Parsing of the JSONL decision log into the records a replay needs: usable
//! trigger records, verdicts joined by remark id, and the skipped lines.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::navigator::pipeline::{Action, TriggerInputs, TriggerKind, NO_REMARK_MS};
use crate::navigator::ports::{Answers, Questions};

/// One v2 `trigger` record, as the replay needs it.
#[derive(Debug, Clone)]
pub struct LoggedTrigger {
    pub trigger_id: String,
    pub event: TriggerKind,
    pub uri: String,
    pub version: i64,
    pub ts: Option<String>,
    /// The pre-filter inputs the log recorded, with `has_intent` and
    /// `has_errors` derived from the stored template inputs.
    pub inputs: TriggerInputs,
    /// The questions the logged pipeline asked, when it got that far.
    pub questions: Option<Questions>,
    /// The logged instinct answers (`None` when the pre-filters skipped).
    pub answers: Option<Answers>,
    /// The structured template variable map, for a re-ask.
    pub template_inputs: BTreeMap<String, String>,
    /// Whether the record stored the template inputs at all.
    pub has_template_inputs: bool,
    /// The action the logged pipeline requested. `Some(Silent)` also covers a
    /// record that never reached a branch: a pre-filter skip requested
    /// nothing, which is silence. `None` is an action this build does not
    /// know, and counts as unknown.
    pub historical_action: Option<Action>,
    /// What was delivered: `remark`, `silent`, `none`, `cancelled`, ...
    pub delivered: String,
    /// The delivered remark, the only thing a verdict can label.
    pub remark_id: Option<String>,
}

/// A parsed decision log: the usable trigger records, the verdicts joined by
/// remark id, and the lines that were skipped.
#[derive(Debug, Clone, Default)]
pub struct ReplayLog {
    /// Where the log was read from, for the report header.
    pub path: Option<PathBuf>,
    pub triggers: Vec<LoggedTrigger>,
    /// Remark id to the driver's verdict, latest wins.
    pub verdicts: BTreeMap<String, String>,
    /// Non-empty lines read.
    pub lines: usize,
    /// Lines that are not usable v2 trigger records, by reason.
    pub skipped_by_reason: BTreeMap<String, usize>,
}

/// Why one line is not a usable trigger record.
#[derive(Debug, Clone, Copy)]
enum Skip {
    /// The prototype-era `trigger` line: no `trigger_id`, no pre-filter
    /// record, nothing a frozen replay can evaluate.
    Legacy,
    /// A pipeline-era line whose fields do not parse.
    Unusable,
}

impl Skip {
    fn reason(self) -> &'static str {
        match self {
            Self::Legacy => "legacy_trigger",
            Self::Unusable => "unusable_trigger",
        }
    }
}

impl ReplayLog {
    /// Parses a whole JSONL log. Unknown kinds, the older prototype format
    /// and unparsable lines are counted, never fatal.
    pub fn parse(text: &str) -> Self {
        let mut log = Self::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            log.lines += 1;
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => {
                    log.skip("not_json");
                    continue;
                }
            };
            match value.get("kind").and_then(Value::as_str) {
                Some("trigger") => match LoggedTrigger::from_value(&value) {
                    Ok(trigger) => log.triggers.push(trigger),
                    Err(skip) => log.skip(skip.reason()),
                },
                // Verdicts label the remark they name, nothing else.
                Some("feedback") => match value.get("id").and_then(Value::as_str) {
                    Some(id) => {
                        let verdict = value
                            .get("verdict")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        log.verdicts.insert(id.to_string(), verdict.to_string());
                    }
                    None => log.skip("unusable_feedback"),
                },
                Some(other) => log.skip(&format!("kind:{other}")),
                None => log.skip("no_kind"),
            }
        }
        log
    }

    /// Reads and parses a log file. A missing or unreadable file is an error;
    /// an unparsable line inside it is not.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let mut log = Self::parse(&std::fs::read_to_string(path)?);
        log.path = Some(path.to_path_buf());
        Ok(log)
    }

    /// Lines that are not usable v2 trigger records.
    pub fn skipped(&self) -> usize {
        self.skipped_by_reason.values().sum()
    }

    fn skip(&mut self, reason: &str) {
        *self
            .skipped_by_reason
            .entry(reason.to_string())
            .or_insert(0) += 1;
    }
}

impl LoggedTrigger {
    fn from_value(value: &Value) -> Result<Self, Skip> {
        // `trigger_id` only exists in the pipeline-era record: its absence is
        // the older prototype format, which carries no questions, no answers
        // and no pre-filter record.
        let trigger_id = value
            .get("trigger_id")
            .and_then(Value::as_str)
            .ok_or(Skip::Legacy)?
            .to_string();
        let event = value
            .get("event")
            .and_then(Value::as_str)
            .and_then(trigger_kind)
            .ok_or(Skip::Unusable)?;
        let inputs = value
            .get("prefilter")
            .and_then(|p| p.get("inputs"))
            .ok_or(Skip::Unusable)?;
        let changed_lines = inputs
            .get("changed_lines")
            .and_then(Value::as_i64)
            .ok_or(Skip::Unusable)?;
        // A trigger that never remarked logged no cooldown: that is the
        // "never" value the server itself uses.
        let since_last_remark_ms = inputs
            .get("since_last_remark_ms")
            .and_then(Value::as_i64)
            .unwrap_or(NO_REMARK_MS);

        // Records from before wakes had their own cooldown carry no such
        // input: they replay as if no wake had happened.
        let since_last_wake_ms = inputs
            .get("since_last_wake_ms")
            .and_then(Value::as_i64)
            .unwrap_or(NO_REMARK_MS);

        let template_inputs: BTreeMap<String, String> = value
            .get("template_inputs")
            .and_then(Value::as_object)
            .map(|vars| {
                vars.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let has_template_inputs = value.get("template_inputs").is_some();
        // Records carry the flags the server computed. Older records did
        // not, so those are derived from the stored template inputs (the
        // intent record, the latest diagnostics, the conversation digest);
        // that guess can be fooled by prose, which is why a logged value
        // always wins.
        let logged_flag = |key: &str| inputs.get(key).and_then(Value::as_bool);
        let has_intent = logged_flag("has_intent").unwrap_or_else(|| {
            template_inputs
                .get("intent")
                .is_some_and(|intent| !intent.trim().is_empty())
        });
        let has_errors = logged_flag("has_errors").unwrap_or_else(|| {
            template_inputs
                .get("diagnostics")
                .is_some_and(|diagnostics| diagnostics.contains("[error]"))
        });
        let has_conversation = logged_flag("has_conversation").unwrap_or_else(|| {
            template_inputs
                .get("conversation")
                .is_some_and(|conversation| !conversation.trim().is_empty())
        });

        Ok(Self {
            trigger_id,
            event,
            uri: value
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            version: value
                .get("version")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            ts: value.get("ts").and_then(Value::as_str).map(str::to_string),
            inputs: TriggerInputs {
                changed_lines,
                since_last_remark_ms,
                since_last_wake_ms,
                // Records from before the call-for-help input replay as none.
                called_for_help: inputs
                    .get("called_for_help")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                has_errors,
                has_intent,
                has_conversation,
            },
            questions: optional_from_value(value.get("questions"))?,
            answers: optional_from_value(value.get("answers"))?,
            has_template_inputs,
            template_inputs,
            historical_action: historical_action(value.get("requested_action"))?,
            delivered: value
                .get("delivered")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            remark_id: value
                .get("remark_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

fn optional_from_value<T: serde::de::DeserializeOwned>(
    value: Option<&Value>,
) -> Result<Option<T>, Skip> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| Skip::Unusable),
    }
}

/// The logged `event`, as the pipeline's `on` lists them.
fn trigger_kind(event: &str) -> Option<TriggerKind> {
    match event {
        "save" => Some(TriggerKind::Save),
        "idle" => Some(TriggerKind::Idle),
        "diagnostics" => Some(TriggerKind::Diagnostics),
        "conversation" => Some(TriggerKind::Conversation),
        _ => None,
    }
}

/// A record with no `requested_action` never reached a branch: a pre-filter
/// skip, a pipeline that could not run, nothing requested. That is silence.
fn historical_action(value: Option<&Value>) -> Result<Option<Action>, Skip> {
    match value {
        None | Some(Value::Null) => Ok(Some(Action::Silent)),
        Some(Value::String(action)) => Ok(match action.as_str() {
            "silent" => Some(Action::Silent),
            "remark" => Some(Action::Remark),
            "wake_cognition" => Some(Action::WakeCognition),
            _ => None,
        }),
        Some(_) => Err(Skip::Unusable),
    }
}
