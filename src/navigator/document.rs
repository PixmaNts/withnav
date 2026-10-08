//! The navigator's view of the editor's documents: per-uri text state, the
//! diagnostics and recent edits tracked beside it, and the diff/excerpt
//! helpers the prompts are built from.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::bus::event::now_rfc3339;
use crate::navigator::help_call::{added_help_comments, without_comments, HelpCalls};
use crate::navigator::log::excerpt;
use crate::navigator::lsp::uri::uri_to_path_str;
use crate::navigator::metacognition::Diag;
use crate::navigator::trigger::{TriggerKind, NO_REMARK_MS};

/// Edit summaries kept per uri in the live state.
const LIVE_STATE_EDITS: usize = 5;

/// Diagnostics lines kept in the live state, in total across uris.
const LIVE_STATE_DIAGS: usize = 20;

/// Total character cap of the rendered `{{live_state}}`.
const LIVE_STATE_MAX_CHARS: usize = 1500;

/// Per-URI document state tracked by the navigator.
#[derive(Debug)]
pub struct DocumentState {
    /// Current text (full sync).
    pub text: String,
    /// Current document version (from didOpen/didChange).
    pub version: i32,
    /// Text at the last remark, or at open.
    pub baseline: String,
    /// Instant of the last emitted remark, if any.
    pub last_remark: Option<Instant>,
    /// Instant of the last wake of the cognition about this document. Apart
    /// from `last_remark`: a wake never silences remarks.
    pub last_wake: Option<Instant>,
    /// 1-based line of the last edit that inserted text (0: none yet).
    /// The conversation trigger shows the code around it.
    pub last_edit_line: usize,
    /// Version at which the in-flight instinct evaluation was started, if any.
    pub in_flight_version: Option<i32>,
    /// Cancel handle for the in-flight instinct evaluation. Typing cancels
    /// it: the answer would describe text that is gone.
    pub in_flight_cancel: Option<CancellationToken>,
    /// Cancel handle for the in-flight remark request. Typing leaves it
    /// running (the remark is delivered if its line survives); a newer
    /// remark or a close cancels it.
    pub remark_cancel: Option<CancellationToken>,
    /// Document version the running remark request was written for. A
    /// trigger for the same version brings nothing new: it would restart
    /// the request on identical content.
    pub remark_version: Option<i32>,
    /// The calls for help this document's remarks are answering or answered.
    pub help: HelpCalls,
}

impl DocumentState {
    pub fn new(text: String, version: i32) -> Self {
        Self {
            text,
            version,
            baseline: String::new(),
            last_remark: None,
            last_wake: None,
            last_edit_line: 0,
            in_flight_version: None,
            in_flight_cancel: None,
            remark_cancel: None,
            remark_version: None,
            help: HelpCalls::default(),
        }
    }

    /// Full-sync update: replaces text and bumps version.
    pub fn apply_change(&mut self, text: String, version: i32) {
        self.text = text;
        self.version = version;
    }

    /// Number of changed lines since the baseline (added + removed).
    pub fn changed_lines_since_baseline(&self) -> usize {
        count_changed_lines(&self.baseline, &self.text)
    }

    /// Rebases the baseline to the current text (after a remark or silence).
    pub fn rebase(&mut self) {
        self.baseline = self.text.clone();
    }

    /// Rebases the baseline to `text`, the text a remark or silence was
    /// about. Edits made since stay in the next diff.
    pub fn rebase_to(&mut self, text: &str) {
        self.baseline = text.to_string();
    }

    /// Cancels the in-flight instinct evaluation (not a running remark).
    pub fn cancel_instinct(&mut self) {
        if let Some(cancel) = self.in_flight_cancel.take() {
            cancel.cancel();
        }
        self.in_flight_version = None;
    }

    /// Cancels everything in flight for this document, a remark included.
    pub fn cancel_in_flight(&mut self) {
        self.cancel_instinct();
        if let Some(cancel) = self.remark_cancel.take() {
            cancel.cancel();
        }
        self.remark_version = None;
        self.help.end();
    }
}

/// One recorded edit of the live state: when it was seen, in which file and
/// at which document version, how many lines changed, where the first change
/// sits and what that line says.
struct EditSummary {
    ts: String,
    file: String,
    version: i32,
    changed_lines: i64,
    first_line: usize,
    excerpt: String,
}

/// One open document: its text state plus everything else tracked per uri
/// (language id, the latest diagnostics, the recent edit summaries rendered
/// into `{{live_state}}`).
pub struct OpenDocument {
    state: DocumentState,
    language_id: Option<String>,
    diagnostics: Vec<Diag>,
    edits: VecDeque<EditSummary>,
}

/// Everything one trigger needs about a document, gathered under one lock.
pub struct DocumentSnapshot {
    pub version: i32,
    pub changed_lines: i64,
    pub since_last_remark_ms: i64,
    pub since_last_wake_ms: i64,
    pub text: String,
    pub baseline: String,
    /// 1-based line of the last edit that inserted text (0: none yet).
    pub last_edit_line: usize,
    pub language_id: String,
    pub diagnostics: Vec<Diag>,
    /// The rendered `{{live_state}}`: recent edits and diagnostics across
    /// all documents.
    pub live_state: String,
    /// The open documents keyed by absolute path (current editor text).
    pub open_buffers: BTreeMap<PathBuf, String>,
    /// The calls for help in the lines added since the baseline that no
    /// remark answered yet (normalized comment text; empty for a
    /// conversation turn).
    pub help_calls: Vec<String>,
    /// A remark is already being written for every one of `help_calls`.
    pub help_in_flight: bool,
    /// A remark request is already running on this exact version of the
    /// text (never for a conversation turn, which judges no edit).
    pub remark_in_flight: bool,
}

/// The documents the editor has open, keyed by uri string. Diagnostics
/// forwarded for a uri that is not open are kept apart and adopted when it
/// opens, so the live state still sees errors in other files.
#[derive(Default)]
pub struct Documents {
    open: HashMap<String, OpenDocument>,
    unopened_diagnostics: HashMap<String, Vec<Diag>>,
}

impl Documents {
    fn entry(&mut self, uri: &str) -> &mut OpenDocument {
        let unopened = &mut self.unopened_diagnostics;
        self.open
            .entry(uri.to_string())
            .or_insert_with(|| OpenDocument {
                state: DocumentState::new(String::new(), 0),
                language_id: None,
                diagnostics: unopened.remove(uri).unwrap_or_default(),
                edits: VecDeque::new(),
            })
    }

    /// `didOpen`: records the language, replaces the text and rebases.
    /// Returns the document version.
    pub fn open(&mut self, uri: &str, language_id: String, text: String, version: i32) -> i32 {
        let doc = self.entry(uri);
        doc.language_id = Some(language_id);
        doc.state.apply_change(text, version);
        doc.state.rebase();
        doc.state.version
    }

    /// `didChange` (full sync): cancels the in-flight instinct evaluation
    /// (a running remark keeps going) and notes where the edit starts. A change for an unknown uri opens it, rebased.
    pub fn change(&mut self, uri: &str, text: String, version: i32) {
        match self.open.get_mut(uri) {
            Some(doc) => {
                doc.state.cancel_instinct();
                let (line, _) = first_changed_line(&doc.state.text, &text);
                if line > 0 {
                    doc.state.last_edit_line = line;
                }
                doc.state.apply_change(text, version);
                let state = &mut doc.state;
                let gone = state.help.forget_gone(&state.text);
                if !gone.is_empty() {
                    state.baseline = without_comments(&state.baseline, &gone);
                }
            }
            None => {
                let doc = self.entry(uri);
                doc.state.apply_change(text, version);
                doc.state.rebase();
            }
        }
    }

    /// `didClose`: forgets everything tracked for the uri, cancelling its
    /// in-flight evaluation.
    pub fn close(&mut self, uri: &str) {
        if let Some(mut doc) = self.open.remove(uri) {
            doc.state.cancel_in_flight();
        }
        self.unopened_diagnostics.remove(uri);
    }

    /// Keeps the latest diagnostics set for a uri.
    pub fn set_diagnostics(&mut self, uri: &str, items: Vec<Diag>) {
        match self.open.get_mut(uri) {
            Some(doc) => doc.diagnostics = items,
            None => {
                self.unopened_diagnostics.insert(uri.to_string(), items);
            }
        }
    }

    pub fn get(&self, uri: &str) -> Option<&DocumentState> {
        self.open.get(uri).map(|d| &d.state)
    }

    pub fn get_mut(&mut self, uri: &str) -> Option<&mut DocumentState> {
        self.open.get_mut(uri).map(|d| &mut d.state)
    }

    /// The current text and language id of an open document (`plaintext`
    /// when the editor never said).
    pub fn text_and_language(&self, uri: &str) -> Option<(String, String)> {
        self.open.get(uri).map(|d| {
            (
                d.state.text.clone(),
                d.language_id
                    .clone()
                    .unwrap_or_else(|| "plaintext".to_string()),
            )
        })
    }

    /// The open documents keyed by absolute path (current editor text): the
    /// metacognition's `read` tool answers from these instead of disk.
    pub fn open_buffers(&self) -> BTreeMap<PathBuf, String> {
        self.open
            .iter()
            .filter_map(|(uri, doc)| {
                uri_to_path_str(uri).map(|p| (PathBuf::from(p), doc.state.text.clone()))
            })
            .collect()
    }

    /// Gathers everything one trigger needs about `uri` under one short
    /// lock. When the trigger sees an edit not recorded yet (a changed text
    /// whose version is not the last one noted), its summary joins the uri's
    /// ring before the live state is rendered. `None` when the uri is not open.
    pub fn snapshot(
        &mut self,
        uri: &str,
        event: TriggerKind,
        now: Instant,
    ) -> Option<DocumentSnapshot> {
        let (help_calls, help_in_flight) = match self.open.get_mut(uri) {
            Some(doc) if event != TriggerKind::Conversation => {
                let state = &doc.state;
                let calls = state
                    .help
                    .unanswered(added_help_comments(&state.baseline, &state.text));
                let in_flight = state.help.covers(&calls);
                (calls, in_flight)
            }
            _ => (Vec::new(), false),
        };
        let doc = self.open.get(uri)?;
        let state = &doc.state;
        let since_last_remark_ms = state
            .last_remark
            .map(|t| (now.duration_since(t)).as_millis() as i64)
            .unwrap_or(NO_REMARK_MS);
        let since_last_wake_ms = state
            .last_wake
            .map(|t| (now.duration_since(t)).as_millis() as i64)
            .unwrap_or(NO_REMARK_MS);
        let changed_lines = state.changed_lines_since_baseline() as i64;
        // Only a real edit joins the history: a conversation turn changed
        // nothing, an unchanged text has nothing to say, and a save followed
        // by an idle on the same version is one edit seen twice.
        let is_new_edit = event != TriggerKind::Conversation
            && changed_lines > 0
            && doc
                .edits
                .back()
                .is_none_or(|last| last.version != state.version);
        let summary = is_new_edit.then(|| {
            let (first_line, first_line_text) = first_changed_line(&state.baseline, &state.text);
            EditSummary {
                ts: now_rfc3339(),
                file: uri.rsplit('/').next().unwrap_or(uri).to_string(),
                version: state.version,
                changed_lines,
                first_line,
                excerpt: excerpt(&first_line_text, 120),
            }
        });
        let mut snapshot = DocumentSnapshot {
            version: state.version,
            changed_lines,
            since_last_remark_ms,
            since_last_wake_ms,
            text: state.text.clone(),
            baseline: state.baseline.clone(),
            last_edit_line: state.last_edit_line,
            language_id: doc
                .language_id
                .clone()
                .unwrap_or_else(|| "plaintext".to_string()),
            diagnostics: doc.diagnostics.clone(),
            live_state: String::new(),
            open_buffers: self.open_buffers(),
            help_calls,
            help_in_flight,
            remark_in_flight: event != TriggerKind::Conversation
                && state.remark_cancel.is_some()
                && state.remark_version == Some(state.version),
        };
        if let Some(summary) = summary {
            if let Some(doc) = self.open.get_mut(uri) {
                doc.edits.push_back(summary);
                while doc.edits.len() > LIVE_STATE_EDITS {
                    doc.edits.pop_front();
                }
            }
        }
        snapshot.live_state = self.render_live_state();
        Some(snapshot)
    }

    /// Renders the live state as plain text: the newest edits first, then
    /// the latest error/warning diagnostics, capped in lines and characters.
    fn render_live_state(&self) -> String {
        let mut edits: Vec<&EditSummary> = self
            .open
            .values()
            .flat_map(|doc| doc.edits.iter())
            .collect();
        edits.sort_by(|a, b| b.ts.cmp(&a.ts));
        let mut out = String::new();
        for e in edits.iter().take(LIVE_STATE_EDITS) {
            let _ = writeln!(
                out,
                "{} {} v{}: changed {} line(s) at {} | {}",
                e.ts, e.file, e.version, e.changed_lines, e.first_line, e.excerpt
            );
        }
        let open = self.open.iter().map(|(uri, d)| (uri, &d.diagnostics));
        let unopened = self.unopened_diagnostics.iter();
        let mut diags: Vec<String> = Vec::new();
        'collect: for (uri, items) in open.chain(unopened) {
            for d in items {
                if !matches!(d.severity.as_str(), "error" | "warning") {
                    continue;
                }
                diags.push(format!(
                    "{} line {}: [{}] {}",
                    uri.rsplit('/').next().unwrap_or(uri),
                    d.line + 1,
                    d.severity,
                    d.message
                ));
                if diags.len() >= LIVE_STATE_DIAGS {
                    break 'collect;
                }
            }
        }
        for d in diags {
            let _ = writeln!(out, "{d}");
        }
        excerpt(out.trim_end(), LIVE_STATE_MAX_CHARS)
    }
}

/// The first changed line (1-based) and its text in the new document:
/// where this edit starts and what it says. A pure deletion has no new line
/// to quote: it reports the line of the new document where the removed text
/// used to sit (clamped to the last line) with an empty text, so the edit
/// point still moves. `(0, "")` when nothing changed or the new text is empty.
pub fn first_changed_line(old: &str, new: &str) -> (usize, String) {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut new_lines_seen = 0usize;
    let mut first_delete = None;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => {
                if let Some(idx) = change.new_index() {
                    return (idx + 1, change.value().trim_end().to_string());
                }
            }
            similar::ChangeTag::Delete => {
                first_delete.get_or_insert(new_lines_seen + 1);
            }
            similar::ChangeTag::Equal => new_lines_seen += 1,
        }
    }
    match first_delete {
        Some(line) => (line.min(new.lines().count()), String::new()),
        None => (0, String::new()),
    }
}

/// Where the 0-based `line` of `old` sits in `new`: its new index when the
/// line survived the edit unchanged, `None` when it was changed or removed
/// (or `old` has no such line). Lets a remark computed on an older text land
/// on the right line of the current one.
pub fn reanchor_line(old: &str, new: &str, line: u32) -> Option<u32> {
    let diff = similar::TextDiff::from_lines(old, new);
    let anchored = diff
        .iter_all_changes()
        .find(|c| c.old_index() == Some(line as usize))
        .filter(|c| matches!(c.tag(), similar::ChangeTag::Equal))
        .and_then(|c| c.new_index())
        .map(|i| i as u32);
    anchored
}

/// Counts added + removed lines between two texts using a line diff.
pub fn count_changed_lines(old: &str, new: &str) -> usize {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .filter(|c| {
            matches!(
                c.tag(),
                similar::ChangeTag::Insert | similar::ChangeTag::Delete
            )
        })
        .count()
}

/// Renders a unified diff of the changed region (3 context lines).
pub fn unified_diff(old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.unified_diff().context_radius(3).to_string()
}

/// The text around `line` (1-based; 0 means the top of the file): a window
/// of `context` lines on each side, every line numbered like
/// [`text_around_changes`]. This is what the conversation trigger shows,
/// as it has no diff to center on.
pub fn text_around_line(text: &str, line: usize, context: usize) -> String {
    let total = text.lines().count();
    let (start, end) = if line == 0 {
        (0, (2 * context + 1).min(total))
    } else {
        let center = (line - 1).min(total.saturating_sub(1));
        (
            center.saturating_sub(context),
            (center + context + 1).min(total),
        )
    };
    text.lines()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|(i, l)| format!("{:>4}| {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extracts the current text truncated around the changed region: a window
/// of `context` lines around every inserted/replaced new line.
pub fn text_around_changes(old: &str, new: &str, context: usize) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let new_line_count = new.lines().count();
    // Mark which new lines are touched (inserts or replacements).
    let mut touched = vec![false; new_line_count];
    for change in diff.iter_all_changes() {
        if matches!(change.tag(), similar::ChangeTag::Insert) {
            if let Some(idx) = change.new_index() {
                touched[idx] = true;
            }
        }
    }
    // Expand: any touched line taints its neighbors within `context`.
    let mut keep = vec![false; new_line_count];
    for (i, t) in touched.iter().enumerate() {
        if *t {
            for k in
                keep[i.saturating_sub(context)..(i + context + 1).min(new_line_count)].iter_mut()
            {
                *k = true;
            }
        }
    }
    // Each kept line carries its 1-based number: the model must answer with a
    // line in the current file, and without numbers it has to guess (observed
    // 2026-09-29: a remark about a `parse().unwrap()` landed one line below).
    let numbered = |(i, l): (usize, &str)| format!("{:>4}| {l}", i + 1);
    let selected: Vec<String> = new
        .lines()
        .enumerate()
        .zip(keep.iter())
        .filter(|(_, k)| **k)
        .map(|(il, _)| numbered(il))
        .collect();
    if selected.is_empty() {
        new.lines()
            .enumerate()
            .map(numbered)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        selected.join("\n")
    }
}

#[cfg(test)]
mod tests;
