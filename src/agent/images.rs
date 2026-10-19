//! Which images a request carries, and where their bytes come from.
//!
//! The transcript holds image references only. Before each request the agent plans, from the
//! history and the model, which occurrences are sent as pictures and which become text; a
//! loader the session installs then fetches the bytes of the sent ones. The projection built here
//! is request-scoped: the transcript is never changed, so a later request (after a model switch,
//! say) decides again from the same references.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::provider::openai::{ChatMessage, ImagePayload, ImageRef};

/// Most image occurrences one request carries. Anthropic's vision documentation (checked
/// 2026-10-19) applies a stricter per-image dimension rule once a request holds more than 20
/// images; staying at 20 keeps every image under the looser rule. Local policy, not a provider
/// guarantee.
pub const MAX_REQUEST_IMAGES: usize = 20;

/// Most base64 bytes of images one request carries: 20 MiB. The same documentation gives a 32 MB
/// request body limit, so this leaves room for the text and the tool definitions. Local policy,
/// not a provider guarantee.
pub const MAX_REQUEST_IMAGE_BASE64: u64 = 20 * 1024 * 1024;

/// Fetches the bytes of stored images for one request. The session installs an implementation
/// bound to its own id, so a loader can only read what its session owns; the provider crate never
/// reads storage.
pub trait AttachmentLoader: Send + Sync {
    /// One entry per requested id, or none for an id the loader knows nothing about (the caller
    /// treats a missing entry as unavailable). A failure is a message for logs, never shown to
    /// the model.
    fn load(
        &self,
        ids: Vec<String>,
    ) -> BoxFuture<'static, HashMap<String, Result<ImagePayload, String>>>;
}

/// A loader handle as the actor holds it.
pub type SharedLoader = Arc<dyn AttachmentLoader>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Send,
    Placeholder(String),
}

/// What happens to every image occurrence of one history, computed by [`plan`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// One entry per history message, in order; empty for a message without images.
    messages: Vec<Vec<Verdict>>,
}

/// Base64 length of `size` raw bytes.
fn base64_len(size: u64) -> u64 {
    size.div_ceil(3) * 4
}

/// Decides, for each image of `history`, between sending it and a text placeholder.
///
/// A model that cannot see images gets a placeholder for every one. Otherwise the newest
/// [`MAX_REQUEST_IMAGES`] occurrences within [`MAX_REQUEST_IMAGE_BASE64`] are sent, counting an
/// image once per occurrence because each occurrence is serialized again. The cut falls on the
/// first occurrence, newest first, that does not fit, and everything older is cut with it: the
/// sent set only changes at its oldest end, so the request prefix, and the provider's prompt
/// cache with it, stays the same between cuts.
pub fn plan(history: &[ChatMessage], accepts_images: bool) -> Plan {
    let mut messages: Vec<Vec<Verdict>> = vec![Vec::new(); history.len()];
    let mut sent = 0usize;
    let mut base64 = 0u64;
    let mut cut = false;
    for (at, message) in history.iter().enumerate().rev() {
        let ChatMessage::User { images, .. } = message else {
            continue;
        };
        let mut verdicts = vec![Verdict::Send; images.len()];
        for (slot, image) in images.iter().enumerate().rev() {
            verdicts[slot] = if !accepts_images {
                Verdict::Placeholder(format!(
                    "[image {}: this model cannot see images]",
                    image.label()
                ))
            } else {
                let next = base64 + base64_len(image.size);
                if cut || sent + 1 > MAX_REQUEST_IMAGES || next > MAX_REQUEST_IMAGE_BASE64 {
                    cut = true;
                    Verdict::Placeholder(format!(
                        "[image {} not included in this request (size limit); ask the Captain to attach it again if needed]",
                        image.label()
                    ))
                } else {
                    sent += 1;
                    base64 = next;
                    Verdict::Send
                }
            };
        }
        messages[at] = verdicts;
    }
    Plan { messages }
}

impl Plan {
    /// The distinct attachment ids whose bytes the request needs, in transcript order.
    pub fn send_ids(&self, history: &[ChatMessage]) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        for (message, verdicts) in history.iter().zip(&self.messages) {
            if let ChatMessage::User { images, .. } = message {
                for (image, verdict) in images.iter().zip(verdicts) {
                    if *verdict == Verdict::Send && seen.insert(image.attachment_id.clone()) {
                        ids.push(image.attachment_id.clone());
                    }
                }
            }
        }
        ids
    }

    /// Whether any image is sent as a picture.
    pub fn sends_any(&self) -> bool {
        self.messages.iter().flatten().any(|v| *v == Verdict::Send)
    }

    /// The history as the request sees it: each unsent image removed from `images` and its
    /// placeholder appended to the message text, one per line. `history` is the slice the plan was
    /// computed from.
    pub fn apply(&self, history: &[ChatMessage]) -> Vec<ChatMessage> {
        self.apply_excluding(history, &HashSet::new())
    }

    /// [`Plan::apply`] where the images in `unavailable` (their bytes could not be loaded)
    /// become `[image <label> unavailable]` instead of being sent.
    pub fn apply_excluding(
        &self,
        history: &[ChatMessage],
        unavailable: &HashSet<String>,
    ) -> Vec<ChatMessage> {
        history
            .iter()
            .enumerate()
            .map(|(at, message)| {
                let verdicts = self.messages.get(at).map(Vec::as_slice).unwrap_or(&[]);
                match message {
                    ChatMessage::User {
                        content,
                        provenance,
                        images,
                    } if !images.is_empty() && verdicts.len() == images.len() => {
                        let mut text = content.clone();
                        let mut kept: Vec<ImageRef> = Vec::new();
                        for (image, verdict) in images.iter().zip(verdicts) {
                            let placeholder = match verdict {
                                Verdict::Send if unavailable.contains(&image.attachment_id) => {
                                    Some(format!("[image {} unavailable]", image.label()))
                                }
                                Verdict::Send => {
                                    kept.push(image.clone());
                                    None
                                }
                                Verdict::Placeholder(text) => Some(text.clone()),
                            };
                            if let Some(placeholder) = placeholder {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(&placeholder);
                            }
                        }
                        ChatMessage::User {
                            content: text,
                            provenance: provenance.clone(),
                            images: kept,
                        }
                    }
                    other => other.clone(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::openai::UserProvenance;

    fn image(id: &str, size: u64) -> ImageRef {
        ImageRef {
            attachment_id: id.into(),
            media_type: "image/png".into(),
            name: format!("{id}.png"),
            size,
            width: 10,
            height: 10,
        }
    }

    fn user(text: &str, images: Vec<ImageRef>) -> ChatMessage {
        ChatMessage::User {
            content: text.into(),
            provenance: UserProvenance::Captain,
            images,
        }
    }

    fn sent_ids(history: &[ChatMessage]) -> Vec<String> {
        history
            .iter()
            .flat_map(|m| match m {
                ChatMessage::User { images, .. } => {
                    images.iter().map(|i| i.attachment_id.clone()).collect()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn a_model_without_images_gets_a_placeholder_and_keeps_the_text() {
        let history = vec![user("look", vec![image("a", 1500)])];
        let projected = plan(&history, false).apply(&history);
        let ChatMessage::User {
            content, images, ..
        } = &projected[0]
        else {
            panic!("user message")
        };
        assert_eq!(
            content,
            "look\n[image a.png, image/png, 2 KB: this model cannot see images]"
        );
        assert!(images.is_empty());
        // The transcript itself is untouched.
        assert!(matches!(&history[0], ChatMessage::User { images, .. } if images.len() == 1));
    }

    #[test]
    fn an_image_only_message_becomes_just_the_placeholder() {
        let history = vec![user("", vec![image("a", 10)])];
        let projected = plan(&history, false).apply(&history);
        assert!(matches!(&projected[0], ChatMessage::User { content, .. }
            if content == "[image a.png, image/png, 10 B: this model cannot see images]"));
    }

    #[test]
    fn the_newest_twenty_occurrences_are_sent_and_older_ones_become_text() {
        let history: Vec<ChatMessage> = (0..23)
            .map(|n| user("", vec![image(&format!("i{n}"), 3)]))
            .collect();
        let plan = plan(&history, true);
        let projected = plan.apply(&history);
        let sent = sent_ids(&projected);
        assert_eq!(sent.len(), MAX_REQUEST_IMAGES);
        assert_eq!(sent.first().unwrap(), "i3");
        assert_eq!(sent.last().unwrap(), "i22");
        assert!(matches!(&projected[0], ChatMessage::User { content, .. }
            if content.contains("not included in this request (size limit)")));
        assert_eq!(plan.send_ids(&history).len(), MAX_REQUEST_IMAGES);
    }

    #[test]
    fn one_message_with_many_images_counts_every_occurrence() {
        let history = vec![user(
            "",
            (0..25).map(|n| image(&format!("i{n}"), 3)).collect(),
        )];
        let projected = plan(&history, true).apply(&history);
        let sent = sent_ids(&projected);
        assert_eq!(sent.len(), MAX_REQUEST_IMAGES);
        // Within a message the newest (last) images are the ones kept.
        assert_eq!(sent.first().unwrap(), "i5");
    }

    #[test]
    fn the_same_attachment_twice_counts_twice_but_loads_once() {
        let history = vec![user("", vec![image("a", 3)]), user("", vec![image("a", 3)])];
        let plan = plan(&history, true);
        assert_eq!(sent_ids(&plan.apply(&history)).len(), 2);
        assert_eq!(plan.send_ids(&history), vec!["a".to_string()]);
    }

    #[test]
    fn the_byte_budget_cuts_from_the_oldest_and_never_skips_back_to_a_smaller_one() {
        // Raw 7.5 MiB encodes to 10 MiB: two fit in 20 MiB, a third does not.
        let big = 7 * 1024 * 1024 + 512 * 1024;
        assert_eq!(base64_len(big), 10 * 1024 * 1024);
        let history = vec![
            user("", vec![image("tiny-old", 3)]),
            user("", vec![image("big1", big)]),
            user("", vec![image("big2", big)]),
            user("", vec![image("big3", big)]),
        ];
        let plan = plan(&history, true);
        let sent = sent_ids(&plan.apply(&history));
        // big3 and big2 fit (20 MiB exactly); big1 does not, and the tiny one older than it stays
        // cut so the sent set only moves at its oldest end.
        assert_eq!(sent, vec!["big2", "big3"]);
    }

    #[test]
    fn unavailable_images_become_placeholders_and_the_rest_are_kept() {
        let history = vec![user("hi", vec![image("a", 5), image("b", 5)])];
        let plan = plan(&history, true);
        let projected = plan.apply_excluding(&history, &HashSet::from(["a".to_string()]));
        let ChatMessage::User {
            content, images, ..
        } = &projected[0]
        else {
            panic!("user message")
        };
        assert_eq!(content, "hi\n[image a.png, image/png, 5 B unavailable]");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].attachment_id, "b");
    }

    #[test]
    fn messages_without_images_are_returned_unchanged() {
        let history = vec![
            ChatMessage::System {
                content: "s".into(),
            },
            user("plain", vec![]),
        ];
        let plan = plan(&history, false);
        assert!(!plan.sends_any());
        let projected = plan.apply(&history);
        assert_eq!(
            serde_json::to_value(&projected).unwrap(),
            serde_json::to_value(&history).unwrap()
        );
    }
}
