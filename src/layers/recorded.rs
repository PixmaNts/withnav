//! A record of the Instinct calls made outside flows: the subagent and root watchdogs, and the
//! compaction pruning judge. Flow runs keep their own records (`flows/records.rs`); before this
//! log these three callers left nothing behind, so a slow or failing Instinct could not be told
//! apart from a quiet one (state size, answers and latency were invisible, 2026-11).
//!
//! One line per call in `<data dir>/instinct-calls.jsonl` ([`crate::paths::instinct_call_log`]).
//! The state is never stored, nor the question instructions: both carry tool output that no
//! redaction has read (the pruning questions quote the output they judge). Its size and SHA-256
//! stand in for it: the hash finds repeated states, the size explains latency. The decision seam
//! returns answers only, so no usage tokens are recorded.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use sha2::Digest;
use tokio_util::sync::CancellationToken;

use crate::provider::systemone::{
    Answers, DecisionError, DecisionModel, Questions, SystemOneClient,
};

/// The file name of the Instinct call log, in the data dir.
pub const LOG_FILE: &str = "instinct-calls.jsonl";

/// Which caller asked: the `caller` field of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Caller {
    /// The watchdog of a subagent session.
    ChildWatchdog,
    /// The watchdog of a root session.
    RootWatchdog,
    /// The compaction judge asking which old tool outputs still matter.
    Prune,
}

/// A decision model that writes one line per call and passes the answer through unchanged. A
/// log that cannot be written is reported to tracing and never changes the answer.
pub struct Recorded {
    inner: Arc<dyn DecisionModel>,
    model: Option<String>,
    caller: Caller,
    session_id: String,
    log: PathBuf,
}

impl std::fmt::Debug for Recorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorded")
            .field("model", &self.model)
            .field("caller", &self.caller)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl Recorded {
    /// Wraps `inner`, writing to `log`. `model` is the identifier sent on the wire when it is
    /// known (`None` for a model that is not a System One client, such as a test judge).
    pub fn new(
        inner: Arc<dyn DecisionModel>,
        model: Option<String>,
        caller: Caller,
        session_id: String,
        log: PathBuf,
    ) -> Self {
        Self {
            inner,
            model,
            caller,
            session_id,
            log,
        }
    }

    /// Wraps the configured Instinct client for `caller` in `session_id`, logging to the call
    /// log of `data_dir`.
    pub fn instinct(
        client: SystemOneClient,
        caller: Caller,
        session_id: impl ToString,
        data_dir: &Path,
    ) -> Arc<dyn DecisionModel> {
        let model = Some(client.model().to_owned());
        Arc::new(Self::new(
            Arc::new(client),
            model,
            caller,
            session_id.to_string(),
            crate::paths::instinct_call_log(data_dir),
        ))
    }
}

impl DecisionModel for Recorded {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        questions: &'a Questions,
        cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<Answers, DecisionError>> {
        Box::pin(async move {
            let mut call = Call {
                recorded: self,
                state,
                questions,
                started: Instant::now(),
                written: false,
            };
            let result = self.inner.decide(state, questions, cancel).await;
            match &result {
                Ok(answers) => call.write(Some(answers), None),
                Err(error) => call.write(None, Some(error_kind(*error))),
            }
            result
        })
    }
}

fn error_kind(error: DecisionError) -> &'static str {
    match error {
        DecisionError::Unavailable => "unavailable",
        DecisionError::Invalid => "invalid",
        DecisionError::Cancelled => "cancelled",
    }
}

/// One call in flight. Dropped before its answer (the caller's deadline or cancellation won the
/// race), it still writes its line, with the error `abandoned`: a call that overran its deadline
/// is the latency this log exists to show.
struct Call<'a> {
    recorded: &'a Recorded,
    state: &'a str,
    questions: &'a Questions,
    started: Instant,
    written: bool,
}

impl Call<'_> {
    fn write(&mut self, answers: Option<&Answers>, error: Option<&'static str>) {
        self.written = true;
        let recorded = self.recorded;
        let line = Line {
            ts: crate::bus::event::now_rfc3339(),
            caller: recorded.caller,
            session_id: &recorded.session_id,
            model: recorded.model.as_deref(),
            state_bytes: self.state.len(),
            state_sha256: hex::encode(sha2::Sha256::digest(self.state.as_bytes())),
            questions: self.questions.keys().map(String::as_str).collect(),
            answers,
            latency_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            error,
        };
        crate::jsonl::append(&recorded.log, &line, "instinct call log");
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        if !self.written {
            self.write(None, Some("abandoned"));
        }
    }
}

/// One line of the call log. `answers` is set on success, `error` otherwise.
#[derive(Serialize)]
struct Line<'a> {
    ts: String,
    caller: Caller,
    session_id: &'a str,
    model: Option<&'a str>,
    state_bytes: usize,
    state_sha256: String,
    questions: Vec<&'a str>,
    answers: Option<&'a Answers>,
    latency_ms: u64,
    error: Option<&'static str>,
}
