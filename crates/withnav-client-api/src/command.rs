//! Command content as it appears on the wire, and the identities that count as the Captain.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Captain text and explicit skill selection. Other transcript block kinds
/// remain rejected at admission; only the server can supply loaded skills.
/// On the wire it carries a snake_case `type` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Validated metadata supplied by human ingress, never a model tool result.
    #[serde(skip)]
    AskAnswer {
        answer: crate::tools::AskAnswer,
    },
    /// Captain-selected skill, resolved by admission before any message is accepted.
    Skill {
        name: String,
    },
    /// Host-only selection marker: content is an opaque snapshot id, never the body.
    #[serde(skip)]
    LoadedSkill {
        name: String,
        content: String,
    },
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

/// Human command identities assigned by the ingress, not by model text.
pub fn is_captain_source(source: &str) -> bool {
    matches!(
        source,
        "captain" | "driver" | "user:http" | "user:cli" | "user:test"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_wire_uses_the_type_tag() {
        let cases = [
            (
                ContentBlock::Text { text: "hi".into() },
                serde_json::json!({"type": "text", "text": "hi"}),
            ),
            (
                ContentBlock::Thinking { text: "hm".into() },
                serde_json::json!({"type": "thinking", "text": "hm"}),
            ),
            (
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"cmd": "ls"}),
                },
                serde_json::json!({"type": "tool_use", "id": "t1", "name": "bash", "args": {"cmd": "ls"}}),
            ),
            (
                ContentBlock::ToolResult {
                    tool_call_id: "t1".into(),
                    content: "ok".into(),
                },
                serde_json::json!({"type": "tool_result", "tool_call_id": "t1", "content": "ok"}),
            ),
        ];
        for (block, wire) in cases {
            assert_eq!(serde_json::to_value(&block).unwrap(), wire);
            let back: ContentBlock = serde_json::from_value(wire).unwrap();
            assert_eq!(back, block);
        }
    }
}
