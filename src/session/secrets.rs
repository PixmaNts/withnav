//! The Captain's secrets, as the session service serves them: the answer to a `request_secret`
//! (the value is stored, the model is told only `stored as $NAME …`), the list a client shows
//! and revocation. The store itself, injection and redaction live in `crate::secrets`.

use std::time::SystemTime;

use ulid::Ulid;

use super::message::{MessageAccepted, accept_host_note, accept_secret_answer};
use super::service::{Service, SessionHandle, SessionId};
use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::ROOT_ACTOR_ID;
use crate::error::ServerError;
use crate::secrets::{
    SecretAccepted, SecretAccess, SecretNote, SecretOutcome, SecretSubmission, SecretView,
    declined_text, stored_text,
};

/// How many parent links the owner lookup follows. Live trees are one level deep (a child cannot
/// admit another child), so a longer chain, or a cycle, only comes from corrupt session data.
const MAX_DEPTH: usize = 16;

/// The root of the tree `id` belongs to, following `parent_of` at most [`MAX_DEPTH`] links. A
/// session still having a parent after that is an error rather than a guessed owner: secrets
/// stored, listed or injected under an intermediate session would belong to the wrong tree.
fn tree_root(
    id: SessionId,
    parent_of: impl Fn(SessionId) -> Option<SessionId>,
) -> Result<SessionId, ServerError> {
    let mut owner = id;
    for _ in 0..MAX_DEPTH {
        match parent_of(owner) {
            Some(parent) => owner = parent,
            None => return Ok(owner),
        }
    }
    match parent_of(owner) {
        None => Ok(owner),
        Some(_) => Err(ServerError::SessionTreeTooDeep {
            session: id.to_string(),
            max: MAX_DEPTH,
        }),
    }
}

impl Service {
    /// The conversation tree a session's secrets belong to: its root, as far as the live
    /// sessions tell (a session whose parent is not live owns its own).
    pub(crate) fn secret_owner(&self, id: SessionId) -> Result<SessionId, ServerError> {
        let sessions = self.lock_sessions();
        tree_root(id, |s| sessions.get(&s).and_then(|h| h.parent_session_id))
    }

    /// What the actor of a session in `workspace` owned by `owner` hands to its tools and
    /// hides from its model.
    pub(crate) fn secret_access(
        &self,
        owner: SessionId,
        workspace: &std::path::Path,
    ) -> SecretAccess {
        SecretAccess::new(self.secrets.clone(), owner, workspace)
    }

    async fn secret_handle(&self, id: SessionId) -> Result<SessionHandle, ServerError> {
        let handle = self.routable_handle(id).await?;
        if handle.parent_session_id.is_some() {
            return Err(ServerError::ChildReadOnly);
        }
        Ok(handle)
    }

    /// `GET /v1/sessions/{id}/secrets`: the secrets this conversation sees, metadata only.
    pub async fn list_secrets(&self, id: SessionId) -> Result<Vec<SecretView>, ServerError> {
        let handle = self.secret_handle(id).await?;
        Ok(self.secrets.list(self.secret_owner(id)?, &handle.workspace))
    }

    /// `DELETE /v1/sessions/{id}/secrets/{name}`: forgets the value (and its saved copy); the
    /// next processes no longer receive it. False when there was none.
    pub async fn revoke_secret(&self, id: SessionId, name: &str) -> Result<bool, ServerError> {
        let handle = self.secret_handle(id).await?;
        Ok(self
            .secrets
            .revoke(self.secret_owner(id)?, &handle.workspace, name))
    }

    /// `POST /v1/sessions/{id}/secrets/{request_id}`: the Captain's answer.
    ///
    /// While the request waits, a decline or the stored value's metadata becomes the request's
    /// result. Once it was settled, a value can still be given (`name` required): it replaces
    /// a secret of that name silently, and otherwise the conversation is told it is now
    /// available, unless another question waits (that note would answer it).
    pub async fn submit_secret(
        &self,
        id: SessionId,
        request_id: &str,
        submission: SecretSubmission,
    ) -> Result<SecretAccepted, ServerError> {
        let handle = self.secret_handle(id).await?;
        let owner = self.secret_owner(id)?;
        let pending = handle.status.pending_secret(request_id);
        if pending.is_none() {
            return self
                .provide_later(id, &handle, owner, request_id, submission)
                .await;
        }
        let mut stored = None;
        let accepted = accept_secret_answer(&handle, request_id, |details| {
            if let Some(name) = &submission.name
                && *name != details.name
            {
                return Err(ServerError::MalformedBody(format!(
                    "the request is for {}, not {name}",
                    details.name
                )));
            }
            let (text, note) = if submission.decline {
                (
                    declined_text(&details.name),
                    note(
                        Some(request_id),
                        &details.name,
                        SecretOutcome::Declined,
                        None,
                    ),
                )
            } else {
                let view = self.store(owner, &handle, &details.name, &submission)?;
                let text = stored_text(&view);
                let note = note(
                    Some(request_id),
                    &details.name,
                    SecretOutcome::Stored,
                    Some(&view),
                );
                stored = Some(view);
                (text, note)
            };
            Ok(note_command(id, text, note))
        })
        .await;
        settle_result(accepted, stored)
    }

    async fn provide_later(
        &self,
        id: SessionId,
        handle: &SessionHandle,
        owner: Ulid,
        request_id: &str,
        submission: SecretSubmission,
    ) -> Result<SecretAccepted, ServerError> {
        if submission.decline {
            return Err(ServerError::SecretStale);
        }
        let name = submission.name.clone().ok_or(ServerError::SecretStale)?;
        if let Some(details) = handle.status.waiting_secret(request_id)
            && details.name != name
        {
            return Err(ServerError::MalformedBody(format!(
                "the request is for {}, not {name}",
                details.name
            )));
        }
        let replaced = self
            .secrets
            .list(owner, &handle.workspace)
            .iter()
            .any(|view| view.name == name);
        let view = self.store(owner, handle, &name, &submission)?;
        if !replaced && !handle.status.pending_ask() && handle.permissions_waiting().is_none() {
            let text = format!("The Captain provided a secret: {}", stored_text(&view));
            let note = note(
                (!request_id.is_empty()).then_some(request_id),
                &name,
                SecretOutcome::Stored,
                Some(&view),
            );
            // The value is stored either way; a note that cannot be routed only stays unsaid.
            if let Err(error) = accept_host_note(handle, note_command(id, text, note)).await {
                tracing::warn!(%error, "secret note not delivered");
            }
        }
        Ok(SecretAccepted {
            accepted: true,
            secret: Some(view),
        })
    }

    fn store(
        &self,
        owner: Ulid,
        handle: &SessionHandle,
        name: &str,
        submission: &SecretSubmission,
    ) -> Result<SecretView, ServerError> {
        let value = submission
            .value
            .as_deref()
            .ok_or_else(|| ServerError::MalformedBody("a value or decline is required".into()))?;
        self.secrets
            .put(
                owner,
                &handle.workspace,
                name,
                value,
                submission.ttl,
                submission.scope,
            )
            .map_err(|e| ServerError::MalformedBody(e.to_string()))
    }
}

impl SessionHandle {
    /// The permission request this session waits on, if any.
    fn permissions_waiting(&self) -> Option<Ulid> {
        self.status.pending_permission()
    }
}

fn note(
    request_id: Option<&str>,
    name: &str,
    outcome: SecretOutcome,
    view: Option<&SecretView>,
) -> SecretNote {
    SecretNote {
        request_id: request_id.map(str::to_owned),
        name: name.to_owned(),
        outcome,
        scope: view.map(|v| v.scope),
        ttl: view.map(|v| v.ttl),
        expires_at: view.and_then(|v| v.expires_at.clone()),
    }
}

/// The Captain command carrying a secret's metadata: what the model reads, never the value.
fn note_command(id: SessionId, text: String, note: SecretNote) -> Command {
    Command {
        command_id: Ulid::generate(),
        session_id: id,
        source_actor_id: "user:http".into(),
        target_actor_id: ROOT_ACTOR_ID.into(),
        mode: DeliveryMode::NextTurn,
        content: vec![
            ContentBlock::Text { text },
            ContentBlock::SecretAnswer { note },
        ],
        created_at: SystemTime::now(),
    }
}

fn settle_result(
    accepted: Result<MessageAccepted, ServerError>,
    stored: Option<SecretView>,
) -> Result<SecretAccepted, ServerError> {
    accepted.map(|_| SecretAccepted {
        accepted: true,
        secret: stored,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A chain of `ancestors + 1` sessions, the last one the root; returns the deepest and the root.
    fn chain(ancestors: usize) -> (HashMap<SessionId, SessionId>, SessionId, SessionId) {
        let ids: Vec<SessionId> = (0..=ancestors).map(|_| Ulid::generate()).collect();
        let parents = ids.windows(2).map(|w| (w[0], w[1])).collect();
        (parents, ids[0], ids[ancestors])
    }

    // Given a session with exactly the allowed number of ancestors
    // Then its owner is the root of the chain
    #[test]
    fn the_owner_of_a_chain_at_the_depth_limit_is_its_root() {
        let (parents, deepest, root) = chain(MAX_DEPTH);
        let owner = tree_root(deepest, |s| parents.get(&s).copied()).unwrap();
        assert_eq!(owner, root);
    }

    // Given a corrupt chain one ancestor past the limit, or a cycle
    // Then the lookup is an explicit error, never the last session it reached
    #[test]
    fn a_chain_past_the_depth_limit_or_a_cycle_is_an_error() {
        let (parents, deepest, _) = chain(MAX_DEPTH + 1);
        let error = tree_root(deepest, |s| parents.get(&s).copied()).unwrap_err();
        assert!(matches!(
            error,
            ServerError::SessionTreeTooDeep { max: MAX_DEPTH, .. }
        ));

        let (a, b) = (Ulid::generate(), Ulid::generate());
        let cycle = HashMap::from([(a, b), (b, a)]);
        let error = tree_root(a, |s| cycle.get(&s).copied()).unwrap_err();
        assert!(matches!(error, ServerError::SessionTreeTooDeep { .. }));
    }
}
