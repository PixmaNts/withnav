use super::*;

#[tokio::test]
async fn switching_uses_one_captured_endpoint_variant_and_source_generation() {
    let (dir, service) = test_service();
    std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
    std::fs::write(
        dir.path().join("cfg/config.json"),
        r#"{"agents":{"build":{"model":"mock/old"},"plan":{"model":"mock/old","variant":"low"}}}"#,
    )
    .unwrap();
    crate::provider::auth::set_key(&service.config.config_dir, "mock", "test").unwrap();
    std::fs::write(dir.path().join("models.json"),r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"key":{"provider":"mock","model":"old","context_window":128000,"variants":{"low":{"reasoning_effort":"low"}}}}}"#).unwrap();
    let id = service
        .create_session(dir.path(), None, None)
        .await
        .unwrap();
    let barrier = Arc::new(crate::session::run_command::Barrier::default());
    *service.preparation_barrier.lock().unwrap() = Some(barrier.clone());
    let task = tokio::spawn({
        let service = service.clone();
        async move { service.run_command(id, "agent", "plan").await }
    });
    barrier.reached.notified().await;
    std::fs::write(dir.path().join("cfg/config.json"), "{}").unwrap();
    std::fs::write(dir.path().join("models.json"),r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:2/v1"}},"models":{"key":{"provider":"mock","model":"new","context_window":128000}}}"#).unwrap();
    barrier.release.notify_one();
    assert!(task.await.unwrap().unwrap().ok);
    let meta = service.history.meta(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "mock/old");
    assert_eq!(meta.variant.as_deref(), Some("low"));
    assert_eq!(
        meta.variant_override.as_deref(),
        Some(r#"{"reasoning_effort":"low"}"#)
    );
    assert!(meta.model_resolution.unwrap().contains("127.0.0.1:1"));
    assert_eq!(
        meta.provenance.unwrap().model,
        crate::session::store::ModelSource::Config
    );
}

#[tokio::test]
async fn preparation_uses_captured_assignment_catalog_and_variant_generation() {
    let (dir, service) = test_service();
    std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
    std::fs::write(
        dir.path().join("cfg/config.json"),
        r#"{"agents":{"build":{"model":"mock/old","variant":"off"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("cfg/auth.json"),
        r#"{"mock":{"api_key":"test"}}"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.path().join("cfg/auth.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(dir.path().join("models.json"), r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"key":{"provider":"mock","model":"old","context_window":128000}}}"#).unwrap();
    let barrier = Arc::new(crate::session::run_command::Barrier::default());
    *service.preparation_barrier.lock().unwrap() = Some(barrier.clone());
    let task = tokio::spawn({
        let service = service.clone();
        let workspace = dir.path().to_path_buf();
        async move { service.create_session(&workspace, None, None).await }
    });
    barrier.reached.notified().await;
    std::fs::write(
        dir.path().join("cfg/config.json"),
        r#"{"default_agent":"plan","agents":{"plan":{"model":"mock/new"}}}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("models.json"), r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:2/v1"}},"models":{"key":{"provider":"mock","model":"new","context_window":128000}}}"#).unwrap();
    barrier.release.notify_one();
    let id = task.await.unwrap().unwrap();
    let meta = service.history.meta(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "mock/old");
    assert_eq!(meta.variant, None);
    assert!(meta.model_resolution.unwrap().contains("127.0.0.1:1"));
    assert_eq!(service.session_summary(id).await.unwrap().agent, "build");
}

/// A service over its own temporary history database; keep the directory
/// alive as long as the service.
pub(crate) fn test_service() -> (tempfile::TempDir, Service) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = SessionConfig {
        models_path: None,
        config_dir: dir.path().join("cfg"),
        history_db: dir.path().join("history.db"),
        data_dir: dir.path().join("data"),
        rules_roots: Default::default(),
    };
    let service = Service::new(config).expect("service over a temp database");
    (dir, service)
}

#[tokio::test]
async fn admission_wait_is_cancelled_without_acquiring_the_gate() {
    let (_dir, service) = test_service();
    let cancel = CancellationToken::new();
    let gate = service.admission_gate.clone().lock_owned().await;
    let release = cancel.clone();
    let holder = tokio::spawn(async move {
        release.cancelled().await;
        drop(gate);
    });
    cancel.cancel();
    assert!(matches!(
        service.child_admission(&cancel).await,
        Err(ServerError::SessionStopping)
    ));
    holder.await.unwrap();
}

#[tokio::test]
async fn concurrent_terminal_winners_release_exactly_their_reservations() {
    let (_dir, service) = test_service();
    let parent = Ulid::new();
    service
        .history()
        .record_session(parent, "/w", "default", None)
        .await
        .unwrap();
    service.subagent_slots.lock().unwrap().insert(parent, 4);
    let children = [Ulid::new(), Ulid::new(), Ulid::new(), Ulid::new()];
    for child in children {
        service
            .history()
            .record_session(child, "/w", "default", None)
            .await
            .unwrap();
        service
            .history()
            .create_child_link(
                child,
                "explore".into(),
                "goal".into(),
                "{}".into(),
                4,
                crate::bus::event::EventEnvelope::new(
                    parent,
                    service
                        .history
                        .meta(parent)
                        .await
                        .unwrap()
                        .unwrap()
                        .last_seq
                        + 1,
                    EventKind::SubagentSpawned,
                    serde_json::json!({}),
                ),
            )
            .await
            .unwrap();
    }
    let (a, b) = tokio::join!(
        service.finish_child(parent, children[0], "done", None, "a".into()),
        service.finish_child(parent, children[1], "done", None, "b".into())
    );
    a.unwrap();
    b.unwrap();
    service
        .finish_child(parent, children[0], "failed", None, "late".into())
        .await
        .unwrap();
    assert_eq!(service.subagent_slots.lock().unwrap()[&parent], 2);
    let reservation = ChildReservation {
        slots: service.subagent_slots.clone(),
        parent,
        committed: false,
    };
    service.subagent_slots.lock().unwrap().insert(parent, 3);
    drop(reservation);
    assert_eq!(service.subagent_slots.lock().unwrap()[&parent], 2);
}

#[tokio::test]
async fn listing_hides_child_until_relationship_commits() {
    let (_dir, service) = test_service();
    let parent = Ulid::new();
    let child = Ulid::new();
    service
        .history()
        .record_session(parent, "/workspace", "default", None)
        .await
        .unwrap();
    service.unpublished_children.lock().unwrap().insert(child);
    service
        .history()
        .record_session(child, "/workspace", "default", None)
        .await
        .unwrap();
    let during = service.list("/workspace").await.unwrap();
    assert_eq!(during.len(), 1);
    assert_eq!(during[0].id, parent);
    service
        .history()
        .create_child_link(
            child,
            "explore".into(),
            "inspect".into(),
            "{}".into(),
            4,
            crate::bus::event::EventEnvelope::new(
                parent,
                0,
                EventKind::SubagentSpawned,
                serde_json::json!({}),
            ),
        )
        .await
        .unwrap();
    let after = service.list("/workspace").await.unwrap();
    let row = after.iter().find(|row| row.id == child).unwrap();
    assert_eq!(
        row.parent_session_id.as_deref(),
        Some(parent.to_string().as_str())
    );
    assert!(!row.promptable);
}
