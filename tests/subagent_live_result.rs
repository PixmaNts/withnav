use serde_json::json;
use ulid::Ulid;
use withnav::bus::event::{EventEnvelope, EventKind};
use withnav::provider::openai::{ChatMessage, UserProvenance};
use withnav::session::store::HistoryStore;
mod common;
use common::deadline::Deadline;
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, text_chunk};

#[tokio::test]
async fn failed_second_steer_commit_preserves_the_acknowledged_prefix_after_restore() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = service
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    service
        .send(common::server::text_command(parent, "work"))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, parent_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let a = service
        .create_child_with_delivery(parent, "vigie", "A", "steer")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream_a) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let b = service
        .create_child_with_delivery(parent, "vigie", "B", "steer")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream_b) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream_a
        .send(text_chunk("first committed finding"))
        .unwrap();
    drop(stream_a);
    async {
        while service
            .history()
            .unconsumed_completions(parent)
            .await
            .unwrap()
            .len()
            != 1
        {
            tokio::task::yield_now().await;
        }
    }
    .bounded(file!(), line!())
    .await;
    stream_b.send(text_chunk("second pending finding")).unwrap();
    drop(stream_b);
    async {
        while service
            .history()
            .unconsumed_completions(parent)
            .await
            .unwrap()
            .len()
            != 2
        {
            tokio::task::yield_now().await;
        }
    }
    .bounded(file!(), line!())
    .await;
    service.fail_delivery_commit(2);
    parent_stream
        .send(common::sse::tool_call_chunk(
            "boundary",
            "glob",
            &json!({"pattern":"*"}),
        ))
        .unwrap();
    drop(parent_stream);
    async {
        loop {
            service.history().flush().await.unwrap();
            let events = service
                .history()
                .replay(parent, None, u64::MAX)
                .await
                .unwrap();
            if events
                .iter()
                .any(|e| e.kind == "turn.end" && e.payload["outcome"] == "error")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    }
    .bounded(file!(), line!())
    .await;
    service
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let loaded = service
        .history()
        .load(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let transcript = serde_json::to_string(&loaded.transcript).unwrap();
    assert!(transcript.contains("first committed finding"));
    assert!(!transcript.contains("second pending finding"));
    let pending = service
        .history()
        .unconsumed_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, b);
    service.fail_delivery_commit(0);
    service
        .create_or_resume_session(&ws, None, false, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, resumed) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("first committed finding"));
    assert!(request.contains("second pending finding"));
    service
        .history()
        .flush()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let events = service
        .history()
        .replay(parent, None, u64::MAX)
        .bounded(file!(), line!())
        .await
        .unwrap();
    for child in [a, b] {
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == "message.received"
                    && e.payload["source"] == "subagent"
                    && e.payload["child_session_id"] == child.to_string())
                .count(),
            1
        );
    }
    drop(resumed);
    service.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn delivered_result_is_live_and_replayed_once_before_the_woken_turn() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = service
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let mut subscription = service
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = service
        .create_child_session(parent, "vigie", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(text_chunk(&"parser finding ".repeat(3000)))
        .unwrap();
    drop(stream);
    let mut result = None;
    loop {
        let event = subscription
            .live
            .as_mut()
            .unwrap()
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        if event.kind == "message.received" && event.payload["source"] == "subagent" {
            assert!(result.is_none());
            assert_eq!(event.payload["child_session_id"], child.to_string());
            assert_eq!(event.payload["agent"], "vigie");
            assert_eq!(event.payload["outcome"], "done");
            assert_eq!(event.payload["target"], "agent:main");
            assert_eq!(event.payload["mode"], "next-turn");
            assert!(event.payload["content"].as_str().unwrap().len() <= 16 * 1024);
            result = Some(event);
        } else if event.kind == "turn.start" {
            assert!(result.as_ref().unwrap().seq < event.seq);
            break;
        }
    }
    let (_, parent_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    service
        .history()
        .flush()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let replay = service
        .history()
        .replay(parent, None, u64::MAX)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let messages = replay
        .iter()
        .filter(|e| e.kind == "message.received" && e.payload["source"] == "subagent")
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0], &result.unwrap());
    drop(parent_stream);
    service.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn committed_transcript_message_replays_after_crash_before_live_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("history.db");
    let parent = Ulid::new();
    let child = Ulid::new();
    let store = HistoryStore::open(&path).unwrap();
    for id in [parent, child] {
        store
            .record_session(id, "/ws", "default", false)
            .bounded(file!(), line!())
            .await
            .unwrap();
    }
    store
        .create_child_link(
            child,
            "vigie".into(),
            "inspect".into(),
            "{}".into(),
            4,
            EventEnvelope::new(parent, 0, EventKind::SubagentSpawned, json!({})),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .finish_child_link(child, "done".into(), None, "answer".into())
        .bounded(file!(), line!())
        .await
        .unwrap();
    let text = "bounded delivered answer";
    let transcript = vec![ChatMessage::User {
        content: text.into(),
        provenance: UserProvenance::Subagent,
    }];
    let message = EventEnvelope::new(
        parent,
        1,
        EventKind::MessageReceived,
        json!({
            "command_id":child,"source":"subagent","target":"agent:main","mode":"next-turn",
            "content":text,"child_session_id":child,"agent":"vigie","outcome":"done"
        }),
    );
    // Execute the sequencer's durable callback, then lose the process before its emitter runs.
    store
        .consume_completions_committed(
            parent,
            vec![child],
            transcript.clone(),
            Some(message.clone()),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    drop(store);
    let recovered = HistoryStore::open(&path).unwrap();
    assert!(recovered
        .unconsumed_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        serde_json::to_value(
            recovered
                .load(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .unwrap(),
        serde_json::to_value(&transcript).unwrap()
    );
    assert!(recovered
        .consume_completions_committed(parent, vec![child], transcript, Some(message.clone()))
        .bounded(file!(), line!())
        .await
        .is_err());
    let replay = recovered
        .replay(parent, None, u64::MAX)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        replay
            .iter()
            .filter(|e| e.kind == "message.received")
            .count(),
        1
    );
    assert_eq!(replay.last().unwrap(), &message);
}
