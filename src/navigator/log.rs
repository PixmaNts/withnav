//! Append-only JSONL log: every trigger decision, remark, silence, error and
//! verdict. The future training set for a decision model.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug)]
pub struct NavLog {
    path: PathBuf,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogRecord<'a> {
    Trigger {
        uri: &'a str,
        event: &'a str,
        outcome: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'a str>,
        changed_lines: usize,
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

/// UTC timestamp with millisecond precision, no external dep.
fn now_iso8601() -> String {
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
        log.append(&LogRecord::Trigger {
            uri: "file:///a",
            event: "idle",
            outcome: "fire",
            reason: None,
            changed_lines: 5,
        });
        log.append(&LogRecord::Feedback {
            id: "r1",
            verdict: "useful",
        });
        let records = log.read_all();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["kind"], "trigger");
        assert_eq!(records[0]["outcome"], "fire");
        assert_eq!(records[1]["kind"], "feedback");
        assert_eq!(records[1]["verdict"], "useful");
        assert!(records[0]["ts"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn civil_from_days_known_date() {
        // 2026-09-29 is day 20725 since epoch.
        assert_eq!(civil_from_days(20_725), (2026, 9, 29));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
