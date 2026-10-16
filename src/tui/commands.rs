//! Command catalog and key mapping; no I/O or rendering.
use super::{
    protocol::{Action, Mode, Overlay},
    state::{NO_VARIANT, State, VariantPick},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
    /// Argument synopsis, empty when the command takes none (`[model_id]`).
    pub args_hint: &'static str,
}
pub const CATALOG: &[Command] = &[
    Command {
        name: "/config",
        description: "edit settings in config.json",
        args_hint: "",
    },
    Command {
        name: "/new",
        description: "Voyage · Session — new conversation in a new tab",
        args_hint: "[agent]",
    },
    Command {
        name: "/sessions",
        description: "Voyage · Session — browse conversations",
        args_hint: "",
    },
    Command {
        name: "/model",
        description: "switch this conversation's model",
        args_hint: "[model_id]",
    },
    Command {
        name: "/skills",
        description: "choose a skill to load for your next message",
        args_hint: "",
    },
    Command {
        name: "/variant",
        description: "choose the model variant for this conversation",
        args_hint: "[name|off]",
    },
    Command {
        name: "/stop",
        description: "stop generation",
        args_hint: "",
    },
    Command {
        name: "/mode",
        description: "cycle delivery mode (steer and interrupt send as next-turn)",
        args_hint: "",
    },
    Command {
        name: "/theme",
        description: "choose the colour theme",
        args_hint: "",
    },
    Command {
        name: "/help",
        description: "show keyboard shortcuts",
        args_hint: "",
    },
    Command {
        name: "/quit",
        description: "quit terminal client",
        args_hint: "",
    },
];

/// One row of the command palette: a client command or one the server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteRow {
    /// With its leading slash.
    pub name: String,
    pub args_hint: String,
    pub description: String,
    /// The server runs it in the conversation.
    pub server: bool,
}

impl PaletteRow {
    /// The command can run with no argument (none, or only optional ones).
    pub fn runs_bare(&self) -> bool {
        self.args_hint.is_empty() || self.args_hint.starts_with('[')
    }
}

/// The client's commands, then the server's that the client does not handle itself.
pub fn palette(state: &State) -> Vec<PaletteRow> {
    let mut rows: Vec<PaletteRow> = CATALOG
        .iter()
        .map(|c| PaletteRow {
            name: c.name.to_string(),
            args_hint: c.args_hint.to_string(),
            description: c.description.to_string(),
            server: false,
        })
        .collect();
    for c in &state.server_commands {
        let name = format!("/{}", c.name);
        if !rows.iter().any(|r| r.name == name) {
            rows.push(PaletteRow {
                name,
                args_hint: c.args_hint.clone(),
                description: c.description.to_lowercase(),
                server: true,
            });
        }
    }
    rows
}

/// The palette rows under `filter`. Typed after `/` (`by_name`), it matches the start of the
/// command name; typed in the palette opened with ctrl+p, any part of the name or description.
pub fn palette_rows(state: &State, filter: &str, by_name: bool) -> Vec<PaletteRow> {
    let filter = filter.to_lowercase();
    palette(state)
        .into_iter()
        .filter(|r| {
            if by_name {
                r.name[1..].starts_with(&filter)
            } else {
                r.name.contains(&filter) || r.description.to_lowercase().contains(&filter)
            }
        })
        .collect()
}

/// What choosing a palette row does: the client's action, or a server run with no argument.
pub fn palette_action(row: &PaletteRow, mode: Mode, revision: Option<u64>) -> Option<Action> {
    if row.server {
        return Some(Action::RunCommand {
            name: row.name[1..].to_string(),
            arguments: String::new(),
            revision,
        });
    }
    command_action(&row.name, mode)
}

pub const KEYS: &[(&str, &str)] = &[
    ("/skills", "choose a skill; enter fills /skill <name>"),
    ("enter", "send / choose"),
    ("alt+enter / ctrl+j", "newline"),
    ("ctrl+p / /", "commands"),
    ("ctrl+n", "new conversation, in a new tab"),
    ("ctrl+o", "sessions (opens in a tab)"),
    (
        "ctrl+d (in sessions)",
        "delete session (asks for confirmation)",
    ),
    ("alt+1 … alt+9", "switch to tab 1 … 9"),
    ("alt+w", "close tab (the conversation is kept)"),
    ("alt+b", "hide / show the tab bar"),
    ("alt+r", "hide / show the side panel"),
    ("alt+s", "open a subagent transcript (read-only)"),
    ("alt+c", "continue a subagent out of steps"),
    (
        "alt+p",
        "Bypass ordinary permission prompts; protected requests still ask",
    ),
    ("f2", "switch this conversation's model"),
    ("ctrl+n (in models)", "open that model in a new tab"),
    ("ctrl+f (in models)", "add / remove the model as a favorite"),
    ("f1", "help"),
    ("tab", "build / plan (empty message)"),
    ("shift+tab", "switch focus"),
    ("page up / page down", "scroll"),
    ("up / down", "select transcript block"),
    (
        "drag permission",
        "copy full review; o once / a always / r reject answer an open prompt",
    ),
    (
        "drag / double-click",
        "select text / word; release copies via OSC 52",
    ),
    ("ctrl+shift+c", "multi-block selections group sender names"),
    (
        "shift+drag",
        "native selection; OSC 52: tmux set-clipboard on",
    ),
    ("space / enter", "expand selected block"),
    ("ctrl+end", "follow newest"),
    ("esc / ctrl+c", "stop / close"),
    ("ctrl+q", "quit"),
];

/// The rows of the variant list for `pick` and the row to start on.
///
/// For a new conversation: the catalog's variants of that model then `off` (no row when it has
/// none), starting on its default. For `/variant`: what the server offers this conversation then
/// `off` (always: a live conversation can drop its variant), starting on the variant in use.
pub fn variant_choices(state: &State, pick: &VariantPick) -> (Vec<String>, usize) {
    let (mut rows, wanted) = match pick {
        VariantPick::NewConversation(model_id) => {
            let entry = state.catalog.iter().find(|m| &m.name == model_id);
            let rows: Vec<String> = entry.map(|m| m.variants.clone()).unwrap_or_default();
            if rows.is_empty() {
                // Nothing to choose: the list shows a note instead.
                return (rows, 0);
            }
            (rows, entry.and_then(|m| m.default_variant.clone()))
        }
        VariantPick::Current => (
            state.variant_offer.clone().unwrap_or_default(),
            state.variant.clone(),
        ),
    };
    rows.retain(|row| row != NO_VARIANT && row != "inherit");
    rows.push(NO_VARIANT.to_string());
    let fallback = rows.len() - 1;
    rows.push("inherit".into());
    let start = wanted
        .and_then(|w| rows.iter().position(|r| *r == w))
        .unwrap_or(fallback);
    (rows, start)
}

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
    /// A client command typed wrong: this says why; nothing runs.
    Refused(String),
}

/// Routes `line` as typed, with the server's rule: only a line whose very first character is `/`
/// is a command (`  /stop` is a message), `//text` is the message `/text`, the name runs to the
/// first whitespace and the rest, trimmed, is the arguments. The client then keeps the commands it
/// handles itself; every other name goes to the server, which answers an unknown one with the
/// close ones.
pub fn route(line: &str, mode: Mode) -> Routed {
    if let Some(rest) = line.strip_prefix("//") {
        // Preserve the escape until submission so a literal /skill is never a selection.
        if rest == "skill"
            || rest
                .strip_prefix("skill")
                .is_some_and(|tail| tail.starts_with(char::is_whitespace))
        {
            return Routed::Message(line.to_string());
        }
        return Routed::Message(format!("/{rest}"));
    }
    let Some(rest) = line.strip_prefix('/') else {
        return Routed::Message(line.to_string());
    };
    let (name, arguments) = match rest.split_once(char::is_whitespace) {
        Some((name, arguments)) => (name, arguments.trim()),
        None => (rest, ""),
    };
    if name.is_empty() {
        return Routed::Message(line.to_string());
    }
    if name == "skill" {
        return if arguments.is_empty() {
            Routed::Refused("usage: /skill <name> [request]".into())
        } else {
            Routed::Message(line.to_string())
        };
    }
    match local_action(name, arguments, mode) {
        Local::Run(action) => Routed::Local(action),
        Local::Refuse(why) => Routed::Refused(why),
        Local::Server => Routed::Server {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
    }
}

enum Local {
    Run(Action),
    /// A client command typed wrong; the reason is shown and nothing is sent.
    Refuse(String),
    /// Not the client's: the server runs it.
    Server,
}

/// The client's own meaning of `/name arguments`.
fn local_action(name: &str, arguments: &str, mode: Mode) -> Local {
    let has_args = !arguments.is_empty();
    match name {
        // `/new <model_id>` opens a tab on that model; `/model <model_id>` moves this conversation to it.
        "new" | "model" if has_args => {
            if arguments.contains(char::is_whitespace) {
                return Local::Refuse(format!("usage: /{name} [agent or model]"));
            }
            let model_id = arguments.to_string();
            Local::Run(if name == "new" {
                Action::PickAgent(model_id)
            } else {
                Action::SwitchModel(model_id)
            })
        }
        // With arguments, `/variant` is the server's; alone it opens the list.
        "variant" if has_args => Local::Server,
        _ => match command_action(&format!("/{name}"), mode) {
            Some(_) if has_args => Local::Refuse(format!("/{name} takes no arguments")),
            Some(action) => Local::Run(action),
            None => Local::Server,
        },
    }
}

pub fn command_action(name: &str, mode: Mode) -> Option<Action> {
    Some(match name {
        "/new" => Action::NewConversation,
        "/sessions" => Action::OpenOverlay(Overlay::Sessions),
        "/model" => Action::OpenOverlay(Overlay::Models),
        "/skills" => Action::OpenOverlay(Overlay::Skills),
        "/variant" => Action::OpenOverlay(Overlay::Variants),
        "/stop" => Action::Stop,
        "/mode" => Action::SetMode(match mode {
            Mode::NextTurn => Mode::Steer,
            Mode::Steer => Mode::Interrupt,
            Mode::Interrupt => Mode::NextTurn,
        }),
        "/theme" => Action::OpenOverlay(Overlay::Themes),
        "/config" => Action::OpenConfig,
        "/help" => Action::OpenOverlay(Overlay::Help),
        "/quit" => Action::Quit,
        _ => return None,
    })
}

/// Shared filtering keeps the selected name and the rendered description in sync.
pub fn skill_rows<'a>(state: &'a State, filter: &str) -> Vec<&'a crate::bus::event::SkillEntry> {
    let filter = filter.to_lowercase();
    if state.skills.disabled_reason.is_some() {
        return vec![];
    }
    state
        .skills
        .skills
        .iter()
        .filter(|skill| {
            skill.name.to_lowercase().contains(&filter)
                || skill.description.to_lowercase().contains(&filter)
        })
        .collect()
}

pub fn key_to_action(key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT) && !ctrl;
    Some(match key.code {
        KeyCode::Char(c @ '1'..='9') if alt => Action::SwitchTab(c as usize - '1' as usize),
        KeyCode::Char('w') if alt => Action::CloseTab,
        KeyCode::Char('b') if alt => Action::ToggleSidebar,
        KeyCode::Char('r') if alt => Action::TogglePanel,
        KeyCode::Char('s') if alt => Action::OpenOverlay(Overlay::Children),
        KeyCode::Char('c') if alt => Action::ContinueChild,
        KeyCode::Char('p') if alt => Action::ToggleBypass,
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
