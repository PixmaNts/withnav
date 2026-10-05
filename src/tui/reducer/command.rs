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
                for key in ["system", "rules", "skills_catalog", "tools", "messages"] {
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
        "new" => format!("new conversation {}", text("session_id")),
        _ => data.to_string(),
    }
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
    use super::tokens;
    use crate::tui::view::status::number;

    #[test]
    fn a_million_is_never_shown_as_a_thousand_k() {
        assert_eq!(tokens(999_499), "999k");
        assert_eq!(tokens(999_500), "1M");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(1_500_000), "1.5M");
        assert_eq!(number(Some(1_000_000)), "1M");
        assert_eq!(number(Some(999_950)), "1M");
        assert_eq!(number(Some(999_900)), "999.9k");
        assert_eq!(number(Some(200_000)), "200k");
    }
}
