//! Pure assignment resolution; callers decide how to display diagnostics and apply fallbacks.
use crate::provider::config::{ApiKind, ModelsConfig, ResolvedModel, effective_variants};
use crate::provider::model_name::ModelName;
use crate::settings::{AssignmentVariant, ModelAssignment, Settings};

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
    pub variant_choice: AssignmentVariant,
    /// Discovered effort tiers require runtime catalog data. When true, choose low
    /// if the runtime offer contains it, otherwise retain the model default.
    pub prefer_low: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayerDisabled {
    #[error("layer '{layer}' disabled: model '{model}' has invalid variant '{variant}'")]
    InvalidVariant {
        layer: Layer,
        model: String,
        variant: String,
    },
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
    "agent '{agent}' has unusable model '{model}': {reason}; fall back to the default agent's model"
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
    #[error("default_agent '{name}' is not primary; valid agents: {}", valid_agents.join(", "))]
    NotPrimary {
        name: String,
        valid_agents: Vec<String>,
    },
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
    /// Settings edits reject unusable choices instead of installing runtime fallbacks.
    pub fn strict_assignment(
        &self,
        assignment: &ModelAssignment,
        api: ApiKind,
        offered: &[String],
    ) -> Result<(), String> {
        let model = self.resolve(assignment)?;
        if model.resolved.model.api != api {
            return Err(format!(
                "required API '{}', actual API '{}'",
                api_name(api),
                api_name(model.resolved.model.api)
            ));
        }
        if let AssignmentVariant::Named(name) = &assignment.variant
            && !effective_variants(&model.resolved, offered).contains_key(name)
        {
            return Err(format!(
                "model {} does not offer variant '{name}'",
                assignment.model
            ));
        }
        Ok(())
    }
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
                }));
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
        if layer == Layer::Metacognition && assignment.variant == AssignmentVariant::Default {
            if effective_variants(&assigned.resolved, &[]).contains_key("low") {
                assigned.variant = Some("low".into());
            } else {
                assigned.prefer_low = assigned.resolved.model.variants.is_none()
                    && assigned.resolved.provider.catalog_format.is_some();
            }
        }
        if let Some(variant) = invalid_variant(&assigned) {
            return Ok(LayerAssignment::Disabled(LayerDisabled::InvalidVariant {
                layer,
                model: assignment.model,
                variant,
            }));
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
                variant: AssignmentVariant::Default,
            })
        });
        let Some(assignment) = assignment else {
            return Ok(AgentModel::Unassigned);
        };
        let resolved = self.resolve(&assignment).and_then(|model| {
            if model.resolved.model.api != ApiKind::Chat {
                return Err(format!(
                    "required API 'chat', actual API '{}'",
                    api_name(model.resolved.model.api)
                ));
            }
            if let Some(variant) = invalid_variant(&model) {
                return Err(format!("invalid variant '{variant}'"));
            }
            Ok(model)
        });
        Ok(match resolved {
            Ok(model) => AgentModel::Assigned(Box::new(model)),
            Err(reason) => AgentModel::Fallback(AgentModelWarning {
                agent: agent_name.into(),
                model: assignment.model,
                reason,
            }),
        })
    }

    /// Each known agent supplies its name, optional header model and primary eligibility.
    pub fn default_agent_check(
        &self,
        known_agents: &[(&str, Option<&str>, bool)],
    ) -> Result<String, DefaultAgentError> {
        let name = self
            .settings
            .default_agent()
            .map_err(DefaultAgentError::InvalidSetting)?;
        let mut valid_agents: Vec<_> = known_agents
            .iter()
            .filter(|(_, _, primary)| *primary)
            .map(|(name, _, _)| (*name).to_owned())
            .collect();
        valid_agents.sort();
        valid_agents.dedup();
        let Some((_, definition, primary)) =
            known_agents.iter().find(|(agent, _, _)| *agent == name)
        else {
            return Err(DefaultAgentError::Unknown {
                name: name.into(),
                valid_agents,
            });
        };
        if !primary {
            return Err(DefaultAgentError::NotPrimary {
                name: name.into(),
                valid_agents,
            });
        }
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
        let variant = match &assignment.variant {
            AssignmentVariant::Default => resolved.model.default_variant.clone(),
            AssignmentVariant::Off => None,
            AssignmentVariant::Named(name) => Some(name.clone()),
        };
        Ok(AssignedModel {
            name,
            resolved,
            variant,
            variant_choice: assignment.variant.clone(),
            prefer_low: false,
        })
    }
}

fn invalid_variant(model: &AssignedModel) -> Option<String> {
    let variant = model.variant.as_ref()?;
    if model.resolved.model.variants.is_none() && model.resolved.provider.catalog_format.is_some() {
        return None;
    }
    (!effective_variants(&model.resolved, &[]).contains_key(variant)).then(|| variant.clone())
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
