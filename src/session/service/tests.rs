use super::*;

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
