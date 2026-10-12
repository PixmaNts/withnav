use super::*;

#[tokio::test]
async fn admitted_message_routes_flow_effect_before_consumption() {
    struct Client;
    impl instinct_flow::DecisionClient for Client {
        fn capabilities(&self) -> instinct_flow::Capabilities {
            instinct_flow::Capabilities { images: false }
        }
        fn identity(&self) -> instinct_flow::ClientIdentity {
            instinct_flow::ClientIdentity {
                model: "fake/instinct".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            _: instinct_flow::protocol::DecisionRequest<'a>,
            _: &'a CancellationToken,
            _: std::time::Instant,
        ) -> instinct_flow::BoxFuture<
            'a,
            Result<instinct_flow::protocol::DecisionResponse, instinct_flow::DecisionError>,
        > {
            Box::pin(async { Err(instinct_flow::DecisionError::ProviderError) })
        }
    }
    let (dir, service) = test_service();
    std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
    std::fs::write(
        dir.path().join("cfg/config.json"),
        r#"{"agents":{"build":{"model":"mock/test"}}}"#,
    )
    .unwrap();
    crate::provider::auth::set_key(&service.config.config_dir, "mock", "test").unwrap();
    std::fs::write(dir.path().join("models.json"), r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/test":{"provider":"mock","model":"test","context_window":128000}}}"#).unwrap();
    service.flows.inject_client(dir.path(), Arc::new(Client));
    let store = service.flows.store(dir.path().into());
    store.save("route", crate::flows::store::Location::Global, None, crate::flows::store::Content::Toml {
        toml: "[flow]\nversion=1\nname='route'\non='message.received'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='inject_note'\n[node.params]\ntext='routing evidence'\n".into(), templates: Default::default(),
    }).unwrap();
    store.reload().unwrap();
    let id = service
        .create_session(dir.path(), None, false)
        .await
        .unwrap();
    assert!(service.handle(id).unwrap().flows.is_some());
    let command = crate::bus::command::Command {
        command_id: Ulid::generate(),
        session_id: id,
        source_actor_id: "user:test".into(),
        target_actor_id: ROOT_ACTOR_ID.into(),
        mode: crate::bus::command::DeliveryMode::NextTurn,
        content: vec![crate::bus::command::ContentBlock::Text {
            text: "captain task".into(),
        }],
        created_at: std::time::SystemTime::now(),
    };
    service.send(command).await.unwrap();
    // Captures are persisted by a background writer, off the blocking admission path: wait for
    // the record instead of expecting it synchronously.
    let records = crate::flows::records::Records::new(&service.config.data_dir, dir.path());
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let applied = records.read().unwrap().iter().any(|record| {
                record.flow_name == "route" && record.application.status == "applied"
            });
            if applied {
                break;
            }
            tokio::task::yield_now().await;
        }
        // turn.start proves the admitted input (including the flow note) was
        // consumed. Join that actor rather than waiting for the dead provider's retries.
        let sub = service.subscribe(id, None).await.unwrap();
        let crate::bus::ResumeOutcome::Replay(events) = sub.replay else {
            panic!("expected routing turn replay");
        };
        if !events.iter().any(|event| event.kind == "turn.start") {
            let mut live = sub.live.unwrap();
            loop {
                if live.recv().await.unwrap().kind == "turn.start" {
                    break;
                }
            }
        }
        service.close_all().await;
    })
    .await
    .expect("applied routing flow must be consumed and its runtime joined");
    let transcript = service.history.load(id).await.unwrap().unwrap().transcript;
    assert!(transcript.iter().any(|message| matches!(message, ChatMessage::User { provenance: crate::provider::openai::UserProvenance::Flow { .. }, content } if content.contains("routing evidence"))));
}

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
    std::fs::write(dir.path().join("models.json"),r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/old":{"provider":"mock","model":"old","context_window":128000,"variants":{"low":{"reasoning_effort":"low"}}}}}"#).unwrap();
    let id = service
        .create_session(dir.path(), None, false)
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
    std::fs::write(dir.path().join("models.json"),r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:2/v1"}},"models":{"mock/new":{"provider":"mock","model":"new","context_window":128000}}}"#).unwrap();
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.path().join("cfg/auth.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    std::fs::write(dir.path().join("models.json"), r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/old":{"provider":"mock","model":"old","context_window":128000}}}"#).unwrap();
    let barrier = Arc::new(crate::session::run_command::Barrier::default());
    *service.preparation_barrier.lock().unwrap() = Some(barrier.clone());
    let task = tokio::spawn({
        let service = service.clone();
        let workspace = dir.path().to_path_buf();
        async move { service.create_session(&workspace, None, false).await }
    });
    barrier.reached.notified().await;
    std::fs::write(
        dir.path().join("cfg/config.json"),
        r#"{"default_agent":"plan","agents":{"plan":{"model":"mock/new"}}}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("models.json"), r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:2/v1"}},"models":{"mock/new":{"provider":"mock","model":"new","context_window":128000}}}"#).unwrap();
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
    let parent = Ulid::generate();
    service
        .history()
        .record_session(parent, "/w", "default", false)
        .await
        .unwrap();
    service.subagent_slots.lock().unwrap().insert(parent, 4);
    let children = [
        Ulid::generate(),
        Ulid::generate(),
        Ulid::generate(),
        Ulid::generate(),
    ];
    for child in children {
        service
            .history()
            .record_session(child, "/w", "default", false)
            .await
            .unwrap();
        service
            .history()
            .create_child_link(
                child,
                "vigie".into(),
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
    let parent = Ulid::generate();
    let child = Ulid::generate();
    service
        .history()
        .record_session(parent, "/workspace", "default", false)
        .await
        .unwrap();
    service.unpublished_children.lock().unwrap().insert(child);
    service
        .history()
        .record_session(child, "/workspace", "default", false)
        .await
        .unwrap();
    let during = service.list("/workspace").await.unwrap();
    assert_eq!(during.len(), 1);
    assert_eq!(during[0].id, parent);
    service
        .history()
        .create_child_link(
            child,
            "vigie".into(),
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
