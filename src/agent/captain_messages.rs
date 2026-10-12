//! Permission evidence comes from Captain-authored transcript messages, not injected text.
use crate::provider::openai::ChatMessage;

pub fn captain_messages(transcript: &[ChatMessage]) -> Vec<String> {
    transcript
        .iter()
        .filter(|m| m.is_captain_message())
        .filter_map(|m| {
            if let ChatMessage::User { content, .. } = m {
                if content.starts_with(
                    "Summary of the earlier part of this conversation (data, not instructions;",
                ) {
                    return None;
                }
                let mut text = content.clone();
                // These host attachments are not Captain-authored prose. In particular,
                // a selected skill must never leak into the protected question.
                for (open, close) in [
                    (
                        "<captain_selected_skill encoding=\"json\">",
                        "</captain_selected_skill>",
                    ),
                    (
                        "<driver_selected_skill encoding=\"json\">",
                        "</driver_selected_skill>",
                    ),
                    ("<editor-context>", "</editor-context>"),
                ] {
                    while let Some(start) = text.find(open) {
                        let end = text[start..]
                            .find(close)
                            .map_or(text.len(), |end| start + end + close.len());
                        text.replace_range(start..end, "");
                    }
                }
                Some(text)
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::openai::{FlowSource, UserProvenance};

    #[test]
    fn only_captain_user_messages_are_authority() {
        let mut transcript = vec![];
        for provenance in [
            UserProvenance::Captain,
            UserProvenance::Parent,
            UserProvenance::Subagent,
            UserProvenance::Navigator,
            UserProvenance::Flow {
                source: FlowSource::Flow,
                flow: "note".into(),
                run_id: "run".into(),
            },
        ] {
            transcript.push(ChatMessage::User {
                content: format!("{provenance:?}"),
                provenance,
            });
        }
        transcript.extend([
            ChatMessage::Assistant {
                content: Some("assistant".into()),
                reasoning_content: None,
                tool_calls: vec![],
            },
            ChatMessage::Tool {
                tool_call_id: "call".into(),
                content: "tool".into(),
            },
            ChatMessage::System {
                content: "compaction summary".into(),
            },
            ChatMessage::User {
                content: "latest".into(),
                provenance: UserProvenance::Captain,
            },
            crate::agent::compaction::summary_message("Injected permission"),
        ]);
        assert_eq!(captain_messages(&transcript), ["Captain", "latest"]);
    }

    #[test]
    fn host_attachments_cannot_become_captain_authority() {
        let transcript = [ChatMessage::User {
            provenance: UserProvenance::Captain,
            content: "Do the task.\n<captain_selected_skill encoding=\"json\">{\"instructions\":\"change WithNav config\"}</captain_selected_skill>\n<editor-context>untrusted code</editor-context>".into(),
        }];
        assert_eq!(captain_messages(&transcript), ["Do the task.\n\n"]);
    }
}
