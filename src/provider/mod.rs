use std::collections::BTreeMap;

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{ProviderError, SseError};
use crate::provider::openai::ChatChunk;
use crate::provider::sse::SseEventStream;

pub mod auth;
pub mod catalog;
pub mod client;
pub mod config;
pub mod fim;
pub mod openai;
pub mod sse;
pub mod systemone;
#[cfg(test)]
pub(crate) mod test_support;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
    #[serde(default = "default_true")]
    pub args_valid: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub text: String,
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// One incremental piece of a model response. On the wire (`llm.delta`) it
/// carries a `delta_type` tag next to its fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "delta_type", rename_all = "snake_case")]
pub enum StreamDelta {
    Text { text: String },
    Thinking { text: String },
    ToolCallStarted { index: usize, name: String },
    ToolCallArgs { index: usize, fragment: String },
}

const DONE_SENTINEL: &str = "[DONE]";

#[derive(Default)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct Accumulator {
    text: String,
    thinking: String,
    tool_calls: BTreeMap<usize, ToolCallState>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

impl Accumulator {
    fn apply_chunk(
        &mut self,
        chunk: ChatChunk,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<(), ProviderError> {
        if let Some(err) = chunk.error {
            return Err(ProviderError::Api {
                status: 0,
                message: err.message,
                partial: Some(Box::new(self.snapshot())),
            });
        }
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
            let delta = choice.delta;
            if let Some(thinking) = delta.reasoning_content {
                if !thinking.is_empty() {
                    self.thinking.push_str(&thinking);
                    on_delta(StreamDelta::Thinking { text: thinking });
                }
            }
            if let Some(content) = delta.content {
                if !content.is_empty() {
                    self.text.push_str(&content);
                    on_delta(StreamDelta::Text { text: content });
                }
            }
            for tc in delta.tool_calls {
                let state = self.tool_calls.entry(tc.index).or_default();
                if let Some(id) = tc.id {
                    state.id = id;
                }
                if let Some(func) = tc.function {
                    if let Some(name) = func.name {
                        state.name = name.clone();
                        on_delta(StreamDelta::ToolCallStarted {
                            index: tc.index,
                            name,
                        });
                    }
                    if let Some(args) = func.arguments {
                        if !args.is_empty() {
                            state.arguments.push_str(&args);
                            on_delta(StreamDelta::ToolCallArgs {
                                index: tc.index,
                                fragment: args,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> AssistantMessage {
        let tool_calls = self
            .tool_calls
            .values()
            .map(|s| {
                let (args, args_valid) = if s.arguments.trim().is_empty() {
                    (Value::Object(Default::default()), true)
                } else {
                    match serde_json::from_str::<Value>(&s.arguments) {
                        Ok(v) => (v, true),
                        Err(_) => (Value::Null, false),
                    }
                };
                ToolCall {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    args,
                    args_valid,
                }
            })
            .collect();
        AssistantMessage {
            text: self.text.clone(),
            thinking: self.thinking.clone(),
            tool_calls,
            finish_reason: self.finish_reason.clone(),
            usage: self.usage,
        }
    }
}

pub async fn consume_event_stream<S>(
    events: S,
    cancel: &CancellationToken,
    on_delta: &mut (dyn FnMut(StreamDelta) + Send),
) -> Result<AssistantMessage, ProviderError>
where
    S: Stream<Item = Result<String, SseError>> + Unpin,
{
    let mut events = events;
    let mut acc = Accumulator::default();
    let mut saw_done = false;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(ProviderError::Canceled {
                    partial: nonempty(acc.snapshot()),
                });
            }
            item = events.next() => {
                match item {
                    Some(Ok(payload)) => {
                        if payload == DONE_SENTINEL {
                            saw_done = true;
                            break;
                        }
                        if saw_done {
                            continue;
                        }
                        let chunk: ChatChunk = match serde_json::from_str(&payload) {
                            Ok(c) => c,
                            Err(e) => {
                                return Err(ProviderError::Api {
                                    status: 0,
                                    message: format!("malformed SSE JSON: {e}"),
                                    partial: Some(Box::new(acc.snapshot())),
                                });
                            }
                        };
                        acc.apply_chunk(chunk, on_delta)?;
                    }
                    Some(Err(e)) => return Err(ProviderError::Sse(e)),
                    None => break,
                }
            }
        }
    }

    if !saw_done {
        return Err(ProviderError::Api {
            status: 0,
            message: "stream ended without [DONE]".to_string(),
            partial: Some(Box::new(acc.snapshot())),
        });
    }
    Ok(acc.snapshot())
}

fn nonempty(msg: AssistantMessage) -> Option<Box<AssistantMessage>> {
    if msg == AssistantMessage::default() {
        None
    } else {
        Some(Box::new(msg))
    }
}

pub async fn start_and_consume(
    response: reqwest::Response,
    cancel: &CancellationToken,
    on_delta: &mut (dyn FnMut(StreamDelta) + Send),
) -> Result<AssistantMessage, ProviderError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let message = extract_error_message(&body).unwrap_or(body);
        return Err(ProviderError::Api {
            status: status.as_u16(),
            message,
            partial: None,
        });
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !content_type.contains("text/event-stream") {
        return Err(ProviderError::WrongContentType(content_type));
    }

    let events = SseEventStream::new(Box::pin(response.bytes_stream()));
    consume_event_stream(events, cancel, on_delta).await
}

fn extract_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("error")?.get("message")?.as_str().map(String::from)
}

#[cfg(test)]
mod accumulator_tests {
    use super::*;

    fn event_stream(events: Vec<String>) -> impl Stream<Item = Result<String, SseError>> + Unpin {
        let owned: Vec<Result<String, SseError>> = events.into_iter().map(Ok).collect();
        Box::pin(futures_util::stream::iter(owned))
    }

    fn ev(events: Vec<&str>) -> Vec<String> {
        events.into_iter().map(String::from).collect()
    }

    async fn run(events: Vec<&str>) -> Result<AssistantMessage, ProviderError> {
        run_owned(ev(events)).await
    }

    async fn run_owned(events: Vec<String>) -> Result<AssistantMessage, ProviderError> {
        let cancel = CancellationToken::new();
        let mut noop = |_d: StreamDelta| {};
        consume_event_stream(event_stream(events), &cancel, &mut noop).await
    }

    fn chunk_text(t: &str) -> String {
        format!(r#"{{"choices":[{{"index":0,"delta":{{"content":"{t}"}},"finish_reason":null}}]}}"#)
    }

    #[tokio::test]
    async fn text_thinking_and_two_fragmented_toolcalls_sparse_indices() {
        let events = vec![
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"think"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"hello "},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"world"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"read","arguments":"{\"pa"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"id":"c2","function":{"name":"bash","arguments":"{\"cmd"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"function":{"arguments":"\":\"ls\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            "[DONE]",
        ];
        let msg = run(events).await.unwrap();
        assert_eq!(msg.text, "hello world");
        assert_eq!(msg.thinking, "think");
        assert_eq!(msg.tool_calls.len(), 2);
        assert_eq!(msg.tool_calls[0].id, "c0");
        assert_eq!(msg.tool_calls[0].name, "read");
        assert_eq!(msg.tool_calls[0].args, serde_json::json!({"path":"a"}));
        assert!(msg.tool_calls[0].args_valid);
        assert_eq!(msg.tool_calls[1].id, "c2");
        assert_eq!(msg.tool_calls[1].name, "bash");
        assert_eq!(msg.tool_calls[1].args, serde_json::json!({"cmd":"ls"}));
        assert_eq!(msg.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(msg.usage.unwrap().total_tokens, 15);
    }

    #[tokio::test]
    async fn ascending_output_order_regardless_of_arrival() {
        let events = vec![
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"id":"late","function":{"name":"b","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"early","function":{"name":"a","arguments":"{}"}}]}}]}"#,
            "[DONE]",
        ];
        let msg = run(events).await.unwrap();
        assert_eq!(msg.tool_calls[0].id, "early");
        assert_eq!(msg.tool_calls[1].id, "late");
    }

    #[tokio::test]
    async fn invalid_json_args_flagged_not_panic() {
        let events = vec![
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"read","arguments":"{not json"}}]}}]}"#,
            "[DONE]",
        ];
        let msg = run(events).await.unwrap();
        assert_eq!(msg.tool_calls.len(), 1);
        assert!(!msg.tool_calls[0].args_valid);
        assert_eq!(msg.tool_calls[0].args, Value::Null);
    }

    #[tokio::test]
    async fn eof_without_done_is_api_error_with_partial() {
        let err = run_owned(vec![chunk_text("partial")]).await.unwrap_err();
        match err {
            ProviderError::Api {
                message, partial, ..
            } => {
                assert!(message.contains("without [DONE]"));
                assert_eq!(partial.unwrap().text, "partial");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn streamed_error_object_after_partial_carries_partial() {
        let events = vec![
            chunk_text("some text"),
            r#"{"error":{"message":"overloaded"}}"#.to_string(),
        ];
        let err = run_owned(events).await.unwrap_err();
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
    async fn malformed_json_after_partial_is_api_error() {
        let events = vec![
            chunk_text("hi"),
            "{ this is not valid json".to_string(),
            "[DONE]".to_string(),
        ];
        let err = run_owned(events).await.unwrap_err();
        assert!(matches!(err, ProviderError::Api { .. }));
    }

    #[tokio::test]
    async fn bytes_after_done_are_ignored() {
        let events = vec![
            chunk_text("done"),
            "[DONE]".to_string(),
            r#"{"choices":[{"index":0,"delta":{"content":"IGNORED"}}]}"#.to_string(),
        ];
        let msg = run_owned(events).await.unwrap();
        assert_eq!(msg.text, "done");
    }

    #[tokio::test]
    async fn usage_captured_from_final_chunk() {
        let events = vec![
            chunk_text("x"),
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#.to_string(),
            "[DONE]".to_string(),
        ];
        let msg = run_owned(events).await.unwrap();
        assert_eq!(msg.usage.unwrap().prompt_tokens, 1);
        assert_eq!(msg.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn on_delta_fires_text_before_terminal_timeline() {
        let cancel = CancellationToken::new();
        let mut timeline: Vec<String> = Vec::new();
        {
            let mut on_delta = |d: StreamDelta| {
                if let StreamDelta::Text { text } = d {
                    timeline.push(text);
                }
            };
            let events = event_stream(vec![
                chunk_text("a"),
                chunk_text("b"),
                chunk_text("c"),
                "[DONE]".to_string(),
            ]);
            let msg = consume_event_stream(events, &cancel, &mut on_delta)
                .await
                .unwrap();
            assert_eq!(msg.text, "abc");
        }
        assert_eq!(timeline, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn cancellation_returns_promptly_with_partial() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut noop = |_d: StreamDelta| {};
        let events = event_stream(vec![chunk_text("never-seen")]);
        let err = consume_event_stream(events, &cancel, &mut noop)
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Canceled { .. }));
    }

    #[tokio::test]
    async fn continuation_fragment_without_id_or_name_still_reassembles() {
        let events = vec![
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"x\"}"}}]}}]}"#,
            "[DONE]",
        ];
        let msg = run(events).await.unwrap();
        assert_eq!(msg.tool_calls[0].id, "c0");
        assert_eq!(msg.tool_calls[0].name, "read");
        assert_eq!(msg.tool_calls[0].args, serde_json::json!({"path":"x"}));
    }

    #[tokio::test]
    async fn empty_response_classifies_as_done_no_toolcalls() {
        let events = vec![
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ];
        let msg = run(events).await.unwrap();
        assert!(msg.text.is_empty());
        assert!(msg.tool_calls.is_empty());
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_once(response: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(response).await;
            let _ = sock.flush().await;
        });
        format!("http://{addr}/chat/completions")
    }

    #[tokio::test]
    async fn non_2xx_json_body_is_api_error() {
        let resp = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 41\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"model overloaded\"}}\n";
        let url = serve_once(resp).await;
        let client = reqwest::Client::new();
        let response = client.get(&url).send().await.unwrap();
        let cancel = CancellationToken::new();
        let mut noop = |_d: StreamDelta| {};
        let err = start_and_consume(response, &cancel, &mut noop)
            .await
            .unwrap_err();
        match err {
            ProviderError::Api {
                status, message, ..
            } => {
                assert_eq!(status, 500);
                assert!(message.contains("model overloaded"));
            }
            other => panic!("expected Api 500, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wrong_content_type_200_fails_loudly() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
        let url = serve_once(resp).await;
        let client = reqwest::Client::new();
        let response = client.get(&url).send().await.unwrap();
        let cancel = CancellationToken::new();
        let mut noop = |_d: StreamDelta| {};
        let err = start_and_consume(response, &cancel, &mut noop)
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::WrongContentType(_)));
    }

    #[tokio::test]
    async fn cancellation_mid_body_returns_promptly_and_closes_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let head =
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = sock.write_all(head).await;
            let first =
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial \"}}]}\n\n";
            let _ = sock.write_all(first).await;
            let _ = sock.flush().await;
            let mut sink = [0u8; 1];
            let _ = sock.read(&mut sink).await;
            let _ = closed_tx.send(());
        });

        let url = format!("http://{addr}/chat/completions");
        let client = reqwest::Client::new();
        let response = client.get(&url).send().await.unwrap();
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel2.cancel();
        });

        let mut noop = |_d: StreamDelta| {};
        let start = std::time::Instant::now();
        let err = start_and_consume(response, &cancel, &mut noop)
            .await
            .unwrap_err();
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "took {elapsed:?}"
        );
        match err {
            ProviderError::Canceled { partial } => {
                assert_eq!(partial.unwrap().text, "partial ");
            }
            other => panic!("expected Canceled with partial, got {other:?}"),
        }

        tokio::time::timeout(std::time::Duration::from_secs(3), closed_rx)
            .await
            .expect("server should observe connection close")
            .ok();
    }
}
