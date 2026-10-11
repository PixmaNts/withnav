//! Axum HTTP surface health, session create/get, message and
//! stop routing. SSE streaming lives in `server::sse`.
//!
//! Response bodies are FROZEN:
//! - `GET  /v1/health`        -> `{status:"ok"}`
//! - `GET /v1/layers?workspace=` -> `{layers:[{layer,status,model,variant,gateway,upstream_id,api,reason,variant_diagnostic?}]}`; exactly three lower layers, usable means validated assignment/API, not connectivity. Cached-only unresolved preferred-low is explicit, never a resolved runtime variant.
//! - `GET /v1/agents?workspace=&session_id=` -> `{default_agent,agents:[{id,description,mode,assignment,effective,override?,warning,inherits_parent}]}`; canonical model identities and sanitized source/variant summaries only.
//!   Session definitions are pinned for live sessions; base assignments project current config.
//!   Missing authoritative saved model/variant provenance returns a diagnostic, never inferred override sources.
//! - `GET  /v1/models?workspace=` -> canonical catalog entries and `default_agent`
//! - `GET /v1/settings?workspace=` -> `{revision,paths,fields,gateways}`; no keys or header values.
//! - `PUT /v1/settings?workspace=` accepts `{revision,key,value}`; null removes, stale revisions
//!   return 409 reason `stale`, invalid choices 400 reason `invalid`. Writes are serialized and atomic.
//!   `restart`: after WithNav restarts (conservative for layers). `new_sessions`: next use, including
//!   new sessions, model/agent switches and newly started subagents; running turns keep their values.
//!   `live`: changing client applies at once; other open clients on their next start.
//! - `POST /v1/gateways/{name}/test?workspace=` -> `{ok,status,latency_ms,model_count,error}`;
//!   probes `/models` without writing the catalogue or returning upstream secrets.
//! - `POST /v1/sessions`      -> `{session_id, root_actor_id:"agent:main", actor_state:"idle"}`
//!   (422 `{"error", "reason":"no_context_window"}` for a chat alias with no known window)
//! - `GET  /v1/sessions/:id`  -> `{session_id, actor_state, workspace, model, variant, current_seq}`
//!   (`variant` is the variant name or null; `POST /v1/sessions` takes an optional `variant`)
//! - `POST /v1/sessions/:id/messages` -> `{command_id, accepted:true, actor_state, notice?}`
//!   (frozen error bodies: 404 unknown id / 409 stopping / 503 `{"accepted":false,"reason":"inbox_full"}`
//!   / 400 unsupported content or invalid skill selection). All errors share the `{"error": ...}` shape.
//!   A stopped conversation (stopped, or stored only) answers 409
//!   `{"error", "reason":"resume_required"}`: resume it first.
//! - `POST /v1/sessions/:id/stop` -> `{accepted:true, actor_state:"stopped"}`
//! - `POST /v1/sessions/:id/permissions/:request_id` `{decision:"allow_once"|"allow_folder"|"deny", message?}`
//!   -> `{accepted:true}`: the typed answer to a `permission.requested`. 404 unknown session or
//!   malformed id, 409 `{"error","reason":"permission_stale"}` for a request that is not waiting
//!   (answered, cancelled, expired, never existed), 400 for a body that is not a decision.
//! - `GET  /v1/sessions/:id/context` -> the context snapshot (`alias, variant?, window, window_source, estimate_total, breakdown{system,rules,skills_catalog,tools,messages}, output_reserve, margin, input_budget, percent, last_usage?{prompt_tokens,completion_tokens,step}, revision`); an estimate of the NEXT request, `last_usage` is the provider's count for the last one
//! - `GET /v1/sessions/:id/skills` -> `{skills:[{name,description}],disabled_reason?}`: only catalog-listed skills; no rows when the agent disables `skill`.
//!   Message content accepts one `{type:"skill",name}` alongside text. Admission loads it through
//!   the skill tool's bounded loader before accepting the message; unknown, omitted, disabled or
//!   unreadable skills return 400 without sending anything. The exact bounded tool-loader output
//!   is JSON-escaped into a delimited block in the Captain's own user message, never a synthetic tool call.
//!   Accepted selection is recorded as optional `message.received.selected_skill:{name,origin:"captain"}`.
//!   If the consuming turn's pinned agent disables skills, the whole command is refused before
//!   transcript mutation; `command.result` carries its command id as operation id and correlation.
//! - `GET  /v1/workspaces`    -> `{workspaces:[{path,name,launched,session_count,updated_at?,branch?,dirty?}]}`:
//!   the workspaces `nav web` was started with (`launched:true`, argument order), then the
//!   history's other workspaces that still exist, newest activity first; child sessions are not counted.
//!   Hidden history workspaces are omitted; creating a session makes its workspace visible again.
//! - `DELETE /v1/workspaces?path=<abs>` -> 204, hides without deleting sessions.
//!   With `sessions=delete`, stops and deletes root sessions and their children, returns `{deleted:N}` (root count).
//!   A command-line workspace returns 409 `{reason:"launched",error}`; invalid paths return 400.
//! - `POST /v1/workspaces?path=<abs>` -> 204, idempotently unhides an existing directory.
//! - `GET  /v1/intent?workspace=<abs path>` -> `{exists,intent,plan,decisions,active_spec,raw}`
//!   from `.withnav/intent.md` (nullable strings; 400 without an absolute `workspace`)
//! - Every other path, only when started by `nav web` (`ServerConfig.web`): the embedded browser
//!   client's files, `index.html` for unknown non-`/v1/` paths; no token needed, unknown `/v1/` paths stay 404.
//! - `GET  /v1/commands`      -> `{commands:[{name, description, args_hint}]}`
//! - `POST /v1/sessions/:id/commands` `{name, arguments?}` -> `{operation_id, ok, data?, error?}`
//!   (a command that cannot run is 200 with `ok:false` and an `error`; the same
//!   statuses as `/messages` for a session that cannot take commands)

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use ulid::Ulid;

use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::ROOT_ACTOR_ID;
use crate::error::ServerError;
use crate::server::session::{AppState, ServerConfig};

/// Frozen health body.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct HealthBody {
    pub status: &'static str,
}

/// Distinguishes an omitted field (`None`, by `serde(default)`) from an
/// explicit `null` (`Some(None)`).
fn present_or_null<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

/// Frozen create-session request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionRequest {
    pub workspace: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub editor: Option<bool>,
    /// The primary agent a new session runs as (`build` when absent). A
    /// resumed session keeps the agent it was stored with.
    #[serde(default)]
    pub agent: Option<String>,
    /// The model variant to start on: a string is the exact name of a
    /// variant the alias offers, `null` means no variant, and an omitted
    /// field means the alias's default variant.
    #[serde(default, deserialize_with = "present_or_null")]
    pub variant: Option<Option<String>>,
    /// Optional id of a past conversation to resume (same id returned; the
    /// conversation continues with its stored events and transcript).
    #[serde(default)]
    pub resume: Option<String>,
}

/// Frozen create-session response.
#[derive(Debug, Serialize, PartialEq)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub root_actor_id: &'static str,
    pub actor_state: &'static str,
}

/// `POST /v1/sessions/:id/messages` request. The server — never
/// the client — assigns `source_actor_id` (`user:http`), so the wire carries
/// no impersonatable sender field.
#[derive(Debug, Deserialize)]
pub struct PostMessageRequest {
    #[serde(default)]
    pub answer: Option<crate::tools::control::AskAnswer>,
    pub target_actor_id: String,
    #[serde(default)]
    pub mode: Option<String>,
    pub content: Vec<ContentBlock>,
}

/// The frozen 503 body.
#[derive(Debug, Serialize, PartialEq)]
pub struct InboxFullResponse {
    pub accepted: bool,
    pub reason: &'static str,
}

/// The frozen stop ack.
#[derive(Debug, Serialize, PartialEq)]
pub struct StopAccepted {
    pub accepted: bool,
    pub actor_state: &'static str,
}

/// The Axum router for this slice's routes. Tests mount it directly via
/// `tower::ServiceExt::oneshot`.
pub fn app(state: AppState) -> Router {
    let token = state.http.bearer_token.clone();
    let web = state.http.web.is_some();
    let router = Router::new()
        .route("/v1/health", get(health))
        .merge(super::flows::router())
        .route("/v1/models", get(list_models))
        .route("/v1/layers", get(list_layers))
        .route("/v1/agents", get(list_agents))
        .route("/v1/skill-grants", get(list_skill_grants))
        .route(
            "/v1/skill-grants/{skill}",
            axum::routing::delete(revoke_skill_grant),
        )
        .route(
            "/v1/settings",
            get(super::settings::get).put(super::settings::put),
        )
        .route("/v1/gateways/{name}/test", post(super::settings::test))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/messages", post(post_message))
        .route("/v1/sessions/{id}/stop", post(stop_session))
        .route(
            "/v1/sessions/{parent}/children/{child}/continue",
            post(continue_child),
        )
        .route(
            "/v1/sessions/{id}/permissions/{request_id}",
            post(answer_permission),
        )
        .route("/v1/sessions/{id}/context", get(get_context))
        .route("/v1/sessions/{id}/skills", get(get_skills))
        .route(
            "/v1/workspaces",
            get(crate::server::web::list_workspaces)
                .delete(crate::server::web::hide_workspace)
                .post(crate::server::web::unhide_workspace),
        )
        .route("/v1/intent", get(crate::server::web::get_intent))
        .route("/v1/commands", get(list_commands))
        .route("/v1/sessions/{id}/commands", post(post_command))
        .route("/v1/sessions/{id}/bypass", post(post_bypass))
        .route(
            "/v1/sessions/{id}/events",
            get(crate::server::sse::get_events),
        )
        .with_state(state);
    let router = if web {
        router.fallback(crate::server::web::serve_asset)
    } else {
        router
    };
    router.layer(axum::middleware::from_fn(
        move |request: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| {
            let token = token.clone();
            async move {
                let path = request.uri().path();
                let public =
                    path == "/v1/health" || (web && crate::server::web::is_public_asset_path(path));
                if !public {
                    if let Some(token) = token {
                        use axum::http::header::AUTHORIZATION;
                        let expected = format!("Bearer {token}");
                        if request
                            .headers()
                            .get(AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            != Some(&expected)
                        {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                    }
                }
                next.run(request).await
            }
        },
    ))
}

/// Loopback bind + graceful shutdown (SIGINT/SIGTERM -> every active session
/// gets its sequencer fence fired, emitting `session.end`, then the server
/// drains and exits).
pub async fn serve(
    config: ServerConfig,
    hooks: std::sync::Arc<dyn crate::session::SessionHooks>,
) -> Result<(), ServerError> {
    config.validate()?;
    let addr: SocketAddr = config.bind;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_state_on(AppState::with_hooks(config, hooks)?, listener).await
}

/// The serve body over an already-bound listener with a fresh shared state —
/// the testable seam for the real-socket acceptance test (ephemeral
/// `127.0.0.1:0` bind).
pub async fn serve_on(
    config: ServerConfig,
    listener: tokio::net::TcpListener,
) -> Result<(), ServerError> {
    serve_state_on(AppState::new(config)?, listener).await
}

/// The serve body over an already-bound listener and a CALLER-SUPPLIED
/// state: the real graceful-shutdown path (`with_graceful_shutdown` +
/// `shutdown_signal`) runs against the same registry the caller holds, so a
/// test can create sessions and observe their events through it.
pub async fn serve_state_on(
    state: AppState,
    listener: tokio::net::TcpListener,
) -> Result<(), ServerError> {
    let mut state = state;
    Arc::make_mut(&mut state.http).bind = listener.local_addr()?;
    let app = app(state.clone());
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(state))
    .await?;
    Ok(())
}

/// Serve until an EXTERNAL shutdown future resolves (the navigator drives it
/// with its LSP exit), then close every session the same way a signal
/// would: each one's fence emits `session.end`, then the server drains.
/// Unlike [`serve_state_on`] there is no signal handler — the caller owns
/// the shutdown condition.
pub async fn serve_until(
    state: AppState,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    let shutdown_state = state.clone();
    let shutdown = async move {
        shutdown.await;
        shutdown_state.close_all().await;
    };
    let mut state = state;
    Arc::make_mut(&mut state.http).bind = listener.local_addr()?;
    let app = app(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}

/// SIGINT/SIGTERM: close all sessions (each emits `session.end{final_seq}`,
/// tombstoning itself) and only then release the server to finish draining.
async fn shutdown_signal(state: AppState) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    state.close_all().await;
}

async fn health() -> Json<HealthBody> {
    Json(HealthBody { status: "ok" })
}

#[derive(Deserialize)]
struct AssignmentQuery {
    workspace: Option<String>,
    session_id: Option<String>,
}

async fn list_layers(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<AssignmentQuery>,
) -> Response {
    let Some(workspace) = query.workspace else {
        return error_response(ServerError::MalformedBody(
            "missing query parameter: workspace".into(),
        ));
    };
    match state
        .assignment_layers(std::path::Path::new(&workspace))
        .await
    {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(error),
    }
}

async fn list_agents(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<AssignmentQuery>,
) -> Response {
    let Some(workspace) = query.workspace else {
        return error_response(ServerError::MalformedBody(
            "missing query parameter: workspace".into(),
        ));
    };
    let session = match query.session_id {
        Some(id) => match id.parse() {
            Ok(id) => Some(id),
            Err(_) => return error_response(ServerError::SessionNotFound(id)),
        },
        None => None,
    };
    match state
        .assignment_agents(std::path::Path::new(&workspace), session)
        .await
    {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(error),
    }
}

async fn create_session(
    State(state): State<AppState>,
    body: Result<Json<CreateSessionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(parsed) => parsed,
        Err(rejection) => {
            return error_response(ServerError::MalformedBody(rejection.body_text()));
        }
    };
    let resume: Option<Ulid> = match request.resume.as_deref().map(str::parse) {
        None => None,
        Some(Ok(parsed)) => Some(parsed),
        Some(Err(_)) => {
            return error_response(ServerError::SessionNotFound(request.resume.unwrap()));
        }
    };
    if resume.is_some()
        && (request.editor.is_some()
            || request.model.is_some()
            || request.agent.is_some()
            || request.variant.is_some())
    {
        return error_response(ServerError::MalformedBody(
            "resume accepts only workspace and resume; saved editor and selection are retained"
                .to_string(),
        ));
    }
    let session_id = match resume {
        Some(_) => {
            state
                .create_or_resume_session(
                    std::path::Path::new(&request.workspace),
                    request.model.as_deref(),
                    false,
                    resume,
                )
                .await
        }
        None => state
            .create_session_with(
                std::path::Path::new(&request.workspace),
                request.model.as_deref(),
                request.editor.unwrap_or(false),
                match request.variant {
                    None => crate::session::VariantChoice::Default,
                    Some(None) => crate::session::VariantChoice::Clear,
                    Some(Some(name)) => crate::session::VariantChoice::Exact(name),
                },
                request.agent.as_deref(),
            )
            .await
            .map(|id| (id, false)),
    };
    match session_id {
        Ok((id, _)) => (
            StatusCode::OK,
            Json(CreateSessionResponse {
                session_id: id.to_string(),
                root_actor_id: ROOT_ACTOR_ID,
                actor_state: "idle",
            }),
        )
            .into_response(),
        Err(err) => error_response(err),
    }
}

async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let parsed = match parse_session_id(id) {
        Ok(parsed) => parsed,
        Err(err) => return error_response(err),
    };
    match state.session_summary(parsed).await {
        Ok(summary) => Json(summary).into_response(),
        Err(err) => error_response(err),
    }
}

/// One row of `GET /v1/sessions?workspace=`: the stored summary plus the
/// registry's liveness.
#[derive(Debug, Serialize)]
pub struct SessionListEntry {
    pub bypass: bool,
    pub bypass_seq: u64,
    pub incarnation: Option<String>,
    pub editor: bool,
    pub read_only: bool,
    pub resume_disabled_reason: Option<String>,
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Number of captain messages (`message.received` events).
    pub message_count: u64,
    /// The primary agent the session runs as.
    pub agent: String,
    pub model: String,
    pub variant: Option<String>,
    pub source: Option<String>,
    pub variant_source: Option<String>,
    pub gateway: String,
    pub upstream_id: String,
    pub current_seq: u64,
    /// True when a live, active session with this id exists in this process.
    pub live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_status: Option<String>,
    #[serde(skip_serializing_if = "is_promptable")]
    pub promptable: bool,
}

fn is_promptable(value: &bool) -> bool {
    *value
}

#[derive(Debug, Serialize)]
pub struct SessionListBody {
    pub sessions: Vec<SessionListEntry>,
}

/// `GET /v1/sessions?workspace=<abs path>`: the workspace's conversations,
/// newest `updated_at` first.
async fn list_sessions(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(workspace) = params.get("workspace") else {
        return error_response(ServerError::MalformedBody(
            "missing query parameter: workspace".to_string(),
        ));
    };
    match state.list(workspace).await {
        Ok(rows) => {
            let mut sessions = Vec::with_capacity(rows.len());
            for row in rows {
                let summary = match state.session_summary(row.id).await {
                    Ok(summary) => summary,
                    Err(error) => {
                        // A session whose stored selection cannot be read must not hide
                        // the other conversations of the workspace.
                        tracing::warn!(session_id = %row.id, %error, "session list: unreadable session skipped");
                        continue;
                    }
                };
                sessions.push(SessionListEntry {
                    bypass: summary.bypass,
                    bypass_seq: summary.bypass_seq,
                    incarnation: summary.incarnation,
                    editor: summary.editor,
                    read_only: row.read_only,
                    resume_disabled_reason: row.resume_disabled_reason,
                    id: row.id.to_string(),
                    title: row.title,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                    message_count: row.message_count,
                    agent: summary.agent,
                    model: summary.model,
                    variant: summary.variant,
                    source: summary.source,
                    variant_source: summary.variant_source,
                    gateway: summary.gateway,
                    upstream_id: summary.upstream_id,
                    current_seq: summary.current_seq,
                    live: row.live,
                    parent_session_id: row.parent_session_id,
                    child_status: row.child_status,
                    promptable: row.promptable,
                });
            }
            (StatusCode::OK, Json(SessionListBody { sessions })).into_response()
        }
        Err(err) => error_response(err),
    }
}

/// `DELETE /v1/sessions/{id}`: 204 on success; 404 only when the id is
/// neither in memory nor on disk.
async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let parsed = match parse_session_id(id) {
        Ok(parsed) => parsed,
        Err(err) => return error_response(err),
    };
    match state.delete(parsed).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error_response(err),
    }
}

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub name: String,
    pub gateway: String,
    pub provider_group: String,
    pub upstream_id: String,
    pub api: crate::provider::config::ApiKind,
    pub supports_tools: bool,
    pub supports_parallel_tool_calls: Option<bool>,
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// Where `context_window` came from: `models.json` or a `discovered.*` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_source: Option<String>,
    /// The provider's model list could not be refreshed and the discovered
    /// values shown are past their 24 h TTL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_age_secs: Option<u64>,
    /// Short, secret-free reason of the failed refresh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// An explicit `context_window` that differs from the provider's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discrepancy: Option<DiscrepancyBody>,
    /// Names of the variants the model offers; never their override objects.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// The default variant, when the model sets one that it offers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_variant: Option<String>,
    /// The configured `default_variant`, when it names a variant that is not
    /// on offer now (a tier the provider does not report, or could not be
    /// asked for); `refresh_error` says why when the lookup failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_variant_unavailable: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiscrepancyBody {
    pub configured: u64,
    pub discovered: u64,
}

#[derive(Debug, Serialize)]
pub struct ModelsBody {
    pub models: Vec<ModelEntry>,
    pub default_agent: String,
}

/// `GET /v1/models?workspace=<abs path>`: the canonical model catalog.
async fn list_models(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(workspace) = params.get("workspace") else {
        return error_response(ServerError::MalformedBody(
            "missing query parameter: workspace".to_string(),
        ));
    };
    let workspace = std::path::Path::new(workspace);
    let catalog = crate::provider::config::load_models(
        state.config.models_path.as_deref(),
        workspace,
        &state.config.config_dir,
    );
    match catalog {
        Ok((catalog, _)) => {
            let default_agent = match crate::settings::load(&state.config.config_dir)
                .map_err(|_| "settings unavailable")
                .and_then(|s| {
                    let id = s.default_agent().map_err(|_| "invalid default_agent")?;
                    crate::agent::definitions::AgentId::parse(id)
                        .map(|_| id.to_string())
                        .map_err(|_| "invalid default_agent")
                }) {
                Ok(agent) => agent,
                Err(reason) => return error_response(ServerError::MalformedBody(reason.into())),
            };
            for model in catalog.models.values() {
                let name = format!("{}/{}", model.provider, model.model);
                let resolved = crate::provider::model_name::ModelName::parse(&name, &catalog)
                    .map_err(|_| "invalid catalog identity")
                    .and_then(|name| {
                        catalog.resolve_name(&name).map_err(|error| match error {
                            crate::error::ProviderError::AmbiguousModel { .. } => {
                                "ambiguous catalog identity"
                            }
                            _ => "unavailable catalog identity",
                        })
                    });
                if let Err(reason) = resolved {
                    return error_response(ServerError::MalformedBody(reason.into()));
                }
            }
            let mut models = catalog
                .models
                .into_iter()
                .map(|(key, m)| {
                    (
                        key,
                        ModelEntry {
                            name: format!("{}/{}", m.provider, m.model),
                            provider_group: m
                                .model
                                .split_once('/')
                                .map_or_else(|| m.provider.clone(), |(group, _)| group.to_string()),
                            gateway: m.provider,
                            upstream_id: m.model,
                            api: m.api,
                            supports_tools: m.supports_tools,
                            supports_parallel_tool_calls: m.supports_parallel_tool_calls,
                            context_window: m.context_window,
                            enabled: None,
                            disabled_reason: None,
                            window_source: None,
                            stale: None,
                            stale_age_secs: None,
                            refresh_error: None,
                            reported_output: None,
                            max_input_tokens: m.max_input_tokens,
                            discrepancy: None,
                            variants: m.variants.unwrap_or_default().into_keys().collect(),
                            default_variant: m.default_variant,
                            default_variant_unavailable: None,
                        },
                    )
                })
                .collect::<Vec<_>>();
            for (key, entry) in &mut models {
                if entry.api != crate::provider::config::ApiKind::Chat {
                    entry.enabled = Some(true);
                    continue;
                }
                match state.resolve_model_metadata(workspace, key).await {
                    Ok(resolution) => {
                        match resolution.default_override(key) {
                            Ok(chosen) => entry.default_variant = chosen.map(|(name, _)| name),
                            Err(unknown) => {
                                entry.default_variant = None;
                                entry.default_variant_unavailable = Some(unknown.variant)
                            }
                        }
                        entry.context_window = Some(resolution.window);
                        entry.enabled = Some(true);
                        entry.window_source = Some(resolution.window_source);
                        entry.stale = Some(resolution.stale);
                        entry.stale_age_secs = resolution.stale_age_secs;
                        entry.refresh_error = resolution.refresh_error;
                        entry.reported_output = resolution.reported_output;
                        entry.max_input_tokens = resolution.max_input_tokens;
                        entry.discrepancy = resolution.discrepancy.map(|d| DiscrepancyBody {
                            configured: d.configured,
                            discovered: d.discovered,
                        });
                        entry.variants = resolution.variants.into_keys().collect();
                    }
                    Err(error) => {
                        entry.enabled = Some(false);
                        entry.disabled_reason = Some(model_catalog_error(&error).into());
                    }
                }
            }
            let mut models: Vec<_> = models.into_iter().map(|(_, entry)| entry).collect();
            models.sort_by(|a, b| a.name.cmp(&b.name));
            (
                StatusCode::OK,
                Json(ModelsBody {
                    models,
                    default_agent,
                }),
            )
                .into_response()
        }
        Err(_) => error_response(ServerError::MalformedBody(
            "model catalog configuration unavailable".into(),
        )),
    }
}

/// Catalog diagnostics never echo supplied configuration, authentication or response bodies.
fn model_catalog_error(error: &ServerError) -> &'static str {
    match error {
        ServerError::Config(_) => "model catalog configuration unavailable",
        ServerError::Model(
            crate::error::ProviderError::Auth(_) | crate::error::ProviderError::MissingApiKey,
        ) => "model authentication unavailable",
        ServerError::NoContextWindow { .. } => "model context window unavailable",
        _ => "model metadata unavailable",
    }
}

/// `GET /v1/sessions/:id/context`: the session's context snapshot.
async fn get_context(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let parsed = match parse_session_id(id) {
        Ok(parsed) => parsed,
        Err(err) => return error_response(err),
    };
    match state.context(parsed).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(err) => error_response(err),
    }
}

async fn get_skills(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let parsed = match parse_session_id(id) {
        Ok(id) => id,
        Err(err) => return error_response(err),
    };
    match state.skills(parsed).await {
        Ok(skills) => Json(skills).into_response(),
        Err(error) => error_response(error),
    }
}

/// `GET /v1/commands`: the registered session commands.
async fn list_commands(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "commands": state.list_commands() }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostBypassRequest {
    enabled: bool,
}

fn bypass_auth_required() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "reason": "auth_required",
            "error": "bypass can only be changed by an authenticated client",
        })),
    )
        .into_response()
}

async fn post_bypass(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<PostBypassRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.http.bearer_token.is_none() {
        return bypass_auth_required();
    }
    let parsed = match parse_session_id(id) {
        Ok(parsed) => parsed,
        Err(err) => return error_response(err),
    };
    let Json(request) = match body {
        Ok(parsed) => parsed,
        Err(rejection) => {
            return error_response(ServerError::MalformedBody(rejection.body_text()));
        }
    };
    match state
        .set_bypass(
            parsed,
            request.enabled,
            crate::bus::runtime::bypass::BypassOrigin::Http,
        )
        .await
    {
        Ok(reply) => Json(reply).into_response(),
        Err(err) => error_response(err),
    }
}

/// `POST /v1/sessions/:id/commands` request.
#[derive(Debug, Deserialize)]
pub struct PostCommandRequest {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

/// Runs one session command; the outcome is also published as a durable
/// `command.result` on the session stream.
async fn post_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<PostCommandRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let parsed = match parse_session_id(id) {
        Ok(parsed) => parsed,
        Err(err) => return error_response(err),
    };
    let Json(request) = match body {
        Ok(parsed) => parsed,
        Err(rejection) => {
            return error_response(ServerError::MalformedBody(rejection.body_text()));
        }
    };
    if state.http.bearer_token.is_none()
        && request
            .name
            .trim()
            .strip_prefix('/')
            .unwrap_or(request.name.trim())
            == "bypass"
        && matches!(request.arguments.trim(), "on" | "off")
    {
        return bypass_auth_required();
    }
    match state
        .run_command(parsed, &request.name, &request.arguments)
        .await
    {
        Ok(outcome) => Json(outcome).into_response(),
        Err(err) => error_response(err),
    }
}

/// The server-assigned sender for HTTP-submitted messages: clients can never
/// impersonate another actor by choosing it themselves.
const HTTP_USER_ACTOR_ID: &str = "user:http";

fn parse_session_id(id: String) -> Result<Ulid, ServerError> {
    id.parse().map_err(|_| ServerError::SessionNotFound(id))
}

async fn routable_session_id(state: &AppState, id: String) -> Result<Ulid, ServerError> {
    let id = parse_session_id(id)?;
    state.routable_handle(id).await?;
    Ok(id)
}

/// Builds the Command for a posted message with a server-assigned sender.
/// Public so the WS route and integration tests share the exact POST core.
pub fn build_command(
    session_id: Ulid,
    request: PostMessageRequest,
    mode: DeliveryMode,
) -> Command {
    Command {
        command_id: Ulid::new(),
        session_id,
        source_actor_id: HTTP_USER_ACTOR_ID.to_string(),
        target_actor_id: request.target_actor_id,
        mode,
        content: {
            let mut content = request.content;
            if let Some(answer) = request.answer {
                content.push(ContentBlock::AskAnswer { answer });
            }
            content
        },
        created_at: SystemTime::now(),
    }
}

pub(crate) fn parse_mode(mode: Option<&str>) -> Result<DeliveryMode, ServerError> {
    match mode.unwrap_or("next-turn") {
        "next-turn" => Ok(DeliveryMode::NextTurn),
        "steer" => Ok(DeliveryMode::Steer),
        "interrupt" => Ok(DeliveryMode::Interrupt),
        other => Err(ServerError::MalformedBody(format!("unknown mode: {other}"))),
    }
}

/// `POST /v1/sessions/:id/messages`: validate the session and the
/// content, sequence `message.received` BEFORE the inbox commit
/// (event-before-action), and ack with the frozen body — never blocking on
/// agent progress. A full inbox reserves the permit back and answers
/// `503 {accepted:false, reason:"inbox_full"}`.
async fn post_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<PostMessageRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(parsed) => parsed,
        Err(rejection) => {
            return error_response(ServerError::MalformedBody(rejection.body_text()));
        }
    };
    let session_id = match routable_session_id(&state, id).await {
        Ok(parsed) => parsed,
        Err(err) => return error_response(err),
    };
    // Unknown or stopping sessions answer 404/409 before the body is judged.
    let mode = match parse_mode(request.mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => return error_response(err),
    };
    let command = build_command(session_id, request, mode);
    match state.send(command).await {
        Ok(accepted) => Json(accepted).into_response(),
        Err(err) => error_response(err),
    }
}

/// `POST /v1/sessions/:id/permissions/:request_id` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerPermissionRequest {
    pub decision: DecisionKind,
    /// What the model reads back on a denial.
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    AllowOnce,
    AllowFolder,
    AllowConfigSession,
    AllowSkillMachine,
    Deny,
}

/// The message a denial carries when the Captain gave none.
const DEFAULT_DENIAL: &str = "the user denied this tool call";

async fn list_skill_grants(State(state): State<AppState>) -> Response {
    let store = crate::agent::skill_grants::SkillGrants {
        file: state.config.config_dir.join("skill-grants.json"),
    };
    match store.read() {
        Ok(grants) => Json(json!({"grants": grants})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

async fn revoke_skill_grant(State(state): State<AppState>, Path(skill): Path<String>) -> Response {
    let store = crate::agent::skill_grants::SkillGrants {
        file: state.config.config_dir.join("skill-grants.json"),
    };
    match store.revoke(&skill) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

async fn answer_permission(
    State(state): State<AppState>,
    Path((id, request_id)): Path<(String, String)>,
    body: Result<Json<AnswerPermissionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let session_id = match routable_session_id(&state, id).await {
        Ok(id) => id,
        Err(err) => return error_response(err),
    };
    let Ok(request_id) = request_id.parse::<Ulid>() else {
        return error_response(ServerError::PermissionStale);
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return error_response(ServerError::MalformedBody(rejection.body_text())),
    };
    use crate::agent::permissions::PermissionDecision;
    let decision = match body.decision {
        DecisionKind::AllowOnce => PermissionDecision::AllowOnce,
        DecisionKind::AllowFolder => PermissionDecision::AllowFolder,
        DecisionKind::AllowConfigSession => PermissionDecision::AllowConfigSession,
        DecisionKind::AllowSkillMachine => PermissionDecision::AllowSkillMachine,
        DecisionKind::Deny => PermissionDecision::Deny {
            message: body
                .message
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_DENIAL.to_string()),
        },
    };
    match state
        .answer_permission(session_id, request_id, decision)
        .await
    {
        Ok(()) => Json(json!({ "accepted": true })).into_response(),
        Err(err) => error_response(err),
    }
}

async fn stop_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let parsed = match parse_session_id(id) {
        Ok(parsed) => parsed,
        Err(err) => return error_response(err),
    };
    match state.close_session(parsed).await {
        Ok(()) => Json(StopAccepted {
            accepted: true,
            actor_state: "stopped",
        })
        .into_response(),
        Err(err) => error_response(err),
    }
}

async fn continue_child(
    State(state): State<AppState>,
    Path((parent, child)): Path<(String, String)>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let (Ok(parent_id), Ok(child_id)) = (parent.parse(), child.parse()) else {
        return error_response(ServerError::SessionNotFound(child));
    };
    let token = match body.and_then(|Json(value)| value.get("pause_token").cloned()) {
        Some(value) if !value.is_null() => match serde_json::from_value(value) {
            Ok(token) => Some(token),
            Err(_) => {
                return error_response(ServerError::MalformedBody("invalid pause token".into()))
            }
        },
        _ => None,
    };
    match state
        .continue_child_with_token(parent_id, child_id, token)
        .await
    {
        Ok(()) => Json(json!({"accepted":true})).into_response(),
        Err(error) => error_response(error),
    }
}

/// The single structured-error funnel: every failure is a typed
/// [`ServerError`] mapped onto a status code + JSON body. The full-inbox
/// rejection carries the FROZEN `{accepted:false, reason:"inbox_full"}`
/// body; every other failure shares `{"error": ...}`.
pub(crate) fn error_response(err: ServerError) -> Response {
    let status = match &err {
        ServerError::SessionArchived { .. } => StatusCode::FORBIDDEN,
        ServerError::ChildReadOnly => StatusCode::FORBIDDEN,
        ServerError::SubagentCapacity { .. } => StatusCode::TOO_MANY_REQUESTS,
        ServerError::SubagentSettings(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::SessionNotFound(_) => StatusCode::NOT_FOUND,
        ServerError::SessionStopping
        | ServerError::ChildNotParked
        | ServerError::ResumeRequired
        | ServerError::PermissionStale => StatusCode::CONFLICT,
        ServerError::NonLoopbackBind(_) | ServerError::Bind(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::InboxFull => StatusCode::SERVICE_UNAVAILABLE,
        ServerError::NonTextContent | ServerError::MalformedBody(_) => StatusCode::BAD_REQUEST,
        ServerError::NoContextWindow { .. } | ServerError::ImpossibleBudget { .. } => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        ServerError::ModelIdentityChanged { .. } | ServerError::VariantUnavailable { .. } => {
            StatusCode::CONFLICT
        }
        ServerError::UnknownVariant { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        ServerError::UnknownAgent { .. } => StatusCode::BAD_REQUEST,
        ServerError::SavedAgentUnavailable { .. } => StatusCode::CONFLICT,
        ServerError::DefaultVariantUnavailable { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::VariantOfferUnknown { .. } => StatusCode::SERVICE_UNAVAILABLE,
        ServerError::SavedResolutionInvalid { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::History(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::Config(_) | ServerError::Model(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if matches!(err, ServerError::InboxFull) {
        return (
            status,
            Json(json!({ "accepted": false, "reason": "inbox_full" })),
        )
            .into_response();
    }
    if let ServerError::SessionArchived { reason, .. } = &err {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "session_archived", "resume_disabled_reason": reason })),
        ).into_response();
    }
    if matches!(err, ServerError::ResumeRequired) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "resume_required" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::ChildReadOnly) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "child_read_only" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::SubagentCapacity { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "subagent_capacity" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::PermissionStale) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "permission_stale" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::NoContextWindow { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "no_context_window" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::SavedAgentUnavailable { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "saved_agent_unavailable" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::ImpossibleBudget { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "impossible_context_budget" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::ModelIdentityChanged { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "model_identity_changed" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::VariantUnavailable { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "variant_unavailable" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::VariantOfferUnknown { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "variant_offer_unknown" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::UnknownVariant { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "unknown_variant" })),
        )
            .into_response();
    }
    (status, Json(json!({ "error": err.to_string() }))).into_response()
}

/// SIGTERM delivery for the graceful-shutdown acceptance test: the test
/// process IS the server process, so raising SIGTERM on itself exercises the
/// production `SignalKind::terminate` branch of `shutdown_signal`.
#[doc(hidden)]
pub fn send_test_sigterm() {
    let pid = std::process::id();
    let _ = std::process::Command::new("kill")
        .args(["-s", "TERM", &pid.to_string()])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::session::ServerConfig;
    use axum::body::Body;
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback bind"),
            models_path: None,
            config_dir: dir.path().join("cfg"),
            allow_non_loopback: false,
            bearer_token: None,
            history_db: dir.path().join("history.db"),
            data_dir: dir.path().join("data"),
            rules_roots: Default::default(),
            web: None,
        })
        .expect("state over a temp database");
        (dir, state)
    }

    // Given a session POST with a malformed body
    // When the handler runs
    // Then 400 with the shared {"error": ...} shape (not a panic/plain text)
    #[tokio::test]
    async fn malformed_create_body_is_a_structured_400() {
        let (_dir, state) = test_state();
        let response = app(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{not json"))
                    .expect("request"),
            )
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert!(
            value.get("error").is_some(),
            "structured 404/400 shape: {value}"
        );
    }
}
