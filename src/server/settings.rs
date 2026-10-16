//! Bearer-protected settings transport and catalogue-aware field policy.
//! `restart`: after WithNav restarts (conservative for layers). `new_sessions`: next use, including
//! new sessions, model/agent switches and newly started subagents; running turns keep their values.
//! `live`: the changing client applies it at once; other open clients on their next start.
use super::session::AppState;
use crate::{
    agent::definitions::{AgentRoots, Agents},
    provider::{
        auth,
        config::{ModelsConfig, load_models},
    },
    settings,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf};
mod editor;

fn failure(status: StatusCode, error: impl ToString) -> Response {
    (status, Json(json!({"error":error.to_string()}))).into_response()
}
fn workspace(query: &HashMap<String, String>) -> Result<PathBuf, Box<Response>> {
    let path = query.get("workspace").ok_or_else(|| {
        failure(
            StatusCode::BAD_REQUEST,
            "missing query parameter: workspace",
        )
    })?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(Box::new(failure(
            StatusCode::BAD_REQUEST,
            "workspace must be an absolute path",
        )));
    }
    Ok(path)
}
fn context(
    state: &AppState,
    ws: &std::path::Path,
) -> Result<(ModelsConfig, PathBuf, Agents), Box<Response>> {
    let (catalog, path) = load_models(
        state.config.models_path.as_deref(),
        ws,
        &state.config.config_dir,
    )
    .map_err(|_| {
        Box::new(failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot load models.json",
        ))
    })?;
    let agents = Agents::load(
        &AgentRoots {
            home: state.config.rules_roots.home.clone(),
            config_dir: Some(state.config.config_dir.clone()),
        },
        ws,
    );
    Ok((catalog, path, agents))
}
fn body(state: &AppState, ws: &std::path::Path) -> Result<Value, Box<Response>> {
    let dir = &state.config.config_dir;
    let (settings, revision) = settings::load_with_revision(dir)
        .map_err(|e| Box::new(failure(StatusCode::INTERNAL_SERVER_ERROR, e)))?;
    let (catalog, models, agents) = context(state, ws)?;
    let fields: Vec<Value> = editor::keys(&agents).into_iter().map(|key| {
        let raw = settings.get_dotted(&key);
        let assignment = key.starts_with("layers.") || key.starts_with("agents.");
        let value = raw.map(|v| if assignment { editor::choice(v) } else { v.clone() }).unwrap_or(Value::Null);
        let error = if raw.is_some_and(Value::is_null) {
            Some(format!("invalid stored value for {key}"))
        } else {
            raw.and_then(|_| editor::validate(&key,&value,&catalog,&agents,&tiers(state,&catalog,&value)).err())
        };
        json!({"key":key,"label":editor::label(&key),"description":editor::description(&key),"value":value,"default":editor::default_value(&key),"set":raw.is_some(),"error":error,"applies":editor::applies(&key)})
    }).collect();
    let mut gateways = Vec::new();
    for (name, p) in &catalog.providers {
        let source = auth::key_source(dir, name, p.api_key_env.as_deref()).map_err(|_| {
            Box::new(failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot read credential provenance",
            ))
        })?;
        let used: Vec<&Value> = fields
            .iter()
            .filter(|f| {
                f["value"]["model"]
                    .as_str()
                    .is_some_and(|m| m.starts_with(&format!("{name}/")))
            })
            .map(|f| &f["key"])
            .collect();
        // A base URL may carry credentials (`user:pass@`, `?api_key=`); clients only ever get
        // the endpoint without them, like every other place WithNav names an endpoint.
        let base_url = crate::provider::catalog::endpoint_without_credentials(&p.base_url);
        gateways.push(json!({"name":name,"base_url":base_url,"api_key_env":p.api_key_env,"key_source":source,"catalog_format":p.catalog_format,"connect_timeout_ms":p.connect_timeout_ms,"request_timeout_ms":p.request_timeout_ms,"headers":p.headers.keys().collect::<Vec<_>>(),"model_count":catalog.models.values().filter(|m| &m.provider == name).count(),"used_by":used}));
    }
    Ok(
        json!({"revision":revision,"paths":{"config":settings::path(dir),"models":models,"agents":[dir.join("agents"),crate::paths::workspace_dir(ws).join("agents")]},"fields":fields,"gateways":gateways}),
    )
}
pub(super) async fn get(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let ws = match workspace(&query) {
        Ok(ws) => ws,
        Err(r) => return *r,
    };
    tokio::task::spawn_blocking(move || match body(&state, &ws) {
        Ok(v) => Json(v).into_response(),
        Err(r) => *r,
    })
    .await
    .unwrap_or_else(|e| failure(StatusCode::INTERNAL_SERVER_ERROR, e))
}
pub(super) async fn put(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    payload: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let ws = match workspace(&query) {
        Ok(ws) => ws,
        Err(r) => return *r,
    };
    let value = match payload {
        Ok(Json(value)) => value,
        Err(rejection) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":rejection.body_text(),"reason":"invalid","key":null})),
            )
                .into_response();
        }
    };
    let _lock = state.settings_write.clone().lock_owned().await;
    tokio::task::spawn_blocking(move || {
        let _lock = _lock;
        put_sync(state, ws, value)
    })
    .await
    .unwrap_or_else(|e| failure(StatusCode::INTERNAL_SERVER_ERROR, e))
}

fn put_sync(state: AppState, ws: PathBuf, value: Value) -> Response {
    let invalid = |error: String| {
        (StatusCode::BAD_REQUEST,Json(json!({"error":error,"reason":"invalid","key":value.get("key").unwrap_or(&Value::Null)}))).into_response()
    };
    let Some(key) = value.get("key").and_then(Value::as_str) else {
        return invalid("expected {revision, key, value}".into());
    };
    let Some(proposed) = value.get("value") else {
        return invalid("expected {revision, key, value}".into());
    };
    let dir = &state.config.config_dir;
    let (catalog, _, agents) = match context(&state, &ws) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let stored = match editor::validate(
        key,
        proposed,
        &catalog,
        &agents,
        &tiers(&state, &catalog, proposed),
    ) {
        Ok(v) => v,
        Err(e) => return invalid(e),
    };
    if let Err(e) = settings::update(dir, |settings, current| {
        if value["revision"].as_str() != Some(current) {
            return Err(UpdateError::Stale(current.to_owned()));
        }
        editor::apply(settings, key, stored);
        Ok(())
    }) {
        return match e {
            UpdateError::Stale(current) => (StatusCode::CONFLICT, Json(json!({"error":"config.json changed since it was read","reason":"stale","revision":current}))).into_response(),
            UpdateError::Storage(e) => failure(StatusCode::INTERNAL_SERVER_ERROR, e),
        };
    }
    match body(&state, &ws) {
        Ok(v) => Json(v).into_response(),
        Err(r) => *r,
    }
}
pub(super) async fn test(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let ws = match workspace(&query) {
        Ok(ws) => ws,
        Err(r) => return *r,
    };
    let (catalog, _, _) = match context(&state, &ws) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let Some(provider) = catalog.providers.get(&name) else {
        return failure(StatusCode::NOT_FOUND, format!("unknown gateway {name}"));
    };
    let key = match auth::resolve_key(&state.config.config_dir,&name,provider.api_key_env.as_deref()) { Ok(k) => k, Err(crate::error::ProviderError::MissingApiKey) => String::new(), Err(_) => return Json(json!({"ok":false,"status":null,"latency_ms":0,"model_count":null,"error":"cannot read credentials"})).into_response() };
    Json(crate::provider::catalog::test_gateway(&name, provider, key).await).into_response()
}

fn tiers(state: &AppState, catalog: &ModelsConfig, value: &Value) -> Vec<String> {
    let Some(model) = value.get("model").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Ok(name) = crate::provider::model_name::ModelName::parse(model, catalog) else {
        return Vec::new();
    };
    let Ok(resolved) = catalog.resolve_name(&name) else {
        return Vec::new();
    };
    state
        .settings_model_data(&resolved)
        .limits
        .and_then(|l| l.effort_tiers)
        .unwrap_or_default()
}

enum UpdateError {
    Stale(String),
    Storage(settings::SettingsError),
}
impl From<settings::SettingsError> for UpdateError {
    fn from(error: settings::SettingsError) -> Self {
        Self::Storage(error)
    }
}
