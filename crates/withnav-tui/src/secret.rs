//! The secret prompt: the Captain types a value an agent asked for with `request_secret`.
//!
//! The value lives in [`SecretEntry`] only while it is typed, is moved into one
//! [`Effect::SubmitSecret`](crate::protocol::Effect) when it is sent, and is cleared from the
//! prompt at once. It is never part of the conversation state, the draft, a notice or a log line:
//! `SecretEntry` and `SecretSubmission` print no value in `Debug`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use withnav_client_api::secrets::{SecretScope, SecretTtl};

use crate::protocol::Action;
use crate::state::{BlockKind, SecretPrompt, State, UiState};

/// The largest value the server stores, in bytes. The prompt refuses a paste or a keystroke that
/// would grow the value past this, so an oversized value becomes a local error here instead of a
/// rejected submission. Mirrors `withnav::secrets::MAX_VALUE_BYTES`; the terminal client cannot
/// depend on the server crate, so the number is duplicated on purpose.
pub const MAX_VALUE_BYTES: usize = 16 * 1024;

/// The prompt open on a secret request (or on a settled one, to give a value anyway).
#[derive(Clone, PartialEq, Default)]
pub struct SecretEntry {
    /// The request's `question_call_id`.
    pub call_id: String,
    pub name: String,
    pub value: String,
    /// The value is shown in clear on screen (it never leaves the prompt either way).
    pub shown: bool,
    pub ttl: SecretTtl,
    pub scope: SecretScope,
    /// Opened on a settled request: Esc closes it instead of declining.
    pub later: bool,
    /// Refused a paste or a keystroke that would pass [`MAX_VALUE_BYTES`]; cleared by the next
    /// accepted edit, by [`SecretEntry::clear`] and by a submission. Never the value itself.
    pub error: Option<String>,
}

impl std::fmt::Debug for SecretEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretEntry")
            .field("call_id", &self.call_id)
            .field("name", &self.name)
            .field(
                "value",
                &format_args!("<{} chars>", self.value.chars().count()),
            )
            .field("shown", &self.shown)
            .field("ttl", &self.ttl)
            .field("scope", &self.scope)
            .field("later", &self.later)
            .field("error", &self.error)
            .finish()
    }
}

impl SecretEntry {
    pub fn for_prompt(prompt: &SecretPrompt, later: bool) -> Self {
        Self {
            call_id: prompt.call_id.clone(),
            name: prompt.request.name.clone(),
            value: String::new(),
            shown: false,
            ttl: prompt.request.default_ttl,
            scope: prompt.request.default_scope,
            later,
            error: None,
        }
    }

    /// What the value field shows: the value, or one bullet per character.
    pub fn display(&self) -> String {
        if self.shown {
            self.value.clone()
        } else {
            "•".repeat(self.value.chars().count())
        }
    }

    /// Empties the value; the prompt keeps its other choices.
    pub fn clear(&mut self) {
        self.value.clear();
        self.shown = false;
        self.error = None;
    }

    /// Adds typed text to the value. A value that would pass [`MAX_VALUE_BYTES`] is refused whole,
    /// so the prompt never holds bytes the server would reject; the value is left unchanged and
    /// `error` says why. An accepted edit forgets an earlier refusal.
    pub fn push(&mut self, text: &str) {
        if self.value.len() + text.len() > MAX_VALUE_BYTES {
            self.error = Some(limit_error());
            return;
        }
        self.value.push_str(text);
        self.error = None;
    }

    /// Adds one typed character; same bound and refusal as [`SecretEntry::push`], counted in the
    /// bytes the character takes.
    pub fn push_char(&mut self, c: char) {
        if self.value.len() + c.len_utf8() > MAX_VALUE_BYTES {
            self.error = Some(limit_error());
            return;
        }
        self.value.push(c);
        self.error = None;
    }

    /// Removes a character from the end; forgetting the refusal, which no longer applies.
    pub fn pop(&mut self) {
        self.value.pop();
        self.error = None;
    }

    /// Removes at most one trailing line ending — the one a terminal adds to a paste — and no
    /// leading bytes: a secret may intentionally start or end with a newline.
    fn without_paste_line_ending(text: &str) -> &str {
        text.strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .or_else(|| text.strip_suffix('\r'))
            .unwrap_or(text)
    }
}

/// One message for every refusal; it names the limit and never the rejected bytes.
fn limit_error() -> String {
    format!("The value is limited to {MAX_VALUE_BYTES} bytes; nothing was added.")
}

/// The newest secret request of the conversation, open or not.
pub fn latest(state: &State) -> Option<&SecretPrompt> {
    state
        .conversation
        .blocks
        .iter()
        .rev()
        .find_map(|b| match &b.kind {
            BlockKind::Secret(p) => Some(p),
            _ => None,
        })
}

/// The secret request waiting for the Captain, if any.
pub fn waiting(state: &State) -> Option<&SecretPrompt> {
    latest(state).filter(|p| p.is_open())
}

fn prompt<'s>(state: &'s State, call_id: &str) -> Option<&'s SecretPrompt> {
    state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::Secret(p) if p.call_id == call_id => Some(p),
            _ => None,
        })
}

/// Before each frame: opens the prompt on a request that waits (unless the Captain put it aside
/// to write instead), and closes a prompt whose request is gone or was settled elsewhere.
pub fn sync(state: &State, ui: &mut UiState) {
    if state.read_only() {
        ui.secret_entry = None;
        return;
    }
    if let Some(entry) = &ui.secret_entry {
        let keep = match prompt(state, &entry.call_id) {
            Some(p) => entry.later || (p.is_open() && !p.sending) || p.sending,
            None => false,
        };
        if !keep {
            ui.secret_entry = None;
        }
    }
    if ui.secret_entry.is_none()
        && let Some(p) = waiting(state)
        && !p.sending
        && ui.secret_put_aside.as_deref() != Some(p.call_id.as_str())
    {
        ui.secret_entry = Some(SecretEntry::for_prompt(p, false));
    }
}

fn next_ttl(ttl: SecretTtl) -> SecretTtl {
    match ttl {
        SecretTtl::Hour => SecretTtl::Day,
        SecretTtl::Day => SecretTtl::Session,
        SecretTtl::Session => SecretTtl::Hour,
    }
}

fn next_scope(scope: SecretScope) -> SecretScope {
    match scope {
        SecretScope::Session => SecretScope::Workspace,
        SecretScope::Workspace => SecretScope::Session,
    }
}

/// Keys while the prompt is open: it takes every key. `None` when the key only edited it.
pub fn key(key: KeyEvent, state: &State, ui: &mut UiState) -> Option<Action> {
    let entry = ui.secret_entry.as_mut()?;
    let sending = prompt(state, &entry.call_id).is_some_and(|p| p.sending);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Esc if entry.later => {
            entry.clear();
            ui.secret_entry = None;
            None
        }
        KeyCode::Esc if !sending => Some(Action::DeclineSecret),
        KeyCode::Enter if !sending && !entry.value.is_empty() => Some(Action::SubmitSecret),
        KeyCode::Tab => {
            entry.ttl = next_ttl(entry.ttl);
            None
        }
        KeyCode::BackTab => {
            entry.scope = next_scope(entry.scope);
            None
        }
        KeyCode::Char('r') if ctrl => {
            entry.shown = !entry.shown;
            None
        }
        // Put the prompt aside to answer in the chat instead: what is sent then answers the request.
        KeyCode::Char('e') if ctrl => {
            let call_id = entry.call_id.clone();
            entry.clear();
            ui.secret_entry = None;
            ui.secret_put_aside = Some(call_id);
            None
        }
        KeyCode::Char('u') if ctrl => {
            entry.value.clear();
            entry.error = None;
            None
        }
        KeyCode::Backspace => {
            entry.pop();
            None
        }
        KeyCode::Char(c) if !ctrl && !alt && !sending => {
            entry.push_char(c);
            None
        }
        _ => None,
    }
}

/// A paste while the prompt is open goes into the value, without the one line break a terminal
/// appends to a pasted line. A paste that would pass [`MAX_VALUE_BYTES`] is refused whole and the
/// refusal is shown; the value never changes silently.
pub fn paste(text: &str, ui: &mut UiState) -> bool {
    let Some(entry) = ui.secret_entry.as_mut() else {
        return false;
    };
    entry.push(SecretEntry::without_paste_line_ending(text));
    true
}

/// "in 59 min", "in 23 h", "when this conversation stops", "at an unknown time" for a timestamp
/// that does not parse, or `None` once past. An unreadable timestamp is not shown as expired: the
/// secret may well still be in use.
pub fn expires_in(expires_at: Option<&str>, now: time::OffsetDateTime) -> Option<String> {
    let Some(at) = expires_at else {
        return Some("when this conversation stops".into());
    };
    let Ok(at) = time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
    else {
        return Some("at an unknown time".into());
    };
    let left = (at - now).whole_seconds();
    if left <= 0 {
        return None;
    }
    let minutes = (left + 59) / 60;
    Some(if minutes < 60 {
        format!("in {minutes} min")
    } else {
        format!("in {} h", (minutes + 30) / 60)
    })
}

pub fn scope_text(scope: SecretScope) -> &'static str {
    match scope {
        SecretScope::Session => "this session",
        SecretScope::Workspace => "this workspace",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_value() {
        let entry = SecretEntry {
            value: "sk-very-secret".into(),
            ..SecretEntry::default()
        };
        let printed = format!("{entry:?}");
        assert!(!printed.contains("very-secret"));
        assert!(printed.contains("<14 chars>"));
    }

    #[test]
    fn masked_display_and_expiry_words() {
        let mut entry = SecretEntry {
            value: "abcd".into(),
            ..SecretEntry::default()
        };
        assert_eq!(entry.display(), "••••");
        entry.shown = true;
        assert_eq!(entry.display(), "abcd");
        let now = time::OffsetDateTime::from_unix_timestamp(0).unwrap();
        assert_eq!(
            expires_in(Some("1970-01-01T00:59:00Z"), now).as_deref(),
            Some("in 59 min")
        );
        assert_eq!(
            expires_in(Some("1970-01-02T00:00:00Z"), now).as_deref(),
            Some("in 24 h")
        );
        assert_eq!(expires_in(Some("1969-12-31T00:00:00Z"), now), None);
        assert_eq!(
            expires_in(Some("not a date"), now).as_deref(),
            Some("at an unknown time")
        );
    }

    /// A paste keeps every byte but the one terminal line ending: an intentional leading or
    /// trailing newline inside a secret survives, a value ending in two newlines loses one.
    #[test]
    fn paste_removes_at_most_one_trailing_line_ending() {
        let strip = SecretEntry::without_paste_line_ending;
        assert_eq!(strip("sk-abc\n"), "sk-abc");
        assert_eq!(strip("sk-abc\r\n"), "sk-abc");
        assert_eq!(strip("sk-abc\r"), "sk-abc");
        assert_eq!(strip("sk-abc"), "sk-abc");
        assert_eq!(strip("sk-abc\n\n"), "sk-abc\n");
        assert_eq!(strip("sk-abc\r\n\n"), "sk-abc\r\n");
        assert_eq!(strip("\nsk-abc\n"), "\nsk-abc");
        assert_eq!(strip("\n"), "");
        assert_eq!(strip(""), "");
    }

    /// The bound counts bytes, so a character that needs more room than is left is refused whole
    /// and the value is never truncated to fit.
    #[test]
    fn a_value_never_passes_the_server_byte_limit() {
        let mut entry = SecretEntry::default();
        entry.push(&"a".repeat(MAX_VALUE_BYTES - 2));
        entry.push("é"); // brings the value to exactly MAX_VALUE_BYTES
        assert_eq!(entry.value.len(), MAX_VALUE_BYTES);
        assert!(entry.error.is_none());
        entry.push("é"); // two more bytes would pass the limit
        assert_eq!(entry.value.len(), MAX_VALUE_BYTES);
        assert!(entry.error.is_some(), "the refusal is reported");

        // One byte of room left, a two-byte character offered: nothing is added.
        let held = "a".repeat(MAX_VALUE_BYTES - 1);
        let mut entry = SecretEntry {
            value: held.clone(),
            ..SecretEntry::default()
        };
        entry.push("é");
        assert_eq!(entry.value, held, "the held bytes are untouched");
        assert!(entry.error.is_some());
        entry.push_char('é');
        assert_eq!(entry.value, held);
        assert!(entry.error.is_some());

        // An accepted edit forgets the refusal and the value grows again.
        entry.pop();
        assert!(entry.error.is_none());
        entry.push_char('é');
        assert_eq!(entry.value.len(), MAX_VALUE_BYTES);
    }

    #[test]
    fn clear_forgets_the_refusal() {
        let mut entry = SecretEntry {
            value: "x".repeat(MAX_VALUE_BYTES),
            error: Some(limit_error()),
            ..SecretEntry::default()
        };
        entry.clear();
        assert!(entry.value.is_empty());
        assert!(entry.error.is_none());
    }
}
