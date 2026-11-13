use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use withnav::bus::event::{EventEnvelope, EventKind};
use withnav::navigator::conversation::{
    ConversationFeed, ConversationTarget, WorkspaceFeedRegistry,
};
use withnav::navigator::ports::{FimAnswer, FimModel, FimRequest, Metacognition, ModelError};
use withnav::navigator::reflex::DEBOUNCE_MS;
use withnav::navigator::{NavigatorDeps, Remark, RemarkContext};

mod common;
use common::navigator::spawn_server_as_is;

struct SilentMetacognition;
impl Metacognition for SilentMetacognition {
    fn remark<'a>(
        &'a self,
        _ctx: &'a RemarkContext,
        _cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<Option<Remark>, ModelError>> {
        Box::pin(async { Ok(None) })
    }
}

struct ScriptedFimModel {
    answer: String,
}
impl FimModel for ScriptedFimModel {
    fn complete<'a>(
        &'a self,
        _request: FimRequest,
        _cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<FimAnswer, ModelError>> {
        let answer = self.answer.clone();
        Box::pin(async move {
            Ok(FimAnswer {
                text: answer,
                latency: Duration::from_millis(1),
                usage: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn two_independent_lsp_connections_equal_request_ids_different_contents_and_detach() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = WorkspaceFeedRegistry::new();
    let feed = registry.get_or_create(tmp.path());

    let fim_a = Arc::new(ScriptedFimModel {
        answer: "completion_a".into(),
    });
    let fim_b = Arc::new(ScriptedFimModel {
        answer: "completion_b".into(),
    });

    let deps_a = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("a.jsonl"))
        .with_dirs(Some(tmp.path().to_path_buf()), None)
        .with_registry(registry.clone())
        .with_conversation(feed.clone())
        .with_fim(fim_a);

    let deps_b = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("b.jsonl"))
        .with_dirs(Some(tmp.path().to_path_buf()), None)
        .with_registry(registry.clone())
        .with_conversation(feed.clone())
        .with_fim(fim_b);

    let mut client_a = spawn_server_as_is(deps_a);
    let mut client_b = spawn_server_as_is(deps_b);

    let init_params = json!({
        "processId": null,
        "capabilities": {},
        "rootUri": format!("file://{}", tmp.path().display()),
        "workspaceFolders": [
            { "uri": format!("file://{}", tmp.path().display()), "name": "tmp" }
        ]
    });

    client_a
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init_params}))
        .await;
    let resp_init_a = client_a.recv().await;
    assert!(resp_init_a.get("result").is_some());
    client_a
        .send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
        .await;

    client_b
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init_params}))
        .await;
    let resp_init_b = client_b.recv().await;
    assert!(resp_init_b.get("result").is_some());
    client_b
        .send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
        .await;

    let uri = format!("file://{}/src/main.rs", tmp.path().display());

    // Both connections open the exact same document URI with different unsaved contents
    client_a.did_open(&uri, "fn main() { /* A */ }\n", 1).await;
    client_b.did_open(&uri, "fn main() { /* B */ }\n", 1).await;

    // Wait for the reflex debounce to elapse
    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 50)).await;

    // Both send inline completion requests on the same URI with equal JSON-RPC request IDs
    client_a
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri,
                "version": 1,
                "line": 0,
                "character": 10
            }
        }))
        .await;

    client_b
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri,
                "version": 1,
                "line": 0,
                "character": 10
            }
        }))
        .await;

    let resp_a = client_a.recv().await;
    let resp_b = client_b.recv().await;

    assert_eq!(resp_a["id"], 42);
    assert_eq!(resp_b["id"], 42);

    let items_a = resp_a["result"]["items"].as_array().unwrap();
    let items_b = resp_b["result"]["items"].as_array().unwrap();

    assert_eq!(items_a.len(), 1);
    assert_eq!(items_b.len(), 1);
    assert_eq!(items_a[0]["text"], "completion_a");
    assert_eq!(items_b[0]["text"], "completion_b");

    // Detach client A via shutdown notification
    client_a
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "shutdown",
            "params": null
        }))
        .await;
    let shutdown_resp = client_a.recv().await;
    assert_eq!(shutdown_resp["id"], 99);

    // Client B continues operating normally without interference
    client_b
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri,
                "version": 1,
                "line": 0,
                "character": 10
            }
        }))
        .await;
    let resp_b2 = client_b.recv().await;
    assert_eq!(resp_b2["id"], 100);
    let items_b2 = resp_b2["result"]["items"].as_array().unwrap();
    assert_eq!(items_b2[0]["text"], "completion_b");
}

struct RecordingFimModel {
    calls: Arc<std::sync::Mutex<Vec<FimRequest>>>,
}
impl FimModel for RecordingFimModel {
    fn complete<'a>(
        &'a self,
        request: FimRequest,
        _cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<FimAnswer, ModelError>> {
        self.calls.lock().unwrap().push(request);
        Box::pin(async move {
            Ok(FimAnswer {
                text: "// completion".into(),
                latency: Duration::from_millis(1),
                usage: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn fake_fim_verifies_exact_input_text_prompt_and_context_isolation() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("root_a");
    let root_b = tmp.path().join("root_b");
    std::fs::create_dir_all(root_a.join(".withnav")).unwrap();
    std::fs::create_dir_all(root_b.join("src")).unwrap();
    std::fs::write(
        root_a.join(".withnav/intent.md"),
        "## Intent\nParser optimization\n",
    )
    .unwrap();

    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fim = Arc::new(RecordingFimModel {
        calls: calls.clone(),
    });

    let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"))
        .with_dirs(Some(root_a.clone()), None)
        .with_fim(fim);

    let mut client = spawn_server_as_is(deps);
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": format!("file://{}", root_a.display()),
                "workspaceFolders": [
                    { "uri": format!("file://{}", root_a.display()), "name": "root_a" },
                    { "uri": format!("file://{}", root_b.display()), "name": "root_b" },
                ]
            }
        }))
        .await;
    let resp_init = client.recv().await;
    assert!(resp_init.get("result").is_some());
    client
        .send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
        .await;

    // 1. Completion in Root A: prompt must contain the Captain's intent from Root A
    let file_a = root_a.join("src/parser.rs");
    std::fs::create_dir_all(file_a.parent().unwrap()).unwrap();
    std::fs::write(&file_a, "fn parse() {\n    let token = 1;\n}\n").unwrap();
    let uri_a = format!("file://{}", file_a.display());

    client
        .did_open(&uri_a, "fn parse() {\n    let token = 1;\n}\n", 1)
        .await;

    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 20)).await;

    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri_a,
                "version": 1,
                "line": 1,
                "character": 4
            }
        }))
        .await;

    let res_a = client.recv().await;
    assert_eq!(res_a["id"], 1);
    assert_eq!(res_a["result"]["items"].as_array().unwrap().len(), 1);

    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    let req_a = &recorded[0];
    assert!(
        req_a.prompt.contains("Parser optimization"),
        "expected prompt to contain intent, got: {}",
        req_a.prompt
    );
    assert!(
        req_a.prompt.contains("fn parse() {"),
        "expected prompt to contain code prefix, got: {}",
        req_a.prompt
    );
    assert!(
        req_a.suffix.contains("let token = 1;"),
        "expected suffix to contain code after cursor, got: {}",
        req_a.suffix
    );

    // 2. Completion in Root B: prompt must NOT contain Root A's intent
    let file_b = root_b.join("src/other.rs");
    std::fs::write(&file_b, "fn other() {\n    let x = 2;\n}\n").unwrap();
    let uri_b = format!("file://{}", file_b.display());

    client
        .did_open(&uri_b, "fn other() {\n    let x = 2;\n}\n", 1)
        .await;

    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 20)).await;

    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri_b,
                "version": 1,
                "line": 1,
                "character": 4
            }
        }))
        .await;

    let res_b = client.recv().await;
    assert_eq!(res_b["id"], 2);
    assert_eq!(res_b["result"]["items"].as_array().unwrap().len(), 1);

    let recorded2 = calls.lock().unwrap().clone();
    assert_eq!(recorded2.len(), 2);
    let req_b = &recorded2[1];
    assert!(
        !req_b.prompt.contains("Parser optimization"),
        "foreign root B prompt must not borrow root A intent, got: {}",
        req_b.prompt
    );
    assert!(
        req_b.prompt.contains("fn other() {"),
        "expected prompt to contain code prefix, got: {}",
        req_b.prompt
    );
}

#[tokio::test]
async fn lsp_serve_detach_guard_cleans_up_on_eof_or_drop() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = WorkspaceFeedRegistry::new();
    let feed = registry.get_or_create(tmp.path());

    let session_id = Ulid::generate();
    feed.register_session(session_id, Arc::new(|_| Box::pin(async { Ok(()) })));

    let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"))
        .with_dirs(Some(tmp.path().to_path_buf()), None)
        .with_registry(registry.clone())
        .with_conversation(feed.clone());

    let mut client = spawn_server_as_is(deps);
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": format!("file://{}", tmp.path().display()),
                "workspaceFolders": [
                    { "uri": format!("file://{}", tmp.path().display()), "name": "tmp" },
                ]
            }
        }))
        .await;
    let resp_init = client.recv().await;
    assert!(resp_init.get("result").is_some());
    client
        .send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
        .await;

    // After initialize, the editor is registered in the workspace feed
    assert!(feed.select_editor(Some(session_id)).is_ok());

    // Drop client (closes duplex streams -> EOF on server stream)
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Detach guard ran on EOF/exit: editor is cleanly unregistered
    assert_eq!(
        feed.select_editor(Some(session_id)).unwrap_err(),
        withnav::navigator::conversation::EditorSelectionError::Unavailable
    );
}

#[tokio::test]
async fn delayed_flow_note_stays_pinned_to_session_target() {
    let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
    let session_a = Ulid::generate();
    let inc_a = Ulid::generate();
    let session_b = Ulid::generate();
    let inc_b = Ulid::generate();

    let sink_a = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_b = Arc::new(std::sync::Mutex::new(Vec::new()));

    let post_a = {
        let sink = sink_a.clone();
        Arc::new(
            move |text: String, flow: String, run_id: String, wake: bool| {
                sink.lock().unwrap().push((text, flow, run_id, wake));
                Ok(())
            },
        )
    };
    let post_b = {
        let sink = sink_b.clone();
        Arc::new(
            move |text: String, flow: String, run_id: String, wake: bool| {
                sink.lock().unwrap().push((text, flow, run_id, wake));
                Ok(())
            },
        )
    };

    feed.register_session_incarnation(
        session_a,
        inc_a,
        Arc::new(|_| Box::pin(async { Ok(()) })),
        true,
    );
    feed.register_flow_poster_incarnation(session_a, inc_a, post_a);

    feed.register_session_incarnation(
        session_b,
        inc_b,
        Arc::new(|_| Box::pin(async { Ok(()) })),
        true,
    );
    feed.register_flow_poster_incarnation(session_b, inc_b, post_b);

    let target_a = ConversationTarget {
        workspace: PathBuf::from("/workspace"),
        session_id: session_a,
        incarnation: inc_a,
    };

    // Note for target A posts into sink A
    assert!(
        feed.post_targeted_flow_note(&target_a, "note A".into(), "f".into(), "r1".into(), false,)
            .is_ok()
    );
    assert_eq!(sink_a.lock().unwrap().len(), 1);
    assert!(sink_b.lock().unwrap().is_empty());

    // Invalidation of session A's incarnation rejects subsequent targeted notes
    feed.invalidate_session_incarnation(session_a, inc_a);
    let late_res = feed.post_targeted_flow_note(
        &target_a,
        "late note".into(),
        "f".into(),
        "r2".into(),
        false,
    );
    assert!(late_res.is_err());
    assert!(late_res.unwrap_err().contains("unavailable or superseded"));
}

#[tokio::test]
async fn targeted_snapshot_retains_target_digest_when_another_session_becomes_active() {
    let tmp = tempfile::tempdir().unwrap();
    let feed = ConversationFeed::with_workspace(tmp.path().to_path_buf());
    let session_a = Ulid::generate();
    let inc_a = Ulid::generate();
    let session_b = Ulid::generate();
    let inc_b = Ulid::generate();

    feed.register_session_incarnation(
        session_a,
        inc_a,
        Arc::new(|_| Box::pin(async { Ok(()) })),
        true,
    );
    feed.register_session_incarnation(
        session_b,
        inc_b,
        Arc::new(|_| Box::pin(async { Ok(()) })),
        true,
    );

    // Make Session A active and observe message in A
    feed.observe_incarnation(
        inc_a,
        &EventEnvelope::new(
            session_a,
            0,
            EventKind::MessageReceived,
            json!({"source": "user:http", "content": "message for session A"}),
        ),
    )
    .await;

    let target_a = ConversationTarget {
        workspace: tmp.path().to_path_buf(),
        session_id: session_a,
        incarnation: inc_a,
    };

    // Now Session B becomes active with a new message
    feed.observe_incarnation(
        inc_b,
        &EventEnvelope::new(
            session_b,
            0,
            EventKind::MessageReceived,
            json!({"source": "user:http", "content": "message for session B"}),
        ),
    )
    .await;

    // Active session is now B, and active digest is B's
    assert_eq!(feed.active_session(), Some(session_b));
    assert!(feed.digest().contains("message for session B"));
    assert!(!feed.digest().contains("message for session A"));

    // But snapshotting pinned target A returns target A and target A's digest (not B)
    let snap_a = feed.snapshot_target(&target_a);
    assert_eq!(snap_a.target, Some(target_a));
    assert!(snap_a.digest.contains("message for session A"));
    assert!(!snap_a.digest.contains("message for session B"));
}

struct BlockedFimModel {
    entered: Arc<tokio::sync::Notify>,
    token: Arc<std::sync::Mutex<Option<CancellationToken>>>,
}

struct ErrorAfterSignal {
    input: tokio::io::DuplexStream,
    fail: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
}

impl tokio::io::AsyncRead for ErrorAfterSignal {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.fail.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(std::io::Error::other("injected read error")));
        }
        std::pin::Pin::new(&mut self.input).poll_read(cx, buf)
    }
}

#[tokio::test]
async fn lsp_input_error_cancels_blocked_fim_and_detaches_editor() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceFeedRegistry::new();
    let feed = registry.get_or_create(dir.path());
    let entered = Arc::new(tokio::sync::Notify::new());
    let token = Arc::new(std::sync::Mutex::new(None));
    let fim = Arc::new(BlockedFimModel {
        entered: entered.clone(),
        token: token.clone(),
    });
    let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), dir.path().join("log"))
        .with_dirs(Some(dir.path().to_path_buf()), None)
        .with_registry(registry)
        .with_conversation(feed.clone())
        .with_fim(fim);
    let (server_input, client_write) = tokio::io::duplex(64 * 1024);
    let (client_read, server_output) = tokio::io::duplex(64 * 1024);
    let fail = CancellationToken::new();
    let reader = ErrorAfterSignal {
        input: server_input,
        fail: Box::pin(fail.clone().cancelled_owned()),
    };
    let server = tokio::spawn(withnav::navigator::serve(reader, server_output, deps));
    let mut client = common::navigator::LspClient::from_streams(client_write, client_read);
    client.initialize(json!({})).await;
    let uri = format!("file://{}/blocked.rs", dir.path().display());
    client.did_open(&uri, "fn x() {}", 1).await;
    client
        .send(
            &json!({"jsonrpc":"2.0","id":2,"method":"withnav/inlineCompletion",
        "params":{"uri":uri,"version":1,"line":0,"character":3}}),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("FIM entered");
    assert!(feed.select_editor(None).is_ok());
    let model_token = token.lock().unwrap().clone().expect("model token");
    fail.cancel();
    tokio::time::timeout(Duration::from_secs(1), model_token.cancelled())
        .await
        .expect("read error cancels FIM");
    assert!(feed.select_editor(None).is_err());
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("server exits")
        .unwrap();
}
impl FimModel for BlockedFimModel {
    fn complete<'a>(
        &'a self,
        _request: FimRequest,
        cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<FimAnswer, ModelError>> {
        let entered = self.entered.clone();
        let token = self.token.clone();
        let cancel = cancel.clone();
        Box::pin(async move {
            *token.lock().unwrap() = Some(cancel.clone());
            entered.notify_one();
            cancel.cancelled().await;
            Err(ModelError::Unavailable("cancelled".into()))
        })
    }
}

#[tokio::test]
async fn cross_editor_bound_and_unbound_reflex_input_and_context_isolation() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = WorkspaceFeedRegistry::new();
    let feed = registry.get_or_create(tmp.path());

    let session_a = Ulid::generate();
    let session_b = Ulid::generate();
    let inc_a = feed.register_session(session_a, Arc::new(|_| Box::pin(async { Ok(()) })));
    let inc_b = feed.register_session(session_b, Arc::new(|_| Box::pin(async { Ok(()) })));

    // Session A has distinct captain message
    feed.observe_incarnation(
        inc_a,
        &EventEnvelope::new(
            session_a,
            0,
            EventKind::MessageReceived,
            json!({"source": "user:http", "content": "Alpha task unique instructions"}),
        ),
    )
    .await;

    // Session B has distinct captain message
    feed.observe_incarnation(
        inc_b,
        &EventEnvelope::new(
            session_b,
            0,
            EventKind::MessageReceived,
            json!({"source": "user:http", "content": "Beta task unique instructions"}),
        ),
    )
    .await;

    let calls_a = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls_b = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fim_a = Arc::new(RecordingFimModel {
        calls: calls_a.clone(),
    });
    let fim_b = Arc::new(RecordingFimModel {
        calls: calls_b.clone(),
    });

    let reflex_context_a = Arc::new(common::navigator::ScriptedReflexContext::new(vec![Ok(
        "Alpha reflex summary text",
    )]));
    let reflex_context_b = Arc::new(common::navigator::ScriptedReflexContext::new(vec![Ok(
        "Beta reflex summary text",
    )]));

    let deps_a = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("a.jsonl"))
        .with_dirs(Some(tmp.path().to_path_buf()), None)
        .with_registry(registry.clone())
        .with_conversation(feed.clone())
        .with_fim(fim_a)
        .with_reflex_context(reflex_context_a.clone());

    let deps_b = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("b.jsonl"))
        .with_dirs(Some(tmp.path().to_path_buf()), None)
        .with_registry(registry.clone())
        .with_conversation(feed.clone())
        .with_fim(fim_b)
        .with_reflex_context(reflex_context_b.clone());

    let mut client_a = spawn_server_as_is(deps_a);
    let mut client_b = spawn_server_as_is(deps_b);

    let init_params = json!({
        "processId": null,
        "capabilities": {},
        "rootUri": format!("file://{}", tmp.path().display()),
        "workspaceFolders": [
            { "uri": format!("file://{}", tmp.path().display()), "name": "workspace" }
        ]
    });

    client_a
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init_params}))
        .await;
    let _ = client_a.recv().await;
    client_a
        .send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
        .await;

    // Capture editor A's registered id
    let editor_id_a = feed.select_editor(None).unwrap().id;

    client_b
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init_params}))
        .await;
    let _ = client_b.recv().await;
    client_b
        .send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
        .await;

    assert_eq!(feed.bind_editor(session_a, editor_id_a), Ok(()));
    // Session B remains unbound while another editor is connected.

    // Make session B active in the feed
    assert_eq!(feed.active_session(), Some(session_b));

    // Client A opens file and requests inline completion
    let uri_a = format!("file://{}/src/file_a.rs", tmp.path().display());
    client_a
        .did_open(&uri_a, "fn fa() {\n    let a = 1;\n}\n", 1)
        .await;
    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 20)).await;

    client_a
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri_a,
                "version": 1,
                "line": 1,
                "character": 4
            }
        }))
        .await;
    let res_a = client_a.recv().await;
    assert_eq!(res_a["id"], 10);

    // Verify ReflexContext A received Session A's digest and not Session B's
    assert_eq!(reflex_context_a.calls(), 1);
    let req_ref_a = &reflex_context_a.requests()[0];
    assert!(
        req_ref_a
            .conversation
            .contains("Alpha task unique instructions"),
        "Reflex context A must receive session A digest, got: {}",
        req_ref_a.conversation
    );
    assert!(
        !req_ref_a
            .conversation
            .contains("Beta task unique instructions"),
        "Reflex context A must not receive session B digest, got: {}",
        req_ref_a.conversation
    );

    // Client B opens file and requests inline completion
    let uri_b = format!("file://{}/src/file_b.rs", tmp.path().display());
    client_b
        .did_open(&uri_b, "fn fb() {\n    let b = 2;\n}\n", 1)
        .await;
    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 20)).await;

    client_b
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri_b,
                "version": 1,
                "line": 1,
                "character": 4
            }
        }))
        .await;
    let res_b = client_b.recv().await;
    assert_eq!(res_b["id"], 20);

    // Verify ReflexContext B received Session B's digest and not Session A's
    assert_eq!(reflex_context_b.calls(), 1);
    let req_ref_b = &reflex_context_b.requests()[0];
    assert!(
        req_ref_b.conversation.is_empty(),
        "unbound editor must not borrow a conversation"
    );
    assert!(
        !req_ref_b
            .conversation
            .contains("Alpha task unique instructions"),
        "Reflex context B must not receive session A digest, got: {}",
        req_ref_b.conversation
    );

    for (client, uri, id) in [(&mut client_a, &uri_a, 11), (&mut client_b, &uri_b, 21)] {
        client
            .send(&json!({"jsonrpc":"2.0", "id":id,
            "method":"withnav/inlineCompletion", "params":{
                "uri":uri, "version":1, "line":1, "character":4
            }}))
            .await;
        assert_eq!(client.recv().await["id"], id);
    }
    let a = calls_a.lock().unwrap();
    let b = calls_b.lock().unwrap();
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 2);
    assert!(a[1].prompt.contains("Alpha reflex summary text"));
    assert!(!a[1].prompt.contains("Beta reflex summary text"));
    assert!(b[1].prompt.contains("Beta reflex summary text"));
    assert!(!b[1].prompt.contains("Alpha reflex summary text"));
}

#[tokio::test]
async fn eof_and_serve_future_abort_with_blocked_work_cancels_and_deregisters() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = WorkspaceFeedRegistry::new();
    let feed = registry.get_or_create(tmp.path());

    let entered = Arc::new(tokio::sync::Notify::new());
    let token = Arc::new(std::sync::Mutex::new(None));
    let fim = Arc::new(BlockedFimModel {
        entered: entered.clone(),
        token: token.clone(),
    });

    let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"))
        .with_dirs(Some(tmp.path().to_path_buf()), None)
        .with_registry(registry.clone())
        .with_conversation(feed.clone())
        .with_fim(fim);

    let mut client = spawn_server_as_is(deps);

    let init_params = json!({
        "processId": null,
        "capabilities": {},
        "rootUri": format!("file://{}", tmp.path().display()),
        "workspaceFolders": [
            { "uri": format!("file://{}", tmp.path().display()), "name": "workspace" }
        ]
    });

    client
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init_params}))
        .await;
    let _ = client.recv().await;
    client
        .send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
        .await;

    // Editor is registered in feed
    assert!(feed.select_editor(None).is_ok());

    // Send inline completion to trigger blocked model call
    let uri = format!("file://{}/src/blocked.rs", tmp.path().display());
    client
        .did_open(&uri, "fn b() {\n    let x = 1;\n}\n", 1)
        .await;
    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 20)).await;

    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri,
                "version": 1,
                "line": 1,
                "character": 4
            }
        }))
        .await;

    // Wait until FIM model is entered and blocked
    entered.notified().await;

    // Part 1: Stream EOF (drop client) while model is in flight
    drop(client);

    // Observe the actual token independently: the select may drop the model future.
    let model_token = token.lock().unwrap().clone().expect("model entered");
    tokio::time::timeout(Duration::from_secs(1), model_token.cancelled())
        .await
        .expect("EOF cancels the model token");

    // Editor is cleanly unregistered from feed
    assert_eq!(
        feed.select_editor(None).unwrap_err(),
        withnav::navigator::conversation::EditorSelectionError::Unavailable
    );

    // Part 2: Serve future abort while work is in flight
    let entered_2 = Arc::new(tokio::sync::Notify::new());
    let token_2 = Arc::new(std::sync::Mutex::new(None));
    let fim_2 = Arc::new(BlockedFimModel {
        entered: entered_2.clone(),
        token: token_2.clone(),
    });

    let deps_2 = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log2.jsonl"))
        .with_dirs(Some(tmp.path().to_path_buf()), None)
        .with_registry(registry.clone())
        .with_conversation(feed.clone())
        .with_fim(fim_2);

    let (mut client_2, serve_handle_2) = common::navigator::spawn_server_with_handle(deps_2);

    client_2
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init_params}))
        .await;
    let _ = client_2.recv().await;
    client_2
        .send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
        .await;

    assert!(feed.select_editor(None).is_ok());

    let uri_2 = format!("file://{}/src/blocked2.rs", tmp.path().display());
    client_2
        .did_open(&uri_2, "fn b2() {\n    let x = 2;\n}\n", 1)
        .await;
    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 20)).await;

    client_2
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "withnav/inlineCompletion",
            "params": {
                "uri": uri_2,
                "version": 1,
                "line": 1,
                "character": 4
            }
        }))
        .await;

    entered_2.notified().await;

    // Abort the serve task future while model call is blocked
    serve_handle_2.abort();
    let _ = serve_handle_2.await;

    let model_token_2 = token_2.lock().unwrap().clone().expect("model entered");
    tokio::time::timeout(Duration::from_secs(1), model_token_2.cancelled())
        .await
        .expect("aborting serve cancels the model token");
    assert_eq!(
        feed.select_editor(None).unwrap_err(),
        withnav::navigator::conversation::EditorSelectionError::Unavailable
    );
}
