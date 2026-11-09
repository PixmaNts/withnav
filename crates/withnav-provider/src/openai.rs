use std::time::Duration;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use withnav_client_api::attachment::{ImageRef, human_size};

use crate::Usage;
use crate::config::{ProviderConfig, RESERVED_VARIANT_KEYS, Variant};
use crate::error::ProviderError;
pub use crate::opaque::{OpaqueItem, OpaqueReasoning, Producer};

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
    pub stream: bool,
    pub stream_options: Option<StreamOptions>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f32>,
    /// The selected variant's override object, merged shallowly into the
    /// body. Empty (the default) adds nothing to the wire bytes.
    pub extra: serde_json::Map<String, Value>,
    /// Bytes of the images this request sends, by attachment id. Request-scoped: filled by the
    /// agent just before the call and dropped with it, never stored in the transcript.
    pub images: HashMap<String, ImagePayload>,
}

/// The body that goes on the wire. `ChatRequest` is not `Serialize` itself because the message
/// list needs the image payload map that sits beside it, which a per-field serializer cannot see.
#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: WireMessages<'a>,
    #[serde(skip_serializing_if = "<[ToolDef]>::is_empty")]
    tools: &'a [ToolDef],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: &'a Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    extra: &'a serde_json::Map<String, Value>,
}

impl Serialize for ChatRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireRequest {
            model: &self.model,
            messages: WireMessages {
                messages: &self.messages,
                images: &self.images,
            },
            tools: &self.tools,
            stream: self.stream,
            stream_options: &self.stream_options,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            extra: &self.extra,
        }
        .serialize(serializer)
    }
}

/// An image's bytes for one request, base64 encoded. Deliberately not `Serialize`/`Deserialize`
/// and with a `Debug` that prints only the length: a payload is megabytes, and one that reached a
/// log, an event or a stored transcript would carry the Captain's screenshot with it.
#[derive(Clone)]
pub struct ImagePayload {
    media_type: String,
    base64: String,
}

/// The image types a request may carry: the four the attachment store's header check produces.
/// Anything else would be refused by the provider, after the upload and the request were paid for.
const SUPPORTED_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

impl ImagePayload {
    /// Refuses a media type outside [`SUPPORTED_IMAGE_TYPES`], so a bad type fails here, as a
    /// placeholder for that image, rather than as a provider error for the whole request.
    pub fn from_bytes(media_type: impl Into<String>, bytes: &[u8]) -> Result<Self, String> {
        use base64::Engine as _;
        let media_type = media_type.into();
        if !SUPPORTED_IMAGE_TYPES.contains(&media_type.as_str()) {
            return Err(format!("unsupported image type {media_type:?}"));
        }
        Ok(Self {
            media_type,
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// The length of the encoded payload, for logs that must not carry the bytes.
    pub fn base64_len(&self) -> usize {
        self.base64.len()
    }

    fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.base64)
    }
}

impl std::fmt::Debug for ImagePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ImagePayload({}, {} base64 chars)",
            self.media_type,
            self.base64.len()
        )
    }
}

impl ChatRequest {
    /// Merges `variant` into the body. A key the request owns is skipped, so
    /// no key is ever written twice (config loading rejects such variants).
    pub fn apply_variant(&mut self, variant: &Variant) {
        for (k, v) in variant {
            if !RESERVED_VARIANT_KEYS.contains(&k.as_str()) {
                self.extra.insert(k.clone(), v.clone());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
        #[serde(default, skip_serializing_if = "UserProvenance::is_captain")]
        provenance: UserProvenance,
        /// Images attached to this message, by reference. Metadata only: the bytes stay in the
        /// attachment store and are added to a request when the model can see them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        tool_calls: Vec<RequestToolCall>,
        /// Reasoning only its producer can read back. Kept in the stored
        /// transcript, never written by the Chat Completions serializer.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::opaque::lenient"
        )]
        opaque: Option<OpaqueReasoning>,
    },
    Tool {
        tool_call_id: String,
        content: String,
        /// Who answered an `ask` call with this text; absent for every other
        /// tool result. Kept in the stored transcript, never sent to the model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answered_by: Option<UserProvenance>,
        /// Images the tool returned (a picture file, a screenshot), by reference. Tool output,
        /// never the Captain's: on the chat-completions wire they follow the batch's tool
        /// messages in one extra user message that exists only in the request.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserProvenance {
    #[default]
    #[serde(alias = "driver")]
    Captain,
    Navigator,
    Parent,
    Subagent,
    #[serde(untagged)]
    Flow {
        source: FlowSource,
        flow: String,
        run_id: String,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowSource {
    Flow,
}
impl UserProvenance {
    fn is_captain(&self) -> bool {
        *self == Self::Captain
    }
}
impl ChatMessage {
    pub fn is_captain_message(&self) -> bool {
        matches!(
            self,
            Self::User {
                provenance: UserProvenance::Captain,
                ..
            }
        )
    }
}

struct WireMessages<'a> {
    messages: &'a [ChatMessage],
    images: &'a HashMap<String, ImagePayload>,
}

impl Serialize for WireMessages<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(None)?;
        // Chat completions has no image part in a `tool` message. The images of one batch of
        // tool results follow its last tool message, in one user message that exists only in
        // this request: the transcript keeps them on the tool messages, as tool output.
        let mut batch: Vec<&ImageRef> = Vec::new();
        for (at, message) in self.messages.iter().enumerate() {
            sequence.serialize_element(&Wire {
                message,
                images: self.images,
            })?;
            if let ChatMessage::Tool { images, .. } = message {
                batch.extend(images.iter());
                let batch_ends =
                    !matches!(self.messages.get(at + 1), Some(ChatMessage::Tool { .. }));
                if batch_ends && !batch.is_empty() {
                    sequence.serialize_element(&ToolImagesMessage {
                        images: &batch,
                        payloads: self.images,
                    })?;
                    batch.clear();
                }
            }
        }
        sequence.end()
    }
}

/// The request-only user message that carries the images of a batch of tool results.
struct ToolImagesMessage<'a> {
    images: &'a [&'a ImageRef],
    payloads: &'a HashMap<String, ImagePayload>,
}

/// The text that opens a [`ToolImagesMessage`]: it says whose images these are, so the model
/// never reads a tool's screenshot as something the user sent.
pub const TOOL_IMAGES_PREFACE: &str =
    "Images returned by the tool calls above (tool output, not a message from the user):";

impl Serialize for ToolImagesMessage<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeSeq, SerializeStruct};
        struct Parts<'a>(&'a ToolImagesMessage<'a>);
        impl Serialize for Parts<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut parts = serializer.serialize_seq(None)?;
                parts.serialize_element(
                    &serde_json::json!({"type": "text", "text": TOOL_IMAGES_PREFACE}),
                )?;
                for image in self.0.images {
                    parts.serialize_element(&serde_json::json!({
                        "type": "text", "text": format!("[image {}]", image.label()),
                    }))?;
                    parts.serialize_element(&image_part(image, self.0.payloads))?;
                }
                parts.end()
            }
        }
        let mut object = serializer.serialize_struct("ChatMessage", 2)?;
        object.serialize_field("role", "user")?;
        object.serialize_field("content", &Parts(self))?;
        object.end()
    }
}

/// One image as a content part: its data URL, or a visible placeholder when its bytes are
/// missing. The agent never sends a reference it has no bytes for; if one slips through, a
/// placeholder is better than a request the provider rejects.
fn image_part(image: &ImageRef, payloads: &HashMap<String, ImagePayload>) -> Value {
    match payloads.get(&image.attachment_id) {
        Some(payload) => serde_json::json!({
            "type": "image_url",
            "image_url": {"url": payload.data_url()},
        }),
        None => serde_json::json!({
            "type": "text",
            "text": format!("[image {} unavailable]", image.label()),
        }),
    }
}

/// One message as the provider sees it: provenance and `answered_by` stripped. A user message
/// with images carries `content` as an array of parts; one without keeps the plain string, so
/// every request without images is byte-identical to what it was before images existed.
struct Wire<'a> {
    message: &'a ChatMessage,
    images: &'a HashMap<String, ImagePayload>,
}

impl Serialize for Wire<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self.message {
            ChatMessage::User {
                content, images, ..
            } if !images.is_empty() => {
                let mut object = serializer.serialize_struct("ChatMessage", 2)?;
                object.serialize_field("role", "user")?;
                object.serialize_field(
                    "content",
                    &UserParts {
                        text: content,
                        images,
                        payloads: self.images,
                    },
                )?;
                object.end()
            }
            ChatMessage::User { content, .. } => {
                let mut object = serializer.serialize_struct("ChatMessage", 2)?;
                object.serialize_field("role", "user")?;
                object.serialize_field("content", content)?;
                object.end()
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
                images,
                ..
            } => {
                let mut object = serializer.serialize_struct("ChatMessage", 3)?;
                object.serialize_field("role", "tool")?;
                object.serialize_field("tool_call_id", tool_call_id)?;
                if images.is_empty() {
                    object.serialize_field("content", content)?;
                } else {
                    let mut text = content.clone();
                    for image in images {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!(
                            "[image {}: attached in the next message]",
                            image.label()
                        ));
                    }
                    object.serialize_field("content", &text)?;
                }
                object.end()
            }
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                // Same keys, same order as the derived form; `opaque` is stored
                // in the transcript but is never part of this wire body.
                let mut object = serializer.serialize_struct("ChatMessage", 4)?;
                object.serialize_field("role", "assistant")?;
                if let Some(content) = content {
                    object.serialize_field("content", content)?;
                }
                if let Some(reasoning_content) = reasoning_content {
                    object.serialize_field("reasoning_content", reasoning_content)?;
                }
                if !tool_calls.is_empty() {
                    object.serialize_field("tool_calls", tool_calls)?;
                }
                object.end()
            }
            other => other.serialize(serializer),
        }
    }
}

struct UserParts<'a> {
    text: &'a str,
    images: &'a [ImageRef],
    payloads: &'a HashMap<String, ImagePayload>,
}

impl Serialize for UserParts<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut parts = serializer.serialize_seq(None)?;
        if !self.text.is_empty() {
            parts.serialize_element(&serde_json::json!({"type": "text", "text": self.text}))?;
        }
        for image in self.images {
            parts.serialize_element(&image_part(image, self.payloads))?;
        }
        parts.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: RequestFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDef {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default, deserialize_with = "null_to_default")]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub error: Option<ApiErrorBody>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: ChunkDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub tool_calls: Vec<ToolCallDelta>,
}

// OpenAI-compatible gateways emit null for absent streaming lists (observed
// 2026-10); both mean no entries, while other non-array values remain errors.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

pub fn build_client(provider: &ProviderConfig) -> Result<reqwest::Client, ProviderError> {
    build_client_with(provider, reqwest::redirect::Policy::default())
}

/// A client that follows only same-origin redirects (same scheme, host and
/// port, at most three), so credentials and custom headers never reach
/// another origin.
fn build_same_origin_client(provider: &ProviderConfig) -> Result<reqwest::Client, ProviderError> {
    build_client_with(
        provider,
        reqwest::redirect::Policy::custom(|attempt| {
            let same = attempt
                .previous()
                .last()
                .is_some_and(|p| p.origin() == attempt.url().origin());
            if same && attempt.previous().len() <= 3 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }),
    )
}

fn build_client_with(
    provider: &ProviderConfig,
    redirect: reqwest::redirect::Policy,
) -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .redirect(redirect)
        .connect_timeout(Duration::from_millis(provider.connect_timeout_ms))
        // Total deadline covering send and body: the configured
        // request_timeout_ms must actually be applied to the client.
        .timeout(Duration::from_millis(provider.request_timeout_ms))
        .no_proxy()
        .build()
        .map_err(ProviderError::Http)
}

/// A per-provider HTTP client cache: one `reqwest::Client` built once per
/// provider and reused, instead of rebuilding a client (and its connection
/// pool) on every call.
///
/// The key includes the timeouts, not only the provider name: layers sharing
/// one provider apply different deadlines (observed 2026-09-29: the instinct's
/// 2 s client was reused by the metacognition on the same `omniroute`
/// provider, cutting every remark at exactly 2000 ms).
#[derive(Default)]
pub struct ClientCache {
    clients: std::sync::Mutex<std::collections::HashMap<(String, u64, u64), reqwest::Client>>,
    same_origin: std::sync::Mutex<std::collections::HashMap<(String, u64, u64), reqwest::Client>>,
}

impl ClientCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the shared client for the named provider, building it on
    /// first use.
    pub fn get(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
    ) -> Result<reqwest::Client, ProviderError> {
        // The lock is only held for a map lookup after the first build, so
        // a Mutex stays cheap and the fast path needs no extra allocation.
        let key = (
            provider_name.to_string(),
            provider.connect_timeout_ms,
            provider.request_timeout_ms,
        );
        let mut clients = self.clients.lock().expect("client cache poisoned");
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let built = build_client(provider)?;
        clients.insert(key, built.clone());
        Ok(built)
    }

    /// [`Self::get`] for a client that never follows a redirect to another
    /// origin (see [`build_same_origin_client`]).
    pub fn get_same_origin(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
    ) -> Result<reqwest::Client, ProviderError> {
        let key = (
            provider_name.to_string(),
            provider.connect_timeout_ms,
            provider.request_timeout_ms,
        );
        let mut clients = self.same_origin.lock().expect("client cache poisoned");
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let built = build_same_origin_client(provider)?;
        clients.insert(key, built.clone());
        Ok(built)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_TRANSCRIPT: &str = concat!(
        "[",
        r#"{"role":"system","content":"be brief"},"#,
        r#"{"role":"user","content":"from the navigator","provenance":"navigator"},"#,
        r#"{"role":"user","content":"from a flow","provenance":{"source":"flow","flow":"nightly","run_id":"r1"}},"#,
        r#"{"role":"assistant","content":"on it","reasoning_content":"think","tool_calls":["#,
        r#"{"id":"c1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}},"#,
        r#"{"id":"c2","type":"function","function":{"name":"ask","arguments":"{}"}}]},"#,
        r#"{"role":"tool","tool_call_id":"c1","content":"file text"},"#,
        r#"{"role":"tool","tool_call_id":"c2","content":"yes","answered_by":"navigator"},"#,
        r#"{"role":"assistant","tool_calls":[{"id":"c3","type":"function","function":{"name":"read","arguments":"{}"}}]}"#,
        "]"
    );

    fn opaque_reasoning(data: &[&str]) -> OpaqueReasoning {
        OpaqueReasoning {
            producer: Producer {
                wire: "anthropic".into(),
                endpoint: "https://gw.example/v1".into(),
                model: "claude-x".into(),
            },
            items: data
                .iter()
                .map(|d| OpaqueItem {
                    kind: "thinking".into(),
                    data: (*d).into(),
                })
                .collect(),
        }
    }

    fn assistant(opaque: Option<OpaqueReasoning>) -> ChatMessage {
        ChatMessage::Assistant {
            content: Some("answer".into()),
            reasoning_content: Some("thoughts".into()),
            tool_calls: vec![RequestToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: RequestFunction {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
            opaque,
        }
    }

    #[test]
    fn a_transcript_written_before_opaque_reasoning_reads_and_writes_back_unchanged() {
        let transcript: Vec<ChatMessage> = serde_json::from_str(LEGACY_TRANSCRIPT).unwrap();
        assert_eq!(transcript.len(), 7);
        assert!(matches!(
            &transcript[3],
            ChatMessage::Assistant { opaque: None, .. }
        ));
        assert_eq!(
            serde_json::to_string(&transcript).unwrap(),
            LEGACY_TRANSCRIPT
        );
    }

    #[test]
    fn opaque_reasoning_round_trips_with_data_byte_identical() {
        let datas = [
            "{\"z\":1,\"signature\":\"caf\u{e9} \u{1F980} \\u00e9 \\\"q\\\" \\\\ \",\"a\":{\"y\":[3,1],\"b\":null}}",
            "  {\"k\" :  \"v\"}  ",
            &format!("{{\"blob\":\"{}\",\"a\":1}}", "A".repeat(1_000_000)),
        ];
        let message = assistant(Some(opaque_reasoning(&datas)));
        let json = serde_json::to_string(&message).unwrap();
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        let ChatMessage::Assistant {
            opaque: Some(opaque),
            ..
        } = &back
        else {
            panic!("opaque lost: {back:?}");
        };
        assert_eq!(opaque.items.len(), 3);
        for (item, data) in opaque.items.iter().zip(datas) {
            assert_eq!(item.data.as_bytes(), data.as_bytes());
        }
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    #[test]
    fn a_bad_opaque_field_never_fails_the_transcript() {
        let json = r#"[{"role":"assistant","content":"x","opaque":{"producer":{"wire":"w"},"items":[{"kind":"k","data":"1"}]}}]"#;
        let transcript: Vec<ChatMessage> = serde_json::from_str(json).unwrap();
        assert!(matches!(
            &transcript[0],
            ChatMessage::Assistant {
                content: Some(_),
                opaque: None,
                ..
            }
        ));
    }

    #[test]
    fn the_request_body_never_carries_opaque_reasoning() {
        let request = |message| ChatRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::System {
                    content: "s".into(),
                },
                message,
            ],
            tools: vec![],
            stream: true,
            stream_options: None,
            max_tokens: None,
            temperature: None,
            extra: Default::default(),
            images: Default::default(),
        };
        let with =
            serde_json::to_string(&request(assistant(Some(opaque_reasoning(&["{}"]))))).unwrap();
        let without = serde_json::to_string(&request(assistant(None))).unwrap();
        assert!(!with.contains("opaque"));
        assert_eq!(with, without);
    }

    fn image(id: &str) -> ImageRef {
        ImageRef {
            attachment_id: id.into(),
            media_type: "image/png".into(),
            name: "shot.png".into(),
            size: 3,
            width: 1,
            height: 1,
        }
    }

    fn request(messages: Vec<ChatMessage>, images: HashMap<String, ImagePayload>) -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages,
            tools: vec![],
            stream: true,
            stream_options: None,
            max_tokens: None,
            temperature: None,
            extra: Default::default(),
            images,
        }
    }

    fn user(content: &str, images: Vec<ImageRef>) -> ChatMessage {
        ChatMessage::User {
            content: content.into(),
            provenance: UserProvenance::Captain,
            images,
        }
    }

    #[test]
    fn a_user_message_with_images_serializes_text_then_data_urls_in_order() {
        let payloads = HashMap::from([
            (
                "a".to_string(),
                ImagePayload::from_bytes("image/png", b"one").unwrap(),
            ),
            (
                "b".to_string(),
                ImagePayload::from_bytes("image/jpeg", b"two").unwrap(),
            ),
        ]);
        let body = serde_json::to_value(request(
            vec![user("look", vec![image("a"), image("b")])],
            payloads,
        ))
        .unwrap();
        assert_eq!(
            body["messages"][0],
            serde_json::json!({"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,b25l"}},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,dHdv"}},
            ]})
        );
    }

    #[test]
    fn empty_text_omits_the_text_part_and_a_missing_payload_is_a_visible_placeholder() {
        let body =
            serde_json::to_value(request(vec![user("", vec![image("gone")])], HashMap::new()))
                .unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            serde_json::json!([{"type": "text", "text": "[image shot.png, image/png, 3 B unavailable]"}])
        );
    }

    #[test]
    fn a_message_without_images_keeps_its_plain_string_content() {
        let with_map = HashMap::from([(
            "a".to_string(),
            ImagePayload::from_bytes("image/png", b"x").unwrap(),
        )]);
        let body = serde_json::to_string(&request(vec![user("hi", vec![])], with_map)).unwrap();
        assert_eq!(
            body,
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true}"#
        );
    }

    #[test]
    fn an_image_payload_never_prints_its_bytes() {
        let payload = ImagePayload::from_bytes("image/png", &[7u8; 300]).unwrap();
        let shown = format!("{payload:?}");
        assert!(!shown.contains(&payload.base64[..40]));
        assert!(shown.contains("400"));
    }

    // Given a media type outside the four raster types a request may carry
    // Then the payload is refused at construction, not at the provider
    #[test]
    fn an_image_payload_refuses_a_media_type_no_provider_reads() {
        for bad in ["image/svg+xml", "text/html", "", "image/png;x", "IMAGE/PNG"] {
            assert!(
                ImagePayload::from_bytes(bad, b"x").is_err(),
                "{bad:?} was accepted"
            );
        }
        for good in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            let payload = ImagePayload::from_bytes(good, b"x").unwrap();
            assert!(
                payload
                    .data_url()
                    .starts_with(&format!("data:{good};base64,"))
            );
        }
    }

    #[test]
    fn image_references_round_trip_through_the_transcript_and_stay_out_when_empty() {
        let with = user("x", vec![image("a")]);
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["images"][0]["attachment_id"], "a");
        let back: ChatMessage = serde_json::from_value(json).unwrap();
        assert!(matches!(back, ChatMessage::User { images, .. } if images.len() == 1));
        let without = serde_json::to_value(user("x", vec![])).unwrap();
        assert!(without.get("images").is_none());
    }

    #[test]
    fn deserialize_reasoning_chunk() {
        let raw = include_str!("../../../tests/fixtures/chunks/reasoning.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            chunk.choices[0].delta.reasoning_content.as_deref(),
            Some("We")
        );
        assert!(chunk.choices[0].delta.content.is_none());
    }

    #[test]
    fn deserialize_toolcall_first_fragment() {
        let raw = include_str!("../../../tests/fixtures/chunks/toolcall_first.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_00_199TsKpFMjN3EXHTQZFE3344"));
        assert_eq!(
            tc.function.as_ref().unwrap().name.as_deref(),
            Some("get_weather")
        );
    }

    #[test]
    fn deserialize_toolcall_args_fragment_has_no_id_or_name() {
        let raw = include_str!("../../../tests/fixtures/chunks/toolcall_args.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert!(tc.id.is_none());
        let f = tc.function.as_ref().unwrap();
        assert!(f.name.is_none());
        assert_eq!(f.arguments.as_deref(), Some("{\"city\":\"Paris\"}"));
    }

    #[test]
    fn deserialize_final_usage_chunk() {
        let raw = include_str!("../../../tests/fixtures/chunks/final_usage.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 365);
        assert_eq!(usage.completion_tokens, 63);
        assert_eq!(usage.total_tokens, 428);
    }

    #[test]
    fn deserialize_error_object_routes_to_error_branch() {
        let raw = r#"{"error":{"message":"model overloaded","type":"server_error","code":"503"}}"#;
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert!(chunk.error.is_some());
        assert_eq!(chunk.error.unwrap().message, "model overloaded");
    }

    #[test]
    fn client_cache_reuses_one_client_per_provider() {
        let provider = ProviderConfig {
            base_url: "http://cache-test.invalid/v1".to_string(),
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 100,
            request_timeout_ms: 250,
            catalog_format: None,
        };
        let cache = ClientCache::new();
        let _ = cache.get("p", &provider).unwrap();
        let _ = cache.get("p", &provider).unwrap();
        let _ = cache.get("q", &provider).unwrap();
        // Two providers, two entries: repeated gets for the same provider
        // never grow the cache (the client is reused, not rebuilt).
        assert_eq!(cache.clients.lock().unwrap().len(), 2);
    }

    /// A local server that answers every request `200` after `delay`.
    async fn slow_server(delay: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        format!("http://{addr}/v1")
    }

    fn provider_at(base_url: String, request_timeout_ms: u64) -> ProviderConfig {
        ProviderConfig {
            base_url,
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 1_000,
            request_timeout_ms,
            catalog_format: None,
        }
    }

    #[tokio::test]
    async fn client_cache_keeps_distinct_deadlines_apart_on_one_provider() {
        let url = slow_server(Duration::from_millis(300)).await;
        let fast = provider_at(url.clone(), 100);
        let slow = provider_at(url.clone(), 10_000);
        let cache = ClientCache::new();
        // The short-deadline client is built first; a layer with a long
        // deadline on the same provider must not inherit it.
        let fast_client = cache.get("omniroute", &fast).unwrap();
        let slow_client = cache.get("omniroute", &slow).unwrap();

        let err = fast_client.get(&url).send().await.unwrap_err();
        assert!(err.is_timeout(), "{err}");
        let ok = slow_client.get(&url).send().await.unwrap();
        assert_eq!(ok.status(), 200);
    }

    #[tokio::test]
    async fn build_client_applies_the_configured_request_timeout() {
        let url = slow_server(Duration::from_secs(30)).await;
        let client = build_client(&provider_at(url.clone(), 200)).unwrap();
        let started = std::time::Instant::now();
        let err = client.get(&url).send().await.unwrap_err();
        let elapsed = started.elapsed();
        assert!(err.is_timeout(), "{err}");
        assert!(
            elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(5),
            "failed after {elapsed:?}, not near the 200 ms deadline"
        );
    }
}
