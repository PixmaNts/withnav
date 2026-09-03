use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, ProviderError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningHistory {
    #[default]
    Preserve,
    Omit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
}

fn default_connect_timeout_ms() -> u64 {
    5000
}

fn default_request_timeout_ms() -> u64 {
    600_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub reasoning_history: ReasoningHistory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    pub providers: BTreeMap<String, ProviderConfig>,
    pub models: BTreeMap<String, ModelConfig>,
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub alias: String,
    pub provider_name: String,
    pub provider: ProviderConfig,
    pub model: ModelConfig,
}

impl ModelsConfig {
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        let cfg: ModelsConfig = serde_json::from_str(s)?;
        cfg.warn_unsupported();
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn resolve(&self, alias: &str) -> Result<ResolvedModel, ProviderError> {
        let model = self
            .models
            .get(alias)
            .ok_or_else(|| ProviderError::UnknownModelAlias(alias.to_string()))?;
        let provider = self
            .providers
            .get(&model.provider)
            .ok_or_else(|| ProviderError::UnknownModelAlias(alias.to_string()))?;
        Ok(ResolvedModel {
            alias: alias.to_string(),
            provider_name: model.provider.clone(),
            provider: provider.clone(),
            model: model.clone(),
        })
    }

    fn warn_unsupported(&self) {
        for (alias, m) in &self.models {
            if let Some(t) = &m.thinking {
                if !matches!(t.as_str(), "low" | "medium" | "high" | "off" | "none") {
                    tracing::warn!(alias, thinking = %t, "unsupported thinking value in models.json");
                }
            }
        }
    }
}

pub fn resolve_models_path(
    explicit: Option<&Path>,
    workspace: &Path,
    config_dir: &Path,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let ws = workspace.join("models.json");
    if ws.is_file() {
        return Some(ws);
    }
    let cfg = config_dir.join("models.json");
    if cfg.is_file() {
        return Some(cfg);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/models.json");

    #[test]
    fn resolves_fast_alias_to_model() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let r = cfg.resolve("fast").unwrap();
        assert_eq!(r.model.model, "opencode-go/deepseek-v4-flash");
        assert_eq!(r.provider_name, "omniroute");
        assert_eq!(r.provider.base_url, "http://localhost:20128/v1");
    }

    #[test]
    fn resolves_default_alias() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let r = cfg.resolve("default").unwrap();
        assert_eq!(r.model.model, "opencode-go/deepseek-v4-flash");
    }

    #[test]
    fn unknown_alias_errors_before_any_request() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let e = cfg.resolve("nonexistent").unwrap_err();
        assert!(matches!(e, ProviderError::UnknownModelAlias(a) if a == "nonexistent"));
    }

    #[test]
    fn reasoning_history_defaults_to_preserve() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let r = cfg.resolve("fast").unwrap();
        assert_eq!(r.model.reasoning_history, ReasoningHistory::Preserve);
    }

    #[test]
    fn api_key_env_is_a_name_not_a_secret() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let p = cfg.providers.get("omniroute").unwrap();
        assert_eq!(p.api_key_env.as_deref(), Some("OMNIROUTE_API_KEY"));
    }

    #[test]
    fn fixture_carries_no_secret_value() {
        assert!(
            !FIXTURE.contains("\"api_key\""),
            "models.json must not contain a secret-bearing api_key field"
        );
        assert!(
            !FIXTURE.contains("sk-"),
            "models.json must not contain an sk- style token"
        );
    }

    #[test]
    fn malformed_json_yields_config_error_not_panic() {
        let bad = "{ \"providers\": { }, \"models\": { }, }";
        let e = ModelsConfig::parse(bad).unwrap_err();
        assert!(matches!(e, ConfigError::ParseJson(_)));
    }

    #[test]
    fn repo_models_json_is_strict_and_resolves_both_aliases() {
        let repo_models = include_str!("../../models.json");
        let cfg = ModelsConfig::parse(repo_models).expect("repo models.json must be strict JSON");
        let fast = cfg.resolve("fast").unwrap();
        let default = cfg.resolve("default").unwrap();
        assert_eq!(fast.model.model, "opencode-go/deepseek-v4-flash");
        assert!(!default.model.model.is_empty());
        assert_eq!(fast.model.reasoning_history, ReasoningHistory::Preserve);
        assert!(!repo_models.contains("\"api_key\""));
        assert!(!repo_models.contains("sk-"));
        assert!(
            !repo_models
                .lines()
                .any(|l| l.trim_start().starts_with("//")),
            "strict JSON must not contain // comment lines"
        );
    }

    #[test]
    fn models_path_precedence_workspace_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let cfgd = tmp.path().join("cfg");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&cfgd).unwrap();
        std::fs::write(ws.join("models.json"), "{}").unwrap();
        std::fs::write(cfgd.join("models.json"), "{}").unwrap();

        let chosen = resolve_models_path(None, &ws, &cfgd).unwrap();
        assert_eq!(chosen, ws.join("models.json"));

        let explicit = tmp.path().join("custom.json");
        std::fs::write(&explicit, "{}").unwrap();
        let chosen2 = resolve_models_path(Some(&explicit), &ws, &cfgd).unwrap();
        assert_eq!(chosen2, explicit);
    }
}
