//! The routes only the browser client needs: its static files (embedded in the binary), the
//! workspace list and a workspace's intent record. The files are served only when the server was
//! started by `nav web` ([`WebConfig`](super::WebConfig)); `/v1/workspaces` and `/v1/intent` are
//! ordinary bearer-protected API routes of every server.

use std::collections::HashMap;
use std::path::PathBuf;

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use include_dir::{include_dir, Dir};
use serde::Serialize;
use serde_json::json;

use crate::error::ServerError;
use crate::server::routes::error_response;
use crate::server::session::AppState;

/// The built single-page app, `webui/dist` at build time.
static DIST: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/webui/dist");

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
                   font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'";

/// True for a request the static-file handler answers without a token: a page or asset path,
/// never an API path.
pub(super) fn is_public_asset_path(path: &str) -> bool {
    path != "/v1" && !path.starts_with("/v1/")
}

/// The router fallback of a `nav web` server: the file at the request path, else `index.html`
/// so the app's own router can show the page. Unknown `/v1/` paths stay 404.
pub(super) async fn serve_asset(method: Method, uri: Uri) -> Response {
    let path = uri.path();
    if !is_public_asset_path(path) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let wanted = path.trim_start_matches('/');
    let (name, file) = match DIST.get_file(wanted).filter(|_| !wanted.is_empty()) {
        Some(file) => (wanted, file),
        None => match DIST.get_file("index.html") {
            Some(file) => ("index.html", file),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let mut response = file.contents().into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(name)),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if name.ends_with(".html") {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        );
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    } else if name.starts_with("assets/") {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    response
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

#[derive(Serialize)]
struct WorkspaceEntry {
    path: String,
    name: String,
    launched: bool,
    session_count: u64,
    updated_at: Option<String>,
    branch: Option<String>,
    dirty: Option<usize>,
}

/// `GET /v1/workspaces`: the workspaces `nav web` was launched with, then the others the
/// history knows that still exist on disk, most recently active first.
pub(super) async fn list_workspaces(State(state): State<AppState>) -> Response {
    let stored = match state.history().workspaces().await {
        Ok(rows) => rows,
        Err(error) => return error_response(ServerError::History(error)),
    };
    let hidden = match state.history().hidden_workspaces().await {
        Ok(paths) => paths,
        Err(error) => return error_response(ServerError::History(error)),
    };
    let launch: Vec<PathBuf> = state
        .http
        .web
        .as_ref()
        .map(|web| web.launch_workspaces.clone())
        .unwrap_or_default();

    // The history records a workspace as the client wrote it; fold the spellings of one
    // directory together on its canonical path.
    let mut seen: HashMap<PathBuf, (u64, Option<String>)> = HashMap::new();
    for row in stored {
        let Ok(canonical) = std::fs::canonicalize(&row.workspace) else {
            continue;
        };
        let entry = seen.entry(canonical).or_default();
        entry.0 += row.session_count;
        if entry.1.as_deref() < Some(row.updated_at.as_str()) {
            entry.1 = Some(row.updated_at);
        }
    }

    let mut rows: Vec<(PathBuf, bool, u64, Option<String>)> = Vec::new();
    for path in &launch {
        let (count, updated) = seen.remove(path).unwrap_or_default();
        if !rows.iter().any(|row| &row.0 == path) {
            rows.push((path.clone(), true, count, updated));
        }
    }
    let mut others: Vec<_> = seen
        .into_iter()
        .filter(|(path, _)| !hidden.contains(path))
        .map(|(path, (count, updated))| (path, false, count, updated))
        .collect();
    others.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    rows.extend(others);

    let git =
        futures::future::join_all(rows.iter().map(|row| crate::workspace::git::read(&row.0))).await;
    let workspaces: Vec<WorkspaceEntry> = rows
        .into_iter()
        .zip(git)
        .map(
            |((path, launched, session_count, updated_at), git)| WorkspaceEntry {
                name: path.file_name().map_or_else(
                    || path.to_string_lossy().into_owned(),
                    |n| n.to_string_lossy().into_owned(),
                ),
                path: crate::tools::canonical_path(&path)
                    .unwrap_or_else(|_| path.clone())
                    .to_string_lossy()
                    .into_owned(),
                launched,
                session_count,
                updated_at,
                branch: git.as_ref().and_then(|g| g.branch.clone()),
                dirty: git.map(|g| g.dirty),
            },
        )
        .collect();
    Json(json!({ "workspaces": workspaces })).into_response()
}

fn workspace_path(params: &HashMap<String, String>) -> Result<PathBuf, ServerError> {
    let path = params
        .get("path")
        .map(PathBuf::from)
        .ok_or_else(|| ServerError::MalformedBody("missing query parameter: path".into()))?;
    if !path.is_absolute() {
        return Err(ServerError::MalformedBody(
            "workspace must be an absolute path".into(),
        ));
    }
    let path = std::fs::canonicalize(path).map_err(|_| {
        ServerError::MalformedBody("workspace must be an existing directory".into())
    })?;
    if !path.is_dir() {
        return Err(ServerError::MalformedBody(
            "workspace must be an existing directory".into(),
        ));
    }
    Ok(path)
}

/// Explicitly reopening a folder makes its saved conversations visible again.
pub(super) async fn unhide_workspace(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match workspace_path(&params) {
        Ok(path) => path,
        Err(error) => return error_response(error),
    };
    match state.history().set_workspace_hidden(&path, false).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(ServerError::History(error)),
    }
}

/// Hide a folder, optionally stopping and deleting its root session trees first.
pub(super) async fn hide_workspace(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match workspace_path(&params) {
        Ok(path) => path,
        Err(error) => return error_response(error),
    };
    if state.http.web.as_ref().is_some_and(|web| {
        web.launch_workspaces
            .iter()
            .any(|launch| std::fs::canonicalize(launch).ok().as_ref() == Some(&path))
    }) {
        return (StatusCode::CONFLICT, Json(json!({
            "reason": "launched",
            "error": "this workspace was opened by nav web on the command line and cannot be hidden"
        }))).into_response();
    }
    let delete = match params.get("sessions").map(String::as_str) {
        None => false,
        Some("delete") => true,
        Some(_) => {
            return error_response(ServerError::MalformedBody(
                "sessions must be delete or omitted".into(),
            ))
        }
    };
    let mut deleted = 0;
    if delete {
        let ids = match state.history().workspace_session_ids(&path).await {
            Ok(ids) => ids,
            Err(error) => return error_response(ServerError::History(error)),
        };
        for id in ids {
            match state.delete(id).await {
                Ok(()) => deleted += 1,
                Err(ServerError::SessionNotFound(_)) => {}
                Err(error) => return error_response(error),
            }
        }
    }
    if let Err(error) = state.history().set_workspace_hidden(&path, true).await {
        return error_response(ServerError::History(error));
    }
    if delete {
        Json(json!({ "deleted": deleted })).into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

/// `GET /v1/intent?workspace=<abs path>`: the workspace's `.withnav/intent.md`.
pub(super) async fn get_intent(Query(params): Query<HashMap<String, String>>) -> Response {
    let Some(workspace) = params.get("workspace") else {
        return error_response(ServerError::MalformedBody(
            "missing query parameter: workspace".to_string(),
        ));
    };
    let workspace = PathBuf::from(workspace);
    if !workspace.is_absolute() {
        return error_response(ServerError::MalformedBody(
            "workspace must be an absolute path".to_string(),
        ));
    }
    match read_intent(workspace).await {
        Ok(Some(record)) => Json(json!({
            "exists": true,
            "intent": record.intent,
            "plan": record.plan,
            "decisions": record.decisions,
            "active_spec": record.active_spec,
            "raw": record.raw,
        }))
        .into_response(),
        Ok(None) => Json(json!({
            "exists": false,
            "intent": null,
            "plan": null,
            "decisions": null,
            "active_spec": null,
            "raw": null,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("cannot read the intent record: {error}") })),
        )
            .into_response(),
    }
}

async fn read_intent(
    workspace: PathBuf,
) -> std::io::Result<Option<crate::navigator::intent::IntentRecord>> {
    tokio::task::spawn_blocking(move || crate::navigator::intent::read_intent(&workspace))
        .await
        .map_err(std::io::Error::other)?
}
