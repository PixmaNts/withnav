//! Server command outcomes as the short text the transcript shows. The client's own wording: it
//! reads the `command.result` data, never the server's code.
use serde_json::Value;

/// The text for one outcome: the error when it failed, else a line or a few per command.
pub fn outcome_text(name: &str, ok: bool, data: Option<&Value>, error: Option<&str>) -> String {
    if !ok {
        return error.unwrap_or("the command failed").to_string();
    }
    let Some(data) = data else {
        return "done".into();
    };
    let text = |key: &str| data[key].as_str().unwrap_or_default().to_string();
    match name {
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
                            "{:<18} {}",
                            usage.trim_end(),
                            c["description"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "model" => {
            let window = match &data["window"] {
                Value::Number(n) => format!("{n} tokens"),
                _ => "unknown".into(),
            };
            format!(
                "{} · {} · window {window}",
                text("model"),
                data["model"].as_str().unwrap_or("unknown")
            )
        }
        "context" => match (
            data["estimate_total"].as_u64(),
            data["window"].as_u64(),
            data["percent"].as_u64(),
        ) {
            (Some(total), Some(window), Some(percent)) => {
                let mut out = format!("~{} / {} · {percent}%", tokens(total), tokens(window));
                if let Some(budget) = data["input_budget"].as_u64() {
                    out.push_str(&format!(" · input budget {}", tokens(budget)));
                }
                let b = &data["breakdown"];
                for key in [
                    "system",
                    "rules",
                    "skills_catalog",
                    "tools",
                    "messages",
                    "goal",
                ] {
                    if let Some(n) = b[key].as_u64() {
                        out.push_str(&format!("\n  {:<15} ~{}", key.replace('_', " "), tokens(n)));
                    }
                }
                out
            }
            _ => data.to_string(),
        },
        "variant" => {
            let current = data["variant"].as_str().unwrap_or("off");
            let offered: Vec<&str> = data["offered"]
                .as_array()
                .map(|l| l.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let offer = if offered.is_empty() {
                "this model has no variants".to_string()
            } else {
                format!("on offer: {}", offered.join(", "))
            };
            match data["applies"].as_str() {
                Some("next_turn") => format!("variant {current}, applies next turn; {offer}"),
                _ => format!("variant {current}; {offer}"),
            }
        }
        "agent" => {
            if data["changed"].as_bool() == Some(true) {
                return format!("agent {} (was {})", text("agent"), text("previous"));
            }
            let others: Vec<String> = data["agents"]
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
                        .collect()
                })
                .unwrap_or_default();
            if others.is_empty() {
                format!("agent {}", text("agent"))
            } else {
                format!("agent {}\n{}", text("agent"), others.join("\n"))
            }
        }
        "stop" => "stopped".into(),
        "mcp_tools" => {
            let names = |key: &str| -> String {
                let listed: Vec<&str> = data[key]
                    .as_array()
                    .map(|l| l.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let more = data[format!("{key}_more")].as_u64().unwrap_or(0);
                match (listed.is_empty(), more) {
                    (true, _) => String::new(),
                    (false, 0) => listed.join(", "),
                    (false, n) => format!("{}, and {n} more", listed.join(", ")),
                }
            };
            ["added", "removed", "warnings"]
                .iter()
                .filter_map(|key| {
                    let listed = names(key);
                    (!listed.is_empty()).then(|| format!("MCP tools {key}: {listed}"))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "new" => format!("new conversation {}", text("session_id")),
        "goal" => goal_text(data),
        "remember" | "forget" | "memory" => {
            if name == "remember"
                && let Some(line) = remembered_line(data)
            {
                return line;
            }
            let item = &data["item"];
            if let Some(title) = item["current"]["title"].as_str() {
                let label = match name {
                    "remember" => "Remembered",
                    "forget" => "Forgot",
                    _ => "Memory",
                };
                format!(
                    "{label} \"{title}\" · {} · {} v{}{}",
                    data["scope_label"]
                        .as_str()
                        .or_else(|| item["scope"].as_str())
                        .unwrap_or_default(),
                    item["id"].as_str().unwrap_or_default(),
                    item["current_version"],
                    data["notice"]
                        .as_str()
                        .map_or_else(String::new, |notice| format!("\n{notice}"))
                )
            } else if let Some(items) = data["items"].as_array() {
                if items.is_empty() {
                    "No memory items.".into()
                } else {
                    items
                        .iter()
                        .map(|item| {
                            format!(
                                "{} · {} v{}",
                                item["current"]["title"].as_str().unwrap_or_default(),
                                item["id"].as_str().unwrap_or_default(),
                                item["current_version"]
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            } else if let Some(versions) = data["versions"].as_array() {
                versions
                    .iter()
                    .map(|v| {
                        format!(
                            "v{} · {} · {}",
                            v["version"],
                            v["change"].as_str().unwrap_or_default(),
                            v["title"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                data.to_string()
            }
        }
        _ => data.to_string(),
    }
}

/// The transcript line for an item `/remember` stored: its title and scope, and how to take it
/// back (`/forget <id>` invalidates the current version). `None` when the result holds no item.
pub fn remembered_line(data: &Value) -> Option<String> {
    let item = &data["item"];
    let title = item["current"]["title"].as_str()?;
    let scope = data["scope_label"]
        .as_str()
        .or_else(|| item["scope"].as_str())
        .unwrap_or_default();
    let mut line = format!(
        "Remembered: \"{title}\" · {scope} · undo with /forget {}",
        item["id"].as_str().unwrap_or_default()
    );
    if let Some(notice) = data["notice"].as_str() {
        line.push('\n');
        line.push_str(notice);
    }
    Some(line)
}

/// The transcript block for a finished goal check that queued a note for the agent: said to be an
/// automatic observation so it is never read as something the Captain wrote. `None` when the
/// check queued no note (its statuses reach the panel through the ledger instead).
pub fn goal_note_line(data: &Value) -> Option<String> {
    if data["note_sent"].as_bool() != Some(true) {
        return None;
    }
    let note = data["note"].as_str().filter(|note| !note.is_empty())?;
    Some(format!(
        "Goal note · automatic observation, not a Captain message\n{note}"
    ))
}

/// `/goal`: the goal (status, revision, objective, numbered criteria), or what ended.
fn goal_text(data: &Value) -> String {
    let goal = &data["goal"];
    let Some(objective) = goal["objective"].as_str() else {
        return match data["revision"].as_u64() {
            Some(revision) => format!("goal cleared (revision {revision})"),
            None => "no goal is set".into(),
        };
    };
    let line = |text: &str| {
        crate::strip_controls(text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut out = format!(
        "goal {} · revision {}\n{}",
        goal["status"].as_str().unwrap_or("active"),
        goal["revision"].as_u64().unwrap_or_default(),
        line(objective)
    );
    let criteria: Vec<&str> = goal["criteria"]
        .as_array()
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if criteria.is_empty() {
        out.push_str("\nno success criteria yet");
    }
    for (n, criterion) in criteria.iter().enumerate() {
        out.push_str(&format!("\n{}. {}", n + 1, line(criterion)));
    }
    out
}

/// `842`, `42k`, `1M`, `1.5M`: rounded for a one-line summary.
pub fn tokens(n: u64) -> String {
    // 999_500 and up round to 1000k: show them as 1M.
    if n >= 999_500 {
        let tenths = (n + 50_000) / 100_000;
        return if tenths.is_multiple_of(10) {
            format!("{}M", tenths / 10)
        } else {
            format!("{}.{}M", tenths / 10, tenths % 10)
        };
    }
    if n >= 1000 {
        return format!("{}k", (n + 500) / 1000);
    }
    n.to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn memory_outcomes_show_scope_and_format_history() {
        let body = serde_json::json!({"scope_label":"project WithNav","item":{"id":"abc123","current_version":1,"current":{"title":"Checklist"}}});
        assert_eq!(
            super::outcome_text("remember", true, Some(&body), None),
            "Remembered: \"Checklist\" · project WithNav · undo with /forget abc123"
        );
        assert_eq!(
            super::outcome_text("forget", true, Some(&body), None),
            "Forgot \"Checklist\" · project WithNav · abc123 v1"
        );
        let history =
            serde_json::json!({"versions":[{"version":1,"change":"created","title":"Checklist"}]});
        assert_eq!(
            super::outcome_text("memory", true, Some(&history), None),
            "v1 · created · Checklist"
        );
    }
    use super::{outcome_text, tokens};

    #[test]
    fn an_mcp_tools_note_lists_what_changed_and_counts_the_rest() {
        let data = serde_json::json!({
            "added": ["mcp__a__x"], "removed": ["mcp__a__y", "mcp__a__z"], "removed_more": 3,
            "warnings": ["mcp__b__x from b is not installed: a already holds the name"],
        });
        assert_eq!(
            outcome_text("mcp_tools", true, Some(&data), None),
            "MCP tools added: mcp__a__x\nMCP tools removed: mcp__a__y, mcp__a__z, and 3 more\n\
             MCP tools warnings: mcp__b__x from b is not installed: a already holds the name"
        );
    }

    #[test]
    fn a_goal_outcome_shows_the_goal_with_numbered_criteria_or_says_what_ended() {
        let data = serde_json::json!({"goal": {"revision": 3, "objective": "ship \u{1b}[2Jit",
            "criteria": ["tests pass", "docs"], "status": "paused"}, "change": "paused"});
        assert_eq!(
            outcome_text("goal", true, Some(&data), None),
            "goal paused · revision 3\nship it\n1. tests pass\n2. docs"
        );
        let set = serde_json::json!({"goal": {"revision": 1, "objective": "ship",
            "criteria": [], "status": "active"}, "change": "set"});
        assert_eq!(
            outcome_text("goal", true, Some(&set), None),
            "goal active · revision 1\nship\nno success criteria yet"
        );
        let none = serde_json::json!({"goal": null});
        assert_eq!(
            outcome_text("goal", true, Some(&none), None),
            "no goal is set"
        );
        let cleared = serde_json::json!({"goal": null, "change": "cleared", "revision": 4});
        assert_eq!(
            outcome_text("goal", true, Some(&cleared), None),
            "goal cleared (revision 4)"
        );
    }

    #[test]
    fn the_context_outcome_lists_the_goal_only_when_requests_carry_it() {
        let data = serde_json::json!({"estimate_total": 1300, "window": 100000, "percent": 1,
            "breakdown": {"system": 1000, "rules": 0, "skills_catalog": 0, "tools": 0,
            "messages": 0, "goal": 300}});
        assert!(
            outcome_text("context", true, Some(&data), None).ends_with("\n  goal            ~300")
        );
        let mut without = data.clone();
        without["breakdown"].as_object_mut().unwrap().remove("goal");
        assert!(!outcome_text("context", true, Some(&without), None).contains("goal"));
    }

    #[test]
    fn a_million_is_never_shown_as_a_thousand_k() {
        assert_eq!(tokens(999_499), "999k");
        assert_eq!(tokens(999_500), "1M");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(1_500_000), "1.5M");
    }
}
