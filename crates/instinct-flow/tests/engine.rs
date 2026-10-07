use instinct_flow::{protocol::*, *};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
fn catalogue() -> Catalogue {
    serde_json::from_str(include_str!("fixtures/catalogue.json")).unwrap()
}
fn compile(text: &str) -> CompiledDefinition {
    Definition::parse(text)
        .unwrap()
        .validate(
            &catalogue(),
            &Capabilities { images: false },
            &BTreeMap::new(),
        )
        .unwrap()
}
fn header(start: &str) -> String {
    format!("[flow]\nversion=1\nname='test'\non='tool.pre_call'\nblocking=true\nbudget_ms=800\nstart={start}\n")
}
fn decision(id: &str, next: &str) -> String {
    format!("\n[[node]]\nid='{id}'\ntype='noul'\ninstructions='Is it so?'\nthreshold=0.5\n{next}\n")
}
fn action(id: &str, name: &str, text: &str) -> String {
    format!("\n[[node]]\nid='{id}'\ntype='action'\naction='{name}'\n[node.params]\n{text}\n")
}
struct Inputs;
impl InputSource for Inputs {
    fn collect<'a>(
        &'a self,
        names: &'a [String],
        _: &'a CancellationToken,
        _: Instant,
    ) -> BoxFuture<'a, Result<BTreeMap<String, InputValue>, InputError>> {
        Box::pin(async move {
            Ok(names
                .iter()
                .map(|n| {
                    (
                        n.clone(),
                        InputValue {
                            value: Some("build".into()),
                            cut: false,
                        },
                    )
                })
                .collect())
        })
    }
}
struct Fake {
    calls: AtomicUsize,
    requests: Mutex<Vec<Questions>>,
    delay: Duration,
    fail: bool,
}
impl Fake {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
            fail: false,
        }
    }
}
impl DecisionClient for Fake {
    fn capabilities(&self) -> Capabilities {
        Capabilities { images: false }
    }
    fn identity(&self) -> ClientIdentity {
        ClientIdentity {
            model: "fake".into(),
            variant: None,
        }
    }
    fn decide<'a>(
        &'a self,
        request: DecisionRequest<'a>,
        _: &'a CancellationToken,
        _: Instant,
    ) -> BoxFuture<'a, Result<DecisionResponse, DecisionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests
                .lock()
                .unwrap()
                .push(request.questions.clone());
            tokio::time::sleep(self.delay).await;
            if self.fail {
                return Err(DecisionError::InvalidAnswer("test failure".into()));
            }
            Ok(DecisionResponse {
                answers: request
                    .questions
                    .keys()
                    .map(|id| {
                        (
                            id.clone(),
                            Answer::Noul {
                                noul: if id == "a" { 0.0 } else { 1.0 },
                            },
                        )
                    })
                    .collect(),
                usage: Some(DecisionUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
            })
        })
    }
}
fn event() -> Event {
    Event {
        trigger: "tool.pre_call".into(),
        payload: serde_json::json!({"tool":"write","args":{},"call_id":"x"}),
    }
}
async fn evaluate(c: &CompiledDefinition, client: &Fake) -> RunOutcome {
    run(
        c,
        &event(),
        &Inputs,
        client,
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(2),
            run_id: "test".into(),
        },
        &CancellationToken::new(),
    )
    .await
}
#[test]
fn normative_fixture_round_trips_through_json_and_toml() {
    let first = Definition::parse(include_str!("fixtures/guard-mutations.toml")).unwrap();
    first
        .validate(
            &catalogue(),
            &Capabilities { images: false },
            &BTreeMap::new(),
        )
        .unwrap();
    let json = serde_json::to_value(&first).unwrap();
    let second: Definition = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(Definition::parse(&second.to_toml().unwrap()).unwrap()).unwrap(),
        json
    );
}
#[test]
fn forbidden_fields_and_unknown_fields_are_rejected() {
    let text = header("['a']") + &decision("a", "condition='true'");
    assert!(Definition::parse(&text).is_err());
    let text = header("['a']") + &decision("a", "surprise=true");
    assert!(Definition::parse(&text).is_err());
}
#[tokio::test]
async fn ready_decisions_batch_and_replay_without_client() {
    let c = compile(&(header("['a','b']") + &decision("a", "") + &decision("b", "")));
    let client = Fake::new();
    let outcome = evaluate(&c, &client).await;
    assert!(matches!(outcome.result, RunResult::Success { .. }));
    assert_eq!(client.calls.load(Ordering::SeqCst), 1);
    let replayed = replay(&c, &outcome.trace, &client.identity());
    assert!(replayed.comparable);
    assert_eq!(client.calls.load(Ordering::SeqCst), 1);
    let mut changed = outcome.trace.clone();
    changed.identity.model = "different".into();
    assert!(!replay(&c, &changed, &client.identity()).comparable);
}
#[tokio::test]
async fn or_fan_in_reads_unvisited_answer_as_absent() {
    let text=header("['a','b']")+&decision("a","[node.next]\nyes=['join']\nno=[]")+&decision("b","[node.next]\nyes=['join']")+
        "\n[[node]]\nid='join'\ntype='expression'\ncondition='is_set(x) || !is_set(y)'\n[node.inputs]\nx='answer.a'\ny='answer.b'\n";
    let c = compile(&text);
    let outcome = evaluate(&c, &Fake::new()).await;
    assert!(matches!(outcome.result, RunResult::Success { .. }));
    assert_eq!(outcome.trace.nodes[2].result, Some(true));
    let text=header("['a']")+&decision("a","[node.next]\nyes=['unused']\nno=['b']")+&decision("unused","[node.next]\nyes=['join']")+&decision("b","[node.next]\nyes=['join']")+
        "\n[[node]]\nid='join'\ntype='expression'\ncondition='!is_set(x)'\n[node.inputs]\nx='answer.unused'\n";
    let outcome = evaluate(&compile(&text), &Fake::new()).await;
    assert_eq!(outcome.trace.nodes[3].result, Some(true));
    assert_eq!(outcome.trace.nodes[1].status, "not_reached");
}
#[tokio::test]
async fn independent_branches_run_in_parallel() {
    let text = header("['a','b']")
        + &decision("a", "[node.inputs]\nx='constant.A'")
        + &decision("b", "[node.inputs]\nx='constant.B'")
        + "\n[constants]\nA='one'\nB='two'\n";
    let c = compile(&text);
    let mut client = Fake::new();
    client.delay = Duration::from_millis(80);
    let start = Instant::now();
    let outcome = evaluate(&c, &client).await;
    assert!(matches!(outcome.result, RunResult::Success { .. }));
    assert_eq!(client.calls.load(Ordering::SeqCst), 2);
    assert!(start.elapsed() < Duration::from_millis(145));
}
#[tokio::test]
async fn failure_discards_already_proposed_deny() {
    let text = header("['deny','b']")
        + "[fallback]\non_invalid_answer={action='ask_driver',params={note='fallback'}}\n"
        + &action("deny", "deny", "note='do not run'")
        + &decision("b", "");
    let c = compile(&text);
    let mut client = Fake::new();
    client.fail = true;
    let outcome = evaluate(&c, &client).await;
    assert!(matches!(
        outcome.result,
        RunResult::Failure {
            kind: FailureKind::InvalidAnswer,
            ..
        }
    ));
    assert!(outcome.trace.actions.is_empty());
    assert_eq!(outcome.trace.fallback_actions[0].action, "ask_driver");
}
#[tokio::test]
async fn exhausted_deadline_selects_timeout_fallback() {
    let text = (header("['a']")
        + "[fallback]\non_timeout={action='no_objection',params={}}\n"
        + &decision("a", ""))
        .replace("budget_ms=800", "budget_ms=10");
    let c = compile(&text);
    let mut client = Fake::new();
    client.delay = Duration::from_secs(1);
    let outcome = evaluate(&c, &client).await;
    assert!(matches!(
        outcome.result,
        RunResult::Failure {
            kind: FailureKind::Timeout,
            ..
        }
    ));
    assert_eq!(outcome.trace.fallback_actions[0].action, "no_objection");
}
#[test]
fn conflicts_limits_and_nonblocking_verdicts_fail_closed() {
    let text = header("['deny','ask']")
        + &action("deny", "deny", "note='deny'")
        + &action("ask", "ask_driver", "note='ask'");
    let errors = Definition::parse(&text)
        .unwrap()
        .validate(
            &catalogue(),
            &Capabilities { images: false },
            &BTreeMap::new(),
        )
        .unwrap_err();
    assert!(errors.iter().any(|e| e.code == "conflicting_verdicts"));
    let starts = (0..17)
        .map(|i| format!("'q{i}'"))
        .collect::<Vec<_>>()
        .join(",");
    let mut text = header(&format!("[{starts}]"));
    for i in 0..17 {
        text += &decision(&format!("q{i}"), "");
    }
    let errors = Definition::parse(&text)
        .unwrap()
        .validate(
            &catalogue(),
            &Capabilities { images: false },
            &BTreeMap::new(),
        )
        .unwrap_err();
    assert!(errors.iter().any(|e| e.code == "limit"));
    let text = (header("['deny']") + &action("deny", "deny", "note='no'"))
        .replace("blocking=true\nbudget_ms=800\n", "");
    assert!(Definition::parse(&text)
        .unwrap()
        .validate(
            &catalogue(),
            &Capabilities { images: false },
            &BTreeMap::new()
        )
        .is_err());
}
#[test]
fn sixteen_independent_decisions_use_memoized_port_choices() {
    let starts = (0..16)
        .map(|i| format!("'q{i}'"))
        .collect::<Vec<_>>()
        .join(",");
    let mut text = header(&format!("[{starts}]"));
    for i in 0..16 {
        text += &decision(&format!("q{i}"), "");
    }
    compile(&text);
}
#[tokio::test]
async fn cancellation_discards_actions_and_unknown_usage_stays_null() {
    let c = compile(
        &(header("['deny','a']") + &action("deny", "deny", "note='no'") + &decision("a", "")),
    );
    let client = Fake::new();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = run(
        &c,
        &event(),
        &Inputs,
        &client,
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(1),
            run_id: "cancel".into(),
        },
        &cancel,
    )
    .await;
    assert!(matches!(
        result.result,
        RunResult::Failure {
            kind: FailureKind::Cancelled,
            ..
        }
    ));
    assert!(result.trace.actions.is_empty());
    assert_eq!(client.calls.load(Ordering::SeqCst), 0);
    let mut client = Fake::new();
    client.fail = true;
    let result = evaluate(&c, &client).await;
    assert!(result.trace.usage.is_none());
}
#[tokio::test]
async fn replay_rejects_changed_question_with_unchanged_template() {
    let text = header("['a']")
        + &decision("a", "[node.next]\nno=['b']")
        + &decision(
            "b",
            "state_template='state.txt'\n[node.inputs]\nx='answer.a'",
        );
    let templates = BTreeMap::from([("state.txt".into(), "fixed state".into())]);
    let definition = Definition::parse(&text).unwrap();
    let compiled = definition
        .validate(&catalogue(), &Capabilities { images: false }, &templates)
        .unwrap();
    let client = Fake::new();
    let result = evaluate(&compiled, &client).await;
    let mut changed = definition.clone();
    if let NodeKind::Noul { instructions, .. } = &mut changed.node[1].kind {
        *instructions = "Changed".into();
    }
    let changed = changed
        .validate(&catalogue(), &Capabilities { images: false }, &templates)
        .unwrap();
    assert!(!replay(&changed, &result.trace, &client.identity()).comparable);
}
