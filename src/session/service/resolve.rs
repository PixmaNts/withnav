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
use crate::provider::config::{effective_variants, load_models, ResolvedModel, Variant, Variants};

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

/// The word that clears the variant in text interfaces (`/variant off`,
/// `navai run --variant off`). Config loading refuses a variant of that name,
/// so it never collides with an offered one.
pub const CLEAR_WORD: &str = "off";

/// What a new session asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantChoice {
    /// The alias's default variant, if it has one.
    Default,
    /// No variant, whatever the alias default is.
    Clear,
    /// This variant of the alias, by its exact name. Never a keyword: a
    /// variant really called `none` is selected by `Exact("none")`.
    Exact(String),
}

impl VariantChoice {
    /// A text interface's optional word: absent is the default, [`CLEAR_WORD`]
    /// clears, anything else is an exact name.
    pub fn from_word(word: Option<&str>) -> Self {
        match word {
            None => Self::Default,
            Some(CLEAR_WORD) => Self::Clear,
            Some(name) => Self::Exact(name.to_string()),
        }
    }
}

/// The variant a saved session row holds.
#[derive(Debug, Clone, Copy)]
pub(super) struct SavedVariant<'a> {
    pub name: &'a str,
    /// The override object saved with it, as JSON; absent for a row from
    /// before it was kept.
    pub override_json: Option<&'a str>,
}

/// Where a session's variant comes from.
pub(super) enum VariantStart<'a> {
    Choice(&'a VariantChoice),
    Saved(Option<SavedVariant<'a>>),
}

/// The offered variant called exactly `name`, or an error listing what the
/// alias offers.
pub(crate) fn exact_variant(
    alias: &str,
    name: &str,
    offered: &Variants,
) -> Result<SelectedVariant, ServerError> {
    if let Some(v) = offered.get(name) {
        return Ok(Some((name.to_string(), v.clone())));
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
        variant: name.to_string(),
        offered: list,
    })
}

/// What a model offers as variants, and whether that is known right now.
#[derive(Debug, Clone, Default)]
pub(crate) struct Offer {
    pub variants: Variants,
    /// False when the offer is generated from the provider's reported effort
    /// tiers and they cannot be confirmed (none cached, or only an old copy
    /// whose refresh failed): the names in `variants` are then not a verdict
    /// on what the model offers.
    pub known: bool,
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
    /// Returns the model with the bounds applied, the key, the snapshot,
    /// whether the snapshot must still be saved to an existing row, and the
    /// models.json the alias was read from.
    pub(super) async fn resolve_for_spawn(
        &self,
        session_id: Ulid,
        workspace: &Path,
        alias: &str,
        resumed: Option<&ResumedSession>,
    ) -> Result<(ResolvedModel, String, ModelResolution, bool, PathBuf), ServerError> {
        let (resolved, models_file) = self.resolve_model(workspace, alias)?;
        let api_key = auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )?;
        let models_path = models_file.as_path();
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
        Ok((resolved, api_key, snapshot, save_late, models_file))
    }

    /// Reads the offer of a live session's alias again, for a session that
    /// started while it was unknown. The alias is resolved from today's
    /// configuration while the session keeps talking to the model it started
    /// on: when the alias now names another model, its offer is refused
    /// (`ModelIdentityChanged`) rather than applied to the running one.
    pub(crate) async fn reread_offer(
        &self,
        session: &str,
        workspace: &Path,
        alias: &str,
        running: Option<&ModelResolution>,
    ) -> Result<Offer, ServerError> {
        let (resolved, models_file) = self.resolve_model(workspace, alias)?;
        if let Some(running) = running {
            let current = identity_of(&resolved);
            if !running.same_identity(&current) {
                return Err(ServerError::ModelIdentityChanged {
                    session: session.to_string(),
                    alias: alias.to_string(),
                    saved: running.identity(),
                    current: current.identity(),
                    models_path: models_file.display().to_string(),
                });
            }
        }
        let api_key = auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )?;
        Ok(self.variant_offer(&resolved, &api_key).await)
    }

    /// What the alias offers as variants. Only the offer is needed, not the
    /// window: a resume must work with discovery down, so a missing window
    /// is not an error here.
    pub(crate) async fn variant_offer(&self, resolved: &ResolvedModel, api_key: &str) -> Offer {
        let generated =
            resolved.model.variants.is_none() && resolved.provider.catalog_format.is_some();
        let data = if generated {
            self.catalog
                .lookup(
                    resolved,
                    api_key,
                    &crate::provider::openai::ClientCache::new(),
                )
                .await
        } else {
            self.catalog.cached(resolved)
        };
        // Tiers that are past their age and could not be refreshed do not
        // prove a name is gone, so they do not count as a known offer.
        let refresh_failed = data.stale && data.error.is_some();
        let limits = data.limits.unwrap_or_default();
        let known = !generated || (limits.effort_tiers.is_some() && !refresh_failed);
        Offer {
            variants: effective_variants(resolved, limits.effort_tiers.as_deref().unwrap_or(&[])),
            known,
        }
    }

    /// The variant a session starts with, and what its alias offers. A fresh
    /// session takes the choice made at creation, else the alias default; a
    /// resumed one takes the variant it saved.
    ///
    /// A variant the alias confirmably does not offer is an error, never a
    /// fallback. When the offer cannot be known (generated from the
    /// provider's tiers, none cached, discovery down) a saved variant is kept
    /// with the override object saved with it, since nothing says it is gone
    /// (a row saved without one waits for the offer), while a choice made now
    /// is refused as retryable.
    pub(super) async fn select_variant(
        &self,
        session_id: Ulid,
        alias: &str,
        resolved: &ResolvedModel,
        api_key: &str,
        models_path: &Path,
        start: VariantStart<'_>,
    ) -> Result<(SelectedVariant, Offer), ServerError> {
        let offer = self.variant_offer(resolved, api_key).await;
        let path = models_path.display().to_string();
        let cannot_confirm = |name: &str| ServerError::VariantOfferUnknown {
            alias: alias.to_string(),
            variant: name.to_string(),
        };
        let selected = match start {
            VariantStart::Saved(None) | VariantStart::Choice(VariantChoice::Clear) => None,
            VariantStart::Saved(Some(saved)) => {
                let name = saved.name;
                match offer.variants.get(name) {
                    // A known offer is the authority: the saved override is
                    // refreshed from the current definition.
                    Some(v) if offer.known => Some((name.to_string(), v.clone())),
                    None if offer.known => {
                        return Err(ServerError::VariantUnavailable {
                            session: session_id.to_string(),
                            alias: alias.to_string(),
                            variant: name.to_string(),
                            models_path: path,
                        })
                    }
                    // The offer cannot be read: the session sends what it
                    // saved, or, with nothing saved, the resume waits.
                    _ => {
                        let saved_override = saved
                            .override_json
                            .and_then(|raw| serde_json::from_str::<Variant>(raw).ok());
                        match saved_override {
                            Some(object) => {
                                tracing::warn!(session = %session_id, alias, variant = name, "the provider's variants are not known; using the saved variant");
                                Some((name.to_string(), object))
                            }
                            None => return Err(cannot_confirm(name)),
                        }
                    }
                }
            }
            VariantStart::Choice(VariantChoice::Exact(name)) => {
                if !offer.known {
                    return Err(cannot_confirm(name));
                }
                exact_variant(alias, name, &offer.variants)?
            }
            VariantStart::Choice(VariantChoice::Default) => {
                match resolved.model.default_variant.as_deref() {
                    None => None,
                    Some(name) => match offer.variants.get(name) {
                        Some(v) => Some((name.to_string(), v.clone())),
                        None if !offer.known => return Err(cannot_confirm(name)),
                        None => {
                            return Err(ServerError::DefaultVariantUnavailable {
                                alias: alias.to_string(),
                                variant: name.to_string(),
                                models_path: path,
                            })
                        }
                    },
                }
            }
        };
        Ok((selected, offer))
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
