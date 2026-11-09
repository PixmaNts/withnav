//! The secret prompt: the Captain types a value an agent asked for with `request_secret`.
//!
//! The value lives in [`SecretEntry`] only while it is typed, is moved into one
//! [`Effect::SubmitSecret`](crate::protocol::Effect) when it is sent, and is cleared from the
//! prompt at once. It is never part of the conversation state, the draft, a notice or a log line:
//! `SecretEntry` and `SecretSubmission` print no value in `Debug`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use withnav_client_api::secrets::{SecretScope, SecretTtl};

use crate::protocol::Action;
use crate::state::{BlockKind, Focus, SecretPrompt, State, UiState};

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

    /// Removes the last user-perceived character: a base character with its combining marks, or
    /// an emoji sequence, goes in one Backspace.
    pub fn pop_grapheme(&mut self) {
        if let Some((at, _)) = self.value.grapheme_indices(true).next_back() {
            self.value.truncate(at);
        }
    }

    /// Empties the value; the prompt keeps its other choices.
    pub fn clear(&mut self) {
        self.value.clear();
        self.shown = false;
    }
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
            // The answer is typed in the editor: with the transcript focused a punctuation-first
            // answer would be taken as a transcript shortcut.
            ui.focus = Focus::Editor;
            None
        }
        KeyCode::Char('u') if ctrl => {
            // `clear` also hides the value, so the next one is typed masked.
            entry.clear();
            None
        }
        KeyCode::Backspace => {
            entry.pop_grapheme();
            None
        }
        KeyCode::Char(c) if !ctrl && !alt && !sending => {
            entry.value.push(c);
            None
        }
        _ => None,
    }
}

/// The prompt is open on a request whose answer is being sent: edits are refused until it
/// comes back.
pub fn sending(state: &State, ui: &UiState) -> bool {
    ui.secret_entry
        .as_ref()
        .and_then(|e| prompt(state, &e.call_id))
        .is_some_and(|p| p.sending)
}

/// A paste while the prompt is open goes into the value, without its trailing line break.
pub fn paste(text: &str, ui: &mut UiState) -> bool {
    let Some(entry) = ui.secret_entry.as_mut() else {
        return false;
    };
    entry.value.push_str(
        text.trim_end_matches(['\r', '\n'])
            .trim_start_matches(['\r', '\n']),
    );
    true
}

/// "in 59 min", "in 23 h", "when this conversation stops", or `None` once past.
pub fn expires_in(expires_at: Option<&str>, now: time::OffsetDateTime) -> Option<String> {
    let Some(at) = expires_at else {
        return Some("when this conversation stops".into());
    };
    let at =
        time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()?;
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

pub fn ttl_text(ttl: SecretTtl) -> &'static str {
    match ttl {
        SecretTtl::Hour => "1 h",
        SecretTtl::Day => "24 h",
        SecretTtl::Session => "Session",
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
    }
}
