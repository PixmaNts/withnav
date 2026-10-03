use crate::agent::actor::{AgentActor, AgentState, PendingInteraction};
use crate::agent::context::Breakdown;
use crate::agent::permissions::{
    needs_permission, PermissionDecision, PermissionError, PermissionRequest,
};
use crate::agent::prompt::{assistant_history_message, build_request};
use crate::agent::response::{classify, HumanRequest, StepOutcome};
use crate::error::{AgentError, ProviderError};
use crate::provider::client::ModelClient;
use crate::provider::openai::{ChatMessage, ClientCache};
use crate::provider::{AssistantMessage, StreamDelta, ToolCall};
use crate::tools::{ToolResult, ToolStatus};

pub const DEFAULT_MAX_STEPS: usize = 50;

pub fn max_steps() -> usize {
    std::env::var("NAVAI_MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_STEPS)
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Done(AssistantMessage),
    Yielded(HumanRequest),
    /// A gated tool call has not run: it waits for a decision, given to
    /// [`resolve_permission`]. The actor holds the same request in its
    /// pending interaction.
    PermissionRequired(PermissionRequest),
    Interrupted,
    StepBudgetExhausted,
}

/// Pure observer events of a run, in the order the loop produces them. The
/// loop's control flow never branches on what the observer does.
///
/// `StepStarted` fires AFTER the per-iteration cancellation check,
/// immediately BEFORE request construction (a cancelled iteration never
/// mints a phantom step). `Delta` fires per streamed delta. `Response`
/// fires immediately AFTER the normalized model response is appended to the
/// transcript (the message already carries `usage` + `finish_reason`).
/// `PreCall` / `PostCall` fire immediately around each `registry.dispatch`.
/// `ContextEstimated` follows `Response` and the end of a step's tool calls.
pub enum AgentEvent<'a> {
    Delta(StreamDelta),
    StepStarted,
    Response(&'a AssistantMessage),
    /// The estimated size of the next request, right after the history
    /// changed: a response was appended, or a step's tool results were.
    ContextEstimated(Breakdown),
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

pub async fn run_agent(
    actor: &mut AgentActor,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Result<RunOutcome, AgentError> {
    actor.state = AgentState::Running;
    // One client (one connection pool) for every step of this run.
    let client = ModelClient::new(
        "agent",
        &actor.provider,
        actor.api_key.clone(),
        &ClientCache::new(),
    )?;

    for _step in 0..actor.max_steps {
        if actor.cancel.is_cancelled() {
            actor.state = AgentState::Idle;
            return Ok(RunOutcome::Interrupted);
        }

        observe(AgentEvent::StepStarted);

        let role_message = actor.role_message();
        let request = build_request(
            &actor.model,
            role_message.as_deref(),
            actor.rules.message(),
            actor.skills.catalog(),
            actor.transcript.clone(),
            actor.registry.schemas(),
            actor.max_tokens,
        );

        let message = match client
            .chat(
                request,
                actor.variant.as_ref(),
                &actor.cancel,
                &mut |delta| observe(AgentEvent::Delta(delta)),
            )
            .await
        {
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
        observe(AgentEvent::Response(&message));
        observe(AgentEvent::ContextEstimated(actor.context_breakdown()));

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
                actor.pending = Some(PendingInteraction::Ask {
                    tool_call_id: request.tool_call_id.clone(),
                });
                actor.state = AgentState::Yielded;
                return Ok(RunOutcome::Yielded(request));
            }
            StepOutcome::Continue { tool_calls } => {
                match run_calls(actor, tool_calls, observe).await {
                    Calls::Finished => {}
                    Calls::Interrupted => return Ok(RunOutcome::Interrupted),
                    Calls::Permission(request) => {
                        return Ok(RunOutcome::PermissionRequired(request))
                    }
                }
            }
        }
    }

    actor.state = AgentState::Yielded;
    Ok(RunOutcome::StepBudgetExhausted)
}

/// Why [`resolve_permission`] did not resume the turn.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The decision matched nothing waiting: nothing was run or recorded.
    #[error(transparent)]
    Rejected(#[from] PermissionError),
    /// The turn resumed and then failed.
    #[error(transparent)]
    Agent(#[from] AgentError),
}

/// Applies a driver's `decision` on the permission request `request_id`, then
/// carries the turn on, BEFORE any new model request:
///
/// * `AllowOnce` / `AllowFolder`: the held call runs exactly once and its real
///   result is appended (`AllowFolder` also records the grant first);
/// * `Deny`: nothing runs; a `denied` result with code `permission_denied`
///   and the driver's message is appended.
///
/// The calls that were behind it then go through the gate in order (any of
/// them may ask again: the outcome is then `PermissionRequired`), and once
/// all are settled the turn continues as [`run_agent`] does.
///
/// `request_id` must be the id of the request the actor waits on; a stale or
/// repeated id is [`PermissionError::Stale`] and changes nothing.
pub async fn resolve_permission(
    actor: &mut AgentActor,
    request_id: ulid::Ulid,
    decision: PermissionDecision,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Result<RunOutcome, ResolveError> {
    match &actor.pending {
        Some(PendingInteraction::Permission { request, .. }) if request.id == request_id => {}
        Some(PendingInteraction::Permission { .. }) | Some(PendingInteraction::Ask { .. }) => {
            return Err(PermissionError::Stale.into());
        }
        None => return Err(PermissionError::Stale.into()),
    }
    let Some(PendingInteraction::Permission {
        request,
        call,
        remaining_calls,
    }) = actor.pending.take()
    else {
        return Err(PermissionError::Stale.into());
    };
    actor.state = AgentState::Running;

    if actor.cancel.is_cancelled() {
        append_interrupted(actor, &call.id);
        for rest in &remaining_calls {
            append_interrupted(actor, &rest.id);
        }
        actor.state = AgentState::Idle;
        return Ok(RunOutcome::Interrupted);
    }
    match decision {
        PermissionDecision::AllowOnce | PermissionDecision::AllowFolder => {
            if decision == PermissionDecision::AllowFolder {
                actor
                    .grants
                    .grant(&request.agent, &request.tool, request.scope.clone());
            }
            let result = execute_call(actor, &call, observe).await;
            append_tool_result(actor, &call.id, &result);
        }
        PermissionDecision::Deny { message } => {
            append_tool_result(actor, &call.id, &ToolResult::permission_denied(message));
        }
    }
    match run_calls(actor, remaining_calls, observe).await {
        Calls::Finished => {}
        Calls::Interrupted => return Ok(RunOutcome::Interrupted),
        Calls::Permission(request) => return Ok(RunOutcome::PermissionRequired(request)),
    }
    Ok(run_agent(actor, observe).await?)
}

/// How a pass over a message's tool calls ended.
enum Calls {
    Finished,
    Interrupted,
    /// The call at hand needs a decision; the actor holds it, and the calls
    /// behind it, as its pending interaction.
    Permission(PermissionRequest),
}

/// Runs `calls` in order. A call that needs approval and has none stops the
/// pass BEFORE it executes: it and the calls behind it are kept in the
/// actor's pending interaction.
async fn run_calls(
    actor: &mut AgentActor,
    calls: Vec<ToolCall>,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Calls {
    let mut calls = std::collections::VecDeque::from(calls);
    while let Some(call) = calls.pop_front() {
        if actor.cancel.is_cancelled() {
            append_interrupted(actor, &call.id);
            for rest in &calls {
                append_interrupted(actor, &rest.id);
            }
            actor.state = AgentState::Idle;
            return Calls::Interrupted;
        }
        if let Some(request) = gate(actor, &call) {
            actor.pending = Some(PendingInteraction::Permission {
                request: request.clone(),
                call,
                remaining_calls: calls.into(),
            });
            actor.state = AgentState::Yielded;
            return Calls::Permission(request);
        }
        let result = execute_call(actor, &call, observe).await;
        append_tool_result(actor, &call.id, &result);
    }
    observe(AgentEvent::ContextEstimated(actor.context_breakdown()));
    Calls::Finished
}

/// The request `call` needs a decision for, or `None` when it may run: a bare
/// runtime has no role and gates nothing, and a grant may already cover it.
fn gate(actor: &AgentActor, call: &ToolCall) -> Option<PermissionRequest> {
    let role = actor.role.as_deref()?;
    let scope = needs_permission(Some(role), &call.name, &call.args, &actor.tool_context)?;
    let agent = role.id.as_str();
    if actor
        .grants
        .covers(agent, &call.name, &scope, &actor.tool_context)
    {
        return None;
    }
    Some(PermissionRequest::new(call, scope, agent))
}

async fn execute_call(
    actor: &AgentActor,
    call: &ToolCall,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> ToolResult {
    observe(AgentEvent::PreCall {
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
    observe(AgentEvent::PostCall {
        tool_call_id: &call.id,
        name: &call.name,
        result: &result,
    });
    result
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
    use crate::tools::native::{GrepTool, ReadTool, WriteTool};
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
                readable_roots: Vec::new(),
            },
            ProviderConfig {
                base_url,
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 2000,
                request_timeout_ms: 600_000,
                catalog_format: None,
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
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "all done"));

        let reqs = server.requests.lock().await;
        assert_eq!(reqs.len(), 2);
        let second = &reqs[1];
        assert!(second.contains("\"role\":\"tool\""));
        assert!(second.contains("\"tool_call_id\":\"call_1\""));
        assert!(second.contains("\"reasoning_content\":\"let me look\""));
        assert!(second.contains("\"tool_calls\""));
    }

    /// Serves keep-alive connections and counts how many were opened.
    async fn spawn_keepalive_mock(
        bodies: Vec<String>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connections.clone();
        let bodies = Arc::new(Mutex::new(bodies.into_iter()));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let bodies = bodies.clone();
                tokio::spawn(async move {
                    loop {
                        let req = read_request(&mut sock).await;
                        if req.is_empty() {
                            break;
                        }
                        let Some(body) = bodies.lock().await.next() else {
                            break;
                        };
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(body.as_bytes()).await;
                        let _ = sock.flush().await;
                    }
                });
            }
        });
        (format!("http://{addr}/v1"), connections)
    }

    #[tokio::test]
    async fn one_connection_serves_every_step_of_a_turn() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
        let (url, connections) =
            spawn_keepalive_mock(vec![sse_body(&[toolcall]), sse_body(&[done])]).await;

        let mut actor = make_actor(url, registry_all(), CancellationToken::new());
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(_)));
        // Two steps, one pooled client, one connection.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
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
        let mut noop = |_e: AgentEvent| {};
        let first = run_agent(&mut actor, &mut noop).await.unwrap();
        match first {
            RunOutcome::Yielded(req) => assert_eq!(req.question, "insert SD card"),
            other => panic!("expected Yielded, got {other:?}"),
        }
        let transcript_len_after_yield = actor.transcript.len();

        actor.push_user("done, it is visible");
        let second = run_agent(&mut actor, &mut noop).await.unwrap();
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
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
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
            let mut on_delta = |e: AgentEvent| {
                if let AgentEvent::Delta(StreamDelta::Text { text }) = e {
                    timeline.push(text);
                }
            };
            let outcome = run_agent(&mut actor, &mut on_delta).await.unwrap();
            assert!(matches!(outcome, RunOutcome::Done(_)));
        }
        assert_eq!(timeline, vec!["one ", "two ", "three"]);
    }

    #[tokio::test]
    async fn max_steps_exhaustion_yields_not_forced_continuation() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[toolcall])),
        ])
        .await;
        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        actor.max_steps = 2;
        actor.push_user("loop forever");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::StepBudgetExhausted));
    }

    /// One assistant message holding the given `(id, tool, arguments)` calls.
    fn calls_chunk(calls: &[(&str, &str, serde_json::Value)]) -> String {
        let calls: Vec<serde_json::Value> = calls
            .iter()
            .enumerate()
            .map(|(index, (id, name, args))| {
                serde_json::json!({
                    "index": index,
                    "id": id,
                    "function": {"name": name, "arguments": args.to_string()},
                })
            })
            .collect();
        serde_json::json!({"choices": [{
            "index": 0,
            "delta": {"tool_calls": calls},
            "finish_reason": "tool_calls",
        }]})
        .to_string()
    }

    fn text_chunk(text: &str) -> String {
        serde_json::json!({"choices": [{
            "index": 0,
            "delta": {"content": text},
            "finish_reason": "stop",
        }]})
        .to_string()
    }

    fn registry_gated() -> Registry {
        let mut r = registry_all();
        r.register(Box::new(WriteTool));
        r.register(Box::new(GrepTool));
        r
    }

    fn as_build(actor: &mut AgentActor) {
        actor.role = Some(
            crate::agent::definitions::Agents::builtin()
                .get("build")
                .unwrap()
                .clone(),
        );
    }

    fn tool_result_of(actor: &AgentActor, id: &str) -> Option<String> {
        actor.transcript.iter().find_map(|m| match m {
            ChatMessage::Tool {
                tool_call_id,
                content,
            } if tool_call_id == id => Some(content.clone()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_write_is_not_executed_and_the_turn_yields_with_a_pending_permission() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let target = actor.tool_context.workspace.join("a.txt");
        let server = spawn_mock(vec![http_sse(&sse_body(&[&calls_chunk(&[(
            "w1",
            "write",
            serde_json::json!({"path": target, "content": "hello"}),
        )])]))])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("write it");
        let mut events = Vec::new();
        let mut observe = |e: AgentEvent| {
            if let AgentEvent::PreCall { name, .. } = e {
                events.push(name.to_string());
            }
        };
        let outcome = run_agent(&mut actor, &mut observe).await.unwrap();
        let RunOutcome::PermissionRequired(request) = outcome else {
            panic!("expected a permission request");
        };
        assert_eq!(request.tool, "write");
        assert_eq!(request.tool_call_id, "w1");
        assert_eq!(request.agent, "build");
        assert!(!target.exists(), "the write must not have run");
        assert!(events.is_empty(), "no pre-call for a gated call");
        assert_eq!(actor.state, AgentState::Yielded);
        assert!(actor.has_pending());
        assert_eq!(actor.pending_permission(), Some(&request));
        assert_eq!(tool_result_of(&actor, "w1"), None);
    }

    #[tokio::test]
    async fn reads_still_run_and_the_siblings_of_a_gated_call_wait_behind_it() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let ws = actor.tool_context.workspace.clone();
        std::fs::write(ws.join("r.txt"), "needle").unwrap();
        let target = ws.join("b.txt");
        let server = spawn_mock(vec![http_sse(&sse_body(&[&calls_chunk(&[
            ("g1", "grep", serde_json::json!({"pattern": "needle"})),
            (
                "w1",
                "write",
                serde_json::json!({"path": target, "content": "x"}),
            ),
            ("r1", "read", serde_json::json!({"path": ws.join("r.txt")})),
        ])]))])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::PermissionRequired(_)));
        assert!(tool_result_of(&actor, "g1").unwrap().contains("needle"));
        assert_eq!(tool_result_of(&actor, "w1"), None);
        assert_eq!(tool_result_of(&actor, "r1"), None);
        let Some(PendingInteraction::Permission {
            call,
            remaining_calls,
            ..
        }) = &actor.pending
        else {
            panic!("expected a pending permission");
        };
        assert_eq!(call.id, "w1");
        assert_eq!(remaining_calls.len(), 1);
        assert_eq!(remaining_calls[0].id, "r1");
    }

    #[tokio::test]
    async fn a_bare_runtime_without_a_role_gates_nothing() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        let target = actor.tool_context.workspace.join("c.txt");
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[&calls_chunk(&[(
                "w1",
                "write",
                serde_json::json!({"path": target, "content": "x"}),
            )])])),
            http_sse(&sse_body(&[&text_chunk("done")])),
        ])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(_)));
        assert!(target.exists());
    }

    #[tokio::test]
    async fn a_granted_folder_lets_a_second_write_through_without_yielding() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let ws = actor.tool_context.workspace.canonicalize().unwrap();
        actor.grants.grant(
            "build",
            "write",
            crate::agent::permissions::Scope::Folder(ws.clone()),
        );
        let target = ws.join("sub").join("d.txt");
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[&calls_chunk(&[(
                "w1",
                "write",
                serde_json::json!({"path": target, "content": "x"}),
            )])])),
            http_sse(&sse_body(&[&text_chunk("done")])),
        ])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(_)));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        assert!(!actor.has_pending());
    }

    /// A build actor with a scripted provider, yielded on a pending `write`
    /// of `a.txt` followed by a second `write` of `b.txt` in the same message.
    /// Returns the two targets and the mock server.
    async fn yielded_on_two_writes(
        then: Vec<Vec<u8>>,
    ) -> (
        AgentActor,
        std::path::PathBuf,
        std::path::PathBuf,
        MockServer,
    ) {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let ws = actor.tool_context.workspace.canonicalize().unwrap();
        let (a, b) = (ws.join("a.txt"), ws.join("sub").join("b.txt"));
        let mut responses = vec![http_sse(&sse_body(&[&calls_chunk(&[
            (
                "w1",
                "write",
                serde_json::json!({"path": a, "content": "A"}),
            ),
            (
                "w2",
                "write",
                serde_json::json!({"path": b, "content": "B"}),
            ),
        ])]))];
        responses.extend(then);
        let server = spawn_mock(responses).await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::PermissionRequired(_)));
        (actor, a, b, server)
    }

    fn pending_id(actor: &AgentActor) -> ulid::Ulid {
        actor.pending_permission().unwrap().id
    }

    #[tokio::test]
    async fn allow_once_runs_that_call_once_then_asks_again_for_the_sibling() {
        let (mut actor, a, b, server) = yielded_on_two_writes(vec![]).await;
        let first = pending_id(&actor);
        let mut precalls = Vec::new();
        let mut observe = |e: AgentEvent| {
            if let AgentEvent::PreCall { tool_call_id, .. } = e {
                precalls.push(tool_call_id.to_string());
            }
        };
        let outcome = resolve_permission(
            &mut actor,
            first,
            PermissionDecision::AllowOnce,
            &mut observe,
        )
        .await
        .unwrap();
        let RunOutcome::PermissionRequired(second) = outcome else {
            panic!("the sibling write must ask again");
        };
        assert_eq!(second.tool_call_id, "w2");
        assert_ne!(second.id, first);
        assert_eq!(precalls, ["w1"]);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "A");
        assert!(!b.exists());
        assert_eq!(tool_result_of(&actor, "w1").unwrap(), "wrote 1 bytes");
        assert_eq!(tool_result_of(&actor, "w2"), None);
        // No model request was made by the resume.
        assert_eq!(server.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn allow_folder_lets_the_sibling_through_and_the_turn_continues() {
        let done = http_sse(&sse_body(&[&text_chunk("all written")]));
        let (mut actor, a, b, server) = yielded_on_two_writes(vec![done]).await;
        let id = pending_id(&actor);
        let mut noop = |_e: AgentEvent| {};
        let outcome =
            resolve_permission(&mut actor, id, PermissionDecision::AllowFolder, &mut noop)
                .await
                .unwrap();
        // `a.txt` sits in the workspace, `sub/b.txt` below it: one grant.
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "all written"));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "B");
        assert!(!actor.has_pending());
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("\"tool_call_id\":\"w1\""));
        assert!(requests[1].contains("\"tool_call_id\":\"w2\""));
    }

    #[tokio::test]
    async fn deny_appends_a_denied_result_and_goes_on_with_the_siblings() {
        let done = http_sse(&sse_body(&[&text_chunk("understood")]));
        let (mut actor, a, _b, _server) = yielded_on_two_writes(vec![done]).await;
        let first = pending_id(&actor);
        let mut noop = |_e: AgentEvent| {};
        let outcome = resolve_permission(
            &mut actor,
            first,
            PermissionDecision::Deny {
                message: "not in this repo".into(),
            },
            &mut noop,
        )
        .await
        .unwrap();
        // The sibling is gated on its own and asks.
        let RunOutcome::PermissionRequired(second) = outcome else {
            panic!("the sibling write must ask");
        };
        assert!(!a.exists(), "a denied call never runs");
        let denied: serde_json::Value =
            serde_json::from_str(&tool_result_of(&actor, "w1").unwrap()).unwrap();
        assert_eq!(denied["status"], "denied");
        assert_eq!(denied["error"]["code"], "permission_denied");
        assert_eq!(denied["error"]["message"], "not in this repo");

        let outcome = resolve_permission(
            &mut actor,
            second.id,
            PermissionDecision::Deny {
                message: "no".into(),
            },
            &mut noop,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "understood"));
    }

    #[tokio::test]
    async fn a_stale_wrong_or_repeated_request_id_is_rejected_and_changes_nothing() {
        let done = http_sse(&sse_body(&[&text_chunk("ok")]));
        let (mut actor, a, _b, _server) = yielded_on_two_writes(vec![done]).await;
        let id = pending_id(&actor);
        let before = actor.transcript.len();
        let mut noop = |_e: AgentEvent| {};

        let wrong = ulid::Ulid::new();
        let rejected =
            resolve_permission(&mut actor, wrong, PermissionDecision::AllowOnce, &mut noop).await;
        assert!(matches!(
            rejected,
            Err(ResolveError::Rejected(PermissionError::Stale))
        ));
        assert_eq!(actor.transcript.len(), before);
        assert_eq!(pending_id(&actor), id, "the real request still waits");
        assert!(!a.exists());

        resolve_permission(&mut actor, id, PermissionDecision::AllowOnce, &mut noop)
            .await
            .unwrap();
        // The same id again: it was answered already.
        let again =
            resolve_permission(&mut actor, id, PermissionDecision::AllowOnce, &mut noop).await;
        assert!(matches!(
            again,
            Err(ResolveError::Rejected(PermissionError::Stale))
        ));
    }

    #[tokio::test]
    async fn an_idle_actor_or_a_pending_ask_rejects_any_decision() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        let mut noop = |_e: AgentEvent| {};
        let idle = resolve_permission(
            &mut actor,
            ulid::Ulid::new(),
            PermissionDecision::AllowOnce,
            &mut noop,
        )
        .await;
        assert!(matches!(idle, Err(ResolveError::Rejected(_))));

        actor.pending = Some(PendingInteraction::Ask {
            tool_call_id: "ask_1".into(),
        });
        let asked = resolve_permission(
            &mut actor,
            ulid::Ulid::new(),
            PermissionDecision::AllowOnce,
            &mut noop,
        )
        .await;
        assert!(matches!(asked, Err(ResolveError::Rejected(_))));
        assert!(matches!(
            actor.pending,
            Some(PendingInteraction::Ask { .. })
        ));
    }

    #[tokio::test]
    async fn user_text_never_answers_a_permission_and_settles_its_calls() {
        let (mut actor, a, b, _server) = yielded_on_two_writes(vec![]).await;
        actor.push_user("yes, go ahead");
        assert!(!actor.has_pending());
        assert!(!a.exists() && !b.exists(), "text is never an approval");
        assert_eq!(tool_result_of(&actor, "w1").unwrap(), "");
        assert_eq!(tool_result_of(&actor, "w2").unwrap(), "");
        assert!(matches!(
            actor.transcript.last(),
            Some(ChatMessage::User { content }) if content == "yes, go ahead"
        ));
    }

    #[tokio::test]
    async fn a_stop_settles_every_held_call_as_interrupted() {
        let (mut actor, a, b, _server) = yielded_on_two_writes(vec![]).await;
        actor.interrupt_pending_permission();
        assert!(!actor.has_pending());
        assert!(!a.exists() && !b.exists());
        assert!(tool_result_of(&actor, "w1").is_some());
        assert!(tool_result_of(&actor, "w2").is_some());
    }
}
