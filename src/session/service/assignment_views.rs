//! Read-only assignment projections. No provider credentials or request overrides cross this seam.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{Service, SessionId};
use crate::agent::definitions::{AgentDefinition, AgentId, AgentMode, AgentRoots, Agents};
use crate::error::ServerError;
use crate::provider::assignments::{
    AgentModel, AssignedModel, AssignmentResolver, DefaultAgentError, Layer, LayerAssignment,
    LayerDisabled,
};
use crate::provider::config::{ApiKind, ModelsConfig, effective_variants, load_models};
use crate::provider::model_name::ModelName;
use crate::session::store::{AgentOverride, AgentVariantOverride, StoredSession};
use crate::settings::{AssignmentVariant, Settings};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentSource {
    LayerPreference,
    Config,
    Header,
    DefaultAgent,
    Parent,
    Override,
    ModelDefault,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssignmentSelection {
    pub model: String,
    pub variant: Option<String>,
    pub source: AssignmentSource,
    pub variant_source: AssignmentSource,
}

/// Must be supplied from the same authoritative snapshot as the stored selection.
/// Identity fields bind provenance to a selection, rather than to today's config.
#[derive(Debug, Clone)]
pub(super) struct PinnedSelectionProvenance {
    pub model: String,
    pub variant: Option<String>,
    pub source: AssignmentSource,
    pub variant_source: AssignmentSource,
}

#[derive(Debug, Serialize)]
pub struct AgentAssignmentEntry {
    pub id: String,
    pub description: String,
    pub mode: AgentMode,
    pub assignment: Option<AssignmentSelection>,
    pub effective: Option<AssignmentSelection>,
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    pub selection_override: Option<AssignmentOverrideView>,
    pub warning: Option<String>,
    pub inherits_parent: bool,
}

#[derive(Debug, Serialize)]
pub struct AssignmentOverrideView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct AgentsBody {
    pub default_agent: String,
    pub agents: Vec<AgentAssignmentEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerStatus {
    Usable,
    Disabled,
}

#[derive(Debug, Serialize)]
pub struct LayerEntry {
    pub layer: &'static str,
    pub status: LayerStatus,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub gateway: Option<String>,
    pub upstream_id: Option<String>,
    pub api: Option<ApiKind>,
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_diagnostic: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct LayersBody {
    pub layers: Vec<LayerEntry>,
}

struct Snapshot {
    workspace: PathBuf,
    settings: Settings,
    catalog: ModelsConfig,
    agents: Agents,
    offers: BTreeMap<String, crate::provider::config::Variants>,
}

fn diagnostic(reason: impl Into<String>) -> ServerError {
    ServerError::MalformedBody(reason.into())
}

fn pinned_provenance(row: &StoredSession) -> Option<PinnedSelectionProvenance> {
    use crate::session::store::{ModelSource, VariantSource};
    let provenance = row.provenance.as_ref()?;
    let source = match provenance.model {
        ModelSource::Config => AssignmentSource::Config,
        ModelSource::Header => AssignmentSource::Header,
        ModelSource::DefaultAgent => AssignmentSource::DefaultAgent,
        ModelSource::Parent => AssignmentSource::Parent,
        ModelSource::Override => AssignmentSource::Override,
    };
    let variant_source = match provenance.variant {
        VariantSource::Config => AssignmentSource::Config,
        VariantSource::Header => AssignmentSource::Header,
        VariantSource::DefaultAgent => AssignmentSource::DefaultAgent,
        VariantSource::Parent => AssignmentSource::Parent,
        VariantSource::Override => AssignmentSource::Override,
        VariantSource::ModelDefault => AssignmentSource::ModelDefault,
        VariantSource::LayerPreference => AssignmentSource::LayerPreference,
    };
    Some(PinnedSelectionProvenance {
        model: row.model.clone(),
        variant: row.variant.clone(),
        source,
        variant_source,
    })
}

impl Service {
    async fn assignment_snapshot(&self, workspace: &Path) -> Result<Snapshot, ServerError> {
        let workspace = workspace.to_path_buf();
        let config = self.config.clone();
        let discovery = self.catalog.clone();
        tokio::task::spawn_blocking(move || {
            if !workspace.is_absolute() {
                return Err(diagnostic("workspace must be an absolute directory"));
            }
            let workspace = workspace
                .canonicalize()
                .map_err(|_| diagnostic("workspace is not an accessible directory"))?;
            if !workspace.is_dir() {
                return Err(diagnostic("workspace must be a directory"));
            }
            let settings = crate::settings::load(&config.config_dir)
                .map_err(|error| diagnostic(format!("invalid config.json: {error}")))?;
            let (catalog, _) = load_models(
                config.models_path.as_deref(),
                &workspace,
                &config.config_dir,
            )
            .map_err(|_| diagnostic("invalid models.json: catalog could not be loaded"))?;
            let roots = AgentRoots {
                home: config.rules_roots.home.clone(),
                config_dir: Some(config.config_dir.clone()),
            };
            let agents = Agents::load(&roots, &workspace);
            let mut offers = BTreeMap::new();
            for model in catalog.models.values() {
                let raw = format!("{}/{}", model.provider, model.model);
                if let Ok(name) = ModelName::parse(&raw, &catalog)
                    && let Ok(resolved) = catalog.resolve_name(&name)
                {
                    let cached = discovery.cached(&resolved);
                    let tiers = cached
                        .limits
                        .as_ref()
                        .and_then(|l| l.effort_tiers.as_deref());
                    if resolved.model.variants.is_some()
                        || resolved.provider.catalog_format.is_none()
                        || tiers.is_some()
                    {
                        offers.insert(raw, effective_variants(&resolved, tiers.unwrap_or(&[])));
                    }
                }
            }
            Ok(Snapshot {
                workspace,
                settings,
                catalog,
                agents,
                offers,
            })
        })
        .await
        .map_err(|_| diagnostic("assignment snapshot unavailable"))?
    }

    /// Validated lower-layer assignments; this never tests provider connectivity.
    pub async fn assignment_layers(&self, workspace: &Path) -> Result<LayersBody, ServerError> {
        let snapshot = self.assignment_snapshot(workspace).await?;
        let resolver = AssignmentResolver::new(&snapshot.catalog, &snapshot.settings);
        let mut layers = Vec::with_capacity(3);
        for layer in [Layer::Reflex, Layer::Instinct, Layer::Metacognition] {
            let requested = snapshot
                .settings
                .layer_assignment(layer)
                .map_err(diagnostic)?;
            let mut entry = LayerEntry {
                layer: layer.config_key(),
                status: LayerStatus::Disabled,
                model: requested
                    .as_ref()
                    .and_then(|a| canonical_name(&a.model, &snapshot.catalog).ok()),
                variant: None,
                gateway: None,
                upstream_id: None,
                api: None,
                reason: None,
                variant_diagnostic: None,
            };
            if requested
                .as_ref()
                .is_some_and(|a| canonical_name(&a.model, &snapshot.catalog).is_err())
            {
                entry.reason = Some("assignment has an invalid canonical model name".into());
                layers.push(entry);
                continue;
            }
            match resolver.layer_assignment(layer).map_err(diagnostic)? {
                LayerAssignment::Disabled(reason) => entry.reason = Some(layer_reason(&reason)),
                LayerAssignment::Enabled(assigned) => {
                    if layer != Layer::Metacognition && assigned.variant.is_some() {
                        entry.reason = Some(format!(
                            "layer '{layer}' disabled: model '{}' does not support variants on its API",
                            assigned.name
                        ));
                    } else if assigned.prefer_low
                        && !snapshot.offers.contains_key(&assigned.name.to_string())
                    {
                        entry.status = LayerStatus::Usable;
                        entry.variant_diagnostic = Some(
                            "preferred-low unresolved: no cached provider effort tiers; runtime discovery is required",
                        );
                    } else {
                        match self.assignment_variant(
                            &snapshot,
                            &assigned,
                            layer == Layer::Metacognition,
                        ) {
                            Ok(variant) => {
                                entry.status = LayerStatus::Usable;
                                entry.variant = variant;
                            }
                            Err(reason) => entry.reason = Some(reason),
                        }
                    }
                    entry.model = Some(assigned.name.to_string());
                    entry.gateway = Some(assigned.name.gateway().into());
                    entry.upstream_id = Some(assigned.name.id().into());
                    entry.api = Some(assigned.resolved.model.api);
                }
            }
            layers.push(entry);
        }
        Ok(LayersBody { layers })
    }

    /// Agent definitions, base choices and durable session-local selections.
    pub async fn assignment_agents(
        &self,
        workspace: &Path,
        session: Option<SessionId>,
    ) -> Result<AgentsBody, ServerError> {
        let mut snapshot = self.assignment_snapshot(workspace).await?;
        let saved = match session {
            Some(id) => {
                let row = self
                    .history()
                    .meta(id)
                    .await
                    .map_err(|_| {
                        diagnostic(
                            "saved session assignment unavailable: history could not be read",
                        )
                    })?
                    .ok_or_else(|| ServerError::SessionNotFound(id.to_string()))?;
                if Path::new(&row.workspace).canonicalize().ok().as_ref()
                    != Some(&snapshot.workspace)
                {
                    return Err(diagnostic("session_id belongs to a different workspace"));
                }
                if let Some(handle) = self.handle(id) {
                    snapshot.agents = handle.status.selection.agents().clone();
                }
                Some(row)
            }
            None => None,
        };
        let resolver = AssignmentResolver::new(&snapshot.catalog, &snapshot.settings);
        let definitions: Vec<_> = snapshot.agents.all().collect();
        let known: Vec<_> = definitions
            .iter()
            .map(|a| (a.id.as_str(), a.model.as_deref(), a.mode.is_primary()))
            .collect();
        let wanted = snapshot.settings.default_agent().map_err(diagnostic)?;
        crate::agent::definitions::AgentId::parse(wanted)
            .map_err(|_| diagnostic("default_agent is not a valid agent id"))?;
        if saved.is_none()
            && let Some(definition) = snapshot.agents.get(wanted)
        {
            let configured = snapshot
                .settings
                .agent_assignment(wanted)
                .map_err(diagnostic)?;
            if configured
                .as_ref()
                .map(|a| a.model.as_str())
                .or(definition.model.as_deref())
                .is_some_and(|raw| canonical_name(raw, &snapshot.catalog).is_err())
            {
                return Err(diagnostic(format!(
                    "default_agent '{wanted}' has an invalid canonical model assignment"
                )));
            }
        }
        let (default_agent, fallback) = match resolver.default_agent_check(&known) {
            Ok(default_agent) => {
                let default = snapshot
                    .agents
                    .get(&default_agent)
                    .ok_or_else(|| diagnostic("default_agent definition unavailable"))?;
                let fallback = self.base_assignment(&snapshot, default)?.0;
                if fallback.is_none() && saved.is_none() {
                    return Err(diagnostic("default_agent has no usable model"));
                }
                (default_agent, fallback)
            }
            Err(_) if saved.is_some() => (wanted.to_owned(), None),
            Err(error) => return Err(diagnostic(default_agent_reason(error))),
        };
        if let Some(row) = &saved {
            let current = row.primary_agent.as_deref().unwrap_or("build");
            AgentId::parse(current)
                .map_err(|_| diagnostic("saved session agent is not a valid agent id"))?;
            if snapshot.agents.get(current).is_none() {
                return Err(diagnostic(format!(
                    "saved session agent '{current}' is unavailable"
                )));
            }
            for agent in row.agent_overrides.keys() {
                AgentId::parse(agent)
                    .map_err(|_| diagnostic("saved override agent is not a valid agent id"))?;
                if snapshot.agents.get(agent).is_none() {
                    return Err(diagnostic(format!(
                        "saved override agent '{agent}' is unavailable"
                    )));
                }
            }
        }
        // Validate the durable identity before projecting today's possibly unusable defaults.
        let saved_effective = saved
            .as_ref()
            .map(|row| saved_selection(row, pinned_provenance(row).as_ref(), &snapshot.catalog))
            .transpose()?;
        let mut agents = Vec::with_capacity(definitions.len());
        for definition in definitions {
            let (base, mut warning) = self.base_assignment(&snapshot, definition)?;
            let inherits_parent =
                base.is_none() && definition.mode == AgentMode::Subagent && warning.is_none();
            if base.is_none() && !inherits_parent && warning.is_none() {
                warning = Some(format!(
                    "agent '{}' has no model assignment; using default_agent '{default_agent}'",
                    definition.id
                ));
            }
            if base.is_none() && !inherits_parent && fallback.is_none() {
                warning = Some(format!(
                    "{}; default_agent '{default_agent}' has no usable fallback",
                    warning.as_deref().unwrap_or("model assignment unavailable")
                ));
            }
            let assignment = if inherits_parent {
                None
            } else {
                base.or_else(|| {
                    fallback.clone().map(|mut selected| {
                        selected.source = AssignmentSource::DefaultAgent;
                        if !matches!(selected.variant_source, AssignmentSource::ModelDefault) {
                            selected.variant_source = AssignmentSource::DefaultAgent;
                        }
                        selected
                    })
                })
            };
            let mut effective = assignment.clone();
            let mut selection_override = None;
            if let Some(row) = &saved {
                if let Some(over) = row.agent_overrides.get(definition.id.as_str()) {
                    if definition.id.as_str() != row.primary_agent.as_deref().unwrap_or("build") {
                        if let Some(model) = &over.model {
                            canonical_name(model, &snapshot.catalog).map_err(diagnostic)?;
                        }
                        match self.override_assignment(&snapshot, assignment.as_ref(), over) {
                            Ok(selected) => effective = Some(selected),
                            Err(_) => {
                                effective = None;
                                warning = Some(format!(
                                    "agent '{}' saved override unavailable with current assignments",
                                    definition.id
                                ));
                            }
                        }
                    }
                    selection_override = Some(override_view(over));
                }
                if definition.id.as_str() == row.primary_agent.as_deref().unwrap_or("build") {
                    effective = saved_effective.clone();
                } else if inherits_parent && selection_override.is_none() {
                    let mut parent = saved_effective
                        .clone()
                        .expect("validated session selection");
                    parent.source = AssignmentSource::Parent;
                    parent.variant_source = AssignmentSource::Parent;
                    effective = Some(parent);
                }
            }
            agents.push(AgentAssignmentEntry {
                id: definition.id.to_string(),
                description: definition.description.clone(),
                mode: definition.mode,
                assignment,
                effective,
                selection_override,
                warning,
                inherits_parent,
            });
        }
        Ok(AgentsBody {
            default_agent,
            agents,
        })
    }

    fn base_assignment(
        &self,
        snapshot: &Snapshot,
        definition: &AgentDefinition,
    ) -> Result<(Option<AssignmentSelection>, Option<String>), ServerError> {
        let configured = snapshot
            .settings
            .agent_assignment(definition.id.as_str())
            .map_err(diagnostic)?;
        let requested = configured
            .as_ref()
            .map(|a| a.model.as_str())
            .or(definition.model.as_deref());
        if requested.is_some_and(|raw| canonical_name(raw, &snapshot.catalog).is_err()) {
            return Ok((
                None,
                Some(format!(
                    "agent '{}' has an invalid canonical model assignment; using default_agent",
                    definition.id
                )),
            ));
        }
        let source = if configured.is_some() {
            AssignmentSource::Config
        } else {
            AssignmentSource::Header
        };
        match AssignmentResolver::new(&snapshot.catalog, &snapshot.settings)
            .agent_model(definition.id.as_str(), definition.model.as_deref())
            .map_err(diagnostic)?
        {
            AgentModel::Unassigned => Ok((None, None)),
            AgentModel::Fallback(warning) => Ok((
                None,
                Some(format!(
                    "agent '{}' model '{}' unavailable; using default_agent",
                    warning.agent,
                    requested.expect("fallback has a requested canonical model")
                )),
            )),
            AgentModel::Assigned(model) => match self.assignment_variant(snapshot, &model, false) {
                Ok(variant) => Ok((
                    Some(AssignmentSelection {
                        model: model.name.to_string(),
                        variant,
                        source,
                        variant_source: if model.variant_choice == AssignmentVariant::Default {
                            AssignmentSource::ModelDefault
                        } else {
                            source
                        },
                    }),
                    None,
                )),
                Err(reason) => Ok((
                    None,
                    Some(format!(
                        "agent '{}' model '{}' unavailable: {reason}; using default_agent",
                        definition.id, model.name
                    )),
                )),
            },
        }
    }

    fn assignment_variant(
        &self,
        snapshot: &Snapshot,
        model: &AssignedModel,
        metacognition: bool,
    ) -> Result<Option<String>, String> {
        let empty = BTreeMap::new();
        let offered = snapshot
            .offers
            .get(&model.name.to_string())
            .unwrap_or(&empty);
        let variant = if metacognition
            && model.variant_choice == AssignmentVariant::Default
            && offered.contains_key("low")
        {
            Some("low".into())
        } else {
            model.variant.clone()
        };
        if let Some(name) = &variant
            && !offered.contains_key(name)
        {
            return Err(if !snapshot.offers.contains_key(&model.name.to_string()) {
                "variant offer unavailable: no cached provider effort tiers".into()
            } else {
                format!(
                    "configured variant is not offered by model '{}'",
                    model.name
                )
            });
        }
        Ok(variant)
    }

    fn override_assignment(
        &self,
        snapshot: &Snapshot,
        base: Option<&AssignmentSelection>,
        over: &AgentOverride,
    ) -> Result<AssignmentSelection, ServerError> {
        let name = over
            .model
            .as_deref()
            .or_else(|| base.map(|b| b.model.as_str()))
            .ok_or_else(|| diagnostic("variant override has no base model"))?;
        canonical_name(name, &snapshot.catalog).map_err(diagnostic)?;
        let model_name = ModelName::parse(name, &snapshot.catalog)
            .map_err(|_| diagnostic("saved override has an invalid canonical model name"))?;
        let resolved = snapshot.catalog.resolve_name(&model_name).map_err(|_| {
            diagnostic(format!(
                "saved override model '{model_name}' is unavailable"
            ))
        })?;
        if resolved.model.api != ApiKind::Chat {
            return Err(diagnostic("saved agent override requires chat API"));
        }
        let choice = match &over.variant {
            AgentVariantOverride::Inherit => AssignmentVariant::Default,
            AgentVariantOverride::Off => AssignmentVariant::Off,
            AgentVariantOverride::Named(name) => AssignmentVariant::Named(name.clone()),
        };
        let variant = match &choice {
            AssignmentVariant::Default if over.model.is_none() => {
                base.and_then(|b| b.variant.clone())
            }
            AssignmentVariant::Default => resolved.model.default_variant.clone(),
            AssignmentVariant::Off => None,
            AssignmentVariant::Named(name) => Some(name.clone()),
        };
        let assigned = AssignedModel {
            name: model_name,
            resolved,
            variant,
            variant_choice: choice,
            prefer_low: false,
        };
        let variant = self
            .assignment_variant(snapshot, &assigned, false)
            .map_err(diagnostic)?;
        Ok(AssignmentSelection {
            model: assigned.name.to_string(),
            variant,
            source: if over.model.is_some() {
                AssignmentSource::Override
            } else {
                base.map_or(AssignmentSource::Override, |b| b.source)
            },
            variant_source: match over.variant {
                AgentVariantOverride::Inherit if over.model.is_none() => {
                    base.map_or(AssignmentSource::ModelDefault, |b| b.variant_source)
                }
                AgentVariantOverride::Inherit => AssignmentSource::ModelDefault,
                _ => AssignmentSource::Override,
            },
        })
    }
}

fn canonical_name(raw: &str, catalog: &ModelsConfig) -> Result<String, String> {
    if raw.contains([':', '@', '?', '#']) {
        return Err("invalid canonical model name".into());
    }
    ModelName::parse(raw, catalog)
        .map(|n| n.to_string())
        .map_err(|_| "invalid canonical model name".into())
}

fn layer_reason(reason: &LayerDisabled) -> String {
    match reason {
        LayerDisabled::Missing { layer } => {
            format!("layer '{layer}' disabled: layers.{layer} is missing")
        }
        LayerDisabled::UnknownModel { layer, .. } => {
            format!("layer '{layer}' disabled: model is unknown, ambiguous or unavailable")
        }
        LayerDisabled::InvalidVariant { layer, .. } => {
            format!("layer '{layer}' disabled: configured variant is not offered")
        }
        LayerDisabled::WrongApi {
            layer,
            expected,
            actual,
            ..
        } => format!("layer '{layer}' disabled: must use '{expected}' API, not '{actual}'"),
    }
}

fn default_agent_reason(error: DefaultAgentError) -> String {
    match error {
        DefaultAgentError::NoUsableModel { name, .. } => format!(
            "default_agent '{name}' has no usable model: assignment is missing, invalid, ambiguous or unavailable"
        ),
        other => other.to_string(),
    }
}

fn override_view(over: &AgentOverride) -> AssignmentOverrideView {
    AssignmentOverrideView {
        model: over.model.clone(),
        variant: match &over.variant {
            AgentVariantOverride::Inherit => None,
            AgentVariantOverride::Off => Some(None),
            AgentVariantOverride::Named(name) => Some(Some(name.clone())),
        },
    }
}

pub(super) fn saved_selection(
    row: &StoredSession,
    provenance: Option<&PinnedSelectionProvenance>,
    catalog: &ModelsConfig,
) -> Result<AssignmentSelection, ServerError> {
    let resolution = row.model_resolution.as_deref().ok_or_else(|| {
        diagnostic("saved session assignment unavailable: no durable model identity")
    })?;
    let resolution =
        crate::provider::catalog::ModelResolution::from_saved(resolution).map_err(|_| {
            diagnostic("saved session assignment unavailable: invalid durable model identity")
        })?;
    if resolution.api != "chat" {
        return Err(diagnostic("saved session assignment requires chat API"));
    }
    let model = canonical_name(
        &format!("{}/{}", resolution.provider, resolution.model),
        catalog,
    )
    .map_err(diagnostic)?;
    let pinned = match provenance {
        Some(pinned) if pinned.model == model && pinned.variant == row.variant => Some(pinned),
        Some(_) => {
            return Err(diagnostic(
                "saved selection provenance does not match durable identity",
            ));
        }
        None => None,
    };
    let over = row
        .agent_overrides
        .get(row.primary_agent.as_deref().unwrap_or("build"));
    if let Some(over) = over {
        if let Some(requested) = &over.model
            && canonical_name(requested, catalog).map_err(diagnostic)? != model
        {
            return Err(diagnostic(
                "saved model override does not match durable selection",
            ));
        }
        let matches = match &over.variant {
            AgentVariantOverride::Inherit => true,
            AgentVariantOverride::Off => row.variant.is_none(),
            AgentVariantOverride::Named(name) => row.variant.as_ref() == Some(name),
        };
        if !matches {
            return Err(diagnostic(
                "saved variant override does not match durable selection",
            ));
        }
    }
    let source = if over.is_some_and(|o| o.model.is_some()) {
        AssignmentSource::Override
    } else {
        pinned.map(|p| p.source).ok_or_else(|| diagnostic("saved selection model provenance unavailable: authoritative pinned metadata required"))?
    };
    let variant_source = match over.map(|o| &o.variant) {
        Some(AgentVariantOverride::Off | AgentVariantOverride::Named(_)) => AssignmentSource::Override,
        _ => pinned.map(|p| p.variant_source).ok_or_else(|| diagnostic("saved selection variant provenance unavailable: authoritative pinned metadata required"))?,
    };
    Ok(AssignmentSelection {
        model,
        variant: row.variant.clone(),
        source,
        variant_source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    #[tokio::test]
    async fn current_pinned_override_survives_catalog_model_and_variant_removal_live_and_stopped() {
        let (root, state) = fixture(json!({"agents":{"build":{"model":"mock/combo/chat"}}}));
        crate::provider::auth::set_key(&state.config.config_dir, "mock", "test").unwrap();
        let id = state
            .create_session_with(
                root.path(),
                Some("mock/combo/chat"),
                false,
                crate::session::VariantChoice::Exact("low".into()),
                None,
            )
            .await
            .unwrap();
        let path = state.config.models_path.as_ref().unwrap();
        let mut catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        catalog["models"]
            .as_object_mut()
            .unwrap()
            .retain(|_, model| model["model"] != "combo/chat");
        catalog["models"]["mock/replacement"] =
            json!({"provider":"mock","model":"replacement","context_window":128000});
        std::fs::write(path, catalog.to_string()).unwrap();
        std::fs::write(
            state.config.config_dir.join("config.json"),
            json!({"agents":{"build":{"model":"mock/replacement"}}}).to_string(),
        )
        .unwrap();
        for stopped in [false, true] {
            if stopped {
                state.close_session(id).await.unwrap();
            }
            let (status, body) =
                get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let build = body["agents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == "build")
                .unwrap();
            assert_eq!(build["effective"]["model"], "mock/combo/chat");
            assert_eq!(build["effective"]["variant"], "low");
            assert_eq!(build["effective"]["source"], "override");
        }
    }

    #[tokio::test]
    async fn pinned_selection_survives_model_removal_without_retargeting_config_live_and_stopped() {
        for overridden in [false, true] {
            let settings = json!({"agents":{"build":{"model":"mock/combo/chat","variant":"low"}}});
            let (root, state) = fixture(settings.clone());
            crate::provider::auth::set_key(&state.config.config_dir, "mock", "test-only-secret")
                .unwrap();
            let id = state
                .create_session_with(
                    root.path(),
                    overridden.then_some("mock/combo/chat"),
                    false,
                    if overridden {
                        crate::session::VariantChoice::Exact("low".into())
                    } else {
                        crate::session::VariantChoice::Default
                    },
                    None,
                )
                .await
                .unwrap();
            state
                .history()
                .update_agent_override(
                    id,
                    "lux",
                    AgentOverride {
                        model: Some("mock/combo/chat".into()),
                        variant: AgentVariantOverride::Named("low".into()),
                    },
                )
                .await
                .unwrap();
            let path = state.config.models_path.as_ref().unwrap();
            let mut catalog: Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            catalog["models"]
                .as_object_mut()
                .unwrap()
                .retain(|_, model| model["model"] != "combo/chat");
            std::fs::write(path, catalog.to_string()).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(
                    &std::fs::read_to_string(state.config.config_dir.join("config.json")).unwrap()
                )
                .unwrap(),
                settings
            );
            for stopped in [false, true] {
                if stopped {
                    state.close_session(id).await.unwrap();
                }
                let (status, body) =
                    get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                let build = agent(&body, "build");
                assert!(build["assignment"].is_null());
                assert_eq!(build["effective"]["model"], "mock/combo/chat");
                assert_eq!(build["effective"]["variant"], "low");
                let source = if overridden { "override" } else { "config" };
                assert_eq!(build["effective"]["source"], source);
                assert_eq!(build["effective"]["variant_source"], source);
                assert!(
                    build["warning"]
                        .as_str()
                        .unwrap()
                        .contains("mock/combo/chat")
                );
                assert!(
                    build["warning"]
                        .as_str()
                        .unwrap()
                        .contains("no usable fallback")
                );
                assert!(agent(&body, "lux")["effective"].is_null());
                assert!(!agent(&body, "lux")["warning"].is_null());
                assert!(agent(&body, "plan")["effective"].is_null());
                assert!(!agent(&body, "plan")["warning"].is_null());
                assert_eq!(
                    agent(&body, "child")["effective"]["model"],
                    "mock/combo/chat"
                );
                assert_eq!(agent(&body, "child")["effective"]["source"], "parent");
                for secret in [
                    "url-secret",
                    "AUTH_SECRET",
                    "header-secret",
                    "test-only-secret",
                ] {
                    assert!(!body.to_string().contains(secret));
                }
                let (status, _) = get(&state, root.path(), "agents", "").await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
            }
            let connection = rusqlite::Connection::open(&state.config.history_db).unwrap();
            connection
                .execute(
                    "UPDATE sessions SET model_resolution=?1 WHERE id=?2",
                    rusqlite::params!["{malformed-secret", id.to_string()],
                )
                .unwrap();
            let (status, body) =
                get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(!body.to_string().contains("malformed-secret"));
        }
    }

    fn fixture(settings: Value) -> (tempfile::TempDir, crate::server::AppState) {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("cfg");
        std::fs::create_dir_all(config_dir.join("agents")).unwrap();
        std::fs::write(config_dir.join("config.json"), settings.to_string()).unwrap();
        let models = json!({"providers":{"mock":{"base_url":"https://url-secret.invalid/v1", "api_key_env":"AUTH_SECRET", "headers":{"Authorization":"header-secret"}}},"models":{
            "mock/combo/chat":{"provider":"mock","model":"combo/chat","api":"chat","context_window":128000,"supports_tools":true,"variants":{"low":{"reasoning_effort":"low"},"high":{"reasoning_effort":"high"}},"default_variant":"high"},
            "mock/fim":{"provider":"mock","model":"fim","api":"fim"},
            "mock/instinct":{"provider":"mock","model":"instinct","api":"systemone"}
        }});
        std::fs::write(config_dir.join("models.json"), models.to_string()).unwrap();
        for (name, header) in [
            ("lux", "mode: primary\nmodel: mock/combo/chat"),
            ("broken", "mode: primary\nmodel: mock/missing"),
            ("child", "mode: subagent"),
        ] {
            std::fs::write(
                config_dir.join("agents").join(format!("{name}.md")),
                format!("---\ndescription: Test role\n{header}\n---\nPrompt"),
            )
            .unwrap();
        }
        let state = crate::server::AppState::new(crate::server::ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            models_path: Some(config_dir.join("models.json")),
            config_dir,
            allow_non_loopback: false,
            bearer_token: None,
            history_db: root.path().join("history.db"),
            data_dir: root.path().join("data"),
            rules_roots: Default::default(),
            web: None,
        })
        .unwrap();
        (root, state)
    }

    async fn get(
        state: &crate::server::AppState,
        root: &Path,
        route: &str,
        extra: &str,
    ) -> (StatusCode, Value) {
        let uri = format!(
            "/v1/{route}?workspace={}{}",
            {
                let mut url = reqwest::Url::parse("http://localhost").unwrap();
                url.query_pairs_mut()
                    .append_pair("workspace", &root.to_string_lossy());
                url.query()
                    .unwrap()
                    .strip_prefix("workspace=")
                    .unwrap()
                    .to_owned()
            },
            extra
        );
        let response = crate::server::routes::app(state.clone())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn agent<'a>(body: &'a Value, id: &str) -> &'a Value {
        body["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()
    }

    #[tokio::test]
    async fn http_layers_are_three_validated_assignments_not_connections() {
        let (root, state) = fixture(
            json!({"layers":{"reflex":"mock/fim","instinct":"mock/combo/chat","metacognition":"mock/combo/chat"}}),
        );
        let (status, body) = get(&state, root.path(), "layers", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["layers"].as_array().unwrap().len(), 3);
        assert_eq!(body["layers"][0]["status"], "usable");
        assert_eq!(body["layers"][1]["status"], "disabled");
        assert!(
            body["layers"][1]["reason"]
                .as_str()
                .unwrap()
                .contains("systemone")
        );
        assert_eq!(body["layers"][2]["variant"], "low");
        let text = body.to_string();
        for secret in [
            "url-secret",
            "AUTH_SECRET",
            "header-secret",
            "legacy-chat",
            "Authorization",
        ] {
            assert!(!text.contains(secret));
        }
        std::fs::write(
            state.config.config_dir.join("config.json"),
            json!({"layers":{"metacognition":{"model":"mock/combo/chat","variant":null}}})
                .to_string(),
        )
        .unwrap();
        let (_, body) = get(&state, root.path(), "layers", "").await;
        assert_eq!(body["layers"][0]["status"], "disabled");
        assert_eq!(body["layers"][2]["status"], "usable");
        assert!(body["layers"][2]["variant"].is_null());
    }

    #[tokio::test]
    async fn http_agents_expose_config_header_fallback_and_parent_inheritance() {
        let (root, state) = fixture(json!({"agents":{"build":{"model":"mock/combo/chat"}}}));
        let (status, body) = get(&state, root.path(), "agents", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["default_agent"], "build");
        assert_eq!(agent(&body, "build")["assignment"]["source"], "config");
        assert_eq!(agent(&body, "lux")["assignment"]["source"], "header");
        assert_eq!(
            agent(&body, "broken")["assignment"]["source"],
            "default_agent"
        );
        assert!(!agent(&body, "broken")["warning"].is_null());
        assert!(agent(&body, "child")["assignment"].is_null());
        assert!(agent(&body, "child")["effective"].is_null());
        assert_eq!(agent(&body, "child")["inherits_parent"], true);
        assert!(agent(&body, "build").get("override").is_none());
        assert!(!body.to_string().contains("legacy-chat"));
        assert!(!agent(&body, "plan")["warning"].is_null());
    }

    #[tokio::test]
    async fn http_session_views_use_durable_identity_and_reject_foreign_workspace() {
        let (root, state) = fixture(json!({"agents":{"build":{"model":"mock/combo/chat"}}}));
        let id = ulid::Ulid::generate();
        state
            .history()
            .record_session(id, root.path().to_str().unwrap(), "legacy-chat", false)
            .await
            .unwrap();
        let resolution = crate::provider::catalog::ModelResolution {
            version: crate::provider::catalog::RESOLUTION_VERSION,
            provider: "mock".into(),
            model: "combo/chat".into(),
            api: "chat".into(),
            endpoint: "https://url-secret.invalid/v1".into(),
            bounds: crate::provider::catalog::Bounds {
                context_window: 128000,
                max_input_tokens: None,
                reported_output: None,
            },
            history_policy: "preserve".into(),
            provenance: crate::provider::catalog::Provenance {
                context_window: "models.json".into(),
                max_input_tokens: String::new(),
                reported_output: String::new(),
            },
        };
        state
            .history()
            .set_model_resolution(id, &resolution)
            .await
            .unwrap();
        let mut row = state.history().meta(id).await.unwrap().unwrap();
        let pinned = PinnedSelectionProvenance {
            model: "mock/combo/chat".into(),
            variant: None,
            source: AssignmentSource::Config,
            variant_source: AssignmentSource::Config,
        };
        let mut changed_catalog = load_models(
            state.config.models_path.as_deref(),
            root.path(),
            &state.config.config_dir,
        )
        .unwrap()
        .0;
        // A config refresh cannot change the provenance of the durable selection.
        changed_catalog
            .models
            .get_mut("mock/combo/chat")
            .unwrap()
            .default_variant = Some("low".into());
        let selection = saved_selection(&row, Some(&pinned), &changed_catalog).unwrap();
        assert_eq!(selection.source, AssignmentSource::Config);
        assert_eq!(selection.variant_source, AssignmentSource::Config);
        assert!(saved_selection(&row, None, &changed_catalog).is_err());
        row.agent_overrides.insert(
            "build".into(),
            AgentOverride {
                model: Some("mock/combo/chat".into()),
                variant: AgentVariantOverride::Inherit,
            },
        );
        let selection = saved_selection(&row, Some(&pinned), &changed_catalog).unwrap();
        assert_eq!(selection.source, AssignmentSource::Override);
        assert_eq!(selection.variant_source, AssignmentSource::Config);
        row.agent_overrides.insert(
            "build".into(),
            AgentOverride {
                model: None,
                variant: AgentVariantOverride::Off,
            },
        );
        let selection = saved_selection(&row, Some(&pinned), &changed_catalog).unwrap();
        assert_eq!(selection.source, AssignmentSource::Config);
        assert_eq!(selection.variant_source, AssignmentSource::Override);
        assert!(saved_selection(&row, None, &changed_catalog).is_err());
        state
            .history()
            .update_agent_override(
                id,
                "build",
                AgentOverride {
                    model: Some("mock/combo/chat".into()),
                    variant: AgentVariantOverride::Off,
                },
            )
            .await
            .unwrap();
        let (status, body) = get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(agent(&body, "build")["effective"]["source"], "override");
        assert!(agent(&body, "build")["effective"]["variant"].is_null());
        assert!(agent(&body, "build")["override"]["variant"].is_null());
        assert_eq!(agent(&body, "child")["effective"]["source"], "parent");
        assert!(!body.to_string().contains("url-secret"));
        let other = tempfile::tempdir().unwrap();
        let (status, body) =
            get(&state, other.path(), "agents", &format!("&session_id={id}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("different workspace")
        );
        let (status, _) = get(
            &state,
            root.path(),
            "agents",
            &format!("&session_id={}", ulid::Ulid::generate()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn http_invalid_defaults_malformed_settings_and_legacy_sessions_are_diagnostic() {
        let (root, state) = fixture(
            json!({"default_agent":"child","agents":{"build":{"model":"mock/combo/chat"}}}),
        );
        let (status, body) = get(&state, root.path(), "agents", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("not primary"));
        std::fs::write(state.config.config_dir.join("config.json"), "{invalid").unwrap();
        let (status, body) = get(&state, root.path(), "layers", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("config.json"));
        std::fs::write(
            state.config.config_dir.join("config.json"),
            json!({"agents":{"build":{"model":"mock/combo/chat"}}}).to_string(),
        )
        .unwrap();
        let id = ulid::Ulid::generate();
        state
            .history()
            .record_session(id, root.path().to_str().unwrap(), "legacy-chat", false)
            .await
            .unwrap();
        let (status, body) = get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("no durable model identity")
        );
    }

    #[tokio::test]
    async fn http_unknown_models_and_alias_requests_never_choose_arbitrary_models() {
        let (root, state) = fixture(
            json!({"layers":{"reflex":"mock/missing","instinct":"legacy-instinct"},"agents":{"build":{"model":"legacy-chat"}}}),
        );
        let (_, body) = get(&state, root.path(), "layers", "").await;
        assert_eq!(body["layers"][0]["status"], "disabled");
        assert_eq!(body["layers"][0]["model"], "mock/missing");
        assert!(body["layers"][1]["model"].is_null());
        let (status, body) = get(&state, root.path(), "agents", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("default_agent"));
        std::fs::write(state.config.config_dir.join("config.json"), json!({"agents":{"build":{"model":"mock/combo/chat"},"lux":{"model":"mock/combo/chat","variant":null}}}).to_string()).unwrap();
        let (_, body) = get(&state, root.path(), "agents", "").await;
        assert_eq!(agent(&body, "lux")["assignment"]["source"], "config");
        assert_eq!(
            agent(&body, "lux")["assignment"]["variant_source"],
            "config"
        );
        assert!(agent(&body, "lux")["assignment"]["variant"].is_null());
    }

    #[tokio::test]
    async fn layer_non_chat_variants_match_runtime_and_discovery_is_explicitly_unresolved() {
        let (root, state) = fixture(
            json!({"layers":{"reflex":{"model":"mock/fim","variant":"low"},"instinct":{"model":"mock/instinct","variant":"low"},"metacognition":"mock/combo/chat"}}),
        );
        let path = state.config.models_path.as_ref().unwrap();
        let mut catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for key in ["mock/fim", "mock/instinct"] {
            catalog["models"][key]["variants"] = json!({"low":{"reasoning_effort":"low"}});
        }
        catalog["models"]["mock/combo/chat"]
            .as_object_mut()
            .unwrap()
            .remove("variants");
        catalog["models"]["mock/combo/chat"]
            .as_object_mut()
            .unwrap()
            .remove("default_variant");
        catalog["providers"]["mock"]["catalog_format"] = json!("omniroute");
        std::fs::write(path, catalog.to_string()).unwrap();
        let models = load_models(Some(path), root.path(), &state.config.config_dir)
            .unwrap()
            .0;
        let (_, body) = get(&state, root.path(), "layers", "").await;
        for (index, layer) in [(0, Layer::Reflex), (1, Layer::Instinct)] {
            assert_eq!(body["layers"][index]["status"], "disabled");
            let error = crate::provider::layers::assigned(&models, &state.config.config_dir, layer)
                .unwrap_err();
            assert_eq!(body["layers"][index]["reason"], error.to_string());
        }
        assert!(body["layers"][2]["variant"].is_null());
        assert!(
            body["layers"][2]["variant_diagnostic"]
                .as_str()
                .unwrap()
                .contains("unresolved")
        );
    }

    #[tokio::test]
    async fn ambiguous_catalog_keys_and_url_overrides_are_not_exposed() {
        let (root, state) = fixture(
            json!({"layers":{"metacognition":"mock/combo/chat"},"agents":{"build":{"model":"mock/combo/chat"}}}),
        );
        let path = state.config.models_path.as_ref().unwrap();
        let mut catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        catalog["models"]["https://raw-key-secret.invalid"] =
            catalog["models"]["mock/combo/chat"].clone();
        std::fs::write(path, catalog.to_string()).unwrap();
        for route in ["layers", "agents"] {
            let (_, body) = get(&state, root.path(), route, "").await;
            assert!(!body.to_string().contains("raw-key-secret"));
        }
        catalog["models"]
            .as_object_mut()
            .unwrap()
            .remove("https://raw-key-secret.invalid");
        std::fs::write(path, catalog.to_string()).unwrap();
        let snapshot = state.assignment_snapshot(root.path()).await.unwrap();
        let over = AgentOverride {
            model: Some("mock/https://auth-secret.invalid".into()),
            variant: AgentVariantOverride::Off,
        };
        let error = state
            .override_assignment(&snapshot, None, &over)
            .unwrap_err();
        assert!(!error.to_string().contains("auth-secret"));

        let (root, state) = fixture(json!({"agents":{"build":{"model":"mock/combo/chat"}}}));
        let secret = "https://user:synthetic-secret@host";
        for name in [secret, "missing-agent"] {
            let id = ulid::Ulid::generate();
            state
                .history()
                .record_session(id, root.path().to_str().unwrap(), "legacy-chat", false)
                .await
                .unwrap();
            state
                .history()
                .set_primary_agent(id, Some(name))
                .await
                .unwrap();
            let (status, body) =
                get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(!body.to_string().contains(secret));
            assert!(!body.to_string().contains("synthetic-secret"));
            assert_eq!(
                body["error"],
                if name == secret {
                    "malformed request body: saved session agent is not a valid agent id"
                        .to_string()
                } else {
                    format!("malformed request body: saved session agent '{name}' is unavailable")
                }
            );

            state
                .history()
                .set_primary_agent(id, Some("build"))
                .await
                .unwrap();
            state
                .history()
                .update_agent_override(id, name, AgentOverride::default())
                .await
                .unwrap();
            let (status, body) =
                get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(!body.to_string().contains(secret));
            assert!(!body.to_string().contains("synthetic-secret"));
            assert_eq!(
                body["error"],
                if name == secret {
                    "malformed request body: saved override agent is not a valid agent id"
                        .to_string()
                } else {
                    format!("malformed request body: saved override agent '{name}' is unavailable")
                }
            );

            // Unknown saved fields can be quoted verbatim by serde's history error.
            let connection = rusqlite::Connection::open(&state.config.history_db).unwrap();
            connection
                .execute(
                    "UPDATE sessions SET agent_overrides=?1 WHERE id=?2",
                    rusqlite::params![json!({"build": {secret: null}}).to_string(), id.to_string()],
                )
                .unwrap();
            assert!(state.history().meta(id).await.unwrap_err().contains(secret));
            let (status, body) =
                get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(
                body["error"],
                "malformed request body: saved session assignment unavailable: history could not be read"
            );
            assert!(!body.to_string().contains(secret));
            assert!(!body.to_string().contains("synthetic-secret"));
        }
    }

    #[tokio::test]
    async fn live_session_uses_pinned_definitions_after_agent_file_removal() {
        let (root, state) = fixture(json!({"agents":{"build":{"model":"mock/combo/chat"}}}));
        crate::provider::auth::set_key(&state.config.config_dir, "mock", "test-only-secret")
            .unwrap();
        let id = state
            .create_session_as(root.path(), Some("mock/combo/chat"), false, Some("lux"))
            .await
            .unwrap();
        let before = state
            .handle(id)
            .unwrap()
            .status
            .selection
            .agents()
            .get("lux")
            .cloned()
            .unwrap();
        std::fs::remove_file(state.config.config_dir.join("agents/lux.md")).unwrap();
        let (status, body) = get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
        assert_eq!(status, StatusCode::OK);
        let lux = body["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == "lux")
            .unwrap();
        assert_eq!(lux["effective"]["source"], "override");
        assert_eq!(
            state
                .handle(id)
                .unwrap()
                .status
                .selection
                .agents()
                .get("lux")
                .unwrap()
                .as_ref(),
            before.as_ref()
        );
        state.close_all().await;
    }
}
