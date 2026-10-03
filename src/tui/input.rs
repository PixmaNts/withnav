//! The message editor and key routing: turns terminal keys and pastes into
//! editor changes or `Action`s. No networking, and no change to `State`.
use super::{
    commands,
    protocol::{Action, Overlay, PermissionAnswer},
    reducer,
    state::{Activity, DenyDraft, Focus, State, UiState, VariantPick},
    theme::ThemeId,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::TextArea;

/// Multiline message editor. `revision` grows on every edit, so the reducer
/// can clear only the text it actually sent, never what was typed after it.
#[derive(Debug, Default)]
pub struct Editor {
    area: TextArea<'static>,
    revision: u64,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The whole text, lines joined with `\n`.
    pub fn text(&self) -> String {
        self.area.lines().join("\n")
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replaces the text and counts as an edit.
    pub fn set_text(&mut self, text: &str) {
        let lines = text.split('\n').map(str::to_string).collect();
        self.area = TextArea::new(lines);
        self.revision += 1;
    }

    /// Puts back a kept draft with the revision it was kept at.
    pub fn restore(&mut self, text: &str, revision: u64) {
        let lines = text.split('\n').map(str::to_string).collect();
        self.area = TextArea::new(lines);
        self.revision = revision;
    }

    /// Clears the editor only if nothing was edited since `revision`; returns
    /// whether it cleared.
    pub fn clear_if_revision(&mut self, revision: u64) -> bool {
        if self.revision != revision {
            return false;
        }
        self.area = TextArea::default();
        self.revision += 1;
        true
    }

    /// The underlying widget, for drawing and for key handling in this module.
    pub fn area(&self) -> &TextArea<'static> {
        &self.area
    }

    pub fn area_mut(&mut self) -> &mut TextArea<'static> {
        &mut self.area
    }

    /// Records an edit made through `area_mut`.
    pub fn bump(&mut self) {
        self.revision += 1;
    }
}

/// Routes one key: overlays first, then global keys, then the editor.
/// Enter on a non-empty editor gives `Action::SubmitAt` and leaves the text in
/// place; the reducer clears it once the server accepted it.
pub fn route_key(
    key: KeyEvent,
    state: &State,
    ui: &mut UiState,
    editor: &mut Editor,
) -> Option<Action> {
    if matches!(key.kind, KeyEventKind::Release) {
        return None;
    }
    if ui.overlay == Some(Overlay::Commands) && ui.palette_typed {
        return typed_palette_key(key, state, ui, editor);
    }
    if let Some(overlay) = ui.overlay {
        let names = overlay_names(overlay, state, ui);
        let selected = ui.selection.entry(format!("{overlay:?}")).or_default();
        match key.code {
            KeyCode::Esc => {
                ui.filter.clear();
                // esc in the second step of the model picker goes back to the models.
                if let Some(VariantPick::NewConversation(alias) | VariantPick::Switch(alias)) =
                    ui.variant_pick.take()
                {
                    ui.overlay = Some(Overlay::Models);
                    let back = overlay_names(Overlay::Models, state, ui)
                        .iter()
                        .position(|a| *a == alias)
                        .unwrap_or(0);
                    ui.selection.insert(format!("{:?}", Overlay::Models), back);
                    return None;
                }
                ui.overlay = None;
                return Some(Action::CloseOverlay);
            }
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(names.len().saturating_sub(1)),
            KeyCode::PageUp => *selected = selected.saturating_sub(8),
            KeyCode::PageDown => *selected = (*selected + 8).min(names.len().saturating_sub(1)),
            KeyCode::Enter if overlay == Overlay::Variants && names.is_empty() => {
                // No variant to choose: a new conversation starts as it is; `/variant` closes.
                if !ui.filter.is_empty() {
                    return None;
                }
                let pick = ui.variant_pick.take();
                ui.overlay = None;
                return Some(match pick {
                    Some(VariantPick::NewConversation(alias)) => Action::PickModel(alias),
                    Some(VariantPick::Switch(alias)) => Action::SwitchModel(alias),
                    _ => Action::CloseOverlay,
                });
            }
            // ctrl+n in the model list: that model in a new tab, whatever this tab holds.
            KeyCode::Char('n')
                if overlay == Overlay::Models && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let name = names.get(*selected)?.clone();
                variant_step(state, ui, VariantPick::NewConversation(name));
                return None;
            }
            KeyCode::Enter => {
                let name = names.get(*selected)?;
                let action = match overlay {
                    Overlay::Models => {
                        // A live conversation switches to it; an empty tab starts on it. The
                        // switch is refused, with the reason, while the agent waits for the driver.
                        let pick = if reducer::occupied(state) {
                            if let Some(why) = reducer::switch_blocked(state) {
                                return Some(Action::ShowNotice(why.into()));
                            }
                            VariantPick::Switch(name.clone())
                        } else {
                            VariantPick::NewConversation(name.clone())
                        };
                        variant_step(state, ui, pick);
                        return None;
                    }
                    Overlay::Variants => Action::PickVariant(name.clone()),
                    Overlay::Sessions => Action::ResumeSession(name.clone()),
                    Overlay::Commands => {
                        let row = commands::palette_rows(state, &ui.filter, false)
                            .into_iter()
                            .find(|r| &r.name == name)?;
                        if !row.runs_bare() {
                            // It needs an argument: the name goes to the editor to finish.
                            editor.set_text(&format!("{} ", row.name));
                            editor
                                .area_mut()
                                .move_cursor(ratatui_textarea::CursorMove::End);
                            ui.overlay = None;
                            ui.filter.clear();
                            return None;
                        }
                        commands::palette_action(&row, ui.mode, None)?
                    }
                    Overlay::Themes => Action::PickTheme(ThemeId::from_id(name)?),
                    Overlay::Help => return None,
                };
                ui.overlay = None;
                ui.filter.clear();
                return Some(action);
            }
            KeyCode::Backspace if overlay != Overlay::Help => {
                ui.filter.pop();
                *selected = 0;
            }
            KeyCode::Char(c)
                if overlay != Overlay::Help && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                ui.filter.push(c);
                *selected = 0;
            }
            _ => {}
        }
        if overlay == Overlay::Themes {
            // The highlighted theme is drawn at once; enter keeps it, esc puts the old one back.
            // The rows are listed again because the key may have changed the filter; with no
            // row left, the theme on screen stays.
            let selected = *selected;
            let previewed = overlay_names(overlay, state, ui)
                .get(selected)
                .and_then(|id| ThemeId::from_id(id));
            if let Some(theme) = previewed.filter(|theme| *theme != ui.theme) {
                return Some(Action::PreviewTheme(theme));
            }
        }
        return None;
    }
    if let Some(action) = permission_key(key, state, ui, editor) {
        return action;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // tab on an empty message switches build/plan; with text in it, or as shift+tab, it moves the
    // focus between the editor and the transcript.
    if key.code == KeyCode::Tab
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && ui.focus == Focus::Editor
        && editor.text().is_empty()
    {
        return Some(Action::ToggleAgent);
    }
    if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
        ui.focus = if ui.focus == Focus::Editor {
            Focus::Transcript
        } else {
            Focus::Editor
        };
        return None;
    }
    if key.code == KeyCode::Esc && ui.quit_confirm {
        ui.quit_confirm = false;
        return None;
    }
    if key.code == KeyCode::End && ctrl {
        ui.follow_bottom = true;
        ui.scroll = 0;
        return None;
    }
    if key.code == KeyCode::Char('c') && ctrl && state.activity != Activity::Running {
        if ui.quit_confirm {
            return Some(Action::Quit);
        }
        ui.quit_confirm = true;
        return None;
    }
    if key.code == KeyCode::Char('q')
        && ctrl
        && (!editor.text().trim().is_empty() || state.activity == Activity::Running)
        && !ui.quit_confirm
    {
        ui.quit_confirm = true;
        return None;
    }
    if let Some(action) = commands::key_to_action(key) {
        if let Action::OpenOverlay(overlay) = action {
            ui.overlay = Some(overlay);
            ui.filter.clear();
            ui.selection.insert(format!("{overlay:?}"), 0);
        }
        if matches!(action, Action::Scroll(_)) {
            ui.follow_bottom = false;
        }
        return Some(action);
    }
    if ui.focus == Focus::Transcript {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let blocks = &state.conversation.blocks;
                if blocks.is_empty() {
                    return None;
                }
                let idx = ui
                    .selected_block
                    .as_ref()
                    .and_then(|id| blocks.iter().position(|b| &b.id == id))
                    .unwrap_or(blocks.len() - 1);
                let next = if key.code == KeyCode::Up {
                    idx.saturating_sub(1)
                } else {
                    (idx + 1).min(blocks.len() - 1)
                };
                ui.selected_block = Some(blocks[next].id.clone());
                ui.follow_bottom = false;
                return None;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                return ui.selected_block.clone().map(Action::ToggleBlock)
            }
            _ => return None,
        }
    }
    if key.code == KeyCode::Char('/') && editor.text().is_empty() {
        // The slash is typed into the editor; the palette follows what comes after it.
        editor.set_text("/");
        editor
            .area_mut()
            .move_cursor(ratatui_textarea::CursorMove::End);
        ui.overlay = Some(Overlay::Commands);
        ui.palette_typed = true;
        ui.filter.clear();
        ui.selection.insert("Commands".into(), 0);
        return Some(Action::OpenOverlay(Overlay::Commands));
    }
    if key.code == KeyCode::Enter && !alt && !key.modifiers.contains(KeyModifiers::SHIFT) {
        let text = editor.text();
        if text.trim().is_empty() {
            return None;
        }
        return Some(match commands::route(&text, ui.mode) {
            commands::Routed::Local(action) => {
                // The client runs it now: the line has done its job.
                editor.clear_if_revision(editor.revision());
                if let Action::OpenOverlay(overlay) = action {
                    ui.overlay = Some(overlay);
                    ui.filter.clear();
                }
                action
            }
            commands::Routed::Server { name, arguments } => Action::RunCommand {
                name,
                arguments,
                revision: Some(editor.revision()),
            },
            commands::Routed::Message(text) => {
                // Text sent while a permission request waits cancels it: the first enter warns,
                // a second one on the same text sends.
                if let Some(open) = state.open_permission() {
                    let pending = (open.request_id.clone(), editor.revision());
                    if ui.send_over_permission.as_ref() != Some(&pending) {
                        ui.send_over_permission = Some(pending);
                        return Some(Action::ShowNotice(
                            "sending text cancels the pending permission; press enter again to send"
                                .into(),
                        ));
                    }
                }
                ui.send_over_permission = None;
                Action::SubmitAt {
                    text,
                    revision: editor.revision(),
                }
            }
            commands::Routed::Refused(why) => Action::ShowNotice(why),
        });
    }
    let changed = if (key.code == KeyCode::Enter
        && (alt || key.modifiers.contains(KeyModifiers::SHIFT)))
        || (key.code == KeyCode::Char('j') && ctrl)
    {
        editor.area_mut().insert_newline();
        true
    } else {
        editor.area_mut().input_without_shortcuts(key)
    };
    if changed {
        editor.bump();
    }
    None
}

/// A key while the palette follows the editor. Every printable key and backspace edit the line;
/// the palette then closes when the line is no longer a command name being typed (a space or an
/// argument after it, a name no row starts with, `//`, an empty line), leaving the line for enter.
fn typed_palette_key(
    key: KeyEvent,
    state: &State,
    ui: &mut UiState,
    editor: &mut Editor,
) -> Option<Action> {
    let rows = commands::palette_rows(state, &ui.filter, true);
    let selected = ui.selection.entry("Commands".into()).or_default();
    match key.code {
        KeyCode::Esc => {
            // The line stays; esc only puts the list away.
            close_palette(ui);
            return Some(Action::CloseOverlay);
        }
        KeyCode::Up => {
            *selected = selected.saturating_sub(1);
            return None;
        }
        KeyCode::Down => {
            *selected = (*selected + 1).min(rows.len().saturating_sub(1));
            return None;
        }
        KeyCode::Tab | KeyCode::Enter => {
            let row = rows.get(*selected)?.clone();
            let complete = key.code == KeyCode::Tab || !row.runs_bare();
            close_palette(ui);
            if complete {
                // Fills in the name; the driver types the argument and sends it with enter.
                editor.set_text(&format!("{} ", row.name));
                editor
                    .area_mut()
                    .move_cursor(ratatui_textarea::CursorMove::End);
                return Some(Action::CloseOverlay);
            }
            let revision = editor.revision();
            let action = commands::palette_action(&row, ui.mode, Some(revision));
            if !row.server {
                // A client command runs now; nothing waits for a server to take the line.
                editor.clear_if_revision(revision);
            }
            return action;
        }
        _ => {}
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    if editor.area_mut().input_without_shortcuts(key) {
        editor.bump();
    }
    follow_typed_line(state, ui, editor)
}

/// The model picker's second step: which variant to start on or switch to (a note when the model
/// has none).
fn variant_step(state: &State, ui: &mut UiState, pick: VariantPick) {
    let (_, start) = commands::variant_choices(state, &pick);
    ui.overlay = Some(Overlay::Variants);
    ui.variant_pick = Some(pick);
    ui.filter.clear();
    ui.selection
        .insert(format!("{:?}", Overlay::Variants), start);
}

/// Keys for the open permission request of the tab on screen. `y`, `a` and `n` answer it while the
/// message is empty (with text in it they type); after `n` every key types the deny message until
/// enter sends it or esc denies without one. `Some(action)` when the key was the prompt's.
fn permission_key(
    key: KeyEvent,
    state: &State,
    ui: &mut UiState,
    editor: &Editor,
) -> Option<Option<Action>> {
    let Some(open) = state.open_permission() else {
        ui.deny_message = None;
        return None;
    };
    let request_id = open.request_id.clone();
    let answer = |decision: &str, message: Option<String>| Action::AnswerPermission {
        request_id: request_id.clone(),
        answer: PermissionAnswer {
            decision: decision.into(),
            message,
        },
    };
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    // A message started on a request that ended (or was replaced) does not carry over.
    if ui.deny_draft(&request_id).is_none() {
        ui.deny_message = None;
    }
    if let Some(DenyDraft { text: message, .. }) = ui.deny_message.as_mut() {
        return Some(match key.code {
            KeyCode::Enter => {
                let message = std::mem::take(message);
                ui.deny_message = None;
                let message = Some(message.trim().to_string()).filter(|m| !m.is_empty());
                Some(answer("deny", message))
            }
            KeyCode::Esc => {
                ui.deny_message = None;
                Some(answer("deny", None))
            }
            KeyCode::Backspace => {
                message.pop();
                None
            }
            KeyCode::Char(c) if plain || key.modifiers == KeyModifiers::SHIFT => {
                message.push(c);
                None
            }
            // Other keys (tab switches, scrolling…) keep working; the message waits.
            _ => return None,
        });
    }
    if !editor.text().is_empty() || !plain {
        return None;
    }
    // Answered already: the prompt's keys wait for the server to end it.
    if state.answered_permission.as_deref() == Some(request_id.as_str()) {
        return matches!(key.code, KeyCode::Char('y' | 'a' | 'n')).then_some(None);
    }
    // Not on screen at the last frame: a key only scrolls to the prompt. Only a
    // frame that actually draws the prompt's keys marks it shown (the render
    // sets `prompt_shown`), so keys queued before that frame, or a terminal too
    // small to show the prompt, never answer an unseen request.
    if matches!(key.code, KeyCode::Char('y' | 'a' | 'n'))
        && ui.prompt_shown.as_deref() != Some(request_id.as_str())
    {
        ui.selected_block = Some(format!("permission:{request_id}"));
        ui.follow_bottom = false;
        return Some(None);
    }
    match key.code {
        KeyCode::Char('y') => Some(Some(answer("allow_once", None))),
        // The plan agent's exceptions are allowed once or denied (the prompt shows no `a`).
        KeyCode::Char('a') if open.agent == "plan" => Some(Some(Action::ShowNotice(
            "this request can be allowed once or denied, not always".into(),
        ))),
        KeyCode::Char('a') => Some(Some(answer("allow_folder", None))),
        KeyCode::Char('n') => {
            ui.deny_message = Some(DenyDraft {
                request_id: request_id.clone(),
                text: String::new(),
            });
            Some(None)
        }
        _ => None,
    }
}

/// After an edit of the line the palette follows: narrows the palette to the name typed so far,
/// or puts it away when the line is no longer a command name being typed (a space or an argument
/// after it, a name no row starts with, `//`, an empty line), leaving the line for enter.
fn follow_typed_line(state: &State, ui: &mut UiState, editor: &Editor) -> Option<Action> {
    let text = editor.text();
    let name = text.strip_prefix('/').unwrap_or_default();
    let still_a_name = text.starts_with('/')
        && !name.starts_with('/')
        && !name.contains(char::is_whitespace)
        && !commands::palette_rows(state, name, true).is_empty();
    if !still_a_name {
        close_palette(ui);
        return Some(Action::CloseOverlay);
    }
    ui.filter = name.to_string();
    ui.selection.insert("Commands".into(), 0);
    None
}

fn close_palette(ui: &mut UiState) {
    ui.overlay = None;
    ui.palette_typed = false;
    ui.filter.clear();
}

/// The keys of the rows an overlay lists under the current filter, in display order.
fn overlay_names(overlay: Overlay, state: &State, ui: &UiState) -> Vec<String> {
    match overlay {
        Overlay::Models => state
            .catalog
            .iter()
            .filter(|x| {
                x.alias.to_lowercase().contains(&ui.filter.to_lowercase())
                    || x.model.to_lowercase().contains(&ui.filter.to_lowercase())
            })
            .map(|x| x.alias.clone())
            .collect(),
        Overlay::Sessions => state
            .sessions
            .iter()
            .filter(|x| {
                x.id.to_lowercase().contains(&ui.filter.to_lowercase())
                    || x.title
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&ui.filter.to_lowercase())
            })
            .map(|x| x.id.clone())
            .collect(),
        Overlay::Commands => commands::palette_rows(state, &ui.filter, ui.palette_typed)
            .into_iter()
            .map(|r| r.name)
            .collect(),
        Overlay::Themes => ThemeId::ALL
            .iter()
            .filter(|x| {
                x.id().contains(&ui.filter.to_lowercase())
                    || x.name().to_lowercase().contains(&ui.filter.to_lowercase())
            })
            .map(|x| x.id().to_string())
            .collect(),
        Overlay::Help => Vec::new(),
        Overlay::Variants => ui
            .variant_pick
            .as_ref()
            .map(|pick| commands::variant_choices(state, pick).0)
            .unwrap_or_default()
            .into_iter()
            .filter(|v| v.to_lowercase().contains(&ui.filter.to_lowercase()))
            .collect(),
    }
}

/// Inserts pasted text into the editor (newlines kept), unless an overlay other than the typed
/// slash palette has the focus.
pub fn route_paste(
    text: String,
    state: &State,
    ui: &mut UiState,
    editor: &mut Editor,
) -> Option<Action> {
    // While a deny message is typed, a paste goes into it (one line); a message left from a
    // request that is no longer the open one is dropped.
    let open = state.open_permission().map(|p| p.request_id.as_str());
    if ui.deny_message.is_some() && open.is_none_or(|id| ui.deny_draft(id).is_none()) {
        ui.deny_message = None;
    }
    if let Some(DenyDraft { text: message, .. }) = ui.deny_message.as_mut() {
        if ui.overlay.is_none() {
            message.push_str(&text.replace(['\n', '\r'], " "));
            return None;
        }
    }
    // The palette opened by typing `/` follows the line: a paste edits it like typed keys.
    let typed_palette = ui.overlay == Some(Overlay::Commands) && ui.palette_typed;
    if (ui.overlay.is_some() && !typed_palette) || ui.focus != Focus::Editor {
        return None;
    }
    if editor.area_mut().insert_str(text) {
        editor.bump();
    }
    if typed_palette {
        return follow_typed_line(state, ui, editor);
    }
    None
}
