//! Pure assignment resolution; callers decide how to display diagnostics and apply fallbacks.
use super::config::{effective_variants, ApiKind, ModelsConfig, ResolvedModel};
use super::model_name::ModelName;
use crate::settings::{ModelAssignment, Settings};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Reflex,
    Instinct,
    Metacognition,
}

impl Layer {
    pub fn config_key(self) -> &'static str {
        match self {
            Self::Reflex => "reflex",
            Self::Instinct => "instinct",
            Self::Metacognition => "metacognition",
        }
    }
    pub fn required_api(self) -> ApiKind {
        match self {
            Self::Reflex => ApiKind::Fim,
            Self::Instinct => ApiKind::SystemOne,
            Self::Metacognition => ApiKind::Chat,
        }
    }
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.config_key())
    }
}

#[derive(Debug, Clone)]
pub struct AssignedModel {
    pub name: ModelName,
    pub resolved: ResolvedModel,
    pub variant: Option<String>,
    /// Discovered effort tiers require runtime catalog data. When true, choose low
    /// if the runtime offer contains it, otherwise retain the model default.
    pub prefer_low: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayerDisabled {
    #[error("layer '{layer}' disabled: layers.{layer} is missing")]
    Missing { layer: Layer },
    #[error("layer '{layer}' disabled: unknown model '{model}': {reason}")]
    UnknownModel {
        layer: Layer,
        model: String,
        reason: String,
    },
    #[error(
        "layer '{layer}' disabled: model '{model}' must use the '{expected}' API, not '{actual}'"
    )]
    WrongApi {
        layer: Layer,
        model: String,
        expected: &'static str,
        actual: &'static str,
    },
}

#[derive(Debug, Clone)]
pub enum LayerAssignment {
    Enabled(Box<AssignedModel>),
    Disabled(LayerDisabled),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "agent '{agent}' has unknown model '{model}': {reason}; fall back to the default agent's model"
)]
pub struct AgentModelWarning {
    pub agent: String,
    pub model: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum AgentModel {
    Assigned(Box<AssignedModel>),
    Unassigned,
    Fallback(AgentModelWarning),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefaultAgentError {
    #[error("invalid default_agent: {0}")]
    InvalidSetting(String),
    #[error("default_agent '{name}' is unknown; valid agents: {}", valid_agents.join(", "))]
    Unknown {
        name: String,
        valid_agents: Vec<String>,
    },
    #[error("default_agent '{name}' has no usable model: {reason}")]
    NoUsableModel { name: String, reason: String },
}

pub struct AssignmentResolver<'a> {
    catalog: &'a ModelsConfig,
    settings: &'a Settings,
}

impl<'a> AssignmentResolver<'a> {
    pub fn new(catalog: &'a ModelsConfig, settings: &'a Settings) -> Self {
        Self { catalog, settings }
    }

    pub fn layer_assignment(&self, layer: Layer) -> Result<LayerAssignment, String> {
        let Some(assignment) = self.settings.layer_assignment(layer)? else {
            return Ok(LayerAssignment::Disabled(LayerDisabled::Missing { layer }));
        };
        let mut assigned = match self.resolve(&assignment) {
            Ok(model) => model,
            Err(reason) => {
                return Ok(LayerAssignment::Disabled(LayerDisabled::UnknownModel {
                    layer,
                    model: assignment.model,
                    reason,
                }))
            }
        };
        if assigned.resolved.model.api != layer.required_api() {
            return Ok(LayerAssignment::Disabled(LayerDisabled::WrongApi {
                layer,
                model: assignment.model,
                expected: api_name(layer.required_api()),
                actual: api_name(assigned.resolved.model.api),
            }));
        }
        if layer == Layer::Metacognition && assignment.variant.is_none() {
            if effective_variants(&assigned.resolved, &[]).contains_key("low") {
                assigned.variant = Some("low".into());
            } else {
                assigned.prefer_low = assigned.resolved.model.variants.is_none()
                    && assigned.resolved.provider.catalog_format.is_some();
            }
        }
        Ok(LayerAssignment::Enabled(Box::new(assigned)))
    }

    pub fn agent_model(
        &self,
        agent_name: &str,
        agent_definition_model: Option<&str>,
    ) -> Result<AgentModel, String> {
        let assignment = self.settings.agent_assignment(agent_name)?.or_else(|| {
            agent_definition_model.map(|model| ModelAssignment {
                model: model.into(),
                variant: None,
            })
        });
        let Some(assignment) = assignment else {
            return Ok(AgentModel::Unassigned);
        };
        Ok(match self.resolve(&assignment) {
            Ok(model) => AgentModel::Assigned(Box::new(model)),
            Err(reason) => AgentModel::Fallback(AgentModelWarning {
                agent: agent_name.into(),
                model: assignment.model,
                reason,
            }),
        })
    }

    /// Each known agent supplies its name and optional definition-header model.
    pub fn default_agent_check(
        &self,
        known_agents: &[(&str, Option<&str>)],
    ) -> Result<String, DefaultAgentError> {
        let name = self
            .settings
            .default_agent()
            .map_err(DefaultAgentError::InvalidSetting)?;
        let Some((_, definition)) = known_agents.iter().find(|(agent, _)| *agent == name) else {
            let mut valid_agents: Vec<_> = known_agents
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect();
            valid_agents.sort();
            valid_agents.dedup();
            return Err(DefaultAgentError::Unknown {
                name: name.into(),
                valid_agents,
            });
        };
        let outcome = self
            .agent_model(name, *definition)
            .map_err(DefaultAgentError::InvalidSetting)?;
        let reason = match outcome {
            AgentModel::Assigned(_) => return Ok(name.into()),
            AgentModel::Unassigned => "no assigned model".into(),
            AgentModel::Fallback(warning) => warning.to_string(),
        };
        Err(DefaultAgentError::NoUsableModel {
            name: name.into(),
            reason,
        })
    }

    fn resolve(&self, assignment: &ModelAssignment) -> Result<AssignedModel, String> {
        let name = ModelName::parse(&assignment.model, self.catalog).map_err(|e| e.to_string())?;
        let resolved = self
            .catalog
            .resolve_name(&name)
            .map_err(|e| e.to_string())?;
        let variant = assignment
            .variant
            .clone()
            .or_else(|| resolved.model.default_variant.clone());
        Ok(AssignedModel {
            name,
            resolved,
            variant,
            prefer_low: false,
        })
    }
}

fn api_name(api: ApiKind) -> &'static str {
    match api {
        ApiKind::Chat => "chat",
        ApiKind::Fim => "fim",
        ApiKind::SystemOne => "systemone",
    }
}

#[cfg(test)]
mod tests;
