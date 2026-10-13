//! Model resolution for [`Service`]: the alias to its provider, key and
//! context window at create, the saved resolution read and checked on
//! resume, and the snapshot written for a row from before the column.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ulid::Ulid;

use super::{ResumedSession, SelectedModel, Service};
use crate::bus::event::ModelIdentity;
use crate::bus::runtime::SelectedVariant;
use crate::bus::runtime::{ModelSwitch, ModelSwitcher, ModelTarget};
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
/// `nav run --variant off`). Config loading refuses a variant of that name,
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
            Some("inherit") => Self::Default,
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

pub(super) struct PreparationConfig {
    pub settings: crate::settings::Settings,
    pub catalog: crate::provider::config::ModelsConfig,
    pub path: PathBuf,
}
type PreparedAssignment = (
    String,
    VariantChoice,
    ResolvedModel,
    Option<String>,
    super::super::store::SelectionProvenance,
);

impl Service {
    pub(super) fn capture_preparation(
        &self,
        workspace: &Path,
    ) -> Result<PreparationConfig, ServerError> {
        let settings = crate::settings::load(&self.config.config_dir)
            .map_err(|e| ServerError::History(e.to_string()))?;
        let (catalog, path) = load_models(
            self.config.models_path.as_deref(),
            workspace,
            &self.config.config_dir,
        )?;
        Ok(PreparationConfig {
            settings,
            catalog,
            path,
        })
    }

    pub(super) fn assigned_from_snapshot(
        &self,
        captured: &PreparationConfig,
        agents: &crate::agent::definitions::Agents,
        agent: &str,
        child: bool,
    ) -> Result<Option<PreparedAssignment>, ServerError> {
        use crate::provider::assignments::{AgentModel, AssignmentResolver};
        use crate::settings::AssignmentVariant;
        let resolver = AssignmentResolver::new(&captured.catalog, &captured.settings);
        if child
            && captured
                .settings
                .agent_assignment(agent)
                .map_err(ServerError::History)?
                .is_none()
            && agents.get(agent).and_then(|a| a.model.as_deref()).is_none()
        {
            return Ok(None);
        }
        let records: Vec<_> = agents
            .all()
            .map(|a| (a.id.as_str(), a.model.as_deref(), a.mode.is_primary()))
            .collect();
        let default = resolver
            .default_agent_check(&records)
            .map_err(|e| ServerError::History(e.to_string()))?;
        let header = agents.get(agent).and_then(|a| a.model.as_deref());
        let outcome = resolver
            .agent_model(agent, header)
            .map_err(ServerError::History)?;
        let mut warning_text = None;
        use super::super::store::{ModelSource, SelectionProvenance, VariantSource};
        let mut source = if captured
            .settings
            .agent_assignment(agent)
            .map_err(ServerError::History)?
            .is_some()
        {
            ModelSource::Config
        } else {
            ModelSource::Header
        };
        let assigned = match outcome {
            AgentModel::Assigned(model) => model,
            AgentModel::Unassigned if child => return Ok(None),
            outcome => {
                source = ModelSource::DefaultAgent;
                if let AgentModel::Fallback(warning) = outcome {
                    tracing::warn!("{warning}");
                    warning_text = Some(warning.to_string());
                }
                let header = agents.get(&default).and_then(|a| a.model.as_deref());
                match resolver
                    .agent_model(&default, header)
                    .map_err(ServerError::History)?
                {
                    AgentModel::Assigned(model) => model,
                    _ => {
                        return Err(ServerError::History(format!(
                            "default_agent '{default}' has no usable model"
                        )))
                    }
                }
            }
        };
        let variant_source =
            if assigned.variant_choice == crate::settings::AssignmentVariant::Default {
                VariantSource::ModelDefault
            } else {
                match source {
                    ModelSource::Config => VariantSource::Config,
                    ModelSource::Header => VariantSource::Header,
                    _ => VariantSource::DefaultAgent,
                }
            };
        let variant = match assigned.variant_choice {
            AssignmentVariant::Default => VariantChoice::Default,
            AssignmentVariant::Off => VariantChoice::Clear,
            AssignmentVariant::Named(name) => VariantChoice::Exact(name),
        };
        Ok(Some((
            assigned.name.to_string(),
            variant,
            assigned.resolved,
            warning_text,
            SelectionProvenance {
                model: source,
                variant: variant_source,
            },
        )))
    }

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
        let name = crate::provider::model_name::ModelName::parse(alias, &config)
            .map_err(|e| ServerError::History(e.to_string()))?;
        let resolved = config.resolve_name(&name)?;
        Ok((resolved, path))
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
        captured_model: Option<(ResolvedModel, PathBuf)>,
    ) -> Result<(ResolvedModel, String, ModelResolution, bool, PathBuf), ServerError> {
        let (resolved, models_file) = match captured_model {
            Some(model) => model,
            None => self.resolve_model(workspace, alias)?,
        };
        if resolved.model.api != crate::provider::config::ApiKind::Chat {
            return Err(ServerError::History(format!(
                "model '{alias}' requires API 'chat'"
            )));
        }
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

    /// Cached discovery shared with the session picker, without starting network discovery.
    pub(crate) fn settings_model_data(
        &self,
        resolved: &ResolvedModel,
    ) -> crate::provider::catalog::CatalogData {
        self.catalog.cached(resolved)
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Service {
    /// How the runtime of session `id` resolves another alias and commits the
    /// switch to it: the same resolution as a new session (window, variant
    /// offer, the alias's default variant), and one store transaction for the
    /// alias, its resolution and its variant.
    pub(super) fn model_switcher(
        &self,
        id: Ulid,
        workspace: PathBuf,
        selected: Arc<Mutex<SelectedModel>>,
    ) -> ModelSwitcher {
        let current = {
            let selected = selected.clone();
            move || lock(&selected).alias.clone()
        };
        let resolver = {
            let (service, selected) = (self.clone(), selected.clone());
            move |request: crate::bus::runtime::model::SelectionRequest| {
                let (service, selected, workspace) =
                    (service.clone(), selected.clone(), workspace.clone());
                Box::pin(async move {
                    let current = lock(&selected).clone();
                    let meta = service
                        .history
                        .meta(id)
                        .await
                        .map_err(|e| e.to_string())?
                        .ok_or("session missing")?;
                    let agent_name = request
                        .target_agent
                        .as_deref()
                        .unwrap_or(&request.requesting_agent);
                    let agents = service.load_agents(&workspace, id, false).await;
                    let agent = agents
                        .get(agent_name)
                        .filter(|a| a.mode.is_primary())
                        .cloned()
                        .ok_or_else(|| format!("unknown primary agent '{agent_name}'"))?;
                    let saved = meta.agent_overrides.get(agent_name).cloned();
                    use super::super::store::{ModelSource, SelectionProvenance, VariantSource};
                    let captured = service
                        .capture_preparation(&workspace)
                        .map_err(|e| e.to_string())?;
                    let assignment = service
                        .assigned_from_snapshot(&captured, &agents, agent_name, false)
                        .map_err(|e| e.to_string())?;
                    let mut provenance = assignment
                        .as_ref()
                        .map(|a| a.4.clone())
                        .or(current.provenance.clone())
                        .unwrap_or(SelectionProvenance {
                            model: ModelSource::Parent,
                            variant: VariantSource::ModelDefault,
                        });
                    let (alias, choice, override_value) = if request.command == "agent" {
                        let assigned = assignment.as_ref().map(|a| (a.0.clone(), a.1.clone()));
                        let (base, variant) =
                            assigned.unwrap_or((current.alias.clone(), VariantChoice::Default));
                        let choice = match saved.as_ref().map(|s| &s.variant) {
                            Some(super::super::store::AgentVariantOverride::Off) => {
                                VariantChoice::Clear
                            }
                            Some(super::super::store::AgentVariantOverride::Named(name)) => {
                                VariantChoice::Exact(name.clone())
                            }
                            Some(super::super::store::AgentVariantOverride::Inherit)
                                if saved.as_ref().and_then(|s| s.model.as_ref()).is_some() =>
                            {
                                VariantChoice::Default
                            }
                            _ => variant,
                        };
                        (
                            saved.as_ref().and_then(|s| s.model.clone()).unwrap_or(base),
                            choice,
                            saved,
                        )
                    } else if request.command == "variant" {
                        let choice = request.variant.clone().unwrap_or(VariantChoice::Default);
                        let value = super::super::store::AgentOverride {
                            model: saved.as_ref().and_then(|s| s.model.clone()),
                            variant: choice.clone().into(),
                        };
                        let (alias, inherited_choice) = if value.model.is_none() {
                            assignment
                                .as_ref()
                                .map(|a| (a.0.clone(), a.1.clone()))
                                .unwrap_or((current.alias.clone(), VariantChoice::Default))
                        } else {
                            (current.alias.clone(), VariantChoice::Default)
                        };
                        let effective_choice = if choice == VariantChoice::Default {
                            inherited_choice
                        } else {
                            choice
                        };
                        (alias, effective_choice, Some(value))
                    } else {
                        let value = super::super::store::AgentOverride {
                            model: Some(request.alias.clone()),
                            variant: super::super::store::AgentVariantOverride::Inherit,
                        };
                        (request.alias.clone(), VariantChoice::Default, Some(value))
                    };
                    let resolved = {
                        let name = crate::provider::model_name::ModelName::parse(
                            &alias,
                            &captured.catalog,
                        )
                        .map_err(|e| e.to_string())?;
                        captured
                            .catalog
                            .resolve_name(&name)
                            .map_err(|e| e.to_string())?
                    };
                    #[cfg(test)]
                    {
                        let barrier = service.preparation_barrier.lock().unwrap().take();
                        if let Some(barrier) = barrier {
                            barrier.reached.notify_one();
                            barrier.release.notified().await;
                        }
                    }
                    let warning = assignment.as_ref().and_then(|a| a.3.clone()).filter(|_| {
                        override_value
                            .as_ref()
                            .and_then(|o| o.model.as_ref())
                            .is_none()
                    });
                    let mut target = service
                        .resolve_switch_target(
                            id,
                            &workspace,
                            &alias,
                            current.resolution.as_deref(),
                            &choice,
                            (resolved, captured.path),
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    target.agent = Some(agent);
                    target.warning = warning;
                    if override_value
                        .as_ref()
                        .and_then(|o| o.model.as_ref())
                        .is_some()
                    {
                        provenance.model = ModelSource::Override;
                        provenance.variant = VariantSource::ModelDefault;
                    }
                    if override_value.as_ref().is_some_and(|o| {
                        o.variant != super::super::store::AgentVariantOverride::Inherit
                    }) {
                        provenance.variant = VariantSource::Override;
                    }
                    target.source = serde_json::to_value(provenance.model)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .into();
                    target.variant_source = serde_json::to_value(provenance.variant)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .into();
                    target.provenance = Some(provenance);
                    target.previous = current.identity();
                    target.new.agent = target.agent.as_ref().map(|a| a.id.to_string());
                    target.new.variant = target.variant.as_ref().map(|(name, _)| name.clone());
                    target.new.source = Some(target.source.clone());
                    target.new.variant_source = Some(target.variant_source.clone());
                    target.override_value = override_value.map(|value| {
                        (
                            value.model,
                            match value.variant {
                                super::super::store::AgentVariantOverride::Inherit => {
                                    VariantChoice::Default
                                }
                                super::super::store::AgentVariantOverride::Off => {
                                    VariantChoice::Clear
                                }
                                super::super::store::AgentVariantOverride::Named(name) => {
                                    VariantChoice::Exact(name)
                                }
                            },
                        )
                    });
                    Ok(target)
                }) as crate::bus::runtime::BoxFuture<Result<ModelTarget, String>>
            }
        };
        let next_revision = {
            let selected = selected.clone();
            move || lock(&selected).revision + 1
        };
        let committer = {
            let (service, selected) = (self.clone(), selected);
            move |target: ModelTarget,
                  floor: usize,
                  staged: Option<crate::bus::runtime::model::StagedCheckpoint>,
                  envelopes: Arc<Vec<crate::bus::event::EventEnvelope>>,
                  install: crate::bus::runtime::Install,
                  cancel: tokio_util::sync::CancellationToken| {
                let (service, selected) = (service.clone(), selected.clone());
                Box::pin(async move {
                    if cancel.is_cancelled() {
                        return Err("selection cancelled before callback".into());
                    }
                    let revision = envelopes
                        .iter()
                        .find(|e| e.kind == "model.changed")
                        .and_then(|e| e.payload["revision"].as_u64())
                        .unwrap_or(0);
                    let agent = target.agent.as_ref().expect("resolved agent");
                    service
                        .history
                        .commit_agent_selection_cancellable(
                            id,
                            agent.id.as_str(),
                            target.override_value.clone().map(|(model, variant)| {
                                super::super::store::AgentOverride {
                                    model,
                                    variant: variant.into(),
                                }
                            }),
                            super::super::store::EffectiveAgentSelection {
                                provenance: target.provenance.clone(),
                                model: target.alias.clone(),
                                resolution: target.resolution.clone(),
                                variant: target.variant.clone(),
                                primary_agent: agent.id.to_string(),
                                reasoning_floor: floor,
                            },
                            staged.map(|staged| super::super::store::SelectionCheckpoint {
                                transcript: staged.transcript,
                                checkpoint: staged.checkpoint,
                            }),
                            envelopes,
                            Some(cancel),
                        )
                        .await?;
                    service.before_switch_install().await;
                    // The model, its variant, its offer, the runtime's variant
                    // slot and the context gauge change under one lock: a
                    // reader sees the old selection or the new, never a mix.
                    let mut guard = lock(&selected);
                    install();
                    guard.provenance = target.provenance.clone();
                    guard.agent = agent.id.to_string();
                    guard.role = agent.clone();
                    guard.alias = target.alias.clone();
                    if let Some(execution) = &guard.execution {
                        let mut resolved = execution.resolved.clone();
                        resolved.alias = target.alias.clone();
                        resolved.provider_name = target.resolution.provider.clone();
                        resolved.provider = target.provider.clone();
                        resolved.model.model = target.model.clone();
                        resolved.model.reasoning_history = target.reasoning_history;
                        resolved.model.max_output_tokens = target.max_tokens;
                        resolved.model.context_window =
                            Some(target.resolution.bounds.context_window);
                        resolved.model.max_input_tokens = target.resolution.bounds.max_input_tokens;
                        guard.execution = Some(Arc::new(super::EffectiveExecution {
                            automatic_permission_review: execution.automatic_permission_review,
                            yolo: execution.yolo,
                            permission_review_timeout_secs: execution
                                .permission_review_timeout_secs,
                            scratch_choice: execution.scratch_choice.clone(),
                            resolved,
                            api_key: target.api_key.clone(),
                        }));
                    }
                    guard.resolution = Some(Arc::new(target.resolution.clone()));
                    guard.revision = revision;
                    guard.variant = target.variant.clone();
                    guard.offer = Arc::new(Offer {
                        variants: target.offer.clone(),
                        known: target.offer_known,
                    });
                    Ok(())
                }) as crate::bus::runtime::BoxFuture<Result<(), String>>
            }
        };
        Arc::new(ModelSwitch {
            current_alias: Box::new(current),
            next_revision: Box::new(next_revision),
            resolve: Box::new(resolver),
            commit: Box::new(committer),
        })
    }

    /// Resolves `alias` as the target of a switch from the conversation that
    /// runs on `running`: always from today's configuration and discovery, like
    /// a new session, and starting on the alias's own default variant.
    async fn resolve_switch_target(
        &self,
        id: Ulid,
        _workspace: &Path,
        alias: &str,
        running: Option<&ModelResolution>,
        choice: &VariantChoice,
        captured: (ResolvedModel, PathBuf),
    ) -> Result<ModelTarget, ServerError> {
        let (mut resolved, models_file) = captured;
        if resolved.model.api != crate::provider::config::ApiKind::Chat {
            return Err(ServerError::Model(
                crate::error::ProviderError::UnknownModelAlias(alias.to_string()),
            ));
        }
        let api_key = auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )?;
        let chat = crate::provider::catalog::resolve_chat(
            &self.catalog,
            &resolved,
            &api_key,
            &crate::provider::openai::ClientCache::new(),
        )
        .await
        .map_err(|e| no_context_window(e, &models_file))?;
        let snapshot = chat.snapshot;
        resolved.model.context_window = Some(snapshot.bounds.context_window);
        resolved.model.max_input_tokens = snapshot.bounds.max_input_tokens;
        let (variant, offered) = self
            .select_variant(
                id,
                alias,
                &resolved,
                &api_key,
                &models_file,
                VariantStart::Choice(choice),
            )
            .await?;
        let limits = crate::agent::context::ContextLimits::from_resolution(
            alias,
            variant.as_ref().map(|(name, _)| name.as_str()),
            &snapshot,
            resolved.model.max_output_tokens,
        )
        .map_err(|e| ServerError::ImpossibleBudget {
            alias: alias.to_string(),
            reason: e.to_string(),
        })?;
        let previous = running
            .map(|r| ModelIdentity::from_upstream(&r.provider, &r.model))
            .unwrap_or_else(|| ModelIdentity::from_upstream("", ""));
        Ok(ModelTarget {
            max_message_tokens: resolved.model.max_message_tokens,
            warning: None,
            provenance: None,
            source: String::new(),
            variant_source: String::new(),
            agent: None,
            override_value: None,
            alias: alias.to_string(),
            provider: resolved.provider.clone(),
            api_key,
            model: resolved.model.model.clone(),
            reasoning_history: resolved.model.reasoning_history,
            max_tokens: resolved.model.max_output_tokens,
            variant,
            new: ModelIdentity::from_upstream(&snapshot.provider, &snapshot.model),
            same_identity: running.is_some_and(|r| r.same_identity(&snapshot)),
            resolution: snapshot,
            limits,
            previous,
            offer: offered.variants,
            offer_known: offered.known,
        })
    }
}
