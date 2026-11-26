//! Captain commands for the memory register, separate from conversation messages.

use std::path::Path;

use serde_json::json;

use super::commands::CommandSpec;
use super::run_command::{CommandFailure, Done};
use super::service::{Service, SessionHandle, SessionId};
use crate::memory::{
    self, Actor, ChangedBy, Kind, MemoryItem, MemoryVersion, NewItem, Provenance, Revision, Scope,
};

fn wire_scope(scope: &Scope) -> withnav_client_api::memory::Scope {
    use withnav_client_api::memory::Scope as Wire;
    match scope {
        Scope::Global => Wire::Global,
        Scope::Group(_) => Wire::Group,
        Scope::Project(_) => Wire::Project,
        Scope::Session(_) => Wire::Session,
    }
}

pub(crate) fn version(row: MemoryVersion) -> withnav_client_api::memory::MemoryVersion {
    withnav_client_api::memory::MemoryVersion {
        version: row.version,
        change: row.change.to_string(),
        title: row.title,
        body: row.body,
        why: row.why,
        quote: row.quote,
        scope: wire_scope(&row.scope),
        scope_ref: row.scope.reference().into(),
        source_actor: row.source_actor.to_string(),
        drafted_by: row.drafted_by.to_string(),
        confirmed_by: row.confirmed_by.map(|actor| actor.to_string()),
        source_session_id: row.source_session_id,
        source_event_seq: row.source_event_seq,
        reason: row.reason,
        at: row.at,
    }
}

pub(crate) fn item(row: MemoryItem) -> withnav_client_api::memory::MemoryItem {
    withnav_client_api::memory::MemoryItem {
        id: row.id,
        kind: row.kind.to_string(),
        scope: wire_scope(&row.scope),
        scope_ref: row.scope.reference().into(),
        status: row.status.to_string(),
        current_version: row.current_version,
        pinned: row.pinned,
        created_at: row.created_at,
        updated_at: row.updated_at,
        current: version(row.current),
    }
}

fn split(text: &str) -> (&str, &str) {
    let (first, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    (first, rest.trim_start())
}

impl Service {
    pub(crate) async fn memory_store(
        &self,
    ) -> Result<&std::sync::Arc<memory::MemoryStore>, memory::Error> {
        self.memory
            .get_or_try_init(|| async {
                let data_dir = self.config.data_dir.clone();
                tokio::task::spawn_blocking(move || {
                    memory::MemoryStore::open(data_dir).map(std::sync::Arc::new)
                })
                .await
                .map_err(|error| memory::Error::Io(std::io::Error::other(error)))?
            })
            .await
    }

    pub(super) async fn initialize_memory_project(&self, session: SessionId, workspace: &Path) {
        let result = async {
            let store = self.memory_store().await?;
            let identity = memory::identity(workspace).await?;
            store
                .run(move |store| {
                    let resolved = store.resolve_project(&identity)?;
                    let by = ChangedBy::new(Actor::Captain, Some(session.to_string()));
                    store.attach_session(&session.to_string(), &resolved.project.id, &by)?;
                    Ok(())
                })
                .await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%session, %error, "session started without a memory project");
        }
    }

    pub(super) async fn memory_command(
        &self,
        session: SessionId,
        handle: &SessionHandle,
        spec: &'static CommandSpec,
        args: &str,
    ) -> Result<Done, CommandFailure> {
        let store = self.memory_store().await.map_err(unavailable)?;
        let owner = self.secret_owner(session).map_err(CommandFailure::Server)?;
        let access = self.secret_access(owner, &handle.workspace);
        let mut args = args.to_owned();
        access.redact_in_place(&mut args);
        store.run(move |store| {
            let session = session.to_string();
            let by = Provenance::captain(Some(session.clone()), None);
            let (first, rest) = split(&args);
            let usage = || memory::Error::Invalid(format!("usage: /{} {}", spec.name, spec.args_hint));
            let missing = |id: &str| memory::Error::NotFound { what: "memory item", id: id.into() };
            let value = match spec.name {
                "remember" => {
                    let project = || store.session_project(&session);
                    let mut notice = None;
                    let (scope, text) = match first.strip_prefix('@') {
                        Some("global") => (Scope::Global, rest),
                        Some("session") => (Scope::Session(session.clone()), rest),
                        Some("project") => (Scope::Project(project()?.ok_or_else(|| memory::Error::Invalid("this session has no project".into()))?.id), rest),
                        Some(name) if name.strip_prefix("group:").is_some() => {
                            let group = name.strip_prefix("group:").unwrap_or_default();
                            let group = store.group_by_name(group)?.ok_or_else(|| memory::Error::NotFound { what: "group", id: group.into() })?;
                            (Scope::Group(group.id), rest)
                        }
                        _ => {
                            let scope = match project()? {
                                Some(project) => Scope::Project(project.id),
                                None => {
                                    notice = Some("stored for this session only: project unknown");
                                    Scope::Session(session.clone())
                                }
                            };
                            (scope, args.as_str())
                        }
                    };
                    if text.is_empty() { return Err(usage()); }
                    let row = store.remember(NewItem { kind: Kind::Decision, scope, title: None, body: text.into(), why: None, quote: None, by })?;
                    json!({"scope_label": scope_label(store, &row.scope)?, "item": item(row), "notice": notice})
                }
                "forget" if !first.is_empty() => {
                    let id = store.resolve_id(first)?;
                    let row = store.invalidate_current(&id, (!rest.is_empty()).then_some(rest), &by)?;
                    json!({"scope_label": scope_label(store, &row.scope)?, "item": item(row)})
                }
                "memory" => match first {
                    "" => json!({"items": store.eligible(&session)?.into_iter().map(item).collect::<Vec<_>>() }),
                    "search" if !rest.is_empty() => json!({"items": store.search(rest, Some(&session))?.into_iter().map(item).collect::<Vec<_>>() }),
                    "show" | "history" | "revise" | "invalidate" | "restore" if !rest.is_empty() => {
                        let (id, tail) = split(rest);
                        let id = store.resolve_id(id)?;
                        match first {
                            "show" if tail.is_empty() => json!({"item": item(store.item(&id)?.ok_or_else(|| missing(&id))?)}),
                            "history" if tail.is_empty() => json!({"versions": store.versions(&id)?.into_iter().map(version).collect::<Vec<_>>() }),
                            "show" | "history" => return Err(usage()),
                            change => {
                                let (expected, text) = split(tail);
                                let expected = expected.strip_prefix('v').and_then(|number| number.parse::<u32>().ok()).ok_or_else(usage)?;
                                let row = match change {
                                    "revise" if !text.is_empty() => store.revise(&id, expected, Revision { body: Some(text.into()), ..Revision::default() }, None, &by)?,
                                    "invalidate" => store.invalidate(&id, expected, (!text.is_empty()).then_some(text), &by)?,
                                    "restore" if text.is_empty() => store.restore(&id, expected, None, None, &by)?,
                                    _ => return Err(usage()),
                                };
                                json!({"scope_label": scope_label(store, &row.scope)?, "item": item(row)})
                            }
                        }
                    }
                    _ => return Err(usage()),
                },
                _ => return Err(usage()),
            };
            Ok(value)
        }).await.map(Done::data).map_err(|error| match error {
            memory::Error::Database(_) | memory::Error::Io(_) => unavailable(error),
            _ => CommandFailure::Refused(error.to_string()),
        })
    }
}

fn unavailable(error: memory::Error) -> CommandFailure {
    tracing::warn!(%error, "memory register unavailable");
    CommandFailure::Refused("memory register unavailable".into())
}

fn scope_label(store: &memory::MemoryStore, scope: &Scope) -> Result<String, memory::Error> {
    Ok(match scope {
        Scope::Global => "global".into(),
        Scope::Session(_) => "session".into(),
        Scope::Project(id) => format!(
            "project {}",
            store
                .project(id)?
                .map_or_else(|| id.clone(), |project| project.name)
        ),
        Scope::Group(id) => format!(
            "group {}",
            store
                .group(id)?
                .map_or_else(|| id.clone(), |group| group.name)
        ),
    })
}
