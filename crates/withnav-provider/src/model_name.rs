//! Catalog identity: one declared gateway followed by its verbatim upstream id.
use super::config::ModelsConfig;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelName(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelNameError {
    #[error("model name '{0}' must be <gateway>/<id> with nonempty segments and no whitespace")]
    Invalid(String),
    #[error("model name '{name}' uses unknown gateway '{gateway}' (not declared in models.json)")]
    UnknownGateway { name: String, gateway: String },
}

impl ModelName {
    pub fn parse(name: &str, catalog: &ModelsConfig) -> Result<Self, ModelNameError> {
        let Some((gateway, _)) = name.split_once('/') else {
            return Err(ModelNameError::Invalid(name.into()));
        };
        if name.split('/').any(str::is_empty)
            || name.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(ModelNameError::Invalid(name.into()));
        }
        if !catalog.providers.contains_key(gateway) {
            return Err(ModelNameError::UnknownGateway {
                name: name.into(),
                gateway: gateway.into(),
            });
        }
        Ok(Self(name.into()))
    }

    pub fn gateway(&self) -> &str {
        self.0.split_once('/').expect("validated model name").0
    }

    pub fn id(&self) -> &str {
        self.0.split_once('/').expect("validated model name").1
    }

    pub fn provider_group(&self) -> &str {
        self.id()
            .split_once('/')
            .map_or(self.gateway(), |(group, _)| group)
    }
}

impl std::fmt::Display for ModelName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
