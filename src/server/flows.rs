//! Studio's private JSON adapter. Every operation authorizes an existing workspace.
use super::AppState;
use crate::flows::{
    records::Records,
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
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(super) struct Host {
    config: PathBuf,
    data: PathBuf,
    models: Option<PathBuf>,
    stores: Mutex<BTreeMap<PathBuf, Arc<FlowStore>>>,
}
impl Host {
    pub fn new(config: PathBuf, data: PathBuf, models: Option<PathBuf>) -> Self {
        Self {
            config,
            data,
            models,
            stores: Mutex::new(BTreeMap::new()),
        }
    }
    fn store(&self, workspace: PathBuf) -> Arc<FlowStore> {
        self.stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(workspace.clone())
            .or_insert_with(|| Arc::new(FlowStore::new(workspace, self.config.clone())))
            .clone()
    }
}
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
    body: Bytes,
) -> Response {
    dispatch(state, query, headers, peer, method, "".into(), body).await
}
async fn tail(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    method: Method,
    body: Bytes,
) -> Response {
    dispatch(state, query, headers, peer, method, tail, body).await
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
        Ok((status, value)) => (status, Json(value)).into_response(),
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
    let path = path.canonicalize().map_err(|_| invalid())?;
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
    let records = Records::new(&state.flows.data, &workspace);
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
        ("GET", "") => ok(json!({"flows":store.list()?})),
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
            ok(json!({"reloaded":true,"errors":[],"flows":store.list()?}))
        }
        ("GET", "trust") | ("POST", "trust") => {
            if method == Method::POST {
                store.set_trust(data["trusted"].as_bool().ok_or_else(invalid)?)?;
            }
            ok(
                json!({"workspace":workspace,"has_workspace_flows":store.list()?.iter().any(|s|s.location==Location::Workspace),"trusted":store.trusted()}),
            )
        }
        ("POST", "sample") => {
            if !store.trusted() {
                return Err(StoreError::reason("untrusted"));
            }
            if !data["session_id"].is_null() {
                return Err(StoreError::reason("unavailable"));
            }
            let compiled = source(&store, &data["source"])?;
            let (models, _) = crate::provider::config::load_models(
                state.flows.models.as_deref(),
                &workspace,
                &state.flows.config,
            )
            .map_err(|_| StoreError::reason("unavailable"))?;
            let client = crate::flows::client::NavaiDecisionClient::new(
                &models,
                &state.flows.config,
                crate::provider::openai::ClientCache::new().into(),
            )
            .map_err(|_| StoreError::reason("unavailable"))?;
            let event = Event {
                trigger: compiled.definition().flow.on.clone(),
                payload: data["payload"].clone(),
            };
            let cancel = CancellationToken::new();
            let context = ContextSnapshot::default();
            let mut outcome = run(
                &compiled,
                &event,
                &context,
                &client,
                RunOptions {
                    deadline: Instant::now()
                        + Duration::from_millis(
                            compiled.definition().flow.budget_ms.unwrap_or(5000),
                        ),
                    run_id: ulid::Ulid::new().to_string(),
                },
                &cancel,
            )
            .await;
            outcome.trace.mode = "sample".into();
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
            let end = time::OffsetDateTime::now_utc();
            let start = end - time::Duration::hours(24);
            let mut rows: BTreeMap<String, (u64, u64, u64, f64, u64)> = BTreeMap::new();
            for r in records.read()? {
                if r.mode != "live"
                    || !time::OffsetDateTime::parse(
                        &r.started_at,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .is_ok_and(|t| t >= start && t <= end)
                {
                    continue;
                }
                let row = rows.entry(r.flow_name).or_default();
                row.0 += 1;
                row.1 += u64::from(r.status == "failure");
                row.2 +=
                    u64::from(!r.fallback_actions.is_empty() && r.application.status == "applied");
                if let Some(cost) = r.cost_usd {
                    row.3 += cost;
                } else {
                    row.4 += 1;
                }
            }
            ok(
                json!({"window_start":start.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),"window_end":end.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),"flows":rows.into_iter().map(|(name,r)|json!({"name":name,"runs":r.0,"failures":r.1,"fallbacks_applied":r.2,"cost_usd":if r.0==r.4 {None} else {Some(r.3)},"cost_unknown_runs":r.4})).collect::<Vec<_>>()}),
            )
        }
        _ => {
            let parts: Vec<_> = tail.split('/').collect();
            if parts.len() >= 2 && parts[1] == "runs" && method == Method::GET {
                let mut runs: Vec<_> = records
                    .read()?
                    .into_iter()
                    .filter(|r| r.flow_name == parts[0])
                    .collect();
                runs.reverse();
                if parts.len() == 3 {
                    return ok(json!(runs
                        .into_iter()
                        .find(|r| r.run_id == parts[2])
                        .ok_or_else(|| StoreError::reason("not_found"))?));
                }
                if let Some(before) = query.get("before") {
                    runs = runs
                        .into_iter()
                        .skip_while(|r| &r.run_id != before)
                        .skip(1)
                        .collect();
                }
                let limit = query
                    .get("limit")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(20)
                    .clamp(1, 100);
                let next = if runs.len() > limit {
                    runs.get(limit - 1).map(|r| r.run_id.clone())
                } else {
                    None
                };
                runs.truncate(limit);
                return ok(
                    json!({"runs":runs.iter().map(|r|json!({"run_id":r.run_id,"flow_name":r.flow_name,"started_at":r.started_at,"status":r.status,"duration_ms":r.duration_ms,"cost_usd":r.cost_usd,"mode":r.mode})).collect::<Vec<_>>(),"next_cursor":next}),
                );
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
        handle(
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
