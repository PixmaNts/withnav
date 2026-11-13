//! Lifecycle tests for `secrets.end_session`: verifying that closing or deleting
//! a root session purges session-scoped and conversation-scoped secrets while
//! timed workspace-scoped secrets survive for other sessions sharing the workspace.

use std::time::Duration;

use withnav::bus::ResumeOutcome;
use withnav::error::ServerError;
use withnav::secrets::{SecretScope, SecretSubmission, SecretTtl, SecretView};
use withnav::session::{Service, SessionId};

mod common;
use common::server::TestServerConfig;

/// Submits a secret value to `session` with the chosen `scope` and `ttl`.
async fn submit_secret(
    service: &Service,
    session: SessionId,
    name: &str,
    value: &str,
    scope: SecretScope,
    ttl: SecretTtl,
) {
    service
        .submit_secret(
            session,
            &format!("req-{}", ulid::Ulid::generate()),
            SecretSubmission {
                name: Some(name.to_string()),
                value: Some(value.to_string()),
                ttl,
                scope,
                decline: false,
            },
        )
        .await
        .expect("submit_secret must succeed");
}

fn secret_names(views: &[SecretView]) -> Vec<&str> {
    views.iter().map(|v| v.name.as_str()).collect()
}

/// Stores `name`=`value` for `session`'s conversation tree, with the chosen
/// scope and ttl, through the same entry point the Captain's "Provide anyway"
/// uses (no request waits).
async fn store_secret(
    service: &Service,
    session: SessionId,
    name: &str,
    value: &str,
    scope: SecretScope,
    ttl: SecretTtl,
) -> SecretView {
    service
        .submit_secret(
            session,
            &format!("late-{}", ulid::Ulid::generate()),
            SecretSubmission {
                name: Some(name.to_string()),
                value: Some(value.to_string()),
                ttl,
                scope,
                decline: false,
            },
        )
        .await
        .expect("store secret")
        .secret
        .expect("stored view")
}

fn names(views: &[SecretView]) -> Vec<&str> {
    views.iter().map(|v| v.name.as_str()).collect()
}

// Given a workspace with two root sessions A and B sharing the directory
// When session A stores a session-scoped secret and a workspace-scoped secret,
//      session B stores a session-scoped secret, and session A is closed
// Then session A's session-scoped secret is purged by secrets.end_session,
//      the workspace-scoped secret remains visible to both resumed A and B,
//      and session B's own session secret is unaffected
#[tokio::test]
async fn close_session_purges_session_secrets_and_preserves_workspace_secrets() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");

    let service = Service::new(
        TestServerConfig::unreachable(tmp.path()).config().split().0,
        std::sync::Arc::new(withnav_provider::openai::ClientCache::new()),
    )
    .expect("service");

    let session_a = service
        .create_session(&ws, None, false)
        .await
        .expect("create session A");
    let session_b = service
        .create_session(&ws, None, false)
        .await
        .expect("create session B");

    // Session A stores both a session-scoped secret (lasts as long as the conversation)
    // and a workspace-scoped secret (persisted across conversations in the workspace).
    submit_secret(
        &service,
        session_a,
        "A_SESSION_SECRET",
        "val-session-a",
        SecretScope::Session,
        SecretTtl::Session,
    )
    .await;
    submit_secret(
        &service,
        session_a,
        "SHARED_WS_SECRET",
        "val-workspace-shared",
        SecretScope::Workspace,
        SecretTtl::Day,
    )
    .await;

    // Session B stores its own session-scoped secret in the same workspace.
    submit_secret(
        &service,
        session_b,
        "B_SESSION_SECRET",
        "val-session-b",
        SecretScope::Session,
        SecretTtl::Session,
    )
    .await;

    // Initial state: A sees its session secret and the workspace secret.
    let a_secrets_initial = service
        .list_secrets(session_a)
        .await
        .expect("list secrets A");
    let a_names = secret_names(&a_secrets_initial);
    assert!(
        a_names.contains(&"A_SESSION_SECRET"),
        "A sees its session secret: {a_names:?}"
    );
    assert!(
        a_names.contains(&"SHARED_WS_SECRET"),
        "A sees the workspace secret: {a_names:?}"
    );
    assert!(
        !a_names.contains(&"B_SESSION_SECRET"),
        "A does not see B's session secret: {a_names:?}"
    );

    // Initial state: B sees its session secret and the workspace secret.
    let b_secrets_initial = service
        .list_secrets(session_b)
        .await
        .expect("list secrets B");
    let b_names = secret_names(&b_secrets_initial);
    assert!(
        b_names.contains(&"B_SESSION_SECRET"),
        "B sees its session secret: {b_names:?}"
    );
    assert!(
        b_names.contains(&"SHARED_WS_SECRET"),
        "B sees the workspace secret: {b_names:?}"
    );
    assert!(
        !b_names.contains(&"A_SESSION_SECRET"),
        "B does not see A's session secret: {b_names:?}"
    );

    // Close session A.
    // In src/session/lifecycle.rs:69-72, Service::close_session invokes
    // self.secrets.end_session(id) when parent_session_id.is_none().
    service
        .close_session(session_a)
        .await
        .expect("close session A");

    // While closed, listing secrets on A requires a resume because the handle is gone from memory.
    assert!(matches!(
        service.list_secrets(session_a).await,
        Err(ServerError::ResumeRequired)
    ));

    // The event log for A terminates with `session.end`.
    let sub = service
        .subscribe(session_a, None)
        .await
        .expect("subscribe to closed A");
    assert!(
        sub.live.is_none(),
        "a closed session has no live event channel"
    );
    let ResumeOutcome::Replay(events) = sub.replay else {
        panic!("expected replay for closed session");
    };
    assert_eq!(
        events.last().map(|e| e.kind.as_str()),
        Some("session.end"),
        "closed session event log concludes with session.end"
    );

    // Session B remains active and untouched: its own session secret and the shared
    // workspace secret are both present.
    let b_secrets_after = service
        .list_secrets(session_b)
        .await
        .expect("list secrets B after A closed");
    let b_names_after = secret_names(&b_secrets_after);
    assert!(
        b_names_after.contains(&"B_SESSION_SECRET"),
        "B retains its session secret: {b_names_after:?}"
    );
    assert!(
        b_names_after.contains(&"SHARED_WS_SECRET"),
        "B retains the workspace secret: {b_names_after:?}"
    );
    assert!(
        !b_names_after.contains(&"A_SESSION_SECRET"),
        "A's session secret is not visible: {b_names_after:?}"
    );

    // Resuming session A restores an active handle; listing its secrets proves
    // that A's session-scoped secret was dropped while the workspace secret remains.
    let (resumed_id, resumed_flag) = service
        .create_or_resume_session(&ws, None, false, Some(session_a))
        .await
        .expect("resume session A");
    assert_eq!(resumed_id, session_a);
    assert!(resumed_flag, "session A was resumed");

    let a_secrets_resumed = service
        .list_secrets(session_a)
        .await
        .expect("list secrets resumed A");
    let a_names_resumed = secret_names(&a_secrets_resumed);
    assert!(
        !a_names_resumed.contains(&"A_SESSION_SECRET"),
        "A's session secret was dropped on end_session: {a_names_resumed:?}"
    );
    assert!(
        a_names_resumed.contains(&"SHARED_WS_SECRET"),
        "A retains the workspace secret after resume: {a_names_resumed:?}"
    );

    service.close_all().await;
}

// Given a workspace with session C holding session and workspace secrets, and
//      session B holding its own session secret
// When session C is deleted through Service::delete
// Then session C becomes completely unroutable (SessionNotFound), while
//      the workspace-scoped secret and session B's session secret remain
#[tokio::test]
async fn delete_session_removes_session_and_preserves_workspace_secrets() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");

    let service = Service::new(
        TestServerConfig::unreachable(tmp.path()).config().split().0,
        std::sync::Arc::new(withnav_provider::openai::ClientCache::new()),
    )
    .expect("service");

    let session_c = service
        .create_session(&ws, None, false)
        .await
        .expect("create session C");
    let session_b = service
        .create_session(&ws, None, false)
        .await
        .expect("create session B");

    submit_secret(
        &service,
        session_c,
        "C_SESSION_SECRET",
        "val-session-c",
        SecretScope::Session,
        SecretTtl::Session,
    )
    .await;
    submit_secret(
        &service,
        session_c,
        "SHARED_WS_SECRET_2",
        "val-workspace-shared-2",
        SecretScope::Workspace,
        SecretTtl::Day,
    )
    .await;
    submit_secret(
        &service,
        session_b,
        "B_SESSION_SECRET",
        "val-session-b",
        SecretScope::Session,
        SecretTtl::Session,
    )
    .await;

    // Delete session C.
    // In src/session/catalog.rs:79, Service::delete calls close_session_gated(id),
    // ends conversation secrets for roots, and discards history and registry entries.
    // Consequence: delete purges C from both memory and disk history, so any
    // routing or secret query for C fails with SessionNotFound.
    service.delete(session_c).await.expect("delete session C");

    // Querying C is now an explicit SessionNotFound error.
    assert!(matches!(
        service.list_secrets(session_c).await,
        Err(ServerError::SessionNotFound(_))
    ));
    assert!(matches!(
        service
            .create_or_resume_session(&ws, None, false, Some(session_c))
            .await,
        Err(ServerError::SessionNotFound(_))
    ));

    // Session B is still alive and sees the workspace secret created in workspace W
    // alongside its own session secret.
    let b_secrets = service
        .list_secrets(session_b)
        .await
        .expect("list secrets B");
    let b_names = secret_names(&b_secrets);
    assert!(
        b_names.contains(&"B_SESSION_SECRET"),
        "B retains its session secret: {b_names:?}"
    );
    assert!(
        b_names.contains(&"SHARED_WS_SECRET_2"),
        "Workspace secret remains available after session deletion: {b_names:?}"
    );
    assert!(
        !b_names.contains(&"C_SESSION_SECRET"),
        "C's session secret is not visible in B: {b_names:?}"
    );

    service.close_all().await;
}

// Given a root session with session and workspace secrets and an active child session
// When the child session is closed
// Then close_session observes parent_session_id.is_some() and skips
//      secrets.end_session, preserving all root session secrets
#[tokio::test]
async fn closing_child_session_does_not_purge_root_secrets() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");

    let service = Service::new(
        TestServerConfig::unreachable(tmp.path()).config().split().0,
        std::sync::Arc::new(withnav_provider::openai::ClientCache::new()),
    )
    .expect("service");

    let parent = service
        .create_session(&ws, None, false)
        .await
        .expect("create parent session");

    submit_secret(
        &service,
        parent,
        "PARENT_SESSION_SECRET",
        "val-session-parent",
        SecretScope::Session,
        SecretTtl::Session,
    )
    .await;
    submit_secret(
        &service,
        parent,
        "PARENT_WS_SECRET",
        "val-workspace-parent",
        SecretScope::Workspace,
        SecretTtl::Day,
    )
    .await;

    // Spawn a child subagent under the parent.
    let child = tokio::time::timeout(
        Duration::from_secs(5),
        service.create_child_session(parent, "vigie", "inspect sources"),
    )
    .await
    .expect("child creation deadline")
    .expect("create child session");

    // Child sessions cannot list or mutate secrets directly at the API boundary;
    // secret_handle rejects child sessions with ChildReadOnly (src/session/secrets.rs:50).
    assert!(matches!(
        service.list_secrets(child).await,
        Err(ServerError::ChildReadOnly)
    ));
    assert!(matches!(
        service
            .submit_secret(
                child,
                "req-child",
                SecretSubmission {
                    name: Some("CHILD_SECRET".to_string()),
                    value: Some("child-val".to_string()),
                    ttl: SecretTtl::Session,
                    scope: SecretScope::Session,
                    decline: false,
                },
            )
            .await,
        Err(ServerError::ChildReadOnly)
    ));

    // Closing the child session routes through finish_child in src/session/lifecycle.rs:74.
    // In src/session/lifecycle.rs:69-72, parent.is_none() is false for the child,
    // so self.secrets.end_session is NOT called.
    tokio::time::timeout(Duration::from_secs(5), service.close_session(child))
        .await
        .expect("child close deadline")
        .expect("close child session");

    // Observable consequence: the parent's session secret and workspace secret both persist.
    let parent_secrets = service
        .list_secrets(parent)
        .await
        .expect("list secrets parent");
    let parent_names = secret_names(&parent_secrets);
    assert!(
        parent_names.contains(&"PARENT_SESSION_SECRET"),
        "Parent session secret persists after child close: {parent_names:?}"
    );
    assert!(
        parent_names.contains(&"PARENT_WS_SECRET"),
        "Parent workspace secret persists after child close: {parent_names:?}"
    );

    // Closing the parent itself now triggers secrets.end_session because parent.is_none() is true.
    tokio::time::timeout(Duration::from_secs(5), service.close_session(parent))
        .await
        .expect("parent close deadline")
        .expect("close parent session");

    // After resuming parent, its session secret is gone, but workspace secret survives.
    let (resumed_id, resumed_flag) = service
        .create_or_resume_session(&ws, None, false, Some(parent))
        .await
        .expect("resume parent");
    assert_eq!(resumed_id, parent);
    assert!(resumed_flag);

    let parent_secrets_resumed = service
        .list_secrets(parent)
        .await
        .expect("list secrets resumed parent");
    let resumed_names = secret_names(&parent_secrets_resumed);
    assert!(
        !resumed_names.contains(&"PARENT_SESSION_SECRET"),
        "Parent session secret purged when root closed: {resumed_names:?}"
    );
    assert!(
        resumed_names.contains(&"PARENT_WS_SECRET"),
        "Parent workspace secret survived root close: {resumed_names:?}"
    );

    service.close_all().await;
}

// Given two root sessions A and B sharing a workspace, where A holds a
// session-availability secret (scope session), a workspace secret that lasts
// this conversation (scope workspace, ttl session) and a timed workspace
// secret (ttl 24h), and B holds its own session-availability secret
// When session A is DELETED
// Then every secret that was to last "as long as this conversation runs" is
//      gone: B no longer sees A's workspace conversation secret, A's own
//      session secret is unreachable, B keeps its own, and A's timed
//      workspace secret survives for B
#[tokio::test]
async fn delete_session_ends_conversation_secrets_and_keeps_timed_workspace_ones() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");

    let service = Service::new(
        TestServerConfig::unreachable(tmp.path()).config().split().0,
        std::sync::Arc::new(withnav_provider::openai::ClientCache::new()),
    )
    .expect("service");

    let a = service
        .create_session(&ws, None, false)
        .await
        .expect("create A");
    let b = service
        .create_session(&ws, None, false)
        .await
        .expect("create B");

    store_secret(
        &service,
        a,
        "A_SESSION_VALUE",
        "a-session-value",
        SecretScope::Session,
        SecretTtl::Session,
    )
    .await;
    store_secret(
        &service,
        a,
        "A_CONVERSATION_WS_VALUE",
        "a-conversation-ws-value",
        SecretScope::Workspace,
        SecretTtl::Session,
    )
    .await;
    store_secret(
        &service,
        a,
        "A_TIMED_WS_VALUE",
        "a-timed-ws-value",
        SecretScope::Workspace,
        SecretTtl::Day,
    )
    .await;
    store_secret(
        &service,
        b,
        "B_SESSION_VALUE",
        "b-session-value",
        SecretScope::Session,
        SecretTtl::Session,
    )
    .await;

    // Sanity before the delete: B sees every secret but A's session one.
    let before_list = service.list_secrets(b).await.expect("list B");
    let before = names(&before_list);
    assert!(
        before.contains(&"A_CONVERSATION_WS_VALUE"),
        "B sees A's workspace conversation secret: {before:?}"
    );
    assert!(
        before.contains(&"A_TIMED_WS_VALUE"),
        "B sees A's timed secret: {before:?}"
    );
    assert!(
        before.contains(&"B_SESSION_VALUE"),
        "B sees its own: {before:?}"
    );
    assert!(
        !before.contains(&"A_SESSION_VALUE"),
        "B never sees A's session secret: {before:?}"
    );

    service.delete(a).await.expect("delete A");

    // The conversation A held is gone: neither its secrets nor its history
    // can be reached again.
    assert!(matches!(
        service.list_secrets(a).await,
        Err(ServerError::SessionNotFound(_))
    ));
    assert!(matches!(
        service
            .create_or_resume_session(&ws, None, false, Some(a))
            .await,
        Err(ServerError::SessionNotFound(_))
    ));

    // B no longer sees the secret that was to last as long as A ran, but
    // keeps its own and A's timed workspace secret.
    let after_list = service.list_secrets(b).await.expect("list B after delete");
    let after = names(&after_list);
    assert_eq!(
        after,
        vec!["A_TIMED_WS_VALUE", "B_SESSION_VALUE"],
        "only the timed workspace secret and B's own survive the delete"
    );

    service.close_all().await;
}

// Given a root session with a session-availability secret and a live child
// When the CHILD is deleted
// Then the root's secret survives: the cleanup belongs to the conversation
//      the secret was given to, and a child owns none
#[tokio::test]
async fn deleting_a_child_keeps_the_roots_secrets() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");

    let service = Service::new(
        TestServerConfig::unreachable(tmp.path()).config().split().0,
        std::sync::Arc::new(withnav_provider::openai::ClientCache::new()),
    )
    .expect("service");

    let root = service
        .create_session(&ws, None, false)
        .await
        .expect("create root");
    store_secret(
        &service,
        root,
        "ROOT_SESSION_VALUE",
        "root-session-value",
        SecretScope::Session,
        SecretTtl::Session,
    )
    .await;
    store_secret(
        &service,
        root,
        "ROOT_CONVERSATION_WS_VALUE",
        "root-conversation-ws-value",
        SecretScope::Workspace,
        SecretTtl::Session,
    )
    .await;

    let child = service
        .create_child_session(root, "vigie", "check the build")
        .await
        .expect("create child");

    service.delete(child).await.expect("delete child");

    let root_list = service.list_secrets(root).await.expect("list root");
    let listed = names(&root_list);
    assert_eq!(
        listed,
        vec!["ROOT_CONVERSATION_WS_VALUE", "ROOT_SESSION_VALUE"],
        "the root keeps every secret after its child was deleted"
    );

    service.close_all().await;
}
