use navai::server::AppState;
use serde_json::{json, Value};
mod common;
use common::deadline::Deadline;
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, text_chunk, tool_call_chunk};

fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn steer_during_provider_request_lands_after_the_complete_tool_batch() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "explore", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .steer_child(parent, child, "parent guidance", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk("glob", "glob", &json!({"pattern":"*"})))
        .unwrap();
    drop(stream);
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let b = body(&request);
    let messages = b["messages"].as_array().unwrap();
    let tool = messages.iter().position(|m| m["role"] == "tool").unwrap();
    let input = messages
        .iter()
        .position(|m| m["content"] == "parent guidance")
        .unwrap();
    assert!(tool < input);
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn interrupt_provider_request_keeps_child_session_and_starts_a_fresh_turn() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state: AppState = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "explore", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, old) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .steer_child(parent, child, "replace task", true)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("replace task"));
    assert_eq!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    stream.send(text_chunk("finished")).unwrap();
    drop(stream);
    drop(old);
    state.close_all().bounded(file!(), line!()).await;
}
