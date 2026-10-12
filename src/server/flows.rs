//! Studio's private JSON adapter. Every operation authorizes an existing workspace.
use super::AppState;
use crate::flows::{
    runtime::ContextSnapshot,
    store::{validate, Content, FlowStore, Location, StoreError},
};
use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use instinct_flow::*;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/flows", any(root))
        .route("/v1/flows/{*tail}", any(tail))
        .layer(axum::extract::DefaultBodyLimit::max(512 * 1024))
}
async fn root(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    method: Method,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(_) => return too_large(),
    };
    dispatch(state, query, headers, peer, method, "".into(), body).await
}
async fn tail(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    method: Method,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(_) => return too_large(),
    };
    dispatch(state, query, headers, peer, method, tail, body).await
}
fn too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(json!({"error":"request body too large","reason":"too_large","errors":[]})),
    )
        .into_response()
}
fn error(e: StoreError) -> Response {
    let status = match e.reason {
        "forbidden" | "untrusted" => StatusCode::FORBIDDEN,
        "not_found" => StatusCode::NOT_FOUND,
        "stale" | "already_exists" | "read_only" => StatusCode::CONFLICT,
        "reload_invalid" => StatusCode::UNPROCESSABLE_ENTITY,
        "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "io" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(json!({"error":e.reason.replace('_'," "),"reason":e.reason,"errors":e.errors})),
    )
        .into_response()
}
async fn dispatch(
    state: AppState,
    query: HashMap<String, String>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    method: Method,
    tail: String,
    body: Bytes,
) -> Response {
    match handle(&state, &query, &headers, peer, method, &tail, &body).await {
        Ok((status, mut value)) => {
            if matches!(tail.as_str(), "sample" | "replay")
                || tail.split('/').nth(1) == Some("runs")
            {
                crate::flows::redaction::response(&mut value);
            }
            (status, Json(value)).into_response()
        }
        Err(e) => error(e),
    }
}
fn invalid() -> StoreError {
    StoreError::reason("invalid")
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, StoreError> {
    serde_json::from_value(value).map_err(|_| invalid())
}
fn location(value: &Value) -> Result<Location, StoreError> {
    parse(value.clone())
}
fn required<'a>(v: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    v.get(key).and_then(Value::as_str).ok_or_else(invalid)
}
fn network(
    state: &AppState,
    headers: &HeaderMap,
    peer: ConnectInfo<SocketAddr>,
) -> Result<(), StoreError> {
    if !peer.0.ip().is_loopback() {
        return Err(StoreError::reason("forbidden"));
    }
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| StoreError::reason("forbidden"))?;
    let port = state.http.bind.port();
    if ![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .iter()
    .any(|h| h == host)
    {
        return Err(StoreError::reason("forbidden"));
    }
    if headers
        .get("origin")
        .is_some_and(|o| o.to_str().ok() != Some(format!("http://{host}").as_str()))
        || headers
            .get("sec-fetch-site")
            .is_some_and(|s| !matches!(s.to_str(), Ok("same-origin" | "none")))
    {
        return Err(StoreError::reason("forbidden"));
    }
    Ok(())
}
async fn workspace(
    state: &AppState,
    query: &HashMap<String, String>,
) -> Result<PathBuf, StoreError> {
    let path = PathBuf::from(query.get("workspace").ok_or_else(invalid)?);
    if !path.is_absolute() {
        return Err(invalid());
    }
    let path = path
        .canonicalize()
        .map_err(|_| StoreError::reason("forbidden"))?;
    if !path.is_dir() {
        return Err(invalid());
    }
    let launched = state.http.web.as_ref().is_some_and(|w| {
        w.launch_workspaces
            .iter()
            .any(|p| p.canonicalize().ok().as_ref() == Some(&path))
    });
    let known = state
        .history()
        .workspaces()
        .await
        .map_err(|_| StoreError::reason("io"))?
        .iter()
        .any(|w| std::path::Path::new(&w.workspace) == path);
    if !launched && !known {
        return Err(StoreError::reason("forbidden"));
    }
    Ok(path)
}
async fn handle(
    state: &AppState,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    method: Method,
    tail: &str,
    body: &[u8],
) -> Result<(StatusCode, Value), StoreError> {
    network(state, headers, peer)?;
    let workspace = workspace(state, query).await?;
    let store = state.flows.store(workspace.clone());
    let records = state.flows.records(&workspace);
    let data: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(body).map_err(|_| invalid())?
    };
    let ok = |v| Ok((StatusCode::OK, v));
    match (method.as_str(), tail) {
        ("GET", "catalogue") => ok(
            serde_json::to_value(crate::flows::catalogue::catalogue()).map_err(|_| invalid())?
        ),
        ("GET", "") => {
            store.refresh();
            ok(
                json!({"flows":store.list()?,"errors":state.flows.diagnostics(&workspace),"propagation":"other WithNav processes reload on their next event"}),
            )
        }
        ("POST", "") => {
            let document = store.save(
                required(&data, "name")?,
                location(&data["location"])?,
                None,
                parse(data["content"].clone())?,
            )?;
            Ok((StatusCode::CREATED, json!(document)))
        }
        ("POST", "validate") => ok(json!(validate(parse::<Content>(data["content"].clone())?))),
        ("POST", "reload") => {
            store.reload()?;
            ok(
                json!({"reloaded":true,"errors":[],"flows":store.list()?,"propagation":"other WithNav processes reload on their next event"}),
            )
        }
        ("GET", "trust") | ("POST", "trust") => {
            if method == Method::POST {
                store.set_trust(data["trusted"].as_bool().ok_or_else(invalid)?)?;
            }
            ok(
                json!({"workspace":workspace,"has_workspace_flows":store.list()?.iter().any(|s|s.location==Location::Workspace),"trusted":store.trusted(),"propagation":"other WithNav processes reload on their next event"}),
            )
        }
        ("POST", "sample") => {
            if !store.trusted() {
                return Err(StoreError::reason("untrusted"));
            }
            let compiled = source(&store, &data["source"])?;
            let runtime = state
                .flows
                .runtime(&workspace)
                .ok_or_else(|| StoreError::reason("unavailable"))?;
            let client = runtime.client();
            let event = Event {
                trigger: compiled.definition().flow.on.clone(),
                payload: data["payload"].clone(),
            };
            let cancel = CancellationToken::new();
            let mut context = ContextSnapshot {
                workspace: Some(workspace.clone()),
                ..Default::default()
            };
            if let Some(id) = data["session_id"].as_str() {
                let id = id.parse().map_err(|_| StoreError::reason("unavailable"))?;
                if let Ok(session) = state.session_summary(id).await {
                    if std::path::Path::new(&session.workspace) != workspace {
                        return Err(StoreError::reason("forbidden"));
                    }
                    context.session(&session);
                }
            }
            let mut outcome = run(
                &compiled,
                &event,
                &context,
                client.as_ref(),
                RunOptions {
                    deadline: Instant::now()
                        + Duration::from_millis(
                            compiled.definition().flow.budget_ms.unwrap_or(5000),
                        ),
                    run_id: ulid::Ulid::generate().to_string(),
                },
                &cancel,
            )
            .await;
            outcome.trace.mode = "sample".into();
            crate::flows::redaction::record(&mut outcome.trace);
            records.append(&outcome.trace)?;
            ok(json!(outcome.trace))
        }
        ("POST", "replay") => {
            if !store.trusted() {
                return Err(StoreError::reason("untrusted"));
            }
            let compiled = source(&store, &data["source"])?;
            let id = required(&data, "run_id")?;
            let record = records
                .read()?
                .into_iter()
                .find(|r| r.run_id == id)
                .ok_or_else(|| StoreError::reason("not_found"))?;
            ok(json!(replay(&compiled, &record, &record.identity)))
        }
        ("GET", "health") => {
            let mut health = records.health()?;
            health["errors"] = json!(state.flows.diagnostics(&workspace));
            ok(health)
        }
        _ => {
            let parts: Vec<_> = tail.split('/').collect();
            if parts.len() >= 2 && parts[1] == "runs" && method == Method::GET {
                if parts.len() == 3 {
                    return ok(json!(records
                        .read()?
                        .into_iter()
                        .rev()
                        .filter(|r| r.flow_name == parts[0])
                        .find(|r| r.run_id == parts[2])
                        .ok_or_else(|| StoreError::reason("not_found"))?));
                }
                let limit = query
                    .get("limit")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(20)
                    .clamp(1, 100);
                return ok(json!(records.summaries(
                    parts[0],
                    query.get("before").map(String::as_str),
                    limit,
                )?));
            }
            if parts.len() != 1 {
                return Err(StoreError::reason("not_found"));
            }
            match method.as_str() {
                "GET" => ok(json!(store.get(
                    tail,
                    parse(json!(query.get("location").ok_or_else(invalid)?))?
                )?)),
                "PUT" => ok(json!(store.save(
                    tail,
                    location(&data["location"])?,
                    Some(required(&data, "revision")?),
                    parse(data["content"].clone())?
                )?)),
                "DELETE" => {
                    store.delete(
                        tail,
                        location(&data["location"])?,
                        required(&data, "revision")?,
                    )?;
                    ok(json!({"deleted":true}))
                }
                _ => Err(StoreError::reason("not_found")),
            }
        }
    }
}
fn source(store: &FlowStore, source: &Value) -> Result<CompiledDefinition, StoreError> {
    let v = if source.get("saved").is_some() {
        let saved = &source["saved"];
        let d = store.get(required(saved, "name")?, location(&saved["location"])?)?;
        if d.revision != required(saved, "revision")? {
            return Err(StoreError::reason("stale"));
        }
        validate(Content::Toml {
            toml: d.toml,
            templates: d.templates,
        })
    } else {
        validate(parse(source["content"].clone())?)
    };
    if !v.valid {
        return Err(StoreError {
            reason: "invalid",
            errors: v.errors,
        });
    }
    v.definition
        .ok_or_else(invalid)?
        .validate(
            &crate::flows::catalogue::catalogue(),
            &Capabilities { images: false },
            &v.templates,
        )
        .map_err(|errors| StoreError {
            reason: "invalid",
            errors,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FakeClient;
    impl DecisionClient for FakeClient {
        fn capabilities(&self) -> Capabilities {
            Capabilities { images: false }
        }
        fn identity(&self) -> ClientIdentity {
            ClientIdentity {
                model: "fake/instinct".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            _: instinct_flow::protocol::DecisionRequest<'a>,
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>>
        {
            Box::pin(async { Err(DecisionError::ProviderError) })
        }
    }
    #[tokio::test]
    async fn router_limits_bodies_and_samples_and_replays_with_shared_client() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let state = AppState::new(super::super::ServerConfig {
            bind: "127.0.0.1:8080".parse().unwrap(),
            models_path: None,
            config_dir: dir.path().join("config"),
            allow_non_loopback: false,
            bearer_token: None,
            history_db: dir.path().join("history.db"),
            data_dir: dir.path().join("data"),
            rules_roots: Default::default(),
            web: Some(super::super::WebConfig {
                launch_workspaces: vec![workspace.clone()],
            }),
        })
        .unwrap();
        state
            .flows
            .inject_client(&workspace, std::sync::Arc::new(FakeClient));
        state
            .flows
            .store(workspace.clone())
            .set_trust(true)
            .unwrap();
        let app = super::super::routes::app(state.clone());
        async fn post(
            app: Router,
            workspace: &std::path::Path,
            endpoint: &str,
            body: Vec<u8>,
        ) -> (StatusCode, Value) {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(format!(
                    "/v1/flows/{endpoint}?workspace={}",
                    workspace.display()
                ))
                .header("host", "127.0.0.1:8080")
                .header("content-type", "application/json")
                .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
                .body(axum::body::Body::from(body))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }
        assert_eq!(
            post(
                app.clone(),
                &workspace,
                "sample",
                vec![b' '; 512 * 1024 + 1]
            )
            .await
            .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let toml = "[flow]\nversion=1\nname='sample-test'\non='save'\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='log'\n[node.params]\ntext='hello'\n";
        let source = json!({"content":{"format":"toml","toml":toml,"templates":{}}});
        let (status, record) = post(app.clone(), &workspace, "sample", serde_json::to_vec(&json!({"source":source,"payload":{"uri":"file:///test","language_id":"rust","version":1,"diff":""},"session_id":null})).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(record["mode"], "sample");
        assert_eq!(record["application"]["status"], "not_applied");
        let captures = state.flows.records(&workspace);
        let mut captured: RunRecord = serde_json::from_value(record.clone()).unwrap();
        for i in 0..105 {
            captured.run_id = i.to_string();
            captures.append(&captured).unwrap();
        }
        captured.flow_name = "other".into();
        captured.run_id = "other".into();
        captures.append(&captured).unwrap();
        captured.flow_name = "sample-test".into();
        captured.started_at = "2020-01-01T00:00:00Z".into();
        captures.append(&captured).unwrap();
        let mut query =
            HashMap::from([("workspace".into(), workspace.to_string_lossy().into_owned())]);
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:8080".parse().unwrap());
        let peer = ConnectInfo("127.0.0.1:1234".parse().unwrap());
        for (limit, expected) in [
            (None, 20),
            (Some("0"), 1),
            (Some("101"), 100),
            (Some("-1"), 20),
            (Some("invalid"), 20),
        ] {
            query.remove("limit");
            if let Some(limit) = limit {
                query.insert("limit".into(), limit.into());
            }
            let (_, page) = handle(
                &state,
                &query,
                &headers,
                peer,
                Method::GET,
                "sample-test/runs",
                b"",
            )
            .await
            .unwrap();
            assert_eq!(page["runs"].as_array().unwrap().len(), expected);
            assert_eq!(page["runs"][0]["run_id"], "104");
            assert_eq!(page["runs"][0].as_object().unwrap().len(), 7);
            assert_eq!(page["next_cursor"], page["runs"][expected - 1]["run_id"]);
        }
        query.insert("before".into(), "0".into());
        let (_, last) = handle(
            &state,
            &query,
            &headers,
            peer,
            Method::GET,
            "sample-test/runs",
            b"",
        )
        .await
        .unwrap();
        assert_eq!(last["runs"][0]["run_id"], record["run_id"]);
        assert_eq!(last["next_cursor"], Value::Null);
        query.insert("before".into(), "absent".into());
        let (_, absent) = handle(
            &state,
            &query,
            &headers,
            peer,
            Method::GET,
            "sample-test/runs",
            b"",
        )
        .await
        .unwrap();
        assert_eq!(absent, json!({"runs":[],"next_cursor":null}));
        let (status, replay) = post(
            app,
            &workspace,
            "replay",
            serde_json::to_vec(&json!({"source":source,"run_id":record["run_id"]})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replay["comparable"], true);
    }
    #[tokio::test]
    async fn studio_documents_revisions_reload_and_network_policy() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let state = AppState::new(super::super::ServerConfig {
            bind: "127.0.0.1:8080".parse().unwrap(),
            models_path: None,
            config_dir: dir.path().join("config"),
            allow_non_loopback: false,
            bearer_token: None,
            history_db: dir.path().join("history.db"),
            data_dir: dir.path().join("data"),
            rules_roots: Default::default(),
            web: Some(super::super::WebConfig {
                launch_workspaces: vec![workspace.clone()],
            }),
        })
        .unwrap();
        let query = HashMap::from([("workspace".into(), workspace.to_string_lossy().into_owned())]);
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:8080".parse().unwrap());
        let peer = ConnectInfo("127.0.0.1:1234".parse().unwrap());
        let text="[flow]\nversion=1\nname='test'\non='save'\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='log'\n[node.params]\ntext='hello'\n";
        let content = json!({"format":"toml","toml":text,"templates":{}});
        let (_, valid) = handle(
            &state,
            &query,
            &headers,
            peer,
            Method::POST,
            "validate",
            &serde_json::to_vec(&json!({"content":content})).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(valid["valid"], true);
        let (_, roundtrip) = handle(&state, &query, &headers, peer, Method::POST, "validate", &serde_json::to_vec(&json!({"content":{"format":"graph","definition":valid["definition"],"templates":{}}})).unwrap()).await.unwrap();
        assert_eq!(roundtrip["valid"], true);
        assert_eq!(roundtrip["definition"], valid["definition"]);
        let (status, document) = handle(
            &state,
            &query,
            &headers,
            peer,
            Method::POST,
            "",
            &serde_json::to_vec(&json!({"name":"test","location":"workspace","content":content}))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            handle(
                &state,
                &query,
                &headers,
                peer,
                Method::POST,
                "",
                &serde_json::to_vec(
                    &json!({"name":"test","location":"workspace","content":content})
                )
                .unwrap()
            )
            .await
            .unwrap_err()
            .reason,
            "already_exists"
        );
        let (_, listed) = handle(&state, &query, &headers, peer, Method::GET, "", b"")
            .await
            .unwrap();
        assert!(listed.to_string().contains("test"));
        assert_eq!(
            listed["propagation"],
            "other WithNav processes reload on their next event"
        );
        let (_, trust) = handle(&state, &query, &headers, peer, Method::GET, "trust", b"")
            .await
            .unwrap();
        assert_eq!(trust["trusted"], false);
        assert_eq!(
            handle(
                &state,
                &query,
                &headers,
                ConnectInfo("192.0.2.1:1234".parse().unwrap()),
                Method::GET,
                "catalogue",
                b""
            )
            .await
            .unwrap_err()
            .reason,
            "forbidden"
        );
        let unknown = HashMap::from([("workspace".into(), "/unknown-workspace".into())]);
        assert_eq!(
            handle(
                &state,
                &unknown,
                &headers,
                peer,
                Method::GET,
                "catalogue",
                b""
            )
            .await
            .unwrap_err()
            .reason,
            "forbidden"
        );
        let stale = handle(
            &state,
            &query,
            &headers,
            peer,
            Method::PUT,
            "test",
            &serde_json::to_vec(
                &json!({"location":"workspace","revision":"wrong","content":content}),
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(stale.reason, "stale");
        let mut get = query.clone();
        get.insert("location".into(), "workspace".into());
        let (_, read) = handle(&state, &get, &headers, peer, Method::GET, "test", b"")
            .await
            .unwrap();
        assert_eq!(read["revision"], document["revision"]);
        handle(
            &state,
            &query,
            &headers,
            peer,
            Method::POST,
            "trust",
            br#"{"trusted":true}"#,
        )
        .await
        .unwrap();
        let (_, reloaded) = handle(
            &state,
            &query,
            &headers,
            peer,
            Method::POST,
            "reload",
            b"{}",
        )
        .await
        .unwrap();
        assert_eq!(reloaded["reloaded"], true);
        assert_eq!(
            reloaded["propagation"],
            "other WithNav processes reload on their next event"
        );
        let (_,saved)=handle(&state,&query,&headers,peer,Method::PUT,"test",&serde_json::to_vec(&json!({"location":"workspace","revision":document["revision"],"content":{"format":"toml","toml":"broken","templates":{}}})).unwrap()).await.unwrap();
        assert!(!saved["errors"].as_array().unwrap().is_empty());
        assert_eq!(
            handle(
                &state,
                &query,
                &headers,
                peer,
                Method::POST,
                "reload",
                b"{}"
            )
            .await
            .unwrap_err()
            .reason,
            "reload_invalid"
        );
        headers.insert("origin", "http://foreign.example".parse().unwrap());
        assert_eq!(
            handle(
                &state,
                &query,
                &headers,
                peer,
                Method::GET,
                "catalogue",
                b""
            )
            .await
            .unwrap_err()
            .reason,
            "forbidden"
        );
    }
}
