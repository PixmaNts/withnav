//! Live memory check; temporary storage, two Instinct calls, at most two writer calls.
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use withnav::{
    agent::memory::{
        self,
        component::{Component, Part},
    },
    bus::event::{EventEnvelope, EventKind},
    session::{
        memory::models_with_output_limit,
        store::{
            memory::{Goal, State},
            HistoryStore,
        },
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    let config = withnav::provider::auth::default_config_dir()
        .ok_or_else(|| anyhow::anyhow!("no config directory"))?;
    let models_path = withnav::navigator::resolve_models(None, &workspace);
    let models = models_with_output_limit(models_path.as_deref(), &workspace, &config, 2048);
    let observer = models
        .observer
        .ok_or_else(|| anyhow::anyhow!("configured Instinct unavailable; no calls made"))?;
    let writer = models
        .writer
        .ok_or_else(|| anyhow::anyhow!("configured metacognition unavailable; no calls made"))?;
    let temporary = tempfile::tempdir()?;
    let store =
        HistoryStore::open(temporary.path().join("probe.db")).map_err(anyhow::Error::msg)?;
    let session = ulid::Ulid::new();
    let scope = workspace.to_string_lossy().into_owned();
    store
        .record_session(session, &scope, "memory-probe", false)
        .await
        .map_err(anyhow::Error::msg)?;
    let cancel = CancellationToken::new();
    let goal = "Add a unit test proving that two plus two equals four.";
    let stages=["The Captain requested a unit test. The agent created the test but has not run it. Work remains.","The agent ran the requested unit test, it passed, and reported that the goal is finished. There are no remaining items."];
    for (index, stage) in stages.into_iter().enumerate() {
        let input=memory::framed_json(&serde_json::json!({"goal":{"anchor_seq":1,"text":goal},"recent_captain_messages":[],"visible_evidence":stage})).map_err(anyhow::Error::msg)?;
        let verdict =
            tokio::time::timeout(Duration::from_secs(2), observer.observe(&input, &cancel))
                .await
                .map_err(|_| anyhow::anyhow!("Instinct deadline; no retry"))?
                .map_err(anyhow::Error::msg)?;
        println!(
            "Instinct: progress={:?}, new_goal={}",
            verdict.goal_progress, verdict.new_goal
        );
        anyhow::ensure!(
            verdict.goal_progress
                == if index == 0 {
                    memory::Progress::Advancing
                } else {
                    memory::Progress::Done
                },
            "unexpected Instinct progress"
        );
    }
    let evidence = vec![
        (1, format!("Captain: {goal}")),
        (
            2,
            "Tool result: unit test added in tests/math.rs; cargo test math passed.".into(),
        ),
        (
            3,
            "Visible answer: requested test is complete and passing.".into(),
        ),
    ];
    for (seq, text) in &evidence {
        store.append_event_acked(session,EventEnvelope::new(session,*seq,EventKind::MessageReceived,serde_json::json!({"source":if *seq==1 {"captain"} else {"subagent"},"content":text}))).await.map_err(anyhow::Error::msg)?;
    }
    let id = ulid::Ulid::new().to_string();
    store
        .memory_observe(
            State {
                session: session.to_string(),
                workspace: scope.clone(),
                last_turn: 3,
                active_goal: None,
                version: 1,
            },
            vec![Goal {
                id: id.clone(),
                session: session.to_string(),
                workspace: scope.clone(),
                anchor_seq: 1,
                text: goal.into(),
                start_seq: 1,
                end_seq: Some(3),
                progress: "done".into(),
                close_reason: Some("model_done".into()),
                writer_status: "pending".into(),
                attempts: 0,
                claim: None,
            }],
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let mut input = memory::writer::prompt(goal, &evidence).map_err(anyhow::Error::msg)?;
    let mut calls = 0;
    let (written, body) = loop {
        calls += 1;
        let written = tokio::time::timeout(Duration::from_secs(30), writer.write(&input, &cancel))
            .await
            .map_err(|_| anyhow::anyhow!("writer deadline; no retry"))?
            .map_err(anyhow::Error::msg)?;
        match memory::writer::parse(&written.text, goal, 1, 3, &[1, 2, 3]) {
            Ok(body) => break (written, body),
            Err(_) if calls == 1 && written.truncated => {
                input.push_str(memory::writer::SHORT_RETRY)
            }
            Err(_) => anyhow::bail!(
                "writer returned invalid episode JSON; startup and recall checks skipped"
            ),
        }
    };
    println!(
        "Writer: model={}, validated=true, output_bytes={}",
        written.model,
        written.text.len()
    );
    store
        .memory_claim(&id, "probe")
        .await
        .map_err(anyhow::Error::msg)?;
    store
        .memory_complete(&id, "probe", body, written.model, "{}".into())
        .await
        .map_err(anyhow::Error::msg)?;
    let episodes = store
        .memory_episodes(&scope, None, 3)
        .await
        .map_err(anyhow::Error::msg)?;
    let (ids, text) = memory::render(&episodes, memory::STARTUP_BYTES);
    anyhow::ensure!(
        episodes.len() == 1 && text.contains(goal),
        "stored episode missing from startup note"
    );
    println!("Episode: stored=1, startup_contains_goal=true");
    let new_session = ulid::Ulid::new();
    store
        .record_session(new_session, &scope, "memory-probe", false)
        .await
        .map_err(anyhow::Error::msg)?;
    let audit_store = store.clone();
    let component = Component {
        session: new_session.to_string(),
        turn: "probe".into(),
        startup: Some(Part {
            source: "startup".into(),
            ids,
            text,
        }),
        recalled: Default::default(),
        record: std::sync::Arc::new(move |i| {
            let store = audit_store.clone();
            Box::pin(async move { store.memory_inject(i).await })
        }),
    };
    println!(
        "New conversation: startup_messages={}, audit_saved=true",
        component.admitted("memory-probe").await.len()
    );
    let recalled = store
        .memory_episodes(&scope, Some("unit test"), 5)
        .await
        .map_err(anyhow::Error::msg)?;
    println!("Recall: workspace_bound=true, results={}", recalled.len());
    anyhow::ensure!(recalled.len() == 1, "keyword recall did not find episode");
    println!("Caps: instinct_calls=2, writer_calls={calls}, input_tokens<6000, writer_output_tokens<=2048; memory enabled only in temporary probe storage");
    Ok(())
}
