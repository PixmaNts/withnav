mod common;
#[tokio::test(flavor = "current_thread")]
async fn settings_locked_put_keeps_runtime_responsive() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(dir.path())
        .bearer_token("secret")
        .state();
    let app = withnav::server::app(state);
    let uri = format!("/v1/settings?workspace={}", {
        let mut url = reqwest::Url::parse("http://localhost").unwrap();
        url.query_pairs_mut()
            .append_pair("workspace", &dir.path().to_string_lossy());
        url.query()
            .unwrap()
            .strip_prefix("workspace=")
            .unwrap()
            .to_owned()
    });
    let (_, current) = request(&app, "GET", &uri, Value::Null, true).await;
    let path = dir.path().join("cfg/config.json.lock");
    let released = Arc::new(AtomicBool::new(false));
    let flag = released.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .unwrap();
        file.lock().unwrap();
        ready_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        flag.store(true, Ordering::SeqCst);
        file.unlock().unwrap();
    });
    ready_rx.recv().unwrap();
    let put = tokio::spawn(async move {
        request(
            &app,
            "PUT",
            &uri,
            json!({"revision":current["revision"],"key":"tui.theme","value":"nord-light"}),
            true,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let responsive = !released.load(Ordering::SeqCst);
    assert_eq!(put.await.unwrap().0, 200);
    holder.join().unwrap();
    assert!(
        responsive,
        "PUT blocked the current-thread runtime until lock release"
    );
}
use axum::{body::Body, http::Request};
use common::server::TestServerConfig;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Value,
    auth: bool,
) -> (u16, Value) {
    request_raw(app, method, uri, body.to_string(), auth).await
}

async fn request_raw(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: String,
    auth: bool,
) -> (u16, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if auth {
        request = request.header("authorization", "Bearer secret");
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn settings_edit_validation_removal_and_stale_revision() {
    let dir = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(dir.path())
        .variants(json!({"high":{}}), None)
        .bearer_token("secret")
        .state();
    let app = withnav::server::app(state);
    let uri = format!("/v1/settings?workspace={}", {
        let mut url = reqwest::Url::parse("http://localhost").unwrap();
        url.query_pairs_mut()
            .append_pair("workspace", &dir.path().to_string_lossy());
        url.query()
            .unwrap()
            .strip_prefix("workspace=")
            .unwrap()
            .to_owned()
    });
    assert_eq!(request(&app, "GET", &uri, Value::Null, false).await.0, 401);
    let path = dir.path().join("cfg/config.json");
    std::fs::remove_file(&path).unwrap();
    let (_, mut current) = request(&app, "GET", &uri, Value::Null, true).await;
    assert_eq!(current["revision"], "absent");
    let (status, error) = request_raw(&app, "PUT", &uri, "{\"key\":".into(), true).await;
    assert_eq!(status, 400);
    assert_eq!(error["reason"], "invalid");
    assert_eq!(error["key"], Value::Null);
    let detail = error["error"].as_str().unwrap();
    assert!(detail.contains("line 1 column"), "{detail}");
    assert!(!detail.contains("expected {revision, key, value}"));
    assert!(!path.exists(), "malformed JSON must not write settings");
    for (key, value) in [
        ("subagents.max_concurrent", json!(12)),
        (
            "permissions.scratch_dirs",
            json!([dir.path().join("custom")]),
        ),
        ("subagents.watchdog_interval_secs", json!(0)),
        ("agent.watchdog_interval_secs", json!(42)),
        ("tui.theme", json!("nord-light")),
        ("default_agent", json!("plan")),
        (
            "agents.build",
            json!({"model":"mock/mock-model","variant":"high"}),
        ),
        (
            "layers.metacognition",
            json!({"model":"mock/mock-model","variant":null}),
        ),
    ] {
        let (status, next) = request(
            &app,
            "PUT",
            &uri,
            json!({"revision":current["revision"],"key":key,"value":value}),
            true,
        )
        .await;
        assert_eq!(status, 200, "{next}");
        current = next;
    }
    for (key, value) in [
        ("unknown", Value::Null),
        ("subagents.max_concurrent", json!(65)),
        ("subagents.max_concurrent", json!(1.5)),
        // Removed with the per-call review: an unknown key now.
        ("permissions.automatic_review", json!(false)),
        ("permissions.review_timeout_secs", json!(40)),
        ("permissions.scratch_dirs", json!(["relative"])),
        ("agent.watchdog_interval_secs", json!(31536001)),
        ("tui.theme", json!("bad")),
        ("default_agent", json!("vigie")),
        ("agents.build", json!({"model":"missing/m"})),
        ("layers.reflex", json!({"model":"mock/mock-model"})),
        (
            "agents.build",
            json!({"model":"mock/mock-model","variant":"bad"}),
        ),
    ] {
        let (status, error) = request(
            &app,
            "PUT",
            &uri,
            json!({"revision":current["revision"],"key":key,"value":value}),
            true,
        )
        .await;
        assert_eq!(status, 400, "{error}");
        assert_eq!(error["reason"], "invalid");
    }
    let (status, error) = request(
        &app,
        "PUT",
        &uri,
        json!({"revision":"absent","key":"tui.theme","value":null}),
        true,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(error["reason"], "stale");
    let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(raw["layers"]["metacognition"].get("variant").is_none());
    raw["unknown"] = json!({"keep":true});
    raw["tui"]["sibling"] = json!(7);
    raw["subagents"]["max_concurrent"] = json!(0);
    std::fs::write(&path, raw.to_string()).unwrap();
    let (_, current) = request(&app, "GET", &uri, Value::Null, true).await;
    assert!(
        current["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "subagents.max_concurrent")
            .unwrap()["error"]
            .is_string()
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &uri,
            json!({"revision":current["revision"],"key":"tui.theme","value":null}),
            true
        )
        .await
        .0,
        200
    );
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["unknown"]["keep"], true);
    assert_eq!(raw["tui"]["sibling"], 7);
    assert!(raw["tui"].get("theme").is_none());
}

#[tokio::test]
async fn settings_gateway_base_url_never_carries_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let state = TestServerConfig::mock(
        dir.path(),
        "http://withnavuser:s3cretpass@127.0.0.1:9/v1?api_key=k3yvalue#frag",
    )
    .bearer_token("secret")
    .state();
    let app = withnav::server::app(state);
    let uri = format!("/v1/settings?workspace={}", {
        let mut url = reqwest::Url::parse("http://localhost").unwrap();
        url.query_pairs_mut()
            .append_pair("workspace", &dir.path().to_string_lossy());
        url.query()
            .unwrap()
            .strip_prefix("workspace=")
            .unwrap()
            .to_owned()
    });
    let (status, body) = request(&app, "GET", &uri, Value::Null, true).await;
    assert_eq!(status, 200);
    assert_eq!(body["gateways"][0]["base_url"], "http://127.0.0.1:9/v1");
    let text = body.to_string();
    for secret in ["withnavuser", "s3cretpass", "k3yvalue", "frag"] {
        assert!(!text.contains(secret), "{secret} leaked in {text}");
    }
}

#[tokio::test]
async fn gateway_probe_never_returns_credentials_or_upstream_body() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };
    let mock = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let state = TestServerConfig::mock(dir.path(), &mock.uri())
        .bearer_token("secret")
        .state();
    let app = withnav::server::app(state);
    let uri = format!("/v1/gateways/mock/test?workspace={}", {
        let mut url = reqwest::Url::parse("http://localhost").unwrap();
        url.query_pairs_mut()
            .append_pair("workspace", &dir.path().to_string_lossy());
        url.query()
            .unwrap()
            .strip_prefix("workspace=")
            .unwrap()
            .to_owned()
    });
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"one"}]})))
        .mount(&mock)
        .await;
    let (status, body) = request(&app, "POST", &uri, Value::Null, true).await;
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["model_count"], 1);
    assert!(!body.to_string().contains("test-key"));
    mock.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("test-key secret"))
        .mount(&mock)
        .await;
    let (_, body) = request(&app, "POST", &uri, Value::Null, true).await;
    assert_eq!(body["ok"], false);
    assert_eq!(body["status"], 401);
    assert!(!body.to_string().contains("test-key"));
}

// Given a settings file that does not name `goal_watch.notes`
// When a client reads the settings, then sets the key to true, to a non-boolean, and removes it
// Then the field is listed off by default, `true` is stored, a non-boolean is refused without a
//      write, and removing it brings the default back
#[tokio::test]
async fn goal_watch_notes_is_listed_off_by_default_and_takes_only_a_boolean() {
    let dir = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(dir.path())
        .bearer_token("secret")
        .state();
    let app = withnav::server::app(state);
    let mut url = reqwest::Url::parse("http://localhost/v1/settings").unwrap();
    url.query_pairs_mut()
        .append_pair("workspace", &dir.path().to_string_lossy());
    let uri = format!("{}?{}", url.path(), url.query().unwrap());
    let field = |body: &Value| {
        body["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "goal_watch.notes")
            .cloned()
            .expect("goal_watch.notes is listed")
    };
    let (_, current) = request(&app, "GET", &uri, Value::Null, true).await;
    let listed = field(&current);
    assert_eq!(listed["default"], json!(false));
    assert_eq!(listed["value"], Value::Null);
    assert_eq!(listed["set"], json!(false));
    assert_eq!(listed["error"], Value::Null);
    assert_eq!(listed["label"], "Goal watch notes");
    assert!(
        listed["description"]
            .as_str()
            .unwrap()
            .contains("off by default")
    );

    let (status, current) = request(
        &app,
        "PUT",
        &uri,
        json!({"revision":current["revision"],"key":"goal_watch.notes","value":true}),
        true,
    )
    .await;
    assert_eq!(status, 200, "{current}");
    assert_eq!(field(&current)["value"], json!(true));
    let path = dir.path().join("cfg/config.json");
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["goal_watch"]["notes"], json!(true));

    for bad in [json!("yes"), json!(1), json!({"on":true})] {
        let (status, error) = request(
            &app,
            "PUT",
            &uri,
            json!({"revision":current["revision"],"key":"goal_watch.notes","value":bad}),
            true,
        )
        .await;
        assert_eq!(status, 400, "{error}");
        assert_eq!(error["reason"], "invalid");
        assert_eq!(error["key"], "goal_watch.notes");
    }
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        raw["goal_watch"]["notes"],
        json!(true),
        "a refused value writes nothing"
    );

    let (status, current) = request(
        &app,
        "PUT",
        &uri,
        json!({"revision":current["revision"],"key":"goal_watch.notes","value":null}),
        true,
    )
    .await;
    assert_eq!(status, 200, "{current}");
    assert_eq!(field(&current)["set"], json!(false));
    assert_eq!(field(&current)["default"], json!(false));
}

// Given a settings file that does not name `permissions.review`
// When a client reads the settings, sets the key to each mode, to a word that is none, and removes it
// Then the field is listed as `shadow` by default, a mode is stored, anything else is refused
//      without a write, and removing it brings the default back; the sibling `scratch_dirs` key is
//      still listed and editable
#[tokio::test]
async fn permissions_review_is_listed_as_on_by_default_and_takes_only_its_three_modes() {
    let dir = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(dir.path())
        .bearer_token("secret")
        .state();
    let app = withnav::server::app(state);
    let mut url = reqwest::Url::parse("http://localhost/v1/settings").unwrap();
    url.query_pairs_mut()
        .append_pair("workspace", &dir.path().to_string_lossy());
    let uri = format!("{}?{}", url.path(), url.query().unwrap());
    let field = |body: &Value, key: &str| {
        body["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("{key} is listed"))
    };
    let (_, current) = request(&app, "GET", &uri, Value::Null, true).await;
    let listed = field(&current, "permissions.review");
    assert_eq!(listed["default"], json!("on"));
    assert_eq!(listed["value"], Value::Null);
    assert_eq!(listed["set"], json!(false));
    assert_eq!(listed["error"], Value::Null);
    assert_eq!(listed["applies"], "new_sessions");
    assert_eq!(listed["label"], "Permission review");
    assert!(
        listed["description"].as_str().unwrap().contains("shadow"),
        "{listed}"
    );
    assert_eq!(
        field(&current, "permissions.scratch_dirs")["applies"],
        "new_sessions"
    );

    let path = dir.path().join("cfg/config.json");
    let mut current = current;
    for mode in ["off", "on", "shadow"] {
        let (status, next) = request(
            &app,
            "PUT",
            &uri,
            json!({"revision":current["revision"],"key":"permissions.review","value":mode}),
            true,
        )
        .await;
        assert_eq!(status, 200, "{next}");
        assert_eq!(field(&next, "permissions.review")["value"], json!(mode));
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["permissions"]["review"], json!(mode));
        current = next;
    }

    for bad in [
        json!("Shadow"),
        json!("auto"),
        json!(true),
        json!(1),
        json!({"on":true}),
    ] {
        let (status, error) = request(
            &app,
            "PUT",
            &uri,
            json!({"revision":current["revision"],"key":"permissions.review","value":bad}),
            true,
        )
        .await;
        assert_eq!(status, 400, "{error}");
        assert_eq!(error["reason"], "invalid");
        assert_eq!(error["key"], "permissions.review");
    }
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        raw["permissions"]["review"],
        json!("shadow"),
        "a refused value writes nothing"
    );

    let (status, current) = request(
        &app,
        "PUT",
        &uri,
        json!({"revision":current["revision"],"key":"permissions.review","value":null}),
        true,
    )
    .await;
    assert_eq!(status, 200, "{current}");
    let listed = field(&current, "permissions.review");
    assert_eq!(listed["set"], json!(false));
    assert_eq!(listed["default"], json!("on"));
}
