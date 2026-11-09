//! Deleting a session runs the same secrets cleanup as closing it: the
//! values whose availability lasts "as long as this conversation runs" end
//! with the conversation, and a timed workspace secret outlives it.

use withnav::error::ServerError;
use withnav::secrets::{SecretScope, SecretSubmission, SecretTtl, SecretView};
use withnav::session::{Service, SessionId};

mod common;
use common::server::TestServerConfig;

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

    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config().split().0)
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

    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config().split().0)
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
