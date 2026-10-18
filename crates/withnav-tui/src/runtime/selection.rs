use super::Runtime;
use crate::{protocol::Action, state::Focus, view};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Selection {
    pub range: Option<(usize, usize)>,
    anchor: Option<(usize, usize)>,
    pub pointer: Option<(u16, u16)>,
    last_click: Option<(u16, u16, Instant)>,
    pub notice_until: Option<Instant>,
}

impl Runtime {
    pub(super) fn selection_key(&mut self, key: KeyEvent) -> bool {
        if self.ui.screen != crate::config::Screen::Session
            || self.ui.overlay.is_some()
            || self.ui.child_confirm.is_some()
        {
            return false;
        }
        let shortcut = key.code == KeyCode::Char('c')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        if !shortcut {
            return false;
        }
        let text = self
            .selection
            .range
            .and_then(|(a, b)| self.cache.transcript.selected_text(a, b))
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.ui
                    .selected_block
                    .as_ref()
                    .and_then(|id| self.state.conversation.blocks.iter().find(|b| &b.id == id))
                    .and_then(|b| match &b.kind {
                        crate::state::BlockKind::Permission(p) => {
                            Some(view::permission_sections(p, &self.state).join("\n"))
                        }
                        _ => view::copy_block(b),
                    })
            });
        if let Some(text) = text {
            self.copy_text(&text);
        } else {
            self.state.notice = Some("Select transcript text or a block to copy".into());
        }
        true
    }

    pub(super) fn selection_mouse(&mut self, event: MouseEvent) {
        if self.ui.overlay.is_some() || self.ui.child_confirm.is_some() {
            if self.ui.overlay.is_some()
                && matches!(
                    event.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                )
            {
                let code = if event.kind == MouseEventKind::ScrollUp {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                };
                self.handle_terminal_event(crossterm::event::Event::Key(KeyEvent::new(
                    code,
                    KeyModifiers::NONE,
                )));
            }
            return;
        }
        let map = &self.cache.transcript;
        match event.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if map.area.contains((event.column, event.row).into()) =>
            {
                if self.ui.selected_block.is_some() || self.ui.follow_bottom {
                    self.ui.scroll = map.scroll;
                }
                self.ui.selected_block = None;
                self.ui.follow_bottom = false;
                self.dispatch(Action::Scroll(if event.kind == MouseEventKind::ScrollUp {
                    3
                } else {
                    -3
                }));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = map.hit(event.column, event.row) {
                    let now = Instant::now();
                    let double = self.selection.last_click.is_some_and(|(x, y, t)| {
                        x == event.column
                            && y == event.row
                            && now.duration_since(t) < Duration::from_millis(400)
                    });
                    self.selection.last_click = Some((event.column, event.row, now));
                    let hit = if double { map.word(hit.0, hit.1) } else { hit };
                    self.selection.anchor = Some(hit);
                    self.selection.range = Some(hit);
                    self.selection.pointer = Some((event.column, event.row));
                    self.ui.focus = Focus::Transcript;
                    self.ui.scroll = map.scroll;
                    self.ui.follow_bottom = false;
                    self.ui.selected_block = None;
                } else {
                    self.selection.range = None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selection.anchor.is_some() => {
                self.selection.pointer = Some((event.column, event.row));
                self.extend_selection();
            }
            MouseEventKind::Up(MouseButton::Left) if self.selection.anchor.is_some() => {
                self.extend_selection();
                self.selection.anchor = None;
                self.selection.pointer = None;
                if let Some((a, b)) = self.selection.range
                    && let Some(text) = self.cache.transcript.selected_text(a, b)
                {
                    self.copy_text(&text);
                }
            }
            _ => return,
        }
        self.dirty = true;
    }

    pub(super) fn extend_selection(&mut self) {
        let (Some((x, y)), Some((a, b))) = (self.selection.pointer, self.selection.anchor) else {
            return;
        };
        let area = self.cache.transcript.area;
        if area.is_empty() {
            return;
        }
        let y = y.clamp(area.y, area.bottom() - 1);
        let x = x.clamp(area.x, area.right() - 1);
        if let Some((c, d)) = self.cache.transcript.hit(x, y) {
            self.selection.range = Some((a.min(c), b.max(d)));
        }
    }

    pub(super) fn copy_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let result = osc52(text).and_then(|sequence| {
            let mut out = io::stdout().lock();
            out.write_all(sequence.as_bytes())?;
            out.flush()
        });
        self.state.notice = Some(match result {
            Ok(()) => format!("Copied {} lines · OSC 52 sent", text.lines().count()),
            Err(err) => format!("Copy failed: {err}"),
        });
        self.selection.notice_until = Some(Instant::now() + Duration::from_secs(4));
    }
}

/// Bound terminal writes; encode every byte so text cannot inject terminal commands.
pub(super) fn osc52(text: &str) -> io::Result<String> {
    if text.len() > 100_000 {
        return Err(io::Error::other(
            "selection exceeds 100 KB; copy a smaller section",
        ));
    }
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(text.len().div_ceil(3) * 4);
    for chunk in text.as_bytes().chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        encoded.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        encoded.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    Ok(format!("\x1b]52;c;{encoded}\x07"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipboard_encoding_and_limit() {
        for (text, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("界", "55WM"),
        ] {
            assert_eq!(osc52(text).unwrap(), format!("\x1b]52;c;{expected}\x07"));
        }
        assert!(osc52(&"x".repeat(100_001)).is_err());
    }
}
