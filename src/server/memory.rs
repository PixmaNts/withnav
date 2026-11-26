//! Captain-facing memory register routes.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use crate::memory::{
    Actor, ChangedBy, Error, ItemFilter, ItemStatus, NewItem, Project, Provenance, Revision, Scope,
};
use crate::session::memory::{item, version};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/memory", get(list).post(remember))
        .route("/v1/memory/{id}", get(detail))
        .route("/v1/memory/{id}/{action}", post(amend))
        .route("/v1/projects", get(projects))
        .route("/v1/groups", post(create_group))
        .route("/v1/groups/{id}", patch(rename_group))
        .route(
            "/v1/projects/{id}/groups/{group}",
            put(add_group).delete(remove_group),
        )
        .route("/v1/projects/{id}/group-answer", post(group_answer))
        .route("/v1/sessions/{id}/project", put(session_project))
        .route("/v1/memory/needs-you", get(needs_you))
        .route("/v1/memory/needs-you/{change}/{decision}", post(decide))
}

fn captain() -> ChangedBy {
    ChangedBy::new(Actor::Captain, None)
}

fn change(row: crate::memory::MembershipChange) -> withnav_client_api::memory::MembershipChange {
    use crate::memory::Relation;
    use withnav_client_api::memory as wire;
    let relation = match row.relation {
        Relation::ProjectGroup {
            project_id,
            group_id,
        } => wire::Relation::ProjectGroup {
            project_id,
            group_id,
        },
        Relation::SessionProject {
            session_id,
            project_id,
        } => wire::Relation::SessionProject {
            linked_session_id: session_id,
            project_id,
        },
    };
    wire::MembershipChange {
        id: row.id,
        relation,
        change: row.change.to_string(),
        status: row.status.to_string(),
        actor: row.actor.to_string(),
        session_id: row.session_id,
        at: row.at,
        decided_at: row.decided_at,
    }
}

#[derive(Deserialize)]
struct Name {
    name: String,
}

async fn create_group(State(state): State<AppState>, Json(body): Json<Name>) -> Response {
    run(state, StatusCode::CREATED, move |store| {
        Ok(json!(group(store, store.create_group(&body.name)?)?))
    })
    .await
}

async fn rename_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Name>,
) -> Response {
    run(state, StatusCode::OK, move |store| {
        Ok(json!(group(store, store.rename_group(&id, &body.name)?)?))
    })
    .await
}

fn group(
    store: &crate::memory::MemoryStore,
    row: crate::memory::Group,
) -> Result<withnav_client_api::memory::Group, Error> {
    let items = store
        .list(&ItemFilter {
            scope: Some(crate::memory::ScopeKind::Group),
            scope_ref: Some(row.id.clone()),
            status: Some(ItemStatus::Active),
            ..ItemFilter::default()
        })?
        .len();
    Ok(withnav_client_api::memory::Group {
        id: row.id,
        name: row.name,
        members: row.members,
        items,
    })
}

#[derive(Deserialize)]
struct Confirm {
    #[serde(default)]
    confirm: bool,
}

async fn add_group(
    State(state): State<AppState>,
    Path((id, group)): Path<(String, String)>,
    body: Option<Json<Confirm>>,
) -> Response {
    run(state, StatusCode::OK, move |store| {
        Ok(json!(change(store.add_project_to_group(
            &id,
            &group,
            body.is_some_and(|body| body.confirm),
            &captain()
        )?)))
    })
    .await
}

async fn remove_group(
    State(state): State<AppState>,
    Path((id, group)): Path<(String, String)>,
) -> Response {
    run(state, StatusCode::OK, move |store| {
        Ok(json!(change(store.remove_project_from_group(
            &id,
            &group,
            &captain()
        )?)))
    })
    .await
}

#[derive(Deserialize)]
struct ProjectSelection {
    project_id: Value,
}

async fn session_project(
    State(state): State<AppState>,
    Path(id): Path<ulid::Ulid>,
    Json(body): Json<ProjectSelection>,
) -> Response {
    if state.session_summary(id).await.is_err() {
        return failure(Error::NotFound {
            what: "session",
            id: id.to_string(),
        });
    }
    run(state, StatusCode::OK, move |store| {
        let id = id.to_string();
        let row = match body.project_id {
            Value::Null => store.detach_session(&id, &captain()),
            Value::String(project) => store.attach_session(&id, &project, &captain()),
            _ => Err(Error::Invalid("project_id must be a string or null".into())),
        }?;
        Ok(json!(change(row)))
    })
    .await
}

#[derive(Deserialize)]
struct GroupAnswer {
    group_id: Option<String>,
    #[serde(default)]
    later: bool,
}

async fn group_answer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GroupAnswer>,
) -> Response {
    run(state, StatusCode::OK, move |store| {
        let row = if body.later {
            store.project(&id)?.ok_or_else(|| Error::NotFound {
                what: "project",
                id: id.clone(),
            })?
        } else {
            if let Some(group_id) = body.group_id {
                let current = store.project(&id)?.ok_or_else(|| Error::NotFound {
                    what: "project",
                    id: id.clone(),
                })?;
                if !current.groups.contains(&group_id) {
                    store.add_project_to_group(&id, &group_id, true, &captain())?;
                }
            }
            store.answer_group_question(&id)?
        };
        Ok(json!(project(row)))
    })
    .await
}

async fn needs_you(State(state): State<AppState>) -> Response {
    run(state, StatusCode::OK, |store| Ok(json!({"changes":store.pending_membership_changes(5)?.into_iter().map(change).collect::<Vec<_>>()}))).await
}

async fn decide(
    State(state): State<AppState>,
    Path((id, decision)): Path<(i64, String)>,
) -> Response {
    run(state, StatusCode::OK, move |store| {
        let row = match decision.as_str() {
            "accept" => store.accept_membership_change(id),
            "reject" => store.reject_membership_change(id),
            _ => Err(Error::NotFound {
                what: "membership decision",
                id: decision,
            }),
        }?;
        Ok(json!(change(row)))
    })
    .await
}

fn project(row: Project) -> withnav_client_api::memory::Project {
    let aliases: Vec<_> = row.aliases.into_iter().map(|alias| alias.alias).collect();
    withnav_client_api::memory::Project {
        id: row.id,
        name: row.name,
        group_answered: row.group_answered,
        groups: row.groups,
        worktrees: aliases.clone(),
        aliases,
        sessions: Vec::new(),
    }
}

fn failure(error: Error) -> Response {
    let (status, body) = match error {
        Error::VersionConflict { current } => (
            StatusCode::CONFLICT,
            json!({"reason":"version_conflict","current_version":current}),
        ),
        Error::WideningRequiresConfirmation { affected_projects } => (
            StatusCode::CONFLICT,
            json!({"reason":"widening_requires_confirmation","affected_projects":affected_projects.into_iter().map(project).collect::<Vec<_>>()}),
        ),
        Error::Invalid(_) => (
            StatusCode::BAD_REQUEST,
            json!({"reason":"invalid_memory_request","message":error.to_string()}),
        ),
        Error::NotFound { .. } => (StatusCode::NOT_FOUND, json!({"reason":"memory_not_found"})),
        Error::Database(_) | Error::Io(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"reason":"memory_unavailable"}),
        ),
    };
    (status, Json(body)).into_response()
}

async fn run(
    state: AppState,
    status: StatusCode,
    work: impl FnOnce(&crate::memory::MemoryStore) -> Result<Value, Error> + Send + 'static,
) -> Response {
    match state.memory_store().await {
        Ok(store) => match store.run(work).await {
            Ok(value) => (status, Json(value)).into_response(),
            Err(error) => failure(error),
        },
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"reason":"memory_unavailable"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct List {
    session: Option<String>,
    scope: Option<String>,
    scope_ref: Option<String>,
    status: Option<String>,
    kind: Option<String>,
    q: Option<String>,
    limit: Option<u32>,
}

async fn list(State(state): State<AppState>, Query(query): Query<List>) -> Response {
    run(state, StatusCode::OK, move |store| {
        let default_status =
            (query.session.is_some() || query.q.is_some()).then_some(ItemStatus::Active);
        let items = store.list(&ItemFilter {
            eligible_for: query.session,
            scope: query.scope.as_deref().map(str::parse).transpose()?,
            scope_ref: query.scope_ref,
            status: query
                .status
                .as_deref()
                .map(str::parse)
                .transpose()?
                .or(default_status),
            kind: query.kind.as_deref().map(str::parse).transpose()?,
            text: query.q,
            limit: query.limit,
        })?;
        Ok(json!({"items":items.into_iter().map(item).collect::<Vec<_>>() }))
    })
    .await
}

#[derive(Deserialize)]
struct Remember {
    text: String,
    title: Option<String>,
    why: Option<String>,
    kind: Option<String>,
    scope: String,
    scope_ref: Option<String>,
    source_session_id: Option<String>,
}

async fn remember(State(state): State<AppState>, Json(mut body): Json<Remember>) -> Response {
    if let Err(error) = redact(
        &state,
        body.source_session_id.as_deref(),
        [&mut body.text]
            .into_iter()
            .chain(body.title.iter_mut())
            .chain(body.why.iter_mut()),
    )
    .await
    {
        return failure(error);
    }
    run(state, StatusCode::CREATED, move |store| {
        let scope = Scope::from_parts(
            body.scope.parse()?,
            body.scope_ref.as_deref().unwrap_or_default(),
        )?;
        store
            .remember(NewItem {
                body: body.text,
                title: body.title,
                why: body.why,
                scope,
                quote: None,
                kind: body.kind.as_deref().unwrap_or("decision").parse()?,
                by: Provenance::captain(body.source_session_id, None),
            })
            .map(|row| json!(item(row)))
    })
    .await
}

async fn redact(
    state: &AppState,
    source: Option<&str>,
    text: impl Iterator<Item = &mut String>,
) -> Result<(), Error> {
    if let Some(source) = source {
        let id = source
            .parse::<ulid::Ulid>()
            .map_err(|_| Error::Invalid("invalid source session".into()))?;
        let session = state
            .session_summary(id)
            .await
            .map_err(|_| Error::NotFound {
                what: "session",
                id: source.into(),
            })?;
        let access = state.secret_access(id, std::path::Path::new(&session.workspace));
        for field in text {
            access.redact_in_place(field);
        }
    }
    Ok(())
}

async fn detail(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    run(state, StatusCode::OK, move |store| {
        let id = store.resolve_id(&id)?;
        let row = store.item(&id)?.ok_or_else(|| Error::NotFound {
            what: "memory item",
            id: id.clone(),
        })?;
        let mut value = json!(item(row));
        value["versions"] = json!(
            store
                .versions(&id)?
                .into_iter()
                .map(version)
                .collect::<Vec<_>>()
        );
        Ok(value)
    })
    .await
}

#[derive(Deserialize)]
struct Amend {
    expected_version: u32,
    text: Option<String>,
    title: Option<String>,
    why: Option<String>,
    reason: Option<String>,
    from_version: Option<u32>,
    scope: Option<String>,
    scope_ref: Option<String>,
    source_session_id: Option<String>,
    #[serde(default)]
    confirm: bool,
}

async fn amend(
    State(state): State<AppState>,
    Path((id, action)): Path<(String, String)>,
    Json(mut body): Json<Amend>,
) -> Response {
    if let Err(error) = redact(
        &state,
        body.source_session_id.as_deref(),
        body.text
            .iter_mut()
            .chain(body.title.iter_mut())
            .chain(body.why.iter_mut())
            .chain(body.reason.iter_mut()),
    )
    .await
    {
        return failure(error);
    }
    run(state, StatusCode::OK, move |store| {
        let id = store.resolve_id(&id)?;
        let by = Provenance::captain(body.source_session_id, None);
        let expected = body.expected_version;
        let reason = body.reason.as_deref();
        match action.as_str() {
            "revise" => store.revise(
                &id,
                expected,
                Revision {
                    title: body.title,
                    body: body.text,
                    why: body.why,
                },
                reason,
                &by,
            ),
            "invalidate" => store.invalidate(&id, expected, reason, &by),
            "restore" => store.restore(&id, expected, body.from_version, reason, &by),
            "rescope" => store.rescope(
                &id,
                expected,
                Scope::from_parts(
                    body.scope.as_deref().unwrap_or_default().parse()?,
                    body.scope_ref.as_deref().unwrap_or_default(),
                )?,
                body.confirm,
                reason,
                &by,
            ),
            _ => Err(Error::NotFound {
                what: "memory action",
                id: action,
            }),
        }
        .map(|row| json!(item(row)))
    })
    .await
}

async fn projects(State(state): State<AppState>) -> Response {
    run(state, StatusCode::OK, |store| {
        let mut projects = store
            .projects()?
            .into_iter()
            .map(project)
            .collect::<Vec<_>>();
        for project in &mut projects {
            project.sessions = store
                .project_sessions(&project.id)?
                .into_iter()
                .map(|row| withnav_client_api::memory::SessionLink {
                    session_id: row.session_id,
                    project_id: row.project_id,
                    active: row.active,
                })
                .collect();
        }
        let groups = store
            .groups()?
            .into_iter()
            .map(|row| group(store, row))
            .collect::<Result<Vec<_>, _>>()?;
        let history = store
            .membership_history(None, 100)?
            .into_iter()
            .map(change)
            .collect::<Vec<_>>();
        Ok(json!({"projects":projects,"groups":groups,"history":history}))
    })
    .await
}
