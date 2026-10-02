//! Command catalog and key mapping; no I/O or rendering.
use super::protocol::{Action, Mode, Overlay};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
}
pub const CATALOG: &[Command] = &[
    Command {
        name: "/new",
        description: "new conversation",
    },
    Command {
        name: "/sessions",
        description: "browse conversations",
    },
    Command {
        name: "/model",
        description: "choose model for a new conversation",
    },
    Command {
        name: "/stop",
        description: "stop generation",
    },
    Command {
        name: "/mode",
        description: "cycle delivery mode (steer and interrupt send as next-turn)",
    },
    Command {
        name: "/theme",
        description: "choose the colour theme",
    },
    Command {
        name: "/help",
        description: "show keyboard shortcuts",
    },
    Command {
        name: "/quit",
        description: "quit terminal client",
    },
];

pub const KEYS: &[(&str, &str)] = &[
    ("enter", "send / choose"),
    ("alt+enter / ctrl+j", "newline"),
    ("ctrl+p / /", "commands"),
    ("ctrl+n", "new conversation"),
    ("ctrl+o", "sessions"),
    ("f2", "models"),
    ("f1", "help"),
    ("tab / shift+tab", "switch focus"),
    ("page up / page down", "scroll"),
    ("up / down", "select transcript block"),
    ("space / enter", "expand selected block"),
    ("ctrl+end", "follow newest"),
    ("esc / ctrl+c", "stop / close"),
    ("ctrl+q", "quit"),
];

pub fn command_action(name: &str, mode: Mode) -> Option<Action> {
    Some(match name {
        "/new" => Action::NewConversation,
        "/sessions" => Action::OpenOverlay(Overlay::Sessions),
        "/model" => Action::OpenOverlay(Overlay::Models),
        "/stop" => Action::Stop,
        "/mode" => Action::SetMode(match mode {
            Mode::NextTurn => Mode::Steer,
            Mode::Steer => Mode::Interrupt,
            Mode::Interrupt => Mode::NextTurn,
        }),
        "/theme" => Action::OpenOverlay(Overlay::Themes),
        "/help" => Action::OpenOverlay(Overlay::Help),
        "/quit" => Action::Quit,
        _ => return None,
    })
}

pub fn key_to_action(key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Char('p') if ctrl => Action::OpenOverlay(Overlay::Commands),
        KeyCode::Char('n') if ctrl => Action::NewConversation,
        KeyCode::Char('o') if ctrl => Action::OpenOverlay(Overlay::Sessions),
        KeyCode::F(2) => Action::OpenOverlay(Overlay::Models),
        KeyCode::F(1) => Action::OpenOverlay(Overlay::Help),
        KeyCode::PageUp => Action::Scroll(-10),
        KeyCode::PageDown => Action::Scroll(10),
        KeyCode::Esc | KeyCode::Char('c') if key.code == KeyCode::Esc || ctrl => Action::Stop,
        KeyCode::Char('q') if ctrl => Action::Quit,
        _ => return None,
    })
}
