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
                            redacted: false,
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
struct Controlled {
    entered: tokio::sync::Notify,
    joined: tokio::sync::Notify,
    release: tokio::sync::Notify,
    join_calls: AtomicUsize,
}
impl Controlled {
    fn new() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            joined: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            join_calls: AtomicUsize::new(0),
        }
    }
}
impl DecisionClient for Controlled {
    fn capabilities(&self) -> Capabilities {
        Capabilities { images: false }
    }
    fn identity(&self) -> ClientIdentity {
        Fake::new().identity()
    }
    fn decide<'a>(
        &'a self,
        request: DecisionRequest<'a>,
        _: &'a CancellationToken,
        _: Instant,
    ) -> BoxFuture<'a, Result<DecisionResponse, DecisionError>> {
        Box::pin(async move {
            if request.questions.contains_key("slow") {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if request.questions.contains_key("join") {
                self.join_calls.fetch_add(1, Ordering::SeqCst);
                self.joined.notify_one();
            }
            Ok(DecisionResponse {
                answers: request
                    .questions
                    .keys()
                    .map(|id| (id.clone(), Answer::Noul { noul: 1.0 }))
                    .collect(),
                usage: None,
            })
        })
    }
}
#[tokio::test]
async fn join_waits_only_for_declared_answers_and_never_runs_twice() {
    for declared in [false, true] {
        let text = header("['fast','slow']")
            + &decision(
                "fast",
                "[node.inputs]\nx='constant.F'\n[node.next]\nyes=['join']",
            )
            + &decision(
                "slow",
                "[node.inputs]\nx='constant.S'\n[node.next]\nyes=['join']",
            )
            + &decision(
                "join",
                if declared {
                    "[node.inputs]\nx='answer.slow'"
                } else {
                    ""
                },
            )
            + "\n[constants]\nF='fast'\nS='slow'\n";
        let c = compile(&text);
        let client = Controlled::new();
        let event = event();
        let cancel = CancellationToken::new();
        let work = run(
            &c,
            &event,
            &Inputs,
            &client,
            RunOptions {
                deadline: Instant::now() + Duration::from_secs(2),
                run_id: "sync".into(),
            },
            &cancel,
        );
        let observe = async {
            client.entered.notified().await;
            if declared {
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), client.joined.notified())
                        .await
                        .is_err()
                );
                client.release.notify_one();
                client.joined.notified().await;
            } else {
                tokio::time::timeout(Duration::from_millis(200), client.joined.notified())
                    .await
                    .unwrap();
                client.release.notify_one();
            }
        };
        let (outcome, ()) = tokio::join!(work, observe);
        assert!(matches!(outcome.result, RunResult::Success { .. }));
        assert_eq!(client.join_calls.load(Ordering::SeqCst), 1);
    }
}
struct BlockingInput<'a>(&'a Controlled);
impl InputSource for BlockingInput<'_> {
    fn collect<'a>(
        &'a self,
        names: &'a [String],
        _: &'a CancellationToken,
        _: Instant,
    ) -> BoxFuture<'a, Result<BTreeMap<String, InputValue>, InputError>> {
        Box::pin(async move {
            self.0.joined.notified().await;
            Ok(names
                .iter()
                .map(|n| {
                    (
                        n.clone(),
                        InputValue {
                            value: Some("ok".into()),
                            cut: false,
                            redacted: false,
                        },
                    )
                })
                .collect())
        })
    }
}
#[tokio::test]
async fn blocked_input_collection_does_not_starve_inflight_decisions() {
    let text = header("['fast','host']")
        + &decision("fast", "[node.next]\nyes=['join']")
        + &decision("host", "[node.inputs]\nx='session.agent'")
        + &decision("join", "");
    let c = compile(&text);
    let client = Controlled::new();
    let result = run(
        &c,
        &event(),
        &BlockingInput(&client),
        &client,
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(1),
            run_id: "collect".into(),
        },
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(result.result, RunResult::Success { .. }));
    assert_eq!(client.join_calls.load(Ordering::SeqCst), 1);
}
struct Values {
    value: serde_json::Value,
    redacted: bool,
}
#[tokio::test]
async fn simultaneous_consumers_share_pending_host_collection() {
    struct Once {
        calls: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl InputSource for Once {
        fn collect<'a>(
            &'a self,
            names: &'a [String],
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<BTreeMap<String, InputValue>, InputError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
                Inputs
                    .collect(names, &CancellationToken::new(), Instant::now())
                    .await
            })
        }
    }
    let c = compile(
        &(header("['a','b']")
            + &decision("a", "[node.inputs]\nx='session.agent'")
            + &decision("b", "[node.inputs]\nx='session.agent'")),
    );
    let source = Once {
        calls: AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    };
    let client = Fake::new();
    let event = event();
    let cancel = CancellationToken::new();
    let work = run(
        &c,
        &event,
        &source,
        &client,
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(1),
            run_id: "once".into(),
        },
        &cancel,
    );
    let (outcome, ()) = tokio::join!(work, async {
        source.entered.notified().await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(client.calls.load(Ordering::SeqCst), 0);
        source.release.notify_one();
    });
    assert!(matches!(outcome.result, RunResult::Success { .. }));
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert!(outcome.trace.nodes.iter().all(|n| n.status == "completed"));
}
impl InputSource for Values {
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
                            value: Some(self.value.clone()),
                            cut: false,
                            redacted: self.redacted,
                        },
                    )
                })
                .collect())
        })
    }
}
async fn with_values(c: &CompiledDefinition, values: &Values) -> RunOutcome {
    run(
        c,
        &event(),
        values,
        &Fake::new(),
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(1),
            run_id: "values".into(),
        },
        &CancellationToken::new(),
    )
    .await
}
#[tokio::test]
async fn typed_parameter_overflow_is_a_limit_not_a_string() {
    let text = header("['log']")
        + &action("log", "log", "text={ref='x'}")
        + "[node.inputs]\nx='workspace.dirty_files'\n";
    let mut cat = catalogue();
    cat.actions
        .iter_mut()
        .find(|a| a.name == "log")
        .unwrap()
        .params[0]
        .value_type = ValueType::Array;
    cat.limits.input_bytes = 8;
    let c = Definition::parse(&text)
        .unwrap()
        .validate(&cat, &Capabilities { images: false }, &BTreeMap::new())
        .unwrap();
    let outcome = with_values(
        &c,
        &Values {
            value: serde_json::json!(["oversized value"]),
            redacted: false,
        },
    )
    .await;
    assert!(matches!(
        outcome.result,
        RunResult::Failure {
            kind: FailureKind::Limit,
            ..
        }
    ));
    assert!(outcome.trace.actions.is_empty());
}
#[tokio::test]
async fn oversized_structured_input_is_cut_for_decisions_and_text_parameters() {
    for body in [
        decision("b", "[node.inputs]\nx='workspace.dirty_files'"),
        action("b", "log", "text='{x}'") + "[node.inputs]\nx='workspace.dirty_files'\n",
    ] {
        let c = compile(&(header("['b']") + &body));
        let outcome = with_values(
            &c,
            &Values {
                value: serde_json::json!(["é".repeat(20000)]),
                redacted: false,
            },
        )
        .await;
        assert!(matches!(outcome.result, RunResult::Success { .. }));
        let input = &outcome.trace.inputs["workspace.dirty_files"];
        assert!(input.cut);
        let text = input.value.as_ref().unwrap().as_str().unwrap();
        assert!(text.starts_with("[\"é"));
        assert!(text.len() <= 16384);
    }
}
fn five_reference_flow() -> String {
    let mut text = header("['b']")
        + &decision(
            "b",
            "state_template='fixed'\n[node.inputs]\nx='session.agent'\n[node.next]\nno=['c','d','e','f']",
        );
    for (id, reference) in [
        ("c", "conversation.last_driver"),
        ("d", "intent.record"),
        ("e", "workspace.root"),
        ("f", "event.tool"),
    ] {
        text += &decision(
            id,
            &format!("state_template='fixed'\n[node.inputs]\nx='{reference}'"),
        );
    }
    text
}
fn fixed_state(text: &str, cat: &Catalogue) -> CompiledDefinition {
    Definition::parse(text)
        .unwrap()
        .validate(
            cat,
            &Capabilities { images: false },
            &BTreeMap::from([("fixed".into(), "fixed".into())]),
        )
        .unwrap()
}
#[tokio::test]
async fn unused_references_do_not_split_input_allowance() {
    let c = fixed_state(&five_reference_flow(), &catalogue());
    let outcome = with_values(
        &c,
        &Values {
            value: "x".repeat(15 * 1024).into(),
            redacted: false,
        },
    )
    .await;
    assert!(matches!(outcome.result, RunResult::Success { .. }));
    assert!(!outcome.trace.inputs["session.agent"].cut);
    assert_eq!(
        outcome.trace.inputs["session.agent"]
            .value
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .len(),
        15 * 1024
    );
}
#[tokio::test]
async fn collected_total_over_64_kib_fails_independently_of_unused_references() {
    let text = five_reference_flow().replace("start=['b']", "start=['b','c','d','e','f']");
    let c = fixed_state(&text, &catalogue());
    let outcome = with_values(
        &c,
        &Values {
            value: "x".repeat(16384).into(),
            redacted: false,
        },
    )
    .await;
    assert!(matches!(
        outcome.result,
        RunResult::Failure {
            kind: FailureKind::Limit,
            ..
        }
    ));
}
#[tokio::test]
async fn replay_rebounds_host_values_and_rejects_changed_cuts() {
    let text = header("['b']")
        + &decision(
            "b",
            "state_template='fixed'\n[node.inputs]\nx='session.agent'",
        );
    let c = fixed_state(&text, &catalogue());
    let record = with_values(
        &c,
        &Values {
            value: "x".repeat(15000).into(),
            redacted: false,
        },
    )
    .await
    .trace;
    let added = fixed_state(&(text + "y='event.tool'\n"), &catalogue());
    assert!(!replay(&added, &record, &Fake::new().identity()).comparable);
    let mut cat = catalogue();
    cat.limits.input_bytes = 8000;
    let smaller = fixed_state(&five_reference_flow(), &cat);
    assert!(!replay(&smaller, &record, &Fake::new().identity()).comparable);
}
#[tokio::test]
async fn replay_compares_original_constants_and_references_even_with_fixed_state() {
    let text = header("['b']")
        + &decision("b", "state_template='fixed'\n[node.inputs]\nx='constant.X'")
        + "\n[constants]\nX='old'\nY='old'\n";
    let templates = BTreeMap::from([("fixed".into(), "unchanged".into())]);
    let make = |text: &str| {
        Definition::parse(text)
            .unwrap()
            .validate(&catalogue(), &Capabilities { images: false }, &templates)
            .unwrap()
    };
    let c = make(&text);
    let record = evaluate(&c, &Fake::new()).await.trace;
    for changed in [
        text.replace("X='old'", "X='new'"),
        text.replace("constant.X", "constant.Y"),
    ] {
        let r = replay(&make(&changed), &record, &Fake::new().identity());
        assert!(!r.comparable);
        assert_eq!(r.nodes[0].status, "question_changed");
    }
    let mut legacy = serde_json::to_value(&record).unwrap();
    legacy["nodes"][0]
        .as_object_mut()
        .unwrap()
        .remove("selected_inputs");
    let legacy: RunRecord = serde_json::from_value(legacy).unwrap();
    assert!(replay(&c, &legacy, &Fake::new().identity()).comparable);
    assert!(
        !replay(
            &make(&text.replace("X='old'", "X='new'")),
            &legacy,
            &Fake::new().identity()
        )
        .comparable
    );
}
#[tokio::test]
async fn changed_expression_and_action_cannot_read_uncaptured_host_inputs() {
    for kind in ["expression", "action"] {
        let original = if kind == "expression" {
            header("['x']") + "[[node]]\nid='x'\ntype='expression'\ncondition='true'\n"
        } else {
            header("['x']") + &action("x", "log", "text='fixed'")
        };
        let record = evaluate(&compile(&original), &Fake::new()).await.trace;
        let changed = original + "[node.inputs]\nx='session.agent'\n";
        let r = replay(&compile(&changed), &record, &Fake::new().identity());
        assert!(!r.comparable);
        assert_eq!(r.nodes[0].status, "missing_capture");
    }
}
#[tokio::test]
async fn redacted_inputs_are_live_but_never_retained_or_replayed_as_absent() {
    for body in [decision("b", "[node.inputs]\nx='session.agent'"), "[[node]]\nid='b'\ntype='expression'\ncondition='is_set(x)'\n[node.inputs]\nx='session.agent'\n".into(), action("b", "log", "text='{x}'") + "[node.inputs]\nx='session.agent'\n"] {
        let c = compile(&(header("['b']") + &body));
        let outcome = with_values(&c, &Values { value: "secret-value".into(), redacted: true }).await;
        assert!(matches!(outcome.result, RunResult::Success { .. }));
        assert!(!serde_json::to_string(&outcome.trace).unwrap().contains("secret-value"));
        let r = replay(&c, &outcome.trace, &Fake::new().identity());
        assert!(!r.comparable);
        assert_eq!(r.nodes[0].status, "missing_capture");
    }
    let old: InputValue = serde_json::from_str(r#"{"value":null,"cut":false}"#).unwrap();
    assert!(!old.redacted);
}
#[tokio::test]
async fn redacted_capture_overflow_omits_state_without_failing_live_run() {
    let text = header("['b']")
        + &decision(
            "b",
            "state_template='repeat'\n[node.inputs]\nx='session.agent'",
        );
    let c = Definition::parse(&text)
        .unwrap()
        .validate(
            &catalogue(),
            &Capabilities { images: false },
            &BTreeMap::from([("repeat".into(), "{{x}}".repeat(7000))]),
        )
        .unwrap();
    let outcome = with_values(
        &c,
        &Values {
            value: "a".into(),
            redacted: true,
        },
    )
    .await;
    assert!(matches!(outcome.result, RunResult::Success { .. }));
    assert!(outcome.trace.batches[0].redacted);
    assert_eq!(outcome.trace.batches[0].state, "<state omitted: redacted>");
    assert!(!replay(&c, &outcome.trace, &Fake::new().identity()).comparable);
}
#[tokio::test]
async fn captured_absence_remains_comparable() {
    struct Absent;
    impl InputSource for Absent {
        fn collect<'a>(
            &'a self,
            _: &'a [String],
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<BTreeMap<String, InputValue>, InputError>> {
            Box::pin(async { Ok(BTreeMap::new()) })
        }
    }
    let c = compile(&(header("['b']") + &decision("b", "[node.inputs]\nx='session.agent'")));
    let outcome = run(
        &c,
        &event(),
        &Absent,
        &Fake::new(),
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(1),
            run_id: "absent".into(),
        },
        &CancellationToken::new(),
    )
    .await;
    assert!(replay(&c, &outcome.trace, &Fake::new().identity()).comparable);
}
#[tokio::test]
async fn failed_batch_reuse_also_requires_original_selected_inputs() {
    let text = header("['b']")
        + &decision("b", "state_template='fixed'\n[node.inputs]\nx='constant.X'")
        + "\n[constants]\nX='old'\n";
    let templates = BTreeMap::from([("fixed".into(), "fixed".into())]);
    let make = |text: &str| {
        Definition::parse(text)
            .unwrap()
            .validate(&catalogue(), &Capabilities { images: false }, &templates)
            .unwrap()
    };
    let mut client = Fake::new();
    client.fail = true;
    let c = make(&text);
    let outcome = evaluate(&c, &client).await;
    assert!(replay(&c, &outcome.trace, &client.identity()).comparable);
    assert!(
        !replay(
            &make(&text.replace("X='old'", "X='new'")),
            &outcome.trace,
            &client.identity()
        )
        .comparable
    );
}
#[tokio::test]
async fn cancellation_while_running_discards_every_proposal() {
    let c = compile(
        &(header("['slow','log']") + &decision("slow", "") + &action("log", "log", "text='ready'")),
    );
    let client = Controlled::new();
    let cancel = CancellationToken::new();
    let event = event();
    let work = run(
        &c,
        &event,
        &Inputs,
        &client,
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(1),
            run_id: "cancel".into(),
        },
        &cancel,
    );
    let (outcome, ()) = tokio::join!(work, async {
        client.entered.notified().await;
        cancel.cancel();
    });
    assert!(matches!(
        outcome.result,
        RunResult::Failure {
            kind: FailureKind::Cancelled,
            ..
        }
    ));
    assert!(outcome.trace.actions.is_empty());
}
#[tokio::test]
async fn semaphore_wait_counts_against_run_deadline() {
    let text = (header("['slow','other']")
        + &decision("slow", "[node.inputs]\nx='constant.S'")
        + &decision("other", "[node.inputs]\nx='constant.O'")
        + "\n[constants]\nS='a'\nO='z'\n")
        .replace("budget_ms=800", "budget_ms=30");
    let mut cat = catalogue();
    cat.limits.concurrent_calls = 1;
    let c = Definition::parse(&text)
        .unwrap()
        .validate(&cat, &Capabilities { images: false }, &BTreeMap::new())
        .unwrap();
    let outcome = run(
        &c,
        &event(),
        &Inputs,
        &Controlled::new(),
        RunOptions {
            deadline: Instant::now() + Duration::from_secs(1),
            run_id: "semaphore".into(),
        },
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        outcome.result,
        RunResult::Failure {
            kind: FailureKind::Timeout,
            ..
        }
    ));
}
#[tokio::test]
async fn fallback_specific_general_default_and_nonblocking_suppression() {
    for (fallback, expected) in [("[fallback]\non_invalid_answer={action='log',params={text='specific'}}\non_failure={action='log',params={text='general'}}\n", "log"), ("[fallback]\non_failure={action='log',params={text='general'}}\n", "log"), ("", "ask_driver")] {
        let c = compile(&(header("['b']") + fallback + &decision("b", "")));
        let mut client = Fake::new(); client.fail = true;
        let outcome = evaluate(&c, &client).await;
        assert_eq!(outcome.trace.fallback_actions[0].action, expected);
        if !fallback.is_empty() { assert_eq!(outcome.trace.fallback_actions[0].params["text"], if fallback.contains("specific") { "specific" } else { "general" }); }
        assert!(replay(&c, &outcome.trace, &client.identity()).comparable);
    }
    let c = compile(
        &(header("['b']") + &decision("b", "")).replace("blocking=true\nbudget_ms=800\n", ""),
    );
    let mut client = Fake::new();
    client.fail = true;
    assert!(evaluate(&c, &client)
        .await
        .trace
        .fallback_actions
        .is_empty());
}
#[tokio::test]
async fn verdict_priority_ties_use_flow_name_and_merge_notes() {
    let mut outcomes = Vec::new();
    for (name, action_name) in [("z", "deny"), ("a", "deny"), ("first", "ask_driver")] {
        let text = (header("['v']") + &action("v", action_name, &format!("note='{name}'")))
            .replace("name='test'", &format!("name='{name}'"));
        outcomes.push(evaluate(&compile(&text), &Fake::new()).await);
    }
    let result = combine_verdicts(&outcomes, &catalogue()).unwrap().unwrap();
    assert_eq!(result.action, "deny");
    assert_eq!(result.params["note"], "a\nz");
}
#[test]
fn exclusive_choice_ports_allow_different_verdicts() {
    compile(&(header("['choice']") + "[[node]]\nid='choice'\ntype='choice'\ninstructions='Pick'\ncriteria={a='A',b='B'}\n[node.next]\na=['deny']\nb=['ask']\n" + &action("deny", "deny", "note='deny'") + &action("ask", "ask_driver", "note='ask'")));
}
#[tokio::test]
async fn reconvergent_diamond_activates_join_once() {
    let text = header("['root']")
        + &decision("root", "[node.next]\nyes=['left','right']")
        + &decision("left", "[node.next]\nyes=['join']")
        + &decision("right", "[node.next]\nyes=['join']")
        + &action("join", "log", "text='one'");
    let outcome = evaluate(&compile(&text), &Fake::new()).await;
    assert!(matches!(outcome.result, RunResult::Success { .. }));
    assert_eq!(outcome.trace.actions.len(), 1);
}
#[test]
fn static_search_exhaustion_fails_closed() {
    let starts = (0..16)
        .map(|i| format!("'q{i}'"))
        .collect::<Vec<_>>()
        .join(",");
    let mut text = header(&format!("[{starts}]"));
    for i in 0..16 {
        text += &decision(&format!("q{i}"), &format!("[node.next]\nyes=['a{i}']"));
    }
    for i in 0..16 {
        text += &action(&format!("a{i}"), "log", "text='x'");
    }
    let errors = Definition::parse(&text)
        .unwrap()
        .validate(
            &catalogue(),
            &Capabilities { images: false },
            &BTreeMap::new(),
        )
        .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.code == "limit" && e.message.contains("search budget")),
        "{errors:?}"
    );
}
struct Ordered {
    first: &'static str,
    arrived: tokio::sync::Notify,
}
impl DecisionClient for Ordered {
    fn capabilities(&self) -> Capabilities {
        Capabilities { images: false }
    }
    fn identity(&self) -> ClientIdentity {
        Fake::new().identity()
    }
    fn decide<'a>(
        &'a self,
        request: DecisionRequest<'a>,
        _: &'a CancellationToken,
        _: Instant,
    ) -> BoxFuture<'a, Result<DecisionResponse, DecisionError>> {
        Box::pin(async move {
            if request.questions.contains_key(self.first) {
                self.arrived.notify_one();
            } else {
                self.arrived.notified().await;
                tokio::task::yield_now().await;
            }
            Ok(DecisionResponse {
                answers: request
                    .questions
                    .keys()
                    .map(|id| (id.clone(), Answer::Noul { noul: 1.0 }))
                    .collect(),
                usage: None,
            })
        })
    }
}
#[tokio::test]
async fn reversed_completion_order_produces_identical_bounded_proposals() {
    let text = header("['left','right']")
        + &decision(
            "left",
            "[node.inputs]\nx='constant.L'\n[node.next]\nyes=['a']",
        )
        + &decision(
            "right",
            "[node.inputs]\nx='constant.R'\n[node.next]\nyes=['b']",
        )
        + &action("a", "log", "text='{x}'")
        + "[node.inputs]\nx='session.agent'\n"
        + &action("b", "log", "text='{x}'")
        + "[node.inputs]\nx='workspace.root'\n"
        + "[constants]\nL='l'\nR='r'\n";
    let mut cat = catalogue();
    cat.limits.run_input_bytes = 24;
    cat.limits.input_bytes = 5;
    let c = Definition::parse(&text)
        .unwrap()
        .validate(&cat, &Capabilities { images: false }, &BTreeMap::new())
        .unwrap();
    let mut results = Vec::new();
    for first in ["left", "right"] {
        let client = Ordered {
            first,
            arrived: tokio::sync::Notify::new(),
        };
        let outcome = run(
            &c,
            &event(),
            &Values {
                value: "abcdefghijk".into(),
                redacted: false,
            },
            &client,
            RunOptions {
                deadline: Instant::now() + Duration::from_secs(1),
                run_id: "order".into(),
            },
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome.result, RunResult::Success { .. }));
        results.push(outcome.trace.actions);
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[0][0].params["text"], "abcde");
}
