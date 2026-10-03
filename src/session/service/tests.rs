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
            parent,
            child,
            "explore".into(),
            "inspect".into(),
            "{}".into(),
            4,
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
