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
        description: "new conversation in a new tab",
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
    ("ctrl+n", "new conversation, in a new tab"),
    ("ctrl+o", "sessions (opens in a tab)"),
    ("alt+1 … alt+9", "switch to tab 1 … 9"),
    ("alt+w", "close tab (the conversation is kept)"),
    ("alt+b", "hide / show the tab bar"),
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

/// How a typed line is handled: the server's rule (`//text` sends the literal `/text`, `/name args`
/// is a command, anything else a message), with the client's own commands kept local.
#[derive(Debug, Clone, PartialEq)]
pub enum Routed {
    /// A command this client runs itself.
    Local(Action),
    /// A command the server runs in the conversation.
    Server { name: String, arguments: String },
    /// A message, sent as this text.
    Message(String),
}

/// Routes `line` (as typed, before trimming). Every `/name` that is not one of the client's
/// commands goes to the server, which answers an unknown name with the close ones.
pub fn route(line: &str, mode: Mode) -> Routed {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("//") {
        return Routed::Message(format!("/{rest}"));
    }
    let Some(rest) = trimmed.strip_prefix('/') else {
        return Routed::Message(line.to_string());
    };
    let (name, arguments) = match rest.split_once(char::is_whitespace) {
        Some((name, arguments)) => (name, arguments.trim()),
        None => (rest, ""),
    };
    if name.is_empty() {
        return Routed::Message(line.to_string());
    }
    if let Some(action) = local_action(name, arguments, mode) {
        return Routed::Local(action);
    }
    Routed::Server {
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}

/// The client's own meaning of `/name arguments`, if it has one.
fn local_action(name: &str, arguments: &str, mode: Mode) -> Option<Action> {
    match (name, arguments.is_empty()) {
        // `/new <alias>` opens a tab on that model, like picking it in the model list.
        ("new", false) => Some(Action::PickModel(arguments.to_string())),
        // With arguments, `/variant` is the server's; alone it opens the list.
        ("variant", false) => None,
        _ => command_action(&format!("/{name}"), mode),
    }
}

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
    let alt = key.modifiers.contains(KeyModifiers::ALT) && !ctrl;
    Some(match key.code {
        KeyCode::Char(c @ '1'..='9') if alt => Action::SwitchTab(c as usize - '1' as usize),
        KeyCode::Char('w') if alt => Action::CloseTab,
        KeyCode::Char('b') if alt => Action::ToggleSidebar,
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
