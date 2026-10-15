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
    if ui.screen == super::config::Screen::Config && ui.overlay.is_none() {
        return config_key(key, state);
    }
    if ui.overlay == Some(Overlay::Sessions) && ui.delete_pending.is_some() {
        return None;
    }
    if ui.overlay == Some(Overlay::Sessions) && ui.delete_confirm.is_some() {
        return match key.code {
            KeyCode::Enter => Some(Action::ConfirmDeleteSession),
            KeyCode::Esc => Some(Action::CancelDeleteSession),
            _ => None,
        };
    }
    if ui.child_confirm.is_some() {
        return match key.code {
            KeyCode::Enter | KeyCode::Char('y') => Some(Action::ConfirmChildControl),
            KeyCode::Esc | KeyCode::Char('n') => Some(Action::CancelChildControl),
            _ => None,
        };
    }
    if ui.overlay == Some(Overlay::Commands) && ui.palette_typed {
        return typed_palette_key(key, state, ui, editor);
    }
    if let Some(overlay) = ui.overlay {
        let names = overlay_names(overlay, state, ui);
        let selected = ui.selection.entry(format!("{overlay:?}")).or_default();
        match key.code {
            KeyCode::Char('d')
                if overlay == Overlay::Sessions
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                return names
                    .get(*selected)
                    .cloned()
                    .map(Action::RequestDeleteSession);
            }
            KeyCode::F(2) if overlay == Overlay::Agents => {
                let name = names.get(*selected)?.clone();
                ui.overlay = Some(Overlay::Models);
                ui.filter.clear();
                return Some(Action::PickAgent(name));
            }
            KeyCode::Esc => {
                ui.filter.clear();
                // esc in the second step of the model picker goes back to the models.
                if let Some(VariantPick::NewConversation(model_id)) = ui.variant_pick.take() {
                    ui.overlay = Some(Overlay::Models);
                    let back = overlay_names(Overlay::Models, state, ui)
                        .iter()
                        .position(|a| *a == model_id)
                        .unwrap_or(0);
                    ui.selection.insert(format!("{:?}", Overlay::Models), back);
                    return None;
                }
                ui.overlay = None;
                ui.settings_target = None;
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
                    Some(VariantPick::NewConversation(model_id)) => Action::PickModel(model_id),
                    _ => Action::CloseOverlay,
                });
            }
            // ctrl+n in the model list: that model in a new tab, whatever this tab holds.
            KeyCode::Char('n')
                if overlay == Overlay::Models && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if ui.settings_target.is_some() {
                    return None;
                }
                let name = names.get(*selected)?.clone();
                if name.starts_with("group:") || name.starts_with(super::catalog::SECTION_PREFIX) {
                    return None;
                }
                variant_step(state, ui, VariantPick::NewConversation(name));
                return None;
            }
            // ctrl+f in the model list: the highlighted model becomes a favorite, or stops being
            // one. A heading does nothing. The highlight stays on the same model: on its row in
            // the Favorites section if it is still there, else on its row in its group.
            KeyCode::Char('f')
                if overlay == Overlay::Models && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let at = *selected;
                let rows = super::catalog::rows(&state.catalog, ui);
                let (name, was_favorite_row) = match rows.get(at)? {
                    super::catalog::Row::Model(m) => (m.name.clone(), false),
                    super::catalog::Row::Favorite(m) => (m.name.clone(), true),
                    _ => return None,
                };
                drop(rows);
                if !ui.favorite_models.remove(&name) {
                    ui.favorite_models.insert(name.clone());
                }
                let after = super::catalog::rows(&state.catalog, ui);
                let position = after
                    .iter()
                    .position(|r| r.key() == name && r.is_favorite_row() == was_favorite_row)
                    .or_else(|| after.iter().position(|r| r.key() == name));
                let count = after.len();
                let selected = ui.selection.entry(format!("{overlay:?}")).or_default();
                *selected = position.unwrap_or(0).min(count.saturating_sub(1));
                return Some(Action::SaveFavoriteModels(
                    ui.favorite_models.iter().cloned().collect(),
                ));
            }
            // alt+a: fold every group, or unfold them all; the selection stays on its row, or
            // moves to the gateway header that held it.
            KeyCode::Char('a')
                if overlay == Overlay::Models && key.modifiers.contains(KeyModifiers::ALT) =>
            {
                let at = *selected;
                let mut gateway = None;
                let mut before = None;
                for (i, row) in super::catalog::rows(&state.catalog, ui).iter().enumerate() {
                    if let super::catalog::Row::Group { key, level: 0, .. } = row {
                        gateway = Some(format!("group:{key}"));
                    }
                    if i == at {
                        before = Some((row.key(), row.is_favorite_row(), gateway.clone()));
                    }
                }
                super::catalog::toggle_all(&state.catalog, ui);
                let after = super::catalog::rows(&state.catalog, ui);
                let found = before.and_then(|(name, favorite, header)| {
                    after
                        .iter()
                        .position(|r| r.key() == name && r.is_favorite_row() == favorite)
                        .or_else(|| header.and_then(|h| after.iter().position(|r| r.key() == h)))
                });
                let count = after.len();
                let selected = ui.selection.entry(format!("{overlay:?}")).or_default();
                *selected = found.unwrap_or(0).min(count.saturating_sub(1));
                return None;
            }
            // The Favorites heading has no fold: these keys do nothing on it.
            KeyCode::Enter | KeyCode::Left | KeyCode::Right
                if overlay == Overlay::Models
                    && names
                        .get(*selected)
                        .is_some_and(|n| n.starts_with(super::catalog::SECTION_PREFIX)) =>
            {
                return None;
            }
            KeyCode::Enter | KeyCode::Left | KeyCode::Right
                if overlay == Overlay::Models
                    && names
                        .get(*selected)
                        .is_some_and(|n| n.starts_with("group:")) =>
            {
                let group = names[*selected].trim_start_matches("group:").to_string();
                if ui.filter.is_empty() {
                    if key.code == KeyCode::Left {
                        ui.collapsed_models.insert(group.clone());
                    } else if key.code == KeyCode::Right {
                        ui.collapsed_models.remove(&group);
                    } else if !ui.collapsed_models.remove(&group) {
                        ui.collapsed_models.insert(group);
                    }
                }
                return None;
            }
            KeyCode::Enter => {
                let name = names.get(*selected)?;
                let action = match overlay {
                    Overlay::Models if ui.settings_target.is_some() => {
                        Action::Config(super::config::ConfigAction::PickModel(name.clone()))
                    }
                    Overlay::Agents => Action::PickAgent(name.clone()),
                    Overlay::Skills => {
                        editor.set_text(&format!("/skill {name} "));
                        editor
                            .area_mut()
                            .move_cursor(ratatui_textarea::CursorMove::End);
                        ui.overlay = None;
                        ui.filter.clear();
                        return Some(Action::CloseOverlay);
                    }
                    // A live conversation switches to it, the model only (the server applies its
                    // default variant), refused with the reason while the agent waits for the
                    // Captain. An empty tab goes on to choose the variant it starts on.
                    Overlay::Models if reducer::occupied(state) => {
                        if let Some(why) = reducer::switch_blocked(state) {
                            return Some(Action::ShowNotice(why.into()));
                        }
                        Action::SwitchModel(name.clone())
                    }
                    Overlay::Models => {
                        variant_step(state, ui, VariantPick::NewConversation(name.clone()));
                        return None;
                    }
                    Overlay::Variants => Action::PickVariant(name.clone()),
                    Overlay::Sessions => Action::ResumeSession(name.clone()),
                    Overlay::Children => Action::OpenChild(name.clone()),
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
    if !state.read_only() {
        if let Some(action) = permission_key(key, state, ui, editor) {
            return action;
        }
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if !state.read_only() && !ctrl && !alt {
        if let Some(p) = state
            .conversation
            .blocks
            .iter()
            .rev()
            .find_map(|b| match &b.kind {
                super::state::BlockKind::Question(p) if p.answer.is_none() => Some(p),
                _ => None,
            })
        {
            if !p.dismissed && editor.text().is_empty() {
                return match key.code {
                    KeyCode::Up => Some(Action::QuestionMove(-1)),
                    KeyCode::Down => Some(Action::QuestionMove(1)),
                    KeyCode::Char(' ') => Some(Action::QuestionToggle),
                    KeyCode::Esc => Some(Action::QuestionDismiss),
                    KeyCode::Enter if p.cursor == p.choices.options.len() => {
                        Some(Action::QuestionCustom)
                    }
                    KeyCode::Enter => {
                        Some(Action::AnswerQuestion(crate::tools::control::AskAnswer {
                            question_call_id: p.call_id.clone(),
                            selected: if p.choices.multiple {
                                p.selected.clone()
                            } else {
                                vec![p.cursor]
                            },
                            custom: None,
                        }))
                    }
                    _ => None,
                };
            }
            if p.custom {
                if key.code == KeyCode::Esc {
                    return Some(Action::QuestionDismiss);
                }
                if key.code == KeyCode::Enter && !editor.text().trim().is_empty() {
                    let custom = editor.text().trim().to_owned();
                    editor.clear_if_revision(editor.revision());
                    return Some(Action::AnswerQuestion(crate::tools::control::AskAnswer {
                        question_call_id: p.call_id.clone(),
                        selected: p.selected.clone(),
                        custom: Some(custom),
                    }));
                }
            }
        }
    }
    // tab on an empty message switches build/plan; with text in it, or as shift+tab, it moves the
    // focus between the editor and the transcript.
    if key.code == KeyCode::Tab
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && ui.focus == Focus::Editor
        && editor.text().is_empty()
        && !state.read_only()
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
    if key.code == KeyCode::Esc && ui.quit_confirm.is_some() {
        ui.quit_confirm = None;
        return None;
    }
    if key.code == KeyCode::End && ctrl {
        ui.follow_bottom = true;
        ui.scroll = 0;
        return None;
    }
    if key.code == KeyCode::Char('c') && ctrl && state.activity != Activity::Running {
        if ui.quit_confirm.is_some() {
            return Some(Action::Quit);
        }
        ui.quit_confirm = Some("ctrl+c");
        return None;
    }
    if key.code == KeyCode::Char('q')
        && ctrl
        && (!editor.text().trim().is_empty() || state.activity == Activity::Running)
        && ui.quit_confirm.is_none()
    {
        ui.quit_confirm = Some("ctrl+q");
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
    if ui.focus == Focus::Transcript || state.read_only() {
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
        editor_input(editor, key)
    };
    if changed {
        editor.bump();
    }
    None
}

/// A key while the palette follows the editor. Every printable key and backspace edit the line;
/// the palette then closes when the line is no longer a command name being typed (a space or an
/// argument after it, a name no row starts with, `//`, an empty line), leaving the line for enter.
fn config_key(key: KeyEvent, state: &State) -> Option<Action> {
    use super::config::{ConfigAction as C, LeaveChoice, Section};
    let config = &state.config;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let action = if config.leave_prompt {
        match key.code {
            KeyCode::Char('w') => C::ResolveLeave(LeaveChoice::Write),
            KeyCode::Char('d') => C::ResolveLeave(LeaveChoice::Discard),
            KeyCode::Esc | KeyCode::Char('s') => C::ResolveLeave(LeaveChoice::Stay),
            _ => return None,
        }
    } else if let Some(text) = &config.list_editor {
        match key.code {
            KeyCode::Esc => C::CancelList,
            KeyCode::Enter => C::ListText(format!("{text}\n")),
            KeyCode::Char('s') if ctrl => C::CommitList,
            KeyCode::Backspace => {
                let mut text = text.clone();
                text.pop();
                C::ListText(text)
            }
            KeyCode::Char(c) if !ctrl => C::ListText(format!("{text}{c}")),
            _ => return None,
        }
    } else if config.filtering {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => C::FilterDone,
            KeyCode::Backspace => {
                let mut text = config.filter.clone();
                text.pop();
                C::Filter(text)
            }
            KeyCode::Char(c) if !ctrl => C::Filter(format!("{}{c}", config.filter)),
            _ => return None,
        }
    } else {
        match key.code {
            KeyCode::Char('s') if ctrl => C::Write,
            KeyCode::Char('z') if ctrl => C::Revert,
            KeyCode::Char(c @ '1'..='6') if !ctrl => {
                C::Jump(Section::ALL[(c as u8 - b'1') as usize])
            }
            KeyCode::BackTab => C::CycleSection(-1),
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => C::CycleSection(-1),
            KeyCode::Tab => C::CycleSection(1),
            KeyCode::Up => C::Move(-1),
            KeyCode::Down => C::Move(1),
            KeyCode::Left => C::Step(-1),
            KeyCode::Right => C::Step(1),
            KeyCode::Char(' ') => C::Toggle,
            KeyCode::Enter => C::Edit,
            KeyCode::Char('/') => C::StartFilter,
            KeyCode::Char('t') => C::TestGateway,
            KeyCode::Esc => C::Leave,
            _ => return None,
        }
    };
    Some(Action::Config(action))
}

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
                // Fills in the name; the Captain types the argument and sends it with enter.
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
    if editor_input(editor, key) {
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

/// Keys for the open permission request of the tab on screen. `o`, `a` and `r` answer it while the
/// message is empty (with text in it they type); after `r` every key types the reject message until
/// enter sends it or esc rejects without one. `Some(action)` when the key was the prompt's.
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
                if message.len() > crate::bus::event::REJECT_MESSAGE_MAX_BYTES {
                    return Some(Some(Action::ShowNotice(
                        crate::bus::event::REJECT_MESSAGE_LIMIT_ERROR.into(),
                    )));
                }
                let message = std::mem::take(message);
                ui.deny_message = None;
                let message = Some(message.trim().to_string()).filter(|m| !m.is_empty());
                Some(answer("reject", message))
            }
            KeyCode::Esc => {
                ui.deny_message = None;
                Some(answer("reject", None))
            }
            KeyCode::Backspace => {
                message.pop();
                None
            }
            KeyCode::Char(c) if plain || key.modifiers == KeyModifiers::SHIFT => {
                if message.len() + c.len_utf8() > crate::bus::event::REJECT_MESSAGE_MAX_BYTES {
                    return Some(Some(Action::ShowNotice(
                        crate::bus::event::REJECT_MESSAGE_LIMIT_ERROR.into(),
                    )));
                }
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
        return matches!(key.code, KeyCode::Char('o' | 'a' | 'r')).then_some(None);
    }
    // Not on screen at the last frame: a key only scrolls to the prompt. Only a
    // frame that actually draws the prompt's keys marks it shown (the render
    // sets `prompt_shown`), so keys queued before that frame, or a terminal too
    // small to show the prompt, never answer an unseen request.
    if matches!(key.code, KeyCode::Char('o' | 'a' | 'r'))
        && ui.prompt_shown.as_deref() != Some(request_id.as_str())
    {
        ui.selected_block = Some(format!("permission:{request_id}"));
        ui.follow_bottom = false;
        return Some(None);
    }
    match key.code {
        KeyCode::Char('o') => Some(Some(answer("allow_once", None))),
        KeyCode::Char('a') if open.always_label.is_some() => {
            Some(Some(answer("allow_always", None)))
        }
        KeyCode::Char('a') => Some(Some(Action::ShowNotice(
            "Always is not offered for this request".into(),
        ))),
        KeyCode::Char('r') => {
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

fn editor_input(editor: &mut Editor, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Left
        | KeyCode::Right
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Home
        | KeyCode::End => editor.area_mut().input(key),
        _ => editor.area_mut().input_without_shortcuts(key),
    }
}

fn close_palette(ui: &mut UiState) {
    ui.overlay = None;
    ui.palette_typed = false;
    ui.filter.clear();
}

/// The keys of the rows an overlay lists under the current filter, in display order.
pub(crate) fn overlay_names(overlay: Overlay, state: &State, ui: &UiState) -> Vec<String> {
    let filter = ui.filter.to_lowercase();
    match overlay {
        Overlay::Agents => state
            .agents
            .iter()
            .filter(|a| {
                matches!(a.mode.as_str(), "primary" | "all")
                    && a.id.to_lowercase().contains(&filter)
            })
            .map(|a| a.id.clone())
            .collect(),
        Overlay::Skills => commands::skill_rows(state, &ui.filter)
            .into_iter()
            .map(|skill| skill.name.clone())
            .collect(),
        Overlay::Models => super::catalog::rows(&state.catalog, ui)
            .iter()
            .map(|row| row.key())
            .collect(),
        Overlay::Sessions => state
            .sessions
            .iter()
            .filter(|x| {
                x.id.to_lowercase().contains(&filter)
                    || x.title
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&filter)
            })
            .map(|x| x.id.clone())
            .collect(),
        Overlay::Commands => commands::palette_rows(state, &ui.filter, ui.palette_typed)
            .into_iter()
            .map(|r| r.name)
            .collect(),
        Overlay::Themes => ThemeId::ALL
            .iter()
            .filter(|x| x.id().contains(&filter) || x.name().to_lowercase().contains(&filter))
            .map(|x| x.id().to_string())
            .collect(),
        Overlay::Help => Vec::new(),
        Overlay::Children => state
            .child_rows(&ui.filter)
            .iter()
            .map(|c| c.session_id.clone())
            .collect(),
        Overlay::Variants => ui
            .variant_pick
            .as_ref()
            .map(|pick| commands::variant_choices(state, pick).0)
            .unwrap_or_default()
            .into_iter()
            .filter(|v| v.to_lowercase().contains(&filter))
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
    if ui.screen == super::config::Screen::Config && ui.overlay.is_none() {
        use super::config::ConfigAction;
        if let Some(draft) = &state.config.list_editor {
            return Some(Action::Config(ConfigAction::ListText(format!(
                "{draft}{text}"
            ))));
        }
        if state.config.filtering {
            return Some(Action::Config(ConfigAction::Filter(format!(
                "{}{text}",
                state.config.filter
            ))));
        }
        return None;
    }
    if state.read_only() {
        return None;
    }
    // While a reject message is typed, a paste goes into it (one line); a message left from a
    // request that is no longer the open one is dropped.
    let open = state.open_permission().map(|p| p.request_id.as_str());
    if ui.deny_message.is_some() && open.is_none_or(|id| ui.deny_draft(id).is_none()) {
        ui.deny_message = None;
    }
    if let Some(DenyDraft { text: message, .. }) = ui.deny_message.as_mut() {
        if ui.overlay.is_none() {
            let text = text.replace(['\n', '\r'], " ");
            if message.len() + text.len() > crate::bus::event::REJECT_MESSAGE_MAX_BYTES {
                return Some(Action::ShowNotice(
                    crate::bus::event::REJECT_MESSAGE_LIMIT_ERROR.into(),
                ));
            }
            message.push_str(&text);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_palette_filters_description_and_enter_fills_draft() {
        let mut state = State::default();
        state.skills.skills = vec![
            crate::bus::event::SkillEntry {
                name: "review".into(),
                description: "Check correctness".into(),
            },
            crate::bus::event::SkillEntry {
                name: "test".into(),
                description: "Check behavior".into(),
            },
        ];
        assert!(commands::palette(&state)
            .iter()
            .any(|row| row.name == "/skills"));
        assert_eq!(
            commands::command_action("/skills", crate::tui::protocol::Mode::NextTurn),
            Some(Action::OpenOverlay(Overlay::Skills))
        );
        let mut ui = UiState {
            overlay: Some(Overlay::Skills),
            filter: "check".into(),
            ..Default::default()
        };
        let mut editor = Editor::new();
        route_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &state,
            &mut ui,
            &mut editor,
        );
        assert_eq!(ui.selection["Skills"], 1);
        route_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &state,
            &mut ui,
            &mut editor,
        );
        ui.filter = "correctness".into();
        let action = route_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &state,
            &mut ui,
            &mut editor,
        );
        assert_eq!(action, Some(Action::CloseOverlay));
        assert_eq!(editor.text(), "/skill review ");
        assert_eq!(editor.area().cursor(), (0, 14));
        let action = route_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &state,
            &mut ui,
            &mut editor,
        );
        assert!(matches!(action, Some(Action::SubmitAt { text, .. }) if text == "/skill review "));
    }

    #[test]
    fn cursor_keys_navigate_multiline_drafts_without_editing() {
        let mut editor = Editor::new();
        editor.set_text("first\nsecond");
        let revision = editor.revision();
        for (code, expected) in [
            (KeyCode::Right, (0, 1)),
            (KeyCode::End, (0, 5)),
            (KeyCode::Down, (1, 5)),
            (KeyCode::Home, (1, 0)),
            (KeyCode::Right, (1, 1)),
            (KeyCode::Left, (1, 0)),
            (KeyCode::Up, (0, 0)),
        ] {
            assert!(!editor_input(
                &mut editor,
                KeyEvent::new(code, KeyModifiers::NONE)
            ));
            assert_eq!(editor.area().cursor(), expected);
        }
        assert_eq!(editor.revision(), revision);
        assert_eq!(editor.text(), "first\nsecond");
    }
}
