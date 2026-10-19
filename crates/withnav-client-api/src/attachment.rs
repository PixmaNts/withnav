//! Images attached to a conversation: the reference that travels in messages, events and the
//! transcript. The bytes never travel with it; they live in the server's attachment store and are
//! read back by `(session, attachment_id)` only when a request to a model needs them.

use serde::{Deserialize, Serialize};

/// One stored image, by reference. A client sends only `attachment_id`; the server fills the rest
/// from the stored row, so a client cannot misstate the type or size of what it uploaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub attachment_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub media_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Raw size in bytes (not base64).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    #[serde(default, skip_serializing_if = "is_zero32")]
    pub width: u32,
    #[serde(default, skip_serializing_if = "is_zero32")]
    pub height: u32,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}
fn is_zero32(value: &u32) -> bool {
    *value == 0
}

impl ImageRef {
    /// `screenshot.png, image/png, 120 KB`: how an image is named in placeholders and chips.
    pub fn label(&self) -> String {
        format!(
            "{}, {}, {}",
            self.name,
            self.media_type,
            human_size(self.size)
        )
    }
}

/// `812 B`, `120 KB`, `3.4 MB` (decimal units, as file managers show them).
pub fn human_size(bytes: u64) -> String {
    if bytes < 1000 {
        format!("{bytes} B")
    } else if bytes < 1_000_000 {
        format!("{} KB", bytes.div_ceil(1000))
    } else {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_reference_needs_only_the_id() {
        let image: ImageRef =
            serde_json::from_value(serde_json::json!({"attachment_id": "a1"})).unwrap();
        assert_eq!(image.attachment_id, "a1");
        assert_eq!(
            serde_json::to_value(&image).unwrap(),
            serde_json::json!({"attachment_id": "a1"})
        );
    }

    #[test]
    fn sizes_read_like_a_file_manager() {
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(120_000), "120 KB");
        assert_eq!(human_size(3_400_000), "3.4 MB");
    }
}
