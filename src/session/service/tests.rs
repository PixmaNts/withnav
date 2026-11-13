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
    store.validate_test_documents();
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
    let records = crate::flows::records::Records::new(
        &service.config.data_dir,
        &dir.path().canonicalize().unwrap(),
    );
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
    assert!(transcript.iter().any(|message| matches!(message, ChatMessage::User { provenance: crate::provider::openai::UserProvenance::Flow { .. }, content, .. } if content.contains("routing evidence"))));
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
    crate::provider::auth::set_key(&dir.path().join("cfg"), "mock", "test").unwrap();
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
    assert_eq!(
        service.session_summary(id).await.unwrap().agent.as_deref(),
        Some("build")
    );
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
    let service = Service::new(
        config,
        Arc::new(crate::provider::openai::ClientCache::new()),
    )
    .expect("service over a temp database");
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

#[tokio::test]
async fn an_attachment_store_failure_on_a_message_is_a_server_error_without_its_detail() {
    let (dir, service) = test_service();
    // The store's table gone: the lookup fails with SQLite text.
    rusqlite::Connection::open(dir.path().join("history.db"))
        .unwrap()
        .execute_batch("DROP TABLE attachments")
        .unwrap();
    let mut command = Command {
        command_id: Ulid::generate(),
        session_id: Ulid::generate(),
        source_actor_id: "user:http".into(),
        target_actor_id: ROOT_ACTOR_ID.into(),
        mode: crate::bus::command::DeliveryMode::NextTurn,
        content: vec![crate::bus::command::ContentBlock::Image(
            withnav_client_api::attachment::ImageRef {
                attachment_id: "a1".into(),
                media_type: String::new(),
                name: String::new(),
                size: 0,
                width: 0,
                height: 0,
            },
        )],
        created_at: std::time::SystemTime::now(),
    };
    let error = service.resolve_images(&mut command).await.unwrap_err();
    assert!(matches!(error, ServerError::AttachmentStore), "{error}");
    assert!(!error.to_string().contains("attachments"), "{error}");
}

/// Keeps the host's sender of the last session started.
#[derive(Debug, Default)]
struct KeepSender(Mutex<Option<SessionSender>>);

impl SessionHooks for KeepSender {
    fn configure_actor(&self, _editor: bool, _actor: &mut crate::agent::actor::AgentActor) {}

    fn started(
        &self,
        _info: StartedSession,
        sender: SessionSender,
        _subscription: crate::session::Subscription,
        _cancel: CancellationToken,
    ) {
        *self.0.lock().unwrap() = Some(sender);
    }
}

#[tokio::test]
async fn a_host_message_is_published_with_the_secret_hidden() {
    const VALUE: &str = "sk-live-7f3a9c2e1d0b4a68";
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
    std::fs::write(
        dir.path().join("cfg/config.json"),
        r#"{"agents":{"build":{"model":"mock/test"}}}"#,
    )
    .unwrap();
    crate::provider::auth::set_key(&dir.path().join("cfg"), "mock", "test").unwrap();
    std::fs::write(dir.path().join("models.json"), r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/test":{"provider":"mock","model":"test","context_window":128000}}}"#).unwrap();
    let hooks = Arc::new(KeepSender::default());
    let service = Service::with_hooks(
        SessionConfig {
            models_path: None,
            config_dir: dir.path().join("cfg"),
            history_db: dir.path().join("history.db"),
            data_dir: dir.path().join("data"),
            rules_roots: Default::default(),
        },
        hooks.clone(),
        Arc::new(crate::provider::openai::ClientCache::new()),
    )
    .unwrap();
    let id = service
        .create_session(dir.path(), None, false)
        .await
        .unwrap();
    let sender = hooks
        .0
        .lock()
        .unwrap()
        .take()
        .expect("the host got a sender");
    // Stored after the session started: the sender sees the store as it is now.
    service
        .secrets
        .put(
            id,
            dir.path(),
            "API_KEY",
            VALUE,
            crate::secrets::SecretTtl::Day,
            crate::secrets::SecretScope::Session,
        )
        .unwrap();
    sender
        .send(
            format!("drift noticed near {VALUE}"),
            crate::navigator::NAVIGATOR_ACTOR_ID,
        )
        .await
        .unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    let crate::bus::ResumeOutcome::Replay(events) = sub.replay else {
        panic!("expected a replay");
    };
    let received = events
        .iter()
        .find(|event| event.kind == "message.received")
        .expect("the host message was accepted");
    let payload = received.payload.to_string();
    assert!(payload.contains("[secret API_KEY]"), "{payload}");
    assert!(!payload.contains(VALUE), "{payload}");
    service.close_all().await;
}

#[tokio::test]
async fn service_settings_wiring_discovers_configured_skills() {
    let (dir, service) = test_service();
    std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
    std::fs::write(
        dir.path().join("cfg/config.json"),
        r#"{"agents":{"build":{"model":"mock/test"}},"skills":{"paths":["custom_skills"]}}"#,
    )
    .unwrap();
    crate::provider::auth::set_key(&service.config.config_dir, "mock", "test").unwrap();
    std::fs::write(
        dir.path().join("models.json"),
        r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/test":{"provider":"mock","model":"test","context_window":128000}}}"#,
    )
    .unwrap();
    let skill_dir = dir.path().join("custom_skills/my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Test skill\n---\nBody of my-skill",
    )
    .unwrap();

    let id = service
        .create_session(dir.path(), None, false)
        .await
        .expect("session created");
    let handle = service.handle(id).expect("session handle");
    assert!(matches!(
        handle.skills.resolve("my-skill"),
        crate::agent::skills::Resolution::Found(_)
    ));
    assert!(handle.skills.catalog().unwrap().contains("my-skill"));
}

// The server and its clients read the session body through one shared type: a summary the
// service builds must survive serialisation and decode back unchanged, live and once stopped.
#[tokio::test]
async fn session_summary_round_trips_through_the_client_wire_type() {
    let (dir, service) = test_service();
    std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
    std::fs::write(
        dir.path().join("cfg/config.json"),
        r#"{"agents":{"build":{"model":"mock/old"}}}"#,
    )
    .unwrap();
    crate::provider::auth::set_key(&service.config.config_dir, "mock", "test").unwrap();
    std::fs::write(dir.path().join("models.json"), r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/old":{"provider":"mock","model":"old","context_window":128000}}}"#).unwrap();
    let id = service
        .create_session(dir.path(), None, false)
        .await
        .unwrap();
    let live = service.session_summary(id).await.unwrap();
    service.close_session(id).await.unwrap();
    let stored = service.session_summary(id).await.unwrap();
    assert_eq!(stored.actor_state, "stopped");
    for summary in [live, stored] {
        let wire = serde_json::to_value(&summary).unwrap();
        let decoded: withnav_client_api::protocol::SessionMetadata =
            serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(decoded, summary);
        assert_eq!(decoded.source.as_deref(), Some("config"));
        assert_eq!(decoded.agent.as_deref(), Some("build"));
        assert_eq!(wire["gateway"], "mock");
    }
}

#[tokio::test]
async fn listing_fifty_conversations_costs_one_store_read() {
    let (dir, service) = test_service();
    let parent = Ulid::generate();
    service
        .history()
        .record_session(parent, "/workspace", "default", false)
        .await
        .unwrap();
    for _ in 0..48 {
        service
            .history()
            .record_session(Ulid::generate(), "/workspace", "default", false)
            .await
            .unwrap();
    }
    let child = Ulid::generate();
    service
        .history()
        .record_session(child, "/workspace", "default", false)
        .await
        .unwrap();
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
    let db = dir.path().join("history.db");
    let before = crate::session::store::read_count::get(&db);
    let listed = service.list("/workspace").await.unwrap();
    assert_eq!(crate::session::store::read_count::get(&db) - before, 1);
    assert_eq!(listed.len(), 50);
    let row = listed.iter().find(|row| row.id == child).unwrap();
    assert_eq!(
        row.summary.parent_session_id.as_deref(),
        Some(parent.to_string().as_str())
    );
    assert_eq!(row.summary.actor_state, "stopped");
    assert!(!row.summary.promptable);
}

#[tokio::test]
async fn two_actors_created_by_one_service_share_client_cache_instance() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.json"),
        r#"{"agents":{"build":{"model":"mock/test"}}}"#,
    )
    .unwrap();
    crate::provider::auth::set_key(&cfg_dir, "mock", "test").unwrap();
    std::fs::write(
        dir.path().join("models.json"),
        r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/test":{"provider":"mock","model":"test","context_window":128000}}}"#,
    )
    .unwrap();

    let client_cache = Arc::new(crate::provider::openai::ClientCache::new());
    let service = Service::new(
        SessionConfig {
            models_path: None,
            config_dir: cfg_dir,
            history_db: dir.path().join("history.db"),
            data_dir: dir.path().join("data"),
            rules_roots: Default::default(),
        },
        client_cache.clone(),
    )
    .unwrap();

    let id1 = service
        .create_session(dir.path(), None, false)
        .await
        .unwrap();
    let id2 = service
        .create_session(dir.path(), None, false)
        .await
        .unwrap();

    let handle1 = service.handle(id1).unwrap();
    let task1 = handle1.runtime_task.lock().unwrap().take().unwrap();
    handle1.session_cancel.cancel();
    let actor1 = task1.await.unwrap();

    let handle2 = service.handle(id2).unwrap();
    let task2 = handle2.runtime_task.lock().unwrap().take().unwrap();
    handle2.session_cancel.cancel();
    let actor2 = task2.await.unwrap();

    assert!(Arc::ptr_eq(&actor1.client_cache, &service.client_cache));
    assert!(Arc::ptr_eq(&actor2.client_cache, &service.client_cache));
    assert!(Arc::ptr_eq(&actor1.client_cache, &actor2.client_cache));
}
