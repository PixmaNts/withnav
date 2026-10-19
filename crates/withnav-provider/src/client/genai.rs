//! `ModelClient::chat` over the genai crate, built with the `genai-chat` feature.
//!
//! The request, the deltas, the final message and the errors keep WithNav's own types; only the
//! transport changes. The endpoint, key, extra headers and the pooled `reqwest::Client` (with the
//! provider's timeouts) are the `ModelClient`'s own, handed to genai as a fixed `ServiceTarget`,
//! so genai resolves nothing from its environment.
//!
//! Wire selection: OpenAI Chat Completions by default (today's behaviour). Setting
//! `WITHNAV_GENAI_WIRE=anthropic` sends every chat request as Anthropic Messages
//! (`{base_url}/messages`, `x-api-key`). It is an environment variable and not a `models.json`
//! key because a new `ProviderConfig` field breaks every struct literal of it outside this crate.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use genai::chat::{
    ChatMessage as GMessage, ChatOptions, ChatRequest as GRequest, ChatStreamEvent, ContentPart,
    MessageContent, StopReason, Tool, ToolCall as GToolCall, ToolResponse,
};
use genai::resolver::{AuthData, Endpoint};
use genai::{ModelIden, ServiceTarget, adapter::AdapterKind};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::ModelClient;
use crate::config::{RESERVED_VARIANT_KEYS, Variant};
use crate::error::{ProviderError, SseError};
use crate::openai::{ChatMessage, ChatRequest};
use crate::{Accumulator, AssistantMessage, StreamDelta, Usage, extract_error_message, nonempty};

/// The environment variable that selects the wire genai speaks.
pub const WIRE_ENV: &str = "WITHNAV_GENAI_WIRE";

/// Which genai adapter carries the chat request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// OpenAI Chat Completions, for OpenAI and every OpenAI-compatible gateway.
    OpenAi,
    /// Anthropic Messages.
    Anthropic,
}

impl Wire {
    pub fn from_env() -> Result<Self, ProviderError> {
        match std::env::var(WIRE_ENV).as_deref() {
            Err(_) | Ok("") | Ok("openai") => Ok(Self::OpenAi),
            Ok("anthropic") => Ok(Self::Anthropic),
            Ok(other) => Err(ProviderError::MalformedResponse(format!(
                "{WIRE_ENV}={other:?}: expected \"openai\" or \"anthropic\""
            ))),
        }
    }

    fn adapter(self) -> AdapterKind {
        match self {
            Self::OpenAi => AdapterKind::OpenAI,
            Self::Anthropic => AdapterKind::Anthropic,
        }
    }
}

/// The request half: WithNav's request turned into genai's request and options.
pub(super) fn to_genai(
    request: ChatRequest,
    variant: Option<&Variant>,
    headers: &[(String, String)],
) -> (String, GRequest, ChatOptions) {
    let mut request = request;
    if let Some(variant) = variant {
        request.apply_variant(variant);
    }

    let mut messages = Vec::with_capacity(request.messages.len());
    for message in request.messages {
        messages.push(match message {
            ChatMessage::System { content } => GMessage::system(content),
            ChatMessage::User { content, .. } => GMessage::user(content),
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                let mut parts = Vec::new();
                if let Some(content) = content {
                    parts.push(ContentPart::Text(content));
                }
                if let Some(reasoning) = reasoning_content {
                    parts.push(ContentPart::ReasoningContent(reasoning));
                }
                for call in tool_calls {
                    // genai stores arguments as JSON and writes them back with `to_string`;
                    // they were produced by `serde_json::to_string` of a `Value`, so the round
                    // trip gives the same text. Unparsable text is kept as a JSON string.
                    let fn_arguments = serde_json::from_str(&call.function.arguments)
                        .unwrap_or(Value::String(call.function.arguments));
                    parts.push(ContentPart::ToolCall(GToolCall {
                        call_id: call.id,
                        fn_name: call.function.name,
                        fn_arguments,
                        thought_signatures: None,
                    }));
                }
                GMessage::assistant(MessageContent::from_parts(parts))
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
                ..
            } => GMessage::from(ToolResponse::new(tool_call_id, content)),
        });
    }

    let tools: Vec<Tool> = request
        .tools
        .into_iter()
        .map(|tool| {
            Tool::new(tool.function.name)
                .with_description(tool.function.description)
                .with_schema(tool.function.parameters)
        })
        .collect();
    let mut chat = GRequest::from_messages(messages);
    if !tools.is_empty() {
        chat = chat.with_tools(tools);
    }

    let mut options = ChatOptions::default()
        // Model ids are opaque catalogue keys: `agy/claude-opus-5-5-high` must reach the
        // gateway as written, never be split into a name and an effort.
        .with_model_name_effort_suffix(false)
        .with_capture_tool_calls(true)
        .with_capture_usage(request.stream_options.is_some_and(|o| o.include_usage));
    if let Some(max_tokens) = request.max_tokens {
        options = options.with_max_tokens(u32::try_from(max_tokens).unwrap_or(u32::MAX));
    }
    if let Some(temperature) = request.temperature {
        // Through its shortest decimal form: `f64::from(0.2f32)` is 0.20000000298023224,
        // which would change the number the server receives.
        let temperature = temperature
            .to_string()
            .parse::<f64>()
            .unwrap_or(f64::from(temperature));
        options = options.with_temperature(temperature);
    }
    let extra: serde_json::Map<String, Value> = request
        .extra
        .into_iter()
        .filter(|(k, _)| !RESERVED_VARIANT_KEYS.contains(&k.as_str()))
        .collect();
    if !extra.is_empty() {
        options = options.with_extra_body(Value::Object(extra));
    }
    if !headers.is_empty() {
        options = options.with_extra_headers(headers.to_vec());
    }
    (request.model, chat, options)
}

/// Where a tool call stands in the stream: genai's events carry the call accumulated so far,
/// so the delta is what was added since the last event for the same call.
struct StreamedCall {
    index: usize,
    seen_args: usize,
}

/// The response half: genai's events folded into WithNav's deltas and message.
#[derive(Default)]
struct Folder {
    acc: Accumulator,
    calls: BTreeMap<String, StreamedCall>,
}

impl Folder {
    fn tool_call(&mut self, call: GToolCall, on_delta: &mut (dyn FnMut(StreamDelta) + Send)) {
        let next = self.calls.len();
        let streamed = self
            .calls
            .entry(call.call_id.clone())
            .or_insert(StreamedCall {
                index: next,
                seen_args: 0,
            });
        let index = streamed.index;
        let state = self.acc.tool_calls.entry(index).or_default();
        state.id = call.call_id;
        if !call.fn_name.is_empty() && state.name != call.fn_name {
            state.name = call.fn_name.clone();
            on_delta(StreamDelta::ToolCallStarted {
                index,
                name: call.fn_name,
            });
        }
        let accumulated = match &call.fn_arguments {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        if let Some(fragment) = accumulated.get(streamed.seen_args..)
            && !fragment.is_empty()
        {
            state.arguments.push_str(fragment);
            on_delta(StreamDelta::ToolCallArgs {
                index,
                fragment: fragment.to_string(),
            });
        }
        streamed.seen_args = accumulated.len();
    }

    fn event(
        &mut self,
        event: ChatStreamEvent,
        wire: Wire,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> bool {
        match event {
            ChatStreamEvent::Start | ChatStreamEvent::Heartbeat => {}
            ChatStreamEvent::ThoughtSignatureChunk(_) => {}
            ChatStreamEvent::Chunk(chunk) if !chunk.content.is_empty() => {
                self.acc.text.push_str(&chunk.content);
                on_delta(StreamDelta::Text {
                    text: chunk.content,
                });
            }
            ChatStreamEvent::ReasoningChunk(chunk) if !chunk.content.is_empty() => {
                self.acc.thinking.push_str(&chunk.content);
                on_delta(StreamDelta::Thinking {
                    text: chunk.content,
                });
            }
            ChatStreamEvent::Chunk(_) | ChatStreamEvent::ReasoningChunk(_) => {}
            ChatStreamEvent::ToolCallChunk(chunk) => self.tool_call(chunk.tool_call, on_delta),
            ChatStreamEvent::End(end) => {
                self.acc.finish_reason = end
                    .captured_stop_reason
                    .map(|reason| finish_reason(&reason, wire));
                self.acc.usage = end.captured_usage.map(|usage| {
                    let prompt = usage.prompt_tokens.unwrap_or(0).max(0) as u64;
                    let completion = usage.completion_tokens.unwrap_or(0).max(0) as u64;
                    Usage {
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        total_tokens: usage
                            .total_tokens
                            .map_or(prompt + completion, |t| t.max(0) as u64),
                    }
                });
                return true;
            }
        }
        false
    }
}

/// The OpenAI wire's finish reason as sent; the Anthropic one in OpenAI's words, which is
/// what callers compare against (`"length"` for a truncated answer).
fn finish_reason(reason: &StopReason, wire: Wire) -> String {
    match (wire, reason) {
        (Wire::OpenAi, _) => reason.raw().to_string(),
        (Wire::Anthropic, StopReason::MaxTokens(_)) => "length".into(),
        (Wire::Anthropic, StopReason::ToolCall(_)) => "tool_calls".into(),
        (Wire::Anthropic, StopReason::Completed(_)) => "stop".into(),
        (Wire::Anthropic, other) => other.raw().to_string(),
    }
}

/// genai's error in WithNav's terms; `partial` is what streamed before it.
fn to_provider_error(error: genai::Error, partial: &Accumulator) -> ProviderError {
    let snapshot = || Some(Box::new(partial.snapshot()));
    match error {
        genai::Error::HttpError { status, body, .. } => http_status_error(status.as_u16(), body),
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            genai::webc::Error::ResponseFailedStatus { status, body, .. } => {
                http_status_error(status.as_u16(), body)
            }
            genai::webc::Error::Reqwest(e) => ProviderError::Http(e),
            other => ProviderError::Api {
                status: 0,
                message: other.to_string(),
                partial: None,
            },
        },
        genai::Error::WebStream { error, .. } => {
            if let Some(genai::Error::HttpError { status, body, .. }) =
                error.downcast_ref::<genai::Error>()
            {
                return http_status_error(status.as_u16(), body.clone());
            }
            match error.downcast::<reqwest::Error>() {
                Ok(e) => ProviderError::Http(*e),
                Err(error) if error.is::<std::str::Utf8Error>() => {
                    ProviderError::Sse(SseError::Utf8)
                }
                Err(error) => ProviderError::Api {
                    status: 0,
                    message: error.to_string(),
                    partial: snapshot(),
                },
            }
        }
        genai::Error::ChatResponse { body, .. } => ProviderError::Api {
            status: 0,
            message: body
                .get("message")
                .or_else(|| body.pointer("/error/message"))
                .and_then(Value::as_str)
                .map_or_else(|| body.to_string(), String::from),
            partial: snapshot(),
        },
        genai::Error::StreamParse { serde_error, .. } => ProviderError::Api {
            status: 0,
            message: format!("malformed SSE JSON: {serde_error}"),
            partial: snapshot(),
        },
        other => ProviderError::Api {
            status: 0,
            message: other.to_string(),
            partial: None,
        },
    }
}

fn http_status_error(status: u16, body: String) -> ProviderError {
    ProviderError::Api {
        status,
        message: extract_error_message(&body).unwrap_or(body),
        partial: None,
    }
}

impl ModelClient {
    pub(super) async fn chat_genai(
        &self,
        request: ChatRequest,
        variant: Option<&Variant>,
        cancel: &CancellationToken,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<AssistantMessage, ProviderError> {
        let wire = Wire::from_env()?;
        let (model, chat, options) = to_genai(request, variant, &self.headers);
        let client = genai::Client::builder()
            .with_reqwest(self.http.clone())
            .build()
            .map_err(|e| ProviderError::MalformedResponse(e.to_string()))?;
        let target = ServiceTarget {
            endpoint: Endpoint::from_owned(format!("{}/", self.base_url)),
            auth: AuthData::from_single(self.api_key.clone()),
            model: ModelIden::new(wire.adapter(), model),
        };

        let mut folder = Folder::default();
        // The request is sent on the first poll of the stream, so cancelling covers the send,
        // the response head and the body alike; dropping the stream drops the connection.
        let response = client
            .exec_chat_stream(target, chat, Some(&options))
            .await
            .map_err(|e| to_provider_error(e, &folder.acc))?;
        let mut stream = response.stream;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(ProviderError::Canceled { partial: nonempty(folder.acc.snapshot()) });
                }
                item = stream.next() => match item {
                    Some(Ok(event)) => {
                        if folder.event(event, wire, on_delta) {
                            return Ok(folder.acc.snapshot());
                        }
                    }
                    Some(Err(error)) => return Err(to_provider_error(error, &folder.acc)),
                    None => {
                        return Err(ProviderError::Api {
                            status: 0,
                            message: "stream ended without [DONE]".to_string(),
                            partial: Some(Box::new(folder.acc.snapshot())),
                        });
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;
    use crate::openai::{ClientCache, RequestFunction, RequestToolCall, StreamOptions, ToolDef};
    use crate::test_support::{http_sse, sse_body};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn client_at(addr: std::net::SocketAddr) -> ModelClient {
        let provider = ProviderConfig {
            base_url: format!("http://{addr}/v1"),
            api_key_env: None,
            headers: [("X-Extra".to_string(), "yes".to_string())].into(),
            connect_timeout_ms: 1_000,
            request_timeout_ms: 600_000,
            catalog_format: None,
        };
        ModelClient::new("p", &provider, "key".into(), &ClientCache::new()).unwrap()
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "agy/claude-opus-5-5-high".into(),
            messages: vec![ChatMessage::User {
                content: "hi".into(),
                provenance: Default::default(),
            }],
            tools: vec![],
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            max_tokens: Some(64),
            temperature: None,
            extra: Default::default(),
        }
    }

    /// Accepts one connection, keeps its request, answers with `response`, then
    /// waits for the client to close; `closed` fires when it does.
    async fn serve(
        response: Vec<u8>,
        hold_open: bool,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Receiver<String>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (req_tx, req_rx) = tokio::sync::oneshot::channel();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let _ = req_tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
            if hold_open {
                let mut sink = [0u8; 1];
                let _ = sock.read(&mut sink).await;
                let _ = closed_tx.send(());
            }
        });
        (addr, req_rx, closed_rx)
    }

    fn sse_head() -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n".to_vec()
    }

    #[tokio::test]
    async fn the_model_id_headers_key_and_variant_reach_the_endpoint_as_configured() {
        let (addr, req, _) = serve(
            http_sse(&sse_body(&[
                r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":1,"total_tokens":5}}"#,
            ])),
            false,
        )
        .await;
        let variant: Variant =
            serde_json::from_str(r#"{"reasoning_effort":"low","thinking":{"type":"enabled"}}"#)
                .unwrap();
        let msg = client_at(addr)
            .chat(
                request(),
                Some(&variant),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .unwrap();
        assert_eq!(msg.text, "ok");
        assert_eq!(msg.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            msg.usage,
            Some(Usage {
                prompt_tokens: 4,
                completion_tokens: 1,
                total_tokens: 5
            })
        );

        let raw = req.await.unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let head = head.to_ascii_lowercase();
        assert!(head.starts_with("post /v1/chat/completions "), "{head}");
        assert!(head.contains("authorization: bearer key"), "{head}");
        assert!(head.contains("x-extra: yes"), "{head}");
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "agy/claude-opus-5-5-high");
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 64);
    }

    #[tokio::test]
    async fn deltas_and_message_match_the_built_in_client_for_one_mixed_frame() {
        let (addr, _, _) = serve(
            http_sse(&sse_body(&[
                r#"{"choices":[{"index":0,"delta":{"reasoning_content":"think","content":"hello","tool_calls":[{"index":0,"id":"c0","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"id":"c2","function":{"name":"bash","arguments":"{not json"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ])),
            false,
        )
        .await;
        let mut deltas = Vec::new();
        let msg = client_at(addr)
            .chat(request(), None, &CancellationToken::new(), &mut |d| {
                deltas.push(d)
            })
            .await
            .unwrap();
        assert_eq!(
            deltas,
            vec![
                StreamDelta::Thinking {
                    text: "think".into()
                },
                StreamDelta::Text {
                    text: "hello".into()
                },
                StreamDelta::ToolCallStarted {
                    index: 0,
                    name: "read".into()
                },
                StreamDelta::ToolCallArgs {
                    index: 0,
                    fragment: "{\"pa".into()
                },
                StreamDelta::ToolCallStarted {
                    index: 1,
                    name: "bash".into()
                },
                StreamDelta::ToolCallArgs {
                    index: 1,
                    fragment: "{not json".into()
                },
                StreamDelta::ToolCallArgs {
                    index: 0,
                    fragment: "th\":\"a\"}".into()
                },
            ]
        );
        assert_eq!(msg.thinking, "think");
        assert_eq!(msg.text, "hello");
        assert_eq!(msg.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(msg.usage, None, "no usage frame, no usage");
        assert_eq!(msg.tool_calls.len(), 2);
        assert_eq!(msg.tool_calls[0].id, "c0");
        assert_eq!(msg.tool_calls[0].args, serde_json::json!({"path": "a"}));
        assert!(msg.tool_calls[0].args_valid);
        assert_eq!(msg.tool_calls[1].id, "c2");
        assert!(!msg.tool_calls[1].args_valid);
        assert_eq!(msg.tool_calls[1].args, Value::Null);
    }

    #[tokio::test]
    async fn an_api_error_keeps_its_status_and_message() {
        let body = r#"{"error":{"message":"slow down please"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (addr, _, _) = serve(response.into_bytes(), false).await;
        let err = client_at(addr)
            .chat(request(), None, &CancellationToken::new(), &mut |_| {})
            .await
            .unwrap_err();
        match err {
            ProviderError::Api {
                status,
                message,
                partial,
            } => {
                assert_eq!(status, 429);
                assert_eq!(message, "slow down please");
                assert!(partial.is_none());
            }
            other => panic!("expected Api 429, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_streamed_error_object_carries_the_partial() {
        let (addr, _, _) = serve(
            http_sse(&sse_body(&[
                r#"{"choices":[{"index":0,"delta":{"content":"some text"}}]}"#,
                r#"{"error":{"message":"overloaded"}}"#,
            ])),
            false,
        )
        .await;
        let err = client_at(addr)
            .chat(request(), None, &CancellationToken::new(), &mut |_| {})
            .await
            .unwrap_err();
        match err {
            ProviderError::Api {
                message, partial, ..
            } => {
                assert_eq!(message, "overloaded");
                assert_eq!(partial.unwrap().text, "some text");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stream_without_done_is_an_api_error_with_the_partial() {
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (addr, _, _) = serve(response.into_bytes(), false).await;
        let err = client_at(addr)
            .chat(request(), None, &CancellationToken::new(), &mut |_| {})
            .await
            .unwrap_err();
        match err {
            ProviderError::Api {
                message, partial, ..
            } => {
                assert!(message.contains("without [DONE]"), "{message}");
                assert_eq!(partial.unwrap().text, "partial");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    /// genai reads any 200 body as SSE: a non-SSE answer is not `WrongContentType`
    /// as with the built-in client, but a stream that ends without `[DONE]`.
    #[tokio::test]
    async fn a_non_sse_success_ends_as_a_stream_without_done() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
        let (addr, _, _) = serve(response.to_vec(), false).await;
        let err = client_at(addr)
            .chat(request(), None, &CancellationToken::new(), &mut |_| {})
            .await
            .unwrap_err();
        match err {
            ProviderError::Api {
                status: 0, message, ..
            } => assert!(message.contains("without [DONE]"), "{message}"),
            other => panic!("expected the missing-[DONE] error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_mid_body_returns_the_partial_and_closes_the_connection() {
        let mut response = sse_head();
        response.extend_from_slice(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial \"}}]}\n\n",
        );
        let (addr, _, closed) = serve(response, true).await;
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let mut texts = Vec::new();
        let started = Instant::now();
        let err = client_at(addr)
            .chat(request(), None, &cancel, &mut |d| {
                if let StreamDelta::Text { text } = d {
                    texts.push(text);
                    trigger.cancel();
                }
            })
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        match err {
            ProviderError::Canceled { partial } => {
                assert_eq!(partial.unwrap().text, "partial ")
            }
            other => panic!("expected Canceled with partial, got {other:?}"),
        }
        assert_eq!(texts, ["partial "]);
        tokio::time::timeout(Duration::from_secs(3), closed)
            .await
            .expect("the server should see the connection close")
            .ok();
    }

    #[tokio::test]
    async fn cancel_before_the_response_head_returns_canceled_promptly() {
        let (addr, _, _) = serve(Vec::new(), true).await;
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let err = client_at(addr)
            .chat(request(), None, &cancel, &mut |_| {})
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(matches!(err, ProviderError::Canceled { partial: None }));
    }

    #[tokio::test]
    async fn a_connection_failure_is_an_http_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = client_at(addr)
            .chat(request(), None, &CancellationToken::new(), &mut |_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Http(_)), "{err:?}");
    }

    #[test]
    fn history_tool_calls_round_trip_their_argument_text() {
        let arguments = serde_json::to_string(&serde_json::json!({
            "command": "echo 世界 ✓", "timeout": 9007199254740993u64
        }))
        .unwrap();
        let mut req = request();
        req.messages.push(ChatMessage::Assistant {
            content: None,
            reasoning_content: Some("plan".into()),
            tool_calls: vec![RequestToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: RequestFunction {
                    name: "bash".into(),
                    arguments: arguments.clone(),
                },
            }],
        });
        req.tools = vec![ToolDef::function(
            "bash",
            "run",
            serde_json::json!({"type": "object"}),
        )];
        let (_, chat, _) = to_genai(req, None, &[]);
        let assistant = &chat.messages[1];
        let call = assistant.content.tool_calls()[0];
        assert_eq!(call.fn_arguments.to_string(), arguments);
        assert!(
            assistant
                .content
                .parts()
                .iter()
                .any(|p| matches!(p, ContentPart::ReasoningContent(r) if r == "plan"))
        );
    }

    #[test]
    fn a_temperature_keeps_its_decimal_value() {
        let mut req = request();
        req.temperature = Some(0.2);
        let (_, _, options) = to_genai(req, None, &[]);
        assert_eq!(options.temperature, Some(0.2));
    }

    #[test]
    fn the_wire_is_openai_unless_anthropic_is_named() {
        // Read the parser on its own: the process environment is shared between tests.
        assert_eq!(Wire::OpenAi.adapter(), AdapterKind::OpenAI);
        assert_eq!(Wire::Anthropic.adapter(), AdapterKind::Anthropic);
        assert_eq!(
            finish_reason(&StopReason::from("max_tokens".to_string()), Wire::Anthropic),
            "length"
        );
        assert_eq!(
            finish_reason(&StopReason::from("tool_use".to_string()), Wire::Anthropic),
            "tool_calls"
        );
        assert_eq!(
            finish_reason(&StopReason::from("length".to_string()), Wire::OpenAi),
            "length"
        );
    }
}
