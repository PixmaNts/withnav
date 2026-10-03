//! The session command registry: which `/commands` exist, how a typed line
//! is routed (command, literal slash, or plain message) and how an unknown
//! name is answered. Execution lives in `Service::run_command`.

use serde::Serialize;
use serde_json::Value;
use ulid::Ulid;

/// One command a client can list and run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CommandSpec {
    /// The name without its leading slash.
    pub name: &'static str,
    pub description: &'static str,
    /// Argument synopsis, empty when the command takes none (`[alias]`).
    pub args_hint: &'static str,
}

/// Every registered command, in listing order. A command that is listed but
/// not built answers [`NOT_AVAILABLE`].
pub const COMMANDS: [CommandSpec; 8] = [
    CommandSpec {
        name: "help",
        description: "List the available commands",
        args_hint: "",
    },
    CommandSpec {
        name: "model",
        description:
            "Show the session's model alias, its context window and where the window comes from",
        args_hint: "",
    },
    CommandSpec {
        name: "new",
        description: "Start a fresh conversation; this one is kept",
        args_hint: "[alias]",
    },
    CommandSpec {
        name: "stop",
        description: "Stop this session",
        args_hint: "",
    },
    CommandSpec {
        name: "context",
        description: "Show how the context window is used",
        args_hint: "",
    },
    CommandSpec {
        name: "variant",
        description: "Show the model variant and the ones on offer, or switch (off clears it)",
        args_hint: "[name|off]",
    },
    CommandSpec {
        name: "compact",
        description: "Compact the conversation",
        args_hint: "[focus]",
    },
    CommandSpec {
        name: "agent",
        description: "Show or change the primary agent",
        args_hint: "[id]",
    },
];

/// Commands that are registered but have no implementation yet.
pub const NOT_AVAILABLE: [&str; 1] = ["compact"];

/// The error text of a registered command with no implementation.
pub fn not_available_error(name: &str) -> String {
    format!("/{name} is not available yet")
}

/// The error text of a command sent while the agent waits for an answer.
pub const ASK_PENDING_ERROR: &str =
    "the agent is waiting for an answer: reply to its question first, then run the command";

/// The name as typed, without surrounding whitespace and without ONE leading
/// `/`: the single normalisation every lookup, error and result goes through.
pub fn normalize_name(name: &str) -> &str {
    let name = name.trim();
    name.strip_prefix('/').unwrap_or(name)
}

/// The registered command called `name`: its [`normalize_name`] form must
/// equal a registered name exactly (`//stop` and `STOP` are unknown).
pub fn find(name: &str) -> Option<&'static CommandSpec> {
    let name = normalize_name(name);
    COMMANDS.iter().find(|c| c.name == name)
}

/// The error text for an unknown name: it lists the close names, or every
/// name when none is close.
pub fn unknown_error(name: &str) -> String {
    let name = normalize_name(name);
    let lower = name.to_lowercase();
    let mut close: Vec<&str> = COMMANDS
        .iter()
        .map(|c| c.name)
        .filter(|c| c.starts_with(&lower) || lower.starts_with(c) || edit_distance(c, &lower) <= 2)
        .collect();
    let all = close.is_empty();
    if all {
        close = COMMANDS.iter().map(|c| c.name).collect();
    }
    let list = close
        .iter()
        .map(|c| format!("/{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    if all {
        format!("unknown command /{name}; available: {list}")
    } else {
        format!("unknown command /{name}; did you mean {list}?")
    }
}

/// Levenshtein distance over chars; names are short.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// How a client routes one typed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    /// `/name arguments`: run through the command endpoint.
    Command { name: String, arguments: String },
    /// A plain message to send as written. `//text` gives `/text`.
    Message(String),
}

/// Routes a typed line: a leading `//` is the escape for a literal slash, a
/// leading `/` followed by a name is a command, anything else is a message.
pub fn route_line(line: &str) -> Routed {
    if let Some(rest) = line.strip_prefix("//") {
        return Routed::Message(format!("/{rest}"));
    }
    if let Some(rest) = line.strip_prefix('/') {
        let (name, arguments) = match rest.split_once(char::is_whitespace) {
            Some((name, arguments)) => (name, arguments.trim()),
            None => (rest, ""),
        };
        if !name.is_empty() {
            return Routed::Command {
                name: name.to_string(),
                arguments: arguments.to_string(),
            };
        }
    }
    Routed::Message(line.to_string())
}

/// What one command run answered, the shape of the `command.result` event
/// and of the HTTP response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandOutcome {
    pub operation_id: Ulid,
    #[serde(skip)]
    pub name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CommandOutcome {
    /// The plain text a terminal shows for this outcome: the command's data
    /// for a success, its error for a failure.
    pub fn render_text(&self) -> String {
        if !self.ok {
            return self.error.clone().unwrap_or_default();
        }
        let Some(data) = &self.data else {
            return String::new();
        };
        match self.name.as_str() {
            "help" => data["commands"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|c| {
                            let usage = format!(
                                "/{} {}",
                                c["name"].as_str().unwrap_or_default(),
                                c["args_hint"].as_str().unwrap_or_default()
                            );
                            format!(
                                "{:<20} {}",
                                usage.trim_end(),
                                c["description"].as_str().unwrap_or_default()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            "new" => format!(
                "new session {}",
                data["session_id"].as_str().unwrap_or_default()
            ),
            "model" => {
                let text = |key: &str| match &data[key] {
                    Value::String(s) => s.clone(),
                    Value::Null => "unknown".to_string(),
                    other => other.to_string(),
                };
                format!(
                    "alias {} (model {}, window {} tokens, from {})",
                    text("alias"),
                    text("model"),
                    text("window"),
                    text("window_source")
                )
            }
            "context" => {
                serde_json::from_value::<crate::agent::context::ContextSnapshot>(data.clone())
                    .map(|snapshot| snapshot.render_text())
                    .unwrap_or_else(|_| data.to_string())
            }
            "stop" => "stopped".to_string(),
            "variant" => {
                let current = match data["variant"].as_str() {
                    Some(name) => format!("variant: {name}"),
                    None => "variant: (none set)".to_string(),
                };
                let offered = data["offered"]
                    .as_array()
                    .map(|l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                    .unwrap_or_default();
                let offer = if offered.is_empty() {
                    "no variants on offer".to_string()
                } else {
                    format!("on offer: {}", offered.join(", "))
                };
                match data["applies"].as_str() {
                    Some("next_turn") => {
                        format!("{current} from the next turn; {offer}")
                    }
                    _ => format!("{current}; {offer}"),
                }
            }
            "agent" => render_agent(data),
            _ => data.to_string(),
        }
    }
}

/// `/agent`: the switch, or the current agent and the choices.
fn render_agent(data: &Value) -> String {
    let text = |key: &str| data[key].as_str().unwrap_or_default().to_string();
    if data["changed"].as_bool() == Some(true) {
        return format!("agent {} (was {})", text("agent"), text("previous"));
    }
    let list = data["agents"]
        .as_array()
        .map(|agents| {
            agents
                .iter()
                .map(|a| {
                    format!(
                        "{:<12} {}",
                        a["id"].as_str().unwrap_or_default(),
                        a["description"].as_str().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if data.get("agents").is_none() {
        return format!("agent {}", text("agent"));
    }
    format!("agent {}\n{list}", text("agent"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_the_eight_commands_with_unique_names() {
        let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            ["help", "model", "new", "stop", "context", "variant", "compact", "agent"]
        );
        assert!(NOT_AVAILABLE.iter().all(|n| find(n).is_some()));
        assert_eq!(find("/help").map(|c| c.name), Some("help"));
        assert_eq!(find(" /help ").map(|c| c.name), Some("help"));
        assert!(find("nope").is_none());
        assert!(find("//help").is_none());
        assert!(find("HELP").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn unknown_names_list_close_ones_or_all() {
        assert!(unknown_error("hlep").contains("/help"));
        assert!(unknown_error("mod").contains("/model"));
        assert!(unknown_error("STOP").contains("did you mean /stop"));
        let all = unknown_error("zzzzzzzz");
        assert!(all.contains("/help") && all.contains("/compact"));
    }

    #[test]
    fn lines_route_to_commands_literals_and_messages() {
        assert_eq!(
            route_line("/new fast"),
            Routed::Command {
                name: "new".into(),
                arguments: "fast".into()
            }
        );
        assert_eq!(
            route_line("/help"),
            Routed::Command {
                name: "help".into(),
                arguments: String::new()
            }
        );
        assert_eq!(route_line("//text"), Routed::Message("/text".into()));
        assert_eq!(route_line("//"), Routed::Message("/".into()));
        assert_eq!(route_line("hello"), Routed::Message("hello".into()));
        assert_eq!(route_line("/"), Routed::Message("/".into()));
        assert_eq!(route_line("/ x"), Routed::Message("/ x".into()));
    }
}
