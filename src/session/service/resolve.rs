//! Model resolution for [`Service`]: the alias to its provider, key and
//! context window at create, the saved resolution read and checked on
//! resume, and the snapshot written for a row from before the column.

use std::path::{Path, PathBuf};

use ulid::Ulid;

use super::{ResumedSession, Service};
use crate::error::ServerError;
use crate::provider::auth;
use crate::provider::catalog::ModelResolution;
use crate::provider::config::{load_models, ResolvedModel};

/// The identity half of a resolution (provider, model, api) of a configured
/// alias, for comparison with a saved one.
fn identity_of(resolved: &ResolvedModel) -> ModelResolution {
    ModelResolution {
        version: crate::provider::catalog::RESOLUTION_VERSION,
        provider: resolved.provider_name.clone(),
        model: resolved.model.model.clone(),
        api: crate::provider::catalog::api_name(resolved.model.api),
        endpoint: String::new(),
        bounds: crate::provider::catalog::Bounds {
            context_window: 0,
            max_input_tokens: None,
            reported_output: None,
        },
        history_policy: String::new(),
        provenance: crate::provider::catalog::Provenance {
            context_window: String::new(),
            max_input_tokens: String::new(),
            reported_output: String::new(),
        },
    }
}

/// The refusal for a chat alias without a window, naming the file to fix.
fn no_context_window(
    e: crate::provider::catalog::NoContextWindow,
    models_path: &Path,
) -> ServerError {
    ServerError::NoContextWindow {
        alias: e.alias,
        provider: e.provider,
        model: e.model,
        models_path: models_path.display().to_string(),
    }
}

impl Service {
    pub(super) fn resolve_model(
        &self,
        workspace: &Path,
        alias: &str,
    ) -> Result<(ResolvedModel, PathBuf), ServerError> {
        let (config, path) = load_models(
            self.config.models_path.as_deref(),
            workspace,
            &self.config.config_dir,
        )?;
        Ok((config.resolve(alias)?, path))
    }

    pub async fn resolve_model_metadata(
        &self,
        workspace: &Path,
        alias: &str,
    ) -> Result<crate::provider::catalog::ResolvedChat, ServerError> {
        let (resolved, models_path) = self.resolve_model(workspace, alias)?;
        let key = auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )?;
        crate::provider::catalog::resolve_chat(
            &self.catalog,
            &resolved,
            &key,
            &crate::provider::openai::ClientCache::new(),
        )
        .await
        .map_err(|e| no_context_window(e, &models_path))
    }

    /// The alias's model, its key and its bounds. A conversation that has a
    /// saved resolution keeps it: no discovery, no refresh, and never another
    /// identity. A fresh session, or a legacy row without one, resolves now.
    /// Returns the model with the bounds applied, the key, the snapshot, and
    /// whether the snapshot must still be saved to an existing row.
    pub(super) async fn resolve_for_spawn(
        &self,
        session_id: Ulid,
        workspace: &Path,
        alias: &str,
        resumed: Option<&ResumedSession>,
    ) -> Result<(ResolvedModel, String, ModelResolution, bool), ServerError> {
        let (resolved, models_path) = self.resolve_model(workspace, alias)?;
        let api_key = auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )?;
        let models_path = models_path.as_path();
        let saved = match resumed {
            Some(loaded) => loaded
                .meta
                .model_resolution
                .as_deref()
                .map(ModelResolution::from_saved)
                .transpose()
                .map_err(|reason| ServerError::SavedResolutionInvalid {
                    session: session_id.to_string(),
                    reason,
                })?,
            None => None,
        };
        let (snapshot, save_late) = match saved {
            Some(saved) => {
                let current = identity_of(&resolved);
                if !saved.same_identity(&current) {
                    return Err(ServerError::ModelIdentityChanged {
                        session: session_id.to_string(),
                        alias: alias.to_string(),
                        saved: saved.identity(),
                        current: current.identity(),
                        models_path: models_path.display().to_string(),
                    });
                }
                tracing::debug!(alias = %alias, window = saved.bounds.context_window, "using the saved model resolution");
                (saved, false)
            }
            None => {
                let metadata = crate::provider::catalog::resolve_chat(
                    &self.catalog,
                    &resolved,
                    &api_key,
                    &crate::provider::openai::ClientCache::new(),
                )
                .await
                .map_err(|e| no_context_window(e, models_path))?;
                match &metadata.discrepancy {
                    Some(d) => tracing::warn!(alias = %alias, window = metadata.window, "{d}"),
                    None => {
                        tracing::debug!(alias = %alias, window = metadata.window, source = %metadata.window_source, "resolved chat window")
                    }
                }
                (metadata.snapshot, resumed.is_some())
            }
        };
        let mut resolved = resolved;
        resolved.model.context_window = Some(snapshot.bounds.context_window);
        resolved.model.max_input_tokens = snapshot.bounds.max_input_tokens;
        Ok((resolved, api_key, snapshot, save_late))
    }

    /// First activation of a row from before the column: it gets its
    /// snapshot now, and keeps it from here on.
    pub(super) async fn save_legacy_snapshot(
        &self,
        session_id: Ulid,
        snapshot: &ModelResolution,
    ) -> Result<(), ServerError> {
        self.history
            .set_model_resolution(session_id, snapshot)
            .await
            .map_err(ServerError::History)
    }
}
