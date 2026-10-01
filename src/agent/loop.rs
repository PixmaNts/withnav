use crate::agent::actor::{AgentActor, AgentState};
use crate::agent::prompt::{assistant_history_message, build_request};
use crate::agent::response::{classify, HumanRequest, StepOutcome};
use crate::error::{AgentError, ProviderError};
use crate::provider::openai::{build_client, build_request as build_http, ChatMessage};
use crate::provider::{start_and_consume, AssistantMessage, StreamDelta};
use crate::tools::{ToolResult, ToolStatus};

pub const DEFAULT_MAX_STEPS: usize = 50;

pub fn max_steps() -> usize {
    std::env::var("RUST_AGENT_MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_STEPS)
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Done(AssistantMessage),
    Yielded(HumanRequest),
    Interrupted,
    StepBudgetExhausted,
}

/// Pure observer events fired immediately around each `registry.dispatch`
/// (todo 5). Borrowing-only: the runtime publishes them to the bus and keeps
/// no state; the loop's control flow never branches on them.
pub enum ToolEvent<'a> {
    PreCall {
        tool_call_id: &'a str,
        name: &'a str,
        args: &'a serde_json::Value,
    },
    PostCall {
        tool_call_id: &'a str,
        name: &'a str,
        result: &'a ToolResult,
    },
}

/// Pure observer events at step boundaries (todo 5): `Started` fires AFTER the
/// per-iteration cancellation check, immediately BEFORE request construction
/// (a cancelled iteration never mints a phantom step); `Response` fires
/// immediately AFTER the normalized model response is appended to the
/// transcript (the message already carries `usage` + `finish_reason`).
pub enum StepEvent<'a> {
    Started,
    Response(&'a AssistantMessage),
}

pub async fn run_agent(
    actor: &mut AgentActor,
    on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    on_tool: &mut (dyn for<'e> FnMut(ToolEvent<'e>) + Send),
    on_step: &mut (dyn for<'e> FnMut(StepEvent<'e>) + Send),
) -> Result<RunOutcome, AgentError> {
    actor.state = AgentState::Running;
    let budget = max_steps();

    for _step in 0..budget {
        if actor.cancel.is_cancelled() {
            actor.state = AgentState::Idle;
            return Ok(RunOutcome::Interrupted);
        }

        on_step(StepEvent::Started);

        let request = build_request(
            &actor.model,
            actor.system_prompt.as_deref(),
            actor.transcript.clone(),
            actor.registry.schemas(),
            actor.max_tokens,
        );

        let client = build_client(&actor.provider)?;
        let http = build_http(&client, &actor.provider, &actor.api_key, &request);
        let response = http.send().await.map_err(ProviderError::Http)?;

        let message = match start_and_consume(response, &actor.cancel, on_delta).await {
            Ok(m) => m,
            Err(ProviderError::Canceled { .. }) => {
                actor.state = AgentState::Idle;
                return Ok(RunOutcome::Interrupted);
            }
            Err(e) => return Err(AgentError::Provider(e)),
        };

        actor
            .transcript
            .push(assistant_history_message(&message, actor.reasoning_history));
        on_step(StepEvent::Response(&message));

        let sibling_ids: Vec<String> = message.tool_calls.iter().map(|tc| tc.id.clone()).collect();

        match classify(message) {
            StepOutcome::Done { response } => {
                actor.state = AgentState::Idle;
                return Ok(RunOutcome::Done(response));
            }
            StepOutcome::Yield { request } => {
                for id in &sibling_ids {
                    if id != &request.tool_call_id {
                        append_interrupted(actor, id);
                    }
                }
                actor.pending_ask_id = Some(request.tool_call_id.clone());
                actor.state = AgentState::Yielded;
                return Ok(RunOutcome::Yielded(request));
            }
            StepOutcome::Continue { tool_calls } => {
                for call in tool_calls {
                    if actor.cancel.is_cancelled() {
                        append_interrupted(actor, &call.id);
                        actor.state = AgentState::Idle;
                        return Ok(RunOutcome::Interrupted);
                    }
                    on_tool(ToolEvent::PreCall {
                        tool_call_id: &call.id,
                        name: &call.name,
                        args: &call.args,
                    });
                    let result = actor
                        .registry
                        .dispatch(
                            &call.name,
                            call.args.clone(),
                            actor.cancel.child_token(),
                            &actor.tool_context,
                        )
                        .await
                        .unwrap_or_else(|e| ToolResult::error("tool_error", e.to_string()));
                    on_tool(ToolEvent::PostCall {
                        tool_call_id: &call.id,
                        name: &call.name,
                        result: &result,
                    });
                    append_tool_result(actor, &call.id, &result);
                }
            }
        }
    }

    actor.state = AgentState::Yielded;
    Ok(RunOutcome::StepBudgetExhausted)
}

fn append_tool_result(actor: &mut AgentActor, tool_call_id: &str, result: &ToolResult) {
    let content = match result.status {
        ToolStatus::Ok | ToolStatus::Interrupted => result.output.clone(),
        _ => serde_json::to_string(result).unwrap_or_else(|_| result.output.clone()),
    };
    actor.transcript.push(ChatMessage::Tool {
        tool_call_id: tool_call_id.to_string(),
        content,
    });
}

fn append_interrupted(actor: &mut AgentActor, tool_call_id: &str) {
    append_tool_result(actor, tool_call_id, &ToolResult::interrupted());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::actor::AgentActor;
    use crate::provider::config::{ProviderConfig, ReasoningHistory};
    use crate::provider::test_support::{http_sse, sse_body};
    use crate::tools::bash::BashTool;
    use crate::tools::control::AskTool;
    use crate::tools::native::ReadTool;
    use crate::tools::{Registry, ToolContext};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let s = String::from_utf8_lossy(&buf);
            if let Some(idx) = s.find("\r\n\r\n") {
                let header = &s[..idx];
                let content_len = header
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let body_start = idx + 4;
                if buf.len() >= body_start + content_len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    struct MockServer {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn spawn_mock(responses: Vec<Vec<u8>>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let req_clone = requests.clone();
        tokio::spawn(async move {
            for resp in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req = read_request(&mut sock).await;
                req_clone.lock().await.push(req);
                let _ = sock.write_all(&resp).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        MockServer {
            url: format!("http://{addr}/v1"),
            requests,
        }
    }

    fn make_actor(base_url: String, registry: Registry, cancel: CancellationToken) -> AgentActor {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        AgentActor::new(
            registry,
            ToolContext {
                workspace: ws.clone(),
                writable_roots: vec![ws],
            },
            ProviderConfig {
                base_url,
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 2000,
                request_timeout_ms: 600_000,
            },
            "test-key".to_string(),
            "test-model".to_string(),
            ReasoningHistory::Preserve,
            Some(256),
            cancel,
        )
    }

    fn registry_all() -> Registry {
        let mut r = Registry::new();
        r.register(Box::new(ReadTool));
        r.register(Box::new(BashTool));
        r.register(Box::new(AskTool));
        r
    }

    #[tokio::test]
    async fn continue_then_done_runs_tool_and_finishes() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"let me look","tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;

        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        actor.push_user("run echo hi");
        let mut noop = |_d: StreamDelta| {};
        let outcome = run_agent(
            &mut actor,
            &mut noop,
            &mut |_e: ToolEvent| {},
            &mut |_s: StepEvent| {},
        )
        .await
        .unwrap();
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "all done"));

        let reqs = server.requests.lock().await;
        assert_eq!(reqs.len(), 2);
        let second = &reqs[1];
        assert!(second.contains("\"role\":\"tool\""));
        assert!(second.contains("\"tool_call_id\":\"call_1\""));
        assert!(second.contains("\"reasoning_content\":\"let me look\""));
        assert!(second.contains("\"tool_calls\""));
    }

    #[tokio::test]
    async fn ask_yields_then_resume_same_transcript() {
        let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"insert SD card\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"thanks, continuing"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[ask])),
            http_sse(&sse_body(&[done])),
        ])
        .await;

        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        actor.push_user("set up the SD card");
        let mut noop = |_d: StreamDelta| {};
        let first = run_agent(
            &mut actor,
            &mut noop,
            &mut |_e: ToolEvent| {},
            &mut |_s: StepEvent| {},
        )
        .await
        .unwrap();
        match first {
            RunOutcome::Yielded(req) => assert_eq!(req.question, "insert SD card"),
            other => panic!("expected Yielded, got {other:?}"),
        }
        let transcript_len_after_yield = actor.transcript.len();

        actor.push_user("done, it is visible");
        let second = run_agent(
            &mut actor,
            &mut noop,
            &mut |_e: ToolEvent| {},
            &mut |_s: StepEvent| {},
        )
        .await
        .unwrap();
        assert!(matches!(second, RunOutcome::Done(m) if m.text == "thanks, continuing"));
        assert!(actor.transcript.len() > transcript_len_after_yield);

        let reqs = server.requests.lock().await;
        let resume = &reqs[1];
        assert!(resume.contains("\"tool_call_id\":\"ask_1\""));
        assert!(resume.contains("done, it is visible"));
    }

    #[tokio::test]
    async fn cancel_mid_loop_returns_interrupted_no_further_tools() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[toolcall]))]).await;

        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel.clone());
        actor.push_user("go");
        cancel.cancel();
        let mut noop = |_d: StreamDelta| {};
        let outcome = run_agent(
            &mut actor,
            &mut noop,
            &mut |_e: ToolEvent| {},
            &mut |_s: StepEvent| {},
        )
        .await
        .unwrap();
        assert!(matches!(outcome, RunOutcome::Interrupted));
    }

    #[tokio::test]
    async fn on_delta_streams_text_incrementally() {
        let c1 = r#"{"choices":[{"index":0,"delta":{"content":"one "}}]}"#;
        let c2 = r#"{"choices":[{"index":0,"delta":{"content":"two "}}]}"#;
        let c3 = r#"{"choices":[{"index":0,"delta":{"content":"three"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[c1, c2, c3]))]).await;

        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        actor.push_user("count");
        let mut timeline: Vec<String> = Vec::new();
        {
            let mut on_delta = |d: StreamDelta| {
                if let StreamDelta::Text { text } = d {
                    timeline.push(text);
                }
            };
            let outcome = run_agent(
                &mut actor,
                &mut on_delta,
                &mut |_e: ToolEvent| {},
                &mut |_s: StepEvent| {},
            )
            .await
            .unwrap();
            assert!(matches!(outcome, RunOutcome::Done(_)));
        }
        assert_eq!(timeline, vec!["one ", "two ", "three"]);
    }

    #[tokio::test]
    async fn max_steps_exhaustion_yields_not_forced_continuation() {
        std::env::set_var("RUST_AGENT_MAX_STEPS", "2");
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[toolcall])),
        ])
        .await;
        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        actor.push_user("loop forever");
        let mut noop = |_d: StreamDelta| {};
        let outcome = run_agent(
            &mut actor,
            &mut noop,
            &mut |_e: ToolEvent| {},
            &mut |_s: StepEvent| {},
        )
        .await
        .unwrap();
        std::env::remove_var("RUST_AGENT_MAX_STEPS");
        assert!(matches!(outcome, RunOutcome::StepBudgetExhausted));
    }
}
