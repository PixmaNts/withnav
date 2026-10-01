//! Reflex service tests: UTF-16 positions with non-ASCII text, superseded
//! requests cancelled, the context block capped, the debounce — over a
//! scripted FIM fake and tokio's paused clock.
//!
//! Requests run as spawned tasks (not lazily-joined futures) so
//! registration order and virtual time are deterministic.

use std::sync::Arc;
use std::time::Duration;

use rust_agent::navigator::protocol::Position;
use rust_agent::navigator::reflex::{
    build_fim_request, render_context_block, ReflexService, DEBOUNCE_MS, MAX_CONTEXT_BYTES,
    MAX_PREFIX_BYTES, MAX_SUFFIX_BYTES,
};

mod common;
use common::navigator::ScriptedFim;

const URI: &str = "file:///ws/src/main.rs";

fn service(model: ScriptedFim) -> Arc<ReflexService> {
    Arc::new(ReflexService::new(Arc::new(model)))
}

/// Runs one request as a task so it registers immediately.
fn spawn_suggest(
    svc: &Arc<ReflexService>,
    uri: &str,
    doc: &str,
    version: i32,
    position: Position,
    context: &str,
) -> tokio::task::JoinHandle<
    Result<
        rust_agent::navigator::reflex::Suggestion,
        rust_agent::navigator::reflex::SuggestOutcome,
    >,
> {
    let svc = Arc::clone(svc);
    let uri = uri.to_string();
    let doc = doc.to_string();
    let context = context.to_string();
    tokio::spawn(async move {
        svc.suggest(&uri, &doc, version, position, &context, "rust")
            .await
    })
}

/// A non-ASCII document, cursor after an emoji: the request prompt must be
/// split at the right byte offset, not the UTF-16 count.
#[tokio::test(start_paused = true)]
async fn non_ascii_cursor_splits_prompt_at_the_byte_offset() {
    let model = ScriptedFim::new(vec!["world".to_string()]);
    let svc = service(model.clone());
    // "héllo\n😀x" — line 1, after the emoji (2 UTF-16 units).
    let doc = "héllo\n😀x";
    let suggestion = spawn_suggest(
        &svc,
        URI,
        doc,
        1,
        Position {
            line: 1,
            character: 2,
        },
        "",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(suggestion.item.text, "world");
    // Insertion range starts exactly at the cursor...
    assert_eq!(
        suggestion.item.range.start,
        Position {
            line: 1,
            character: 2
        }
    );
    // ...and the model saw prefix "héllo\n😀" (11 bytes) and suffix "x".
    let req = model.last_request();
    assert_eq!(req.prompt, "héllo\n😀");
    assert_eq!(req.suffix, "x");
}

/// The reflex context is rendered into the prompt as a capped comment block
/// above the code prefix.
#[tokio::test(start_paused = true)]
async fn reflex_context_renders_as_comment_block() {
    let model = ScriptedFim::new(vec!["x".to_string()]);
    let svc = service(model.clone());
    let _ = spawn_suggest(
        &svc,
        URI,
        "fn a() {",
        1,
        Position {
            line: 0,
            character: 8,
        },
        "intent: write tests\nsig: pub fn run()",
    )
    .await
    .unwrap()
    .unwrap();
    let req = model.last_request();
    assert!(req.prompt.starts_with("// navigator context"));
    assert!(req.prompt.contains("// intent: write tests"));
    assert!(req.prompt.contains("// sig: pub fn run()"));
    assert!(req.prompt.ends_with("fn a() {"));
}

/// A newer request for the same document during the older one's debounce
/// cancels it before the model is called.
#[tokio::test(start_paused = true)]
async fn newer_request_cancels_the_older_one() {
    let model = ScriptedFim::new(vec!["first".to_string()]);
    let svc = service(model.clone());
    let doc = "fn main() {}";

    let older = spawn_suggest(
        &svc,
        URI,
        doc,
        1,
        Position {
            line: 0,
            character: 4,
        },
        "",
    );
    // Let the older request register and enter its debounce sleep.
    tokio::time::advance(Duration::from_millis(50)).await;
    // A newer request for the same document at a later version.
    let newer = spawn_suggest(
        &svc,
        URI,
        doc,
        2,
        Position {
            line: 0,
            character: 4,
        },
        "",
    );
    tokio::time::advance(Duration::from_millis(DEBOUNCE_MS + 100)).await;

    let outcome_older = older.await.unwrap();
    let suggestion = newer.await.unwrap().unwrap();
    // The superseded request never surfaced and never called the model.
    assert!(outcome_older.is_err());
    assert_eq!(suggestion.item.text, "first");
    assert_eq!(
        model.calls(),
        1,
        "the older request must not call the model"
    );
}

/// A request superseded while the model is answering never surfaces; the
/// newer one gets the next answer.
#[tokio::test(start_paused = true)]
async fn superseded_while_answering_never_surfaces() {
    let model = ScriptedFim::new(vec!["late".to_string(), "fresh".to_string()]).with_delay(100);
    let svc = service(model.clone());

    let older = spawn_suggest(
        &svc,
        URI,
        "fn main() {}",
        5,
        Position {
            line: 0,
            character: 4,
        },
        "",
    );
    // Past the debounce: drive the runtime (advance wakes the timer but
    // never polls the task) until the older request is inside the model
    // call, then supersede it.
    for _ in 0..1000 {
        tokio::time::advance(Duration::from_millis(10)).await;
        if model.calls() == 1 {
            break;
        }
    }
    assert_eq!(model.calls(), 1, "the older request must reach the model");
    let newer = spawn_suggest(
        &svc,
        URI,
        "fn main() { ",
        6,
        Position {
            line: 0,
            character: 4,
        },
        "",
    );
    tokio::time::advance(Duration::from_millis(DEBOUNCE_MS + 300)).await;

    let outcome_older = older.await.unwrap();
    let outcome_newer = newer.await.unwrap();
    assert!(outcome_older.is_err(), "stale answer must not surface");
    assert_eq!(outcome_newer.unwrap().item.text, "fresh");
}

/// A model error or empty text resolves as ModelSilent, never a panic.
#[tokio::test(start_paused = true)]
async fn empty_model_answer_is_silent() {
    let svc = service(ScriptedFim::new(vec![String::new()]));
    let outcome = spawn_suggest(
        &svc,
        URI,
        "fn a() {}",
        1,
        Position {
            line: 0,
            character: 4,
        },
        "",
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.unwrap_err(),
        rust_agent::navigator::reflex::SuggestOutcome::ModelSilent
    );
}

/// An out-of-bounds position resolves as ModelSilent without calling the
/// model.
#[tokio::test(start_paused = true)]
async fn out_of_bounds_position_is_silent_without_a_model_call() {
    let model = ScriptedFim::new(vec!["x".to_string()]);
    let svc = service(model.clone());
    let outcome = spawn_suggest(
        &svc,
        URI,
        "fn a() {}",
        1,
        Position {
            line: 99,
            character: 0,
        },
        "",
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.unwrap_err(),
        rust_agent::navigator::reflex::SuggestOutcome::ModelSilent
    );
    assert_eq!(model.calls(), 0);
}

/// The debounce holds the request back: nothing is sent before it elapses.
#[tokio::test(start_paused = true)]
async fn nothing_is_sent_before_the_debounce_elapses() {
    let model = ScriptedFim::new(vec!["x".to_string()]);
    let svc = service(model.clone());
    let task = spawn_suggest(
        &svc,
        URI,
        "fn a() {}",
        1,
        Position {
            line: 0,
            character: 4,
        },
        "",
    );
    tokio::time::advance(Duration::from_millis(DEBOUNCE_MS - 1)).await;
    assert_eq!(model.calls(), 0, "the debounce must hold the request");
    tokio::time::advance(Duration::from_millis(2)).await;
    task.await.unwrap().unwrap();
    assert_eq!(model.calls(), 1);
}

/// Two documents do not cancel each other: both requests run to a
/// suggestion (which answer lands on which document is scheduling order,
/// so the pair is compared as a set).
#[tokio::test(start_paused = true)]
async fn different_documents_do_not_cancel_each_other() {
    let model = ScriptedFim::new(vec!["one".to_string(), "two".to_string()]);
    let svc = service(model);
    let a = spawn_suggest(
        &svc,
        "file:///a.rs",
        "fn a() {}",
        1,
        Position {
            line: 0,
            character: 4,
        },
        "",
    );
    let b = spawn_suggest(
        &svc,
        "file:///b.rs",
        "fn b() {}",
        1,
        Position {
            line: 0,
            character: 4,
        },
        "",
    );
    let (ra, rb) = tokio::join!(a, b);
    let mut texts = [
        ra.unwrap().unwrap().item.text,
        rb.unwrap().unwrap().item.text,
    ];
    texts.sort();
    assert_eq!(texts, ["one", "two"]);
}

/// Pure-function checks: the caps hold on oversized inputs.
#[test]
fn context_block_is_capped_to_about_300_tokens() {
    let block = render_context_block(&"intent ".repeat(4000), "rust");
    assert!(block.len() <= MAX_CONTEXT_BYTES);
    assert!(block.ends_with('\n'));
}

#[test]
fn fim_request_caps_prefix_and_suffix() {
    let big: String = "line\n".repeat(10_000);
    let offset = big.len() / 2;
    let req = build_fim_request(&big, offset, "", "rust");
    assert!(req.prompt.len() <= MAX_PREFIX_BYTES);
    assert!(req.suffix.len() <= MAX_SUFFIX_BYTES);
}
