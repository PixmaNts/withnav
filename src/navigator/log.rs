//! Append-only JSONL log: every trigger decision, model call, remark,
//! silence, error and verdict. The future training set for the instinct's
//! decision pipeline, and the frozen history replay runs against.
//!
//! Record kinds:
//! - `trigger`: one trigger with its pre-filter inputs/outcome, the pipeline
//!   snapshot and instinct question/answer (when a pipeline ran), the
//!   requested action and the delivered outcome.
//! - `call`: one cost-relevant model call (layer, model, latency, tokens).
//! - `reflex_shown`: the extension reporting when a suggestion was displayed.
//! - `remark` / `silent` / `malformed` / `error` / `feedback`: the prototype
//!   commenter path and the driver's verdicts.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug)]
pub struct NavLog {
    path: PathBuf,
}

/// Pre-filter inputs and outcome for a trigger: the named scalars the
/// pre-filters saw (`changed_lines`, cooldown state, ...) and whether they
/// let the trigger through.
#[derive(Serialize)]
pub struct PrefilterRecord<'a> {
    pub inputs: &'a BTreeMap<String, Value>,
    /// "pass" | "skip"
    pub outcome: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
}

/// One trigger, start to delivered outcome. The pipeline-era fields
/// (`pipeline_hash` onward) are filled only when a decision pipeline ran;
/// the prototype commenter path leaves them out.
#[derive(Serialize)]
pub struct TriggerRecord<'a> {
    pub trigger_id: &'a str,
    /// One per server process.
    pub run_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<&'a str>,
    pub uri: &'a str,
    /// Document version at the trigger.
    pub version: i32,
    pub event: &'a str,
    pub prefilter: PrefilterRecord<'a>,
    /// Snapshot hash of the pipeline (TOML plus every referenced prompt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_hash: Option<&'a str>,
    /// The structured template variable map (not the rendered text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_inputs: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered_state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub questions: Option<&'a Value>,
    /// The resolved instinct model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answers: Option<&'a Value>,
    /// Index of the branch that matched, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<usize>,
    /// The action the chosen branch requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_action: Option<&'a str>,
    /// What was actually delivered: "remark" (with `remark_id`), "silent",
    /// "cancelled", "provider_error", "malformed", or "none" (with `reason`).
    pub delivered: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remark_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
    /// The cognition session a `wake_cognition` action targeted: set when
    /// delivered `wake` or `wake_queued`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// The trigger record is the fat one by design: one structured decision per
// line. Records are built on the stack for one append and dropped; boxing
// the variant would trade a copy for an allocation on every log line.
#[allow(clippy::large_enum_variant)]
pub enum LogRecord<'a> {
    Trigger(TriggerRecord<'a>),
    Call {
        /// "reflex" | "instinct" | "metacognition" | "cognition"
        layer: &'a str,
        model: &'a str,
        latency_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        cancelled: bool,
    },
    ReflexShown {
        request_id: &'a str,
        shown_at_ms: u64,
    },
    Remark {
        id: &'a str,
        uri: &'a str,
        line: u32,
        text: &'a str,
        model: &'a str,
        latency_ms: u128,
    },
    Silent {
        uri: &'a str,
        model: &'a str,
        latency_ms: u128,
    },
    Malformed {
        uri: &'a str,
        model: &'a str,
        latency_ms: u128,
        /// Truncated raw model output, for post-mortem and tuning.
        raw_excerpt: &'a str,
    },
    Error {
        uri: &'a str,
        message: &'a str,
    },
    Feedback {
        id: &'a str,
        verdict: &'a str,
    },
}

impl NavLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Appends one record; I/O failures are logged to tracing, never fatal
    /// (a flaky log must not break the editor session).
    pub fn append(&self, record: &LogRecord<'_>) {
        let mut entry = serde_json::to_value(record).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut entry {
            map.insert("ts".to_string(), Value::String(now_iso8601().to_string()));
        }
        let mut line = serde_json::to_string(&entry).unwrap_or_default();
        line.push('\n');
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = res {
            tracing::warn!(path = %self.path.display(), error = %e, "navigator log append failed");
        }
    }

    /// Reads back all records (tests only).
    pub fn read_all(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Fresh lowercase ULID: trigger, remark and run ids share one format.
pub fn new_ulid_string() -> String {
    ulid::Ulid::new().to_string().to_lowercase()
}

/// Truncates a string for a log field, cutting on a char boundary and
/// marking the cut with an ellipsis.
pub fn excerpt(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max_chars).collect();
    s.push('…');
    s
}

/// UTC timestamp with millisecond precision, no external dep.
pub(crate) fn now_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_jsonl_records_with_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let log = NavLog::new(tmp.path().join("nav.jsonl"));
        let inputs = BTreeMap::from([("changed_lines".to_string(), Value::from(5))]);
        log.append(&LogRecord::Trigger(TriggerRecord {
            trigger_id: "t1",
            run_id: "run1",
            workspace: Some("file:///ws"),
            uri: "file:///a",
            version: 3,
            event: "idle",
            prefilter: PrefilterRecord {
                inputs: &inputs,
                outcome: "pass",
                reason: None,
            },
            pipeline_hash: None,
            template_inputs: None,
            rendered_state: None,
            questions: None,
            model: None,
            answers: None,
            branch: None,
            requested_action: None,
            delivered: "remark",
            remark_id: Some("r1"),
            reason: None,
            session: None,
        }));
        log.append(&LogRecord::Feedback {
            id: "r1",
            verdict: "useful",
        });
        let records = log.read_all();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["kind"], "trigger");
        assert_eq!(records[0]["prefilter"]["outcome"], "pass");
        assert_eq!(records[0]["prefilter"]["inputs"]["changed_lines"], 5);
        assert_eq!(records[0]["delivered"], "remark");
        assert_eq!(records[0]["remark_id"], "r1");
        // Pipeline-era fields are absent, not null, until a pipeline runs.
        assert!(records[0].get("pipeline_hash").is_none());
        assert!(records[0].get("questions").is_none());
        assert_eq!(records[1]["kind"], "feedback");
        assert_eq!(records[1]["verdict"], "useful");
        assert!(records[0]["ts"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn call_and_reflex_shown_records_serialize() {
        let tmp = tempfile::tempdir().unwrap();
        let log = NavLog::new(tmp.path().join("nav.jsonl"));
        log.append(&LogRecord::Call {
            layer: "instinct",
            model: "typesafe/jev-1.13",
            latency_ms: 340,
            input_tokens: Some(454),
            output_tokens: Some(96),
            cancelled: false,
        });
        log.append(&LogRecord::ReflexShown {
            request_id: "t2",
            shown_at_ms: 1_759_000_000_000,
        });
        let records = log.read_all();
        assert_eq!(records[0]["kind"], "call");
        assert_eq!(records[0]["layer"], "instinct");
        assert_eq!(records[0]["input_tokens"], 454);
        assert_eq!(records[1]["kind"], "reflex_shown");
        assert_eq!(records[1]["request_id"], "t2");
        assert_eq!(
            records[1]["shown_at_ms"],
            serde_json::json!(1_759_000_000_000u64)
        );
    }

    #[test]
    fn malformed_record_carries_raw_excerpt() {
        let tmp = tempfile::tempdir().unwrap();
        let log = NavLog::new(tmp.path().join("nav.jsonl"));
        log.append(&LogRecord::Malformed {
            uri: "file:///a",
            model: "fast",
            latency_ms: 900,
            raw_excerpt: "I think you should",
        });
        let records = log.read_all();
        assert_eq!(records[0]["kind"], "malformed");
        assert_eq!(records[0]["raw_excerpt"], "I think you should");
    }

    #[test]
    fn excerpt_cuts_on_char_boundary_with_marker() {
        assert_eq!(excerpt("abc", 5), "abc");
        assert_eq!(excerpt("abcde", 3), "abc…");
        assert_eq!(excerpt("héllo", 2), "hé…");
    }

    #[test]
    fn ulids_are_unique_lowercase() {
        let a = new_ulid_string();
        let b = new_ulid_string();
        assert_ne!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
    }

    #[test]
    fn civil_from_days_known_date() {
        // 2026-09-29 is day 20725 since epoch.
        assert_eq!(civil_from_days(20_725), (2026, 9, 29));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
