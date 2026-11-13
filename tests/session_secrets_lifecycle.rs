//! Service-level tests for `revoke_secret` and `submit_secret` lifecycle semantics.
//!
//! Covers:
//! - Root-session access versus child-session rejection (`ServerError::ChildReadOnly`).
//! - Session- versus workspace-scoped secret revocation and name shadowing.
//! - First-answer-wins concurrency race on a pending secret request.
//! - Stale decline rejection after request settlement.
//! - Late provision (`provide_later`) replacing stored secrets and routing host notes.
//! - Failed command routing after successful secret storage.
//! - Workspace-level secret sharing across multiple independent root sessions.

#![deny(clippy::all, warnings)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

mod common;
use common::server::{TestServerConfig, collect_until, send_text};
use common::sse::{http_sse, spawn_mock_provider, sse_body, text_chunk, tool_call_chunk};
use withnav::agent::actor::AgentActor;
use withnav::error::ServerError;
use withnav::secrets::{SecretScope, SecretSubmission, SecretTtl};
use withnav::server::session::AppState;
use withnav::session::{SessionHooks, SessionSender, StartedSession, Subscription};

fn tool_turn(id: &str, name: &str, args: Value) -> Vec<u8> {
    http_sse(&sse_body(&[&tool_call_chunk(id, name, &args)]))
}

fn text_turn(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

fn request_secret_turn(req_id: &str, secret_name: &str) -> Vec<u8> {
    tool_turn(
        req_id,
        "request_secret",
        json!({
            "name": secret_name,
            "label": "API Key",
            "reason": "authentication for external service"
        }),
    )
}

#[derive(Debug, Default)]
struct CaptureCancelHooks {
    cancel_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<CancellationToken>>>,
}

impl SessionHooks for CaptureCancelHooks {
    fn configure_actor(&self, _editor: bool, _actor: &mut AgentActor) {}

    fn started(
        &self,
        _info: StartedSession,
        _sender: SessionSender,
        _subscription: Subscription,
        cancel: CancellationToken,
    ) {
        if let Some(tx) = self
            .cancel_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = tx.send(cancel);
        }
    }
}

/// Given an active root session and a child session spawned from it,
/// When secret queries, revocations, and submissions are directed to the child session,
/// Then all child operations fail with `ServerError::ChildReadOnly` while root session calls succeed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_session_access_vs_child_rejection() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let state = TestServerConfig::unreachable(tmp.path())
            .allow_everything()
            .state();

        let root_id = state.create_session(&ws, None, false).await.unwrap();
        let child_id = state
            .create_child_session(root_id, "vigie", "inspect child behavior")
            .await
            .unwrap();

        // Child session rejections
        let list_err = state.list_secrets(child_id).await.unwrap_err();
        assert!(matches!(list_err, ServerError::ChildReadOnly));

        let revoke_err = state.revoke_secret(child_id, "ANY_KEY").await.unwrap_err();
        assert!(matches!(revoke_err, ServerError::ChildReadOnly));

        let submit_err = state
            .submit_secret(
                child_id,
                "req_1",
                SecretSubmission {
                    name: Some("ANY_KEY".into()),
                    value: Some("sk-secret-val-12345".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(submit_err, ServerError::ChildReadOnly));

        // Root session success
        let initial_secrets = state.list_secrets(root_id).await.unwrap();
        assert!(initial_secrets.is_empty());

        let unrevoked = state.revoke_secret(root_id, "NONEXISTENT").await.unwrap();
        assert!(!unrevoked);

        let accepted = state
            .submit_secret(
                root_id,
                "req_root",
                SecretSubmission {
                    name: Some("ROOT_KEY".into()),
                    value: Some("sk-root-token-9999".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(accepted.accepted);

        let listed = state.list_secrets(root_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "ROOT_KEY");
        assert_eq!(listed[0].last4, "9999");

        let revoked = state.revoke_secret(root_id, "ROOT_KEY").await.unwrap();
        assert!(revoked);
        assert!(state.list_secrets(root_id).await.unwrap().is_empty());

        state.close_all().await;
    })
    .await
    .expect("test timed out");
}

/// Given a root session holding both session-scoped and workspace-scoped secrets,
/// When one scoped secret is revoked,
/// Then the other scope remains intact; and when both scopes share the same secret name,
/// revoking removes the shadowing session secret first, revealing the workspace-scoped value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_vs_workspace_scoped_revoke() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let state = TestServerConfig::unreachable(tmp.path())
            .allow_everything()
            .state();

        let id = state.create_session(&ws, None, false).await.unwrap();

        // 1. Distinct secret names with distinct scopes
        state
            .submit_secret(
                id,
                "sub_session",
                SecretSubmission {
                    name: Some("SESSION_VAR".into()),
                    value: Some("session-secret-val-1111".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        state
            .submit_secret(
                id,
                "sub_workspace",
                SecretSubmission {
                    name: Some("WORKSPACE_VAR".into()),
                    value: Some("workspace-secret-val-2222".into()),
                    scope: SecretScope::Workspace,
                    ttl: SecretTtl::Day,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let listed = state.list_secrets(id).await.unwrap();
        assert_eq!(listed.len(), 2);

        // Revoking session secret leaves workspace secret intact
        let revoked_session = state.revoke_secret(id, "SESSION_VAR").await.unwrap();
        assert!(revoked_session);

        let listed_after = state.list_secrets(id).await.unwrap();
        assert_eq!(listed_after.len(), 1);
        assert_eq!(listed_after[0].name, "WORKSPACE_VAR");
        assert_eq!(listed_after[0].scope, SecretScope::Workspace);

        let revoked_workspace = state.revoke_secret(id, "WORKSPACE_VAR").await.unwrap();
        assert!(revoked_workspace);
        assert!(state.list_secrets(id).await.unwrap().is_empty());

        // 2. Same secret name: session scope shadows workspace scope
        state
            .submit_secret(
                id,
                "shadow_ws",
                SecretSubmission {
                    name: Some("SHARED_NAME".into()),
                    value: Some("workspace-hidden-3333".into()),
                    scope: SecretScope::Workspace,
                    ttl: SecretTtl::Day,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        state
            .submit_secret(
                id,
                "shadow_ses",
                SecretSubmission {
                    name: Some("SHARED_NAME".into()),
                    value: Some("session-priority-4444".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let shadowed = state.list_secrets(id).await.unwrap();
        assert_eq!(shadowed.len(), 1);
        assert_eq!(shadowed[0].name, "SHARED_NAME");
        assert_eq!(shadowed[0].scope, SecretScope::Session);
        assert_eq!(shadowed[0].last4, "4444");

        // First revoke removes the session-scoped secret
        let revoked_first = state.revoke_secret(id, "SHARED_NAME").await.unwrap();
        assert!(revoked_first);

        // Uncovered workspace secret is now visible
        let revealed = state.list_secrets(id).await.unwrap();
        assert_eq!(revealed.len(), 1);
        assert_eq!(revealed[0].name, "SHARED_NAME");
        assert_eq!(revealed[0].scope, SecretScope::Workspace);
        assert_eq!(revealed[0].last4, "3333");

        // Second revoke removes the workspace secret
        let revoked_second = state.revoke_secret(id, "SHARED_NAME").await.unwrap();
        assert!(revoked_second);
        assert!(state.list_secrets(id).await.unwrap().is_empty());

        state.close_all().await;
    })
    .await
    .expect("test timed out");
}

/// Given an agent paused waiting on a secret request,
/// When two submissions concurrently race to settle that request,
/// Then exactly one submission succeeds and the other fails with `ServerError::SecretStale`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_answer_wins_race() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let provider = spawn_mock_provider(vec![
            request_secret_turn("sec_race", "API_KEY"),
            text_turn("turn completed successfully"),
        ])
        .await;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let state = TestServerConfig::mock(tmp.path(), &provider.url)
            .allow_everything()
            .state();

        let id = state.create_session(&ws, None, false).await.unwrap();

        let sub = state.subscribe(id, None).await.unwrap();
        send_text(&state, id, "begin operation requiring secret").await;
        collect_until(sub, |e| e.kind == "agent.yield").await;

        // Reply to this pending request only. Supplying a name also permits a later
        // replacement after the first answer settles, which is a different API operation.
        let race_a = state.submit_secret(
            id,
            "sec_race",
            SecretSubmission {
                name: None,
                value: Some("sk-winner-choice-1111".into()),
                scope: SecretScope::Session,
                ttl: SecretTtl::Session,
                decline: false,
            },
        );
        let race_b = state.submit_secret(
            id,
            "sec_race",
            SecretSubmission {
                name: None,
                value: Some("sk-loser-choice-2222".into()),
                scope: SecretScope::Session,
                ttl: SecretTtl::Session,
                decline: false,
            },
        );

        let (res_a, res_b) = tokio::join!(race_a, race_b);
        let (winner, loser) = match (res_a, res_b) {
            (Ok(w), Err(l)) => (w, l),
            (Err(l), Ok(w)) => (w, l),
            other => panic!("expected exactly one Ok and one Err, got {other:?}"),
        };

        assert!(winner.accepted);
        assert!(matches!(loser, ServerError::SecretStale));

        let secrets = state.list_secrets(id).await.unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].name, "API_KEY");
        assert!(secrets[0].last4 == "1111" || secrets[0].last4 == "2222");

        state.close_all().await;
    })
    .await
    .expect("test timed out");
}

/// Given a pending secret request that has already been settled by a value submission,
/// When a late decline is submitted for that request,
/// Then the decline is rejected with `ServerError::SecretStale` and the stored secret remains unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_decline_rejected() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let provider = spawn_mock_provider(vec![
            request_secret_turn("sec_stale", "AUTH_TOKEN"),
            text_turn("token processed"),
        ])
        .await;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let state = TestServerConfig::mock(tmp.path(), &provider.url)
            .allow_everything()
            .state();

        let id = state.create_session(&ws, None, false).await.unwrap();

        let sub = state.subscribe(id, None).await.unwrap();
        send_text(&state, id, "authenticate now").await;
        collect_until(sub, |e| e.kind == "agent.yield").await;

        // Settle the request with an initial value
        let settled = state
            .submit_secret(
                id,
                "sec_stale",
                SecretSubmission {
                    value: Some("sk-initial-valid-token-1234".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    decline: false,
                    name: None,
                },
            )
            .await
            .unwrap();
        assert!(settled.accepted);

        // Attempting to decline an already settled request yields SecretStale
        let stale_decline = state
            .submit_secret(
                id,
                "sec_stale",
                SecretSubmission {
                    decline: true,
                    name: None,
                    value: None,
                    ttl: SecretTtl::Session,
                    scope: SecretScope::Session,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(stale_decline, ServerError::SecretStale));

        // The stored secret was preserved
        let stored = state.list_secrets(id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "AUTH_TOKEN");
        assert_eq!(stored[0].last4, "1234");

        state.close_all().await;
    })
    .await
    .expect("test timed out");
}

/// Given a conversation where a secret was either already settled or never requested,
/// When `submit_secret` provides a secret value with a name (`provide_later`),
/// Then an existing secret is silently replaced without duplicate note routing,
/// and an unprompted secret is stored and announced via a host note.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_provision_replaces_and_announces() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let provider = spawn_mock_provider(vec![
            request_secret_turn("sec_late", "DEPLOY_KEY"),
            text_turn("done deployment"),
        ])
        .await;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let state = TestServerConfig::mock(tmp.path(), &provider.url)
            .allow_everything()
            .state();

        let id = state.create_session(&ws, None, false).await.unwrap();

        let sub = state.subscribe(id, None).await.unwrap();
        send_text(&state, id, "deploy service").await;
        collect_until(sub, |e| e.kind == "agent.yield").await;

        // 1. Settle the waiting request
        state
            .submit_secret(
                id,
                "sec_late",
                SecretSubmission {
                    value: Some("sk-orig-key-val-1111".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    decline: false,
                    name: None,
                },
            )
            .await
            .unwrap();

        let initial = state.list_secrets(id).await.unwrap();
        assert_eq!(initial[0].last4, "1111");

        // 2. Late replacement for the settled request id: requires `name`
        let missing_name = state
            .submit_secret(
                id,
                "sec_late",
                SecretSubmission {
                    value: Some("sk-replaced-key-val-2222".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    decline: false,
                    name: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(missing_name, ServerError::SecretStale));

        let replaced = state
            .submit_secret(
                id,
                "sec_late",
                SecretSubmission {
                    name: Some("DEPLOY_KEY".into()),
                    value: Some("sk-replaced-key-val-2222".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    decline: false,
                },
            )
            .await
            .unwrap();
        assert!(replaced.accepted);

        let updated = state.list_secrets(id).await.unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].name, "DEPLOY_KEY");
        assert_eq!(updated[0].last4, "2222");

        // 3. Unprompted late provision introduces a new secret and routes a host note
        let note_sub = state.subscribe(id, None).await.unwrap();
        let unprompted = state
            .submit_secret(
                id,
                "unprompted_id",
                SecretSubmission {
                    name: Some("NEW_EXTRA_KEY".into()),
                    value: Some("sk-new-token-value-3333".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    decline: false,
                },
            )
            .await
            .unwrap();
        assert!(unprompted.accepted);

        let note_events = collect_until(note_sub, |e| {
            e.kind == "message.received"
                && e.payload["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("The Captain provided a secret"))
        })
        .await;
        assert!(!note_events.is_empty());

        let final_list = state.list_secrets(id).await.unwrap();
        assert_eq!(final_list.len(), 2);
        assert!(
            final_list
                .iter()
                .any(|v| v.name == "NEW_EXTRA_KEY" && v.last4 == "3333")
        );

        state.close_all().await;
    })
    .await
    .expect("test timed out");
}

/// Given a session whose command transport ceases between secret storage and message delivery,
/// When `submit_secret` answers a pending request,
/// Then the secret is successfully stored in the secrets store while the caller observes `ServerError::SessionStopping`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_routing_after_storage_preserves_secret() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let provider = spawn_mock_provider(vec![
            request_secret_turn("sec_fail_route", "FAIL_ROUTE_KEY"),
            text_turn("done"),
        ])
        .await;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let hooks = Arc::new(CaptureCancelHooks {
            cancel_tx: std::sync::Mutex::new(Some(cancel_tx)),
        });

        let config = TestServerConfig::mock(tmp.path(), &provider.url)
            .allow_everything()
            .config();
        let state = AppState::with_hooks(
            config,
            hooks,
            Arc::new(withnav_provider::openai::ClientCache::new()),
        )
        .unwrap();

        let id = state.create_session(&ws, None, false).await.unwrap();
        let session_cancel = cancel_rx.await.unwrap();

        let sub = state.subscribe(id, None).await.unwrap();
        send_text(&state, id, "perform secure task").await;
        collect_until(sub, |e| e.kind == "agent.yield").await;

        // Terminate the runtime actor task; drops the inbox receiver while leaving session active
        session_cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let err = state
            .submit_secret(
                id,
                "sec_fail_route",
                SecretSubmission {
                    name: Some("FAIL_ROUTE_KEY".into()),
                    value: Some("sk-stored-despite-failure-5555".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    decline: false,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ServerError::SessionStopping));

        // Secret was stored despite the routing failure
        let stored = state.list_secrets(id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "FAIL_ROUTE_KEY");
        assert_eq!(stored[0].last4, "5555");

        state.close_all().await;
    })
    .await
    .expect("test timed out");
}

/// Given two separate root sessions sharing the same workspace directory,
/// When a workspace-scoped secret is created in session A,
/// Then it is visible in session B and revoking it from session B removes it for both sessions,
/// while a session-scoped secret created in session A is neither visible nor revocable in session B.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_sharing_workspace() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let state = TestServerConfig::unreachable(tmp.path())
            .allow_everything()
            .state();

        let id_a = state.create_session(&ws, None, false).await.unwrap();
        let id_b = state.create_session(&ws, None, false).await.unwrap();

        // Session A adds a workspace-scoped secret and a session-scoped secret
        state
            .submit_secret(
                id_a,
                "shared_sub",
                SecretSubmission {
                    name: Some("SHARED_WS_KEY".into()),
                    value: Some("sk-shared-workspace-val-7777".into()),
                    scope: SecretScope::Workspace,
                    ttl: SecretTtl::Day,
                    decline: false,
                },
            )
            .await
            .unwrap();

        state
            .submit_secret(
                id_a,
                "private_sub",
                SecretSubmission {
                    name: Some("PRIVATE_SESSION_KEY".into()),
                    value: Some("sk-private-session-val-8888".into()),
                    scope: SecretScope::Session,
                    ttl: SecretTtl::Session,
                    decline: false,
                },
            )
            .await
            .unwrap();

        // Verification of visibility in Session A
        let list_a = state.list_secrets(id_a).await.unwrap();
        assert_eq!(list_a.len(), 2);
        assert!(list_a.iter().any(|v| v.name == "SHARED_WS_KEY"));
        assert!(list_a.iter().any(|v| v.name == "PRIVATE_SESSION_KEY"));

        // Verification of visibility in Session B
        let list_b = state.list_secrets(id_b).await.unwrap();
        assert_eq!(list_b.len(), 1);
        assert_eq!(list_b[0].name, "SHARED_WS_KEY");
        assert_eq!(list_b[0].scope, SecretScope::Workspace);
        assert_eq!(list_b[0].last4, "7777");

        // Session B revokes the shared workspace secret
        let revoked_shared = state.revoke_secret(id_b, "SHARED_WS_KEY").await.unwrap();
        assert!(revoked_shared);

        // Disappears from Session B
        assert!(state.list_secrets(id_b).await.unwrap().is_empty());

        // Disappears from Session A, leaving only Session A's private secret
        let list_a_after = state.list_secrets(id_a).await.unwrap();
        assert_eq!(list_a_after.len(), 1);
        assert_eq!(list_a_after[0].name, "PRIVATE_SESSION_KEY");

        // Session B cannot revoke Session A's private secret
        let revoke_private_from_b = state
            .revoke_secret(id_b, "PRIVATE_SESSION_KEY")
            .await
            .unwrap();
        assert!(!revoke_private_from_b);

        // Session A's private secret is still intact
        let list_a_still = state.list_secrets(id_a).await.unwrap();
        assert_eq!(list_a_still.len(), 1);
        assert_eq!(list_a_still[0].name, "PRIVATE_SESSION_KEY");

        // Session A revokes its own private secret
        let revoke_private_from_a = state
            .revoke_secret(id_a, "PRIVATE_SESSION_KEY")
            .await
            .unwrap();
        assert!(revoke_private_from_a);
        assert!(state.list_secrets(id_a).await.unwrap().is_empty());

        state.close_all().await;
    })
    .await
    .expect("test timed out");
}
