//! Model resolution for [`Service`]: the alias to its provider, key and
//! context window at create, the saved resolution read and checked on
//! resume, and the snapshot written for a row from before the column.

use std::path::{Path, PathBuf};

use ulid::Ulid;

use super::{ResumedSession, Service};
use crate::bus::runtime::SelectedVariant;
use crate::error::ServerError;
use crate::provider::auth;
use crate::provider::catalog::ModelResolution;
use crate::provider::config::{effective_variants, load_models, ResolvedModel, Variants};

/// The identity half of a resolution (provider, model, api, endpoint) of a
/// configured alias, for comparison with a saved one.
fn identity_of(resolved: &ResolvedModel) -> ModelResolution {
    ModelResolution {
        version: crate::provider::catalog::RESOLUTION_VERSION,
        provider: resolved.provider_name.clone(),
        model: resolved.model.model.clone(),
        api: crate::provider::catalog::api_name(resolved.model.api),
        endpoint: crate::provider::catalog::endpoint_without_credentials(
            &resolved.provider.base_url,
        ),
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

/// What a new session asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantChoice {
    /// The alias's default variant, if it has one.
    Default,
    /// No variant, whatever the alias default is.
    Clear,
    /// A variant of the alias by name, or `none` for no variant (a variant
    /// the alias really calls `none` wins over the word).
    Word(String),
}

impl VariantChoice {
    /// A client's optional word: absent is the default.
    pub fn from_request(word: Option<&str>) -> Self {
        word.map_or(Self::Default, |w| Self::Word(w.to_string()))
    }
}

/// Where a session's variant comes from.
pub(super) enum VariantStart<'a> {
    Choice(&'a VariantChoice),
    Saved(Option<&'a str>),
}

/// Turns a client's word into the selection: an offered name selects it,
/// `none` clears, anything else is an error listing what the alias offers.
pub(crate) fn pick_variant(
    alias: &str,
    word: &str,
    offered: &Variants,
) -> Result<SelectedVariant, ServerError> {
    if let Some(v) = offered.get(word) {
        return Ok(Some((word.to_string(), v.clone())));
    }
    if word.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let list = if offered.is_empty() {
        "it offers no variants".to_string()
    } else {
        format!(
            "it offers: {}",
            offered.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    };
    Err(ServerError::UnknownVariant {
        alias: alias.to_string(),
        variant: word.to_string(),
        offered: list,
    })
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
                let mut saved = saved;
                if saved.endpoint.is_empty() {
                    // Saved before the endpoint was recorded: it is the one
                    // the alias maps to now, and is kept from here on.
                    saved.endpoint = current.endpoint;
                    if let Some(raw) = resumed.and_then(|r| r.meta.model_resolution.as_deref()) {
                        self.history
                            .fill_resolution_endpoint(session_id, raw, &saved)
                            .await
                            .map_err(ServerError::History)?;
                    }
                }
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

    /// The variant a session starts with, and the variants its alias offers.
    /// A fresh session takes the choice made at creation, else the alias
    /// default; a resumed one takes the variant it saved. A named variant
    /// the alias does not offer is an error, never a fallback.
    pub(super) async fn select_variant(
        &self,
        session_id: Ulid,
        alias: &str,
        resolved: &ResolvedModel,
        api_key: &str,
        start: VariantStart<'_>,
    ) -> Result<(SelectedVariant, Variants), ServerError> {
        let models_path = || {
            self.config
                .models_path
                .clone()
                .unwrap_or_else(|| self.config.config_dir.join("models.json"))
                .display()
                .to_string()
        };
        // Only the offer is needed here, not the window: a resume must work
        // with discovery down, so a missing window is not an error.
        let needs_tiers =
            resolved.model.variants.is_none() && resolved.provider.catalog_format.is_some();
        let limits = if needs_tiers {
            self.catalog
                .lookup(
                    resolved,
                    api_key,
                    &crate::provider::openai::ClientCache::new(),
                )
                .await
        } else {
            self.catalog.cached(resolved)
        }
        .limits
        .unwrap_or_default();
        let offered = effective_variants(resolved, limits.effort_tiers.as_deref().unwrap_or(&[]));
        let selected = match start {
            VariantStart::Saved(None) => None,
            VariantStart::Saved(Some(name)) => match offered.get(name) {
                Some(v) => Some((name.to_string(), v.clone())),
                None => {
                    return Err(ServerError::VariantUnavailable {
                        session: session_id.to_string(),
                        alias: alias.to_string(),
                        variant: name.to_string(),
                        models_path: models_path(),
                    })
                }
            },
            VariantStart::Choice(VariantChoice::Clear) => None,
            VariantStart::Choice(VariantChoice::Word(word)) => pick_variant(alias, word, &offered)?,
            VariantStart::Choice(VariantChoice::Default) => {
                match resolved.model.default_variant.as_deref() {
                    None => None,
                    Some(name) => match offered.get(name) {
                        Some(v) => Some((name.to_string(), v.clone())),
                        None => {
                            return Err(ServerError::DefaultVariantUnavailable {
                                alias: alias.to_string(),
                                variant: name.to_string(),
                                models_path: models_path(),
                            })
                        }
                    },
                }
            }
        };
        Ok((selected, offered))
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
