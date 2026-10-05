use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ConfigError, ProviderError};

/// Reject repeated catalog keys before a map can discard either declaration.
fn deserialize_unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct UniqueMap<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for UniqueMap<T> {
        type Value = BTreeMap<String, T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map with unique keys")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> Result<Self::Value, A::Error> {
            let mut map = BTreeMap::new();
            while let Some(key) = access.next_key::<String>()? {
                if map.contains_key(&key) {
                    // Raw keys have not been validated and may contain secrets.
                    return Err(serde::de::Error::custom("duplicate catalog map key"));
                }
                map.insert(key, access.next_value()?);
            }
            Ok(map)
        }
    }

    deserializer.deserialize_map(UniqueMap(PhantomData))
}

fn deserialize_optional_variants<'de, D>(deserializer: D) -> Result<Option<Variants>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct UniqueVariants(#[serde(deserialize_with = "deserialize_unique_map")] Variants);

    Option::<UniqueVariants>::deserialize(deserializer).map(|variants| variants.map(|v| v.0))
}

/// One variant: a JSON object merged shallowly into the chat request body.
/// Only the keys the request owns are refused ([`RESERVED_VARIANT_KEYS`]);
/// any other key is allowed on purpose, including ones the request does not
/// otherwise set (such as `response_format`), so a variant can carry
/// provider-specific options.
pub type Variant = serde_json::Map<String, Value>;
/// The variants of one alias, by name.
pub type Variants = BTreeMap<String, Variant>;

/// Keys the request itself owns. A variant cannot set them: it never changes
/// the model, the conversation, the tools or the output limits.
pub const RESERVED_VARIANT_KEYS: &[&str] = &[
    "model",
    "messages",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "functions",
    "function_call",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "n",
    "temperature",
];

/// The format of a provider's `GET /models` list, when it is richer than the
/// plain OpenAI one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogFormat {
    /// OmniRoute: models carry `capabilities.effort_tiers`.
    Omniroute,
}

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
    /// `"omniroute"`: the model list reports effort tiers, from which an alias
    /// that omits `variants` gets one variant per tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_format: Option<CatalogFormat>,
}

fn default_connect_timeout_ms() -> u64 {
    5000
}

fn default_request_timeout_ms() -> u64 {
    600_000
}

/// Which wire API a model speaks. `chat` is the OpenAI-style streaming
/// endpoint existing models use; `fim` and `systemone` are the
/// fill-in-the-middle and typed-decision endpoints used by the reflex and
/// instinct layers of the navigator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
    #[default]
    Chat,
    Fim,
    SystemOne,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub api: ApiKind,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_parallel_tool_calls: Option<bool>,
    /// Deprecated and ignored: write `default_variant` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// The variant a new conversation on this alias starts with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_variant: Option<String>,
    /// Named request overrides. Omitted: generated from the model's effort
    /// tiers when the provider's `catalog_format` is `omniroute`. `{}`: none.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_variants"
    )]
    pub variants: Option<Variants>,
    #[serde(default)]
    pub reasoning_history: ReasoningHistory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    #[serde(deserialize_with = "deserialize_unique_map")]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(deserialize_with = "deserialize_unique_map")]
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
    /// Resolves a canonical gateway/upstream catalog identity.
    pub fn resolve_name(
        &self,
        name: &super::model_name::ModelName,
    ) -> Result<ResolvedModel, ProviderError> {
        let keys: Vec<_> = self
            .models
            .iter()
            .filter(|(_, model)| model.provider == name.gateway() && model.model == name.id())
            .map(|(key, _)| key.clone())
            .collect();
        if keys.len() > 1 {
            return Err(ProviderError::AmbiguousModel {
                model: name.to_string(),
                keys,
            });
        }
        let key = keys
            .first()
            .ok_or_else(|| ProviderError::UnknownModel(name.to_string()))?;
        if !self.providers.contains_key(name.gateway()) {
            return Err(ProviderError::UnknownModel(name.to_string()));
        }
        self.resolve(key)
    }

    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        let cfg: ModelsConfig = serde_json::from_str(s)?;
        for (key, model) in &cfg.models {
            let name = super::model_name::ModelName::parse(key, &cfg).map_err(|_| {
                ConfigError::Invalid(
                    "catalog keys must be declared gateway/upstream identities".into(),
                )
            })?;
            if name.gateway() != model.provider || name.id() != model.model {
                return Err(ConfigError::Invalid(
                    "catalog key disagrees with gateway/upstream identity".into(),
                ));
            }
        }
        cfg.validate_variants()?;
        cfg.warn_deprecated();
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn resolve(&self, alias: &str) -> Result<ResolvedModel, ProviderError> {
        super::model_name::ModelName::parse(alias, self)
            .map_err(|_| ProviderError::UnknownModel(alias.into()))?;
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

    /// Rejects, naming the alias, variant and key: a variant that sets a key
    /// the request owns, an unusable variant name, and a `default_variant`
    /// that no variant of the alias can satisfy.
    fn validate_variants(&self) -> Result<(), ConfigError> {
        for (alias, m) in &self.models {
            for (name, variant) in m.variants.iter().flatten() {
                if name.is_empty() || name.len() > 64 || name.chars().any(char::is_whitespace) {
                    return Err(ConfigError::Invalid(format!(
                        "model '{alias}': variant name {name:?} must be 1-64 characters without whitespace"
                    )));
                }
                if name == crate::session::CLEAR_WORD {
                    return Err(ConfigError::Invalid(format!(
                        "model '{alias}': a variant cannot be named '{name}': that word clears the variant"
                    )));
                }
                if let Some(key) = RESERVED_VARIANT_KEYS
                    .iter()
                    .find(|k| variant.contains_key(**k))
                {
                    return Err(ConfigError::Invalid(format!(
                        "model '{alias}': variant '{name}' sets '{key}', which the request owns \
                         (reserved: {})",
                        RESERVED_VARIANT_KEYS.join(", ")
                    )));
                }
            }
            let Some(default) = m.default_variant.as_deref() else {
                continue;
            };
            let declared = match &m.variants {
                Some(map) => map.contains_key(default),
                // Generated from the provider's catalog: checked when the
                // tiers are known.
                None => self
                    .providers
                    .get(&m.provider)
                    .is_some_and(|p| p.catalog_format.is_some()),
            };
            if !declared {
                return Err(ConfigError::Invalid(format!(
                    "model '{alias}': default_variant '{default}' names no variant of this alias"
                )));
            }
        }
        Ok(())
    }

    /// One message per deprecated or unsupported setting.
    pub fn deprecations(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (alias, m) in &self.models {
            let Some(t) = &m.thinking else { continue };
            out.push(match t.as_str() {
                "off" | "none" => format!(
                    "model '{alias}': `thinking` is deprecated and ignored; remove it \
                     (no `default_variant` means no variant)"
                ),
                "low" | "medium" | "high" => format!(
                    "model '{alias}': `thinking` is deprecated and ignored; write \
                     `\"default_variant\": \"{t}\"` instead"
                ),
                _ => format!(
                    "model '{alias}': `thinking` ({t:?}) is deprecated and ignored; write \
                     `default_variant` naming one of the alias's variants instead"
                ),
            });
        }
        out
    }

    fn warn_deprecated(&self) {
        for message in self.deprecations() {
            tracing::warn!("{message}");
        }
    }
}

/// The variants an alias offers: its own map when it has one (`{}` is none),
/// else, for a provider with a `catalog_format`, one
/// `"<tier>": {"reasoning_effort": "<tier>"}` per effort tier the catalog
/// reports for the model. Generation never picks a default.
pub fn effective_variants(resolved: &ResolvedModel, effort_tiers: &[String]) -> Variants {
    if let Some(own) = &resolved.model.variants {
        return own.clone();
    }
    if resolved.provider.catalog_format.is_none() {
        return Variants::new();
    }
    effort_tiers
        .iter()
        .map(|tier| {
            let mut v = Variant::new();
            v.insert("reasoning_effort".into(), Value::String(tier.clone()));
            (tier.clone(), v)
        })
        .collect()
}

/// A `default_variant` that the alias's variants do not contain (a generated
/// tier the catalog no longer reports). See `ResolvedChat::default_override`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownVariant {
    pub alias: String,
    pub variant: String,
}

impl std::fmt::Display for UnknownVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "model '{}' has no variant '{}'",
            self.alias, self.variant
        )
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

/// Finds `models.json` (explicit path, then workspace, then config dir) and
/// loads it. Returns the config with the path it came from; no file found is
/// a `NotFound` read error.
pub fn load_models(
    explicit: Option<&Path>,
    workspace: &Path,
    config_dir: &Path,
) -> Result<(ModelsConfig, PathBuf), ConfigError> {
    let path = resolve_models_path(explicit, workspace, config_dir).ok_or_else(|| {
        ConfigError::Read(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no models.json found (explicit, workspace, or config dir)",
        ))
    })?;
    let config = ModelsConfig::load(&path)?;
    Ok((config, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/models.json");

    #[test]
    fn catalog_rejects_duplicate_model_keys_even_with_identical_metadata() {
        let first = r#"{"provider":"p","model":"m","context_window":1000,"supports_tools":true}"#;
        for second in [
            first,
            r#"{"provider":"p","model":"m","context_window":2000,"supports_tools":false}"#,
        ] {
            // Raw JSON is essential: a Value/map fixture would lose duplicates.
            let raw = format!(
                r#"{{"providers":{{"p":{{"base_url":"http://unused"}}}},"models":{{"p/m":{first},"p/m":{second}}}}}"#
            );
            let error = ModelsConfig::parse(&raw).unwrap_err();
            assert!(matches!(error, ConfigError::ParseJson(_)));
            assert!(error.to_string().contains("duplicate catalog map key"));
            assert!(serde_json::from_str::<ModelsConfig>(&raw).is_err());
        }
    }

    #[test]
    fn catalog_rejects_duplicate_gateway_and_variant_keys() {
        let raw = r#"{"providers":{"p":{"base_url":"http://first"},"p":{"base_url":"http://second"}},"models":{}}"#;
        assert!(ModelsConfig::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("duplicate catalog map key"));
        for second in ["high", "low"] {
            let raw = config_with(
                "",
                &format!(
                    r#", "variants": {{"high": {{"reasoning_effort": "high"}}, "high": {{"reasoning_effort": "{second}"}}}}"#
                ),
            );
            assert!(ModelsConfig::parse(&raw)
                .unwrap_err()
                .to_string()
                .contains("duplicate catalog map key"));
        }
    }

    #[test]
    fn duplicate_key_errors_do_not_echo_unvalidated_keys_or_values() {
        for raw in [
            r#"{"providers":{"secret-gateway":{"base_url":"http://secret-first"},"secret-gateway":{"base_url":"http://secret-second"}},"models":{}}"#.to_string(),
            r#"{"providers":{},"models":{"secret-model":{"provider":"p","model":"m"},"secret-model":{"provider":"p","model":"m"}}}"#.to_string(),
            config_with("", r#", "variants":{"secret-variant":{"reasoning_effort":"secret-first"},"secret-variant":{"reasoning_effort":"secret-second"}}"#),
        ] {
            let error = ModelsConfig::parse(&raw).unwrap_err().to_string();
            assert!(error.contains("duplicate catalog map key"), "{error}");
            assert!(!error.contains("secret"), "{error}");
        }
    }

    #[test]
    fn catalog_rejects_duplicate_decoded_keys() {
        let raw = r#"{"providers":{"p":{"base_url":"http://unused"}},"models":{"p/m":{"provider":"p","model":"m"},"p\/\u006d":{"provider":"p","model":"m"}}}"#;
        assert!(ModelsConfig::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("duplicate catalog map key"));
    }

    #[test]
    fn unique_nested_catalog_round_trips_without_changing_fields() {
        let raw = r#"{"providers":{"p":{"base_url":"http://unused","catalog_format":"omniroute"}},"models":{"p/upstream/m":{"provider":"p","model":"upstream/m","context_window":1000,"max_input_tokens":800,"max_output_tokens":200,"supports_tools":true,"supports_parallel_tool_calls":false,"default_variant":"high","variants":{"high":{"reasoning_effort":"high"},"think":{"thinking":{"type":"enabled","budget_tokens":50}}}},"p/other/m":{"provider":"p","model":"other/m","variants":null}}}"#;
        let config = ModelsConfig::parse(raw).unwrap();
        let model = config.resolve("p/upstream/m").unwrap().model;
        assert_eq!(model.context_window, Some(1000));
        assert_eq!(model.max_input_tokens, Some(800));
        assert_eq!(model.max_output_tokens, Some(200));
        assert!(model.supports_tools);
        assert_eq!(model.supports_parallel_tool_calls, Some(false));
        assert_eq!(model.default_variant.as_deref(), Some("high"));
        assert_eq!(
            model.variants.unwrap()["think"]["thinking"]["budget_tokens"],
            50
        );
        assert!(config.models["p/other/m"].variants.is_none());
        let serialized = serde_json::to_string(&config).unwrap();
        let reparsed = ModelsConfig::parse(&serialized).unwrap();
        assert_eq!(serialized, serde_json::to_string(&reparsed).unwrap());
    }

    #[test]
    fn catalog_rejects_legacy_keys_and_mismatched_canonical_identity() {
        for key in ["default", "p/other", "unknown/m"] {
            let raw = serde_json::json!({"providers":{"p":{"base_url":"http://unused"}},"models":{key:{"provider":"p","model":"m"}}});
            assert!(ModelsConfig::parse(&raw.to_string()).is_err());
        }
        let config = ModelsConfig::parse(&serde_json::json!({"providers":{"p":{"base_url":"http://unused"}},"models":{"p/m":{"provider":"p","model":"m"}}}).to_string()).unwrap();
        assert!(config.resolve("default").is_err());
        assert_eq!(config.resolve("p/m").unwrap().model.model, "m");
    }

    #[test]
    fn resolves_fast_alias_to_model() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let r = cfg
            .resolve("omniroute/opencode-go/deepseek-v4-flash")
            .unwrap();
        assert_eq!(r.model.model, "opencode-go/deepseek-v4-flash");
        assert_eq!(r.provider_name, "omniroute");
        assert_eq!(r.provider.base_url, "http://localhost:20128/v1");
    }

    #[test]
    fn resolves_default_alias() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let r = cfg
            .resolve("omniroute/opencode-go/deepseek-v4-flash")
            .unwrap();
        assert_eq!(r.model.model, "opencode-go/deepseek-v4-flash");
    }

    #[test]
    fn unknown_alias_errors_before_any_request() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let e = cfg.resolve("nonexistent").unwrap_err();
        assert!(matches!(e, ProviderError::UnknownModel(a) if a == "nonexistent"));
    }

    #[test]
    fn reasoning_history_defaults_to_preserve() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let r = cfg
            .resolve("omniroute/opencode-go/deepseek-v4-flash")
            .unwrap();
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
    fn api_kind_defaults_to_chat() {
        let cfg = ModelsConfig::parse(FIXTURE).unwrap();
        let r = cfg
            .resolve("omniroute/opencode-go/deepseek-v4-flash")
            .unwrap();
        assert_eq!(r.model.api, ApiKind::Chat);
    }

    #[test]
    fn api_kind_parses_fim_and_systemone() {
        let raw = serde_json::json!({
            "providers": { "p": { "base_url": "http://x/v1" } },
            "models": {
                "p/fill": { "provider": "p", "model": "fill", "api": "fim" },
                "p/decision": { "provider": "p", "model": "decision", "api": "systemone" }
            }
        })
        .to_string();
        let cfg = ModelsConfig::parse(&raw).unwrap();
        assert_eq!(cfg.resolve("p/fill").unwrap().model.api, ApiKind::Fim);
        assert_eq!(
            cfg.resolve("p/decision").unwrap().model.api,
            ApiKind::SystemOne
        );
    }

    #[test]
    fn unknown_api_kind_is_rejected() {
        let raw = serde_json::json!({
            "providers": { "p": { "base_url": "http://x/v1" } },
            "models": { "r": { "provider": "p", "model": "m", "api": "weird" } }
        })
        .to_string();
        assert!(ModelsConfig::parse(&raw).is_err());
    }

    #[test]
    fn repo_models_json_is_strict_and_resolves_both_aliases() {
        let repo_models = include_str!("../../models.example.json");
        let cfg = ModelsConfig::parse(repo_models).expect("repo models.json must be strict JSON");
        let fast = cfg.resolve("omniroute/combo/deepseek-v4-flash").unwrap();
        let default = cfg.resolve("omniroute/cc/claude-opus-5-5").unwrap();
        assert_eq!(fast.model.model, "combo/deepseek-v4-flash");
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

    #[test]
    fn load_models_prefers_explicit_then_workspace_then_config_dir() {
        let ws = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let explicit = ws.path().join("explicit.json");
        std::fs::write(&explicit, FIXTURE).unwrap();
        std::fs::write(ws.path().join("models.json"), FIXTURE).unwrap();
        std::fs::write(cfg.path().join("models.json"), FIXTURE).unwrap();

        let (_, path) = load_models(Some(&explicit), ws.path(), cfg.path()).unwrap();
        assert_eq!(path, explicit);
        let (config, path) = load_models(None, ws.path(), cfg.path()).unwrap();
        assert_eq!(path, ws.path().join("models.json"));
        assert!(config
            .resolve("omniroute/opencode-go/deepseek-v4-flash")
            .is_ok());
        std::fs::remove_file(ws.path().join("models.json")).unwrap();
        let (_, path) = load_models(None, ws.path(), cfg.path()).unwrap();
        assert_eq!(path, cfg.path().join("models.json"));
    }

    #[test]
    fn load_models_without_any_file_is_a_not_found_error() {
        let ws = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        match load_models(None, ws.path(), cfg.path()) {
            Err(ConfigError::Read(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected a NotFound read error, got {other:?}"),
        }
    }

    fn config_with(provider_extra: &str, model_extra: &str) -> String {
        format!(
            r#"{{"providers": {{"p": {{"base_url": "http://x/v1"{provider_extra}}}}},
               "models": {{"p/mm": {{"provider": "p", "model": "mm"{model_extra}}}}}}}"#
        )
    }

    fn tiers(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn variants_and_default_parse() {
        let raw = config_with(
            "",
            r#", "default_variant": "high",
                 "variants": {"high": {"reasoning_effort": "high"},
                              "think": {"thinking": {"type": "enabled", "budget_tokens": 8000}}}"#,
        );
        let cfg = ModelsConfig::parse(&raw).unwrap();
        let m = &cfg.models["p/mm"];
        assert_eq!(m.default_variant.as_deref(), Some("high"));
        let variants = m.variants.as_ref().unwrap();
        assert_eq!(variants["high"]["reasoning_effort"], "high");
        assert_eq!(variants["think"]["thinking"]["budget_tokens"], 8000);
    }

    #[test]
    fn a_variant_may_not_set_a_key_the_request_owns() {
        for key in RESERVED_VARIANT_KEYS {
            let raw = config_with("", &format!(r#", "variants": {{"v": {{"{key}": 1}}}}"#));
            let err = ModelsConfig::parse(&raw).unwrap_err().to_string();
            assert!(
                err.contains("'p/mm'") && err.contains("'v'") && err.contains(&format!("'{key}'")),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn a_default_variant_naming_no_variant_is_a_config_error() {
        let raw = config_with(
            "",
            r#", "default_variant": "max", "variants": {"high": {"reasoning_effort": "high"}}"#,
        );
        let err = ModelsConfig::parse(&raw).unwrap_err().to_string();
        assert!(err.contains("'p/mm'") && err.contains("'max'"), "{err}");
        // `{}` offers nothing to default to; with no catalog_format nothing is generated.
        let raw = config_with("", r#", "default_variant": "high", "variants": {}"#);
        assert!(ModelsConfig::parse(&raw).is_err());
        let raw = config_with("", r#", "default_variant": "high""#);
        assert!(ModelsConfig::parse(&raw).is_err());
    }

    #[test]
    fn a_default_variant_may_name_a_generated_tier() {
        let raw = config_with(
            r#", "catalog_format": "omniroute""#,
            r#", "default_variant": "high""#,
        );
        assert!(ModelsConfig::parse(&raw).is_ok());
    }

    #[test]
    fn an_unusable_variant_name_is_rejected() {
        for name in ["", "two words", "off"] {
            let raw = config_with("", &format!(r#", "variants": {{"{name}": {{}}}}"#));
            assert!(ModelsConfig::parse(&raw).is_err(), "{name:?}");
        }
    }

    #[test]
    fn an_unknown_catalog_format_is_rejected() {
        assert!(ModelsConfig::parse(&config_with(r#", "catalog_format": "x""#, "")).is_err());
    }

    fn resolve_in(provider_extra: &str, model_extra: &str) -> ResolvedModel {
        ModelsConfig::parse(&config_with(provider_extra, model_extra))
            .unwrap()
            .resolve("p/mm")
            .unwrap()
    }

    #[test]
    fn opus_tiers_generate_one_variant_each() {
        let r = resolve_in(r#", "catalog_format": "omniroute""#, "");
        let v = effective_variants(&r, &tiers(&["low", "medium", "high", "xhigh", "max"]));
        assert_eq!(
            v.keys().map(String::as_str).collect::<Vec<_>>(),
            ["high", "low", "max", "medium", "xhigh"]
        );
        assert_eq!(v["xhigh"]["reasoning_effort"], "xhigh");
        assert_eq!(v["xhigh"].len(), 1);
    }

    #[test]
    fn deepseek_tiers_include_none() {
        let r = resolve_in(r#", "catalog_format": "omniroute""#, "");
        let v = effective_variants(&r, &tiers(&["none", "low", "high", "max"]));
        assert_eq!(v.len(), 4);
        assert_eq!(v["none"]["reasoning_effort"], "none");
    }

    #[test]
    fn an_explicit_map_wins_over_the_catalog() {
        let r = resolve_in(
            r#", "catalog_format": "omniroute""#,
            r#", "variants": {"deep": {"reasoning_effort": "max"}}"#,
        );
        let v = effective_variants(&r, &tiers(&["low", "high"]));
        assert_eq!(v.keys().collect::<Vec<_>>(), ["deep"]);
    }

    #[test]
    fn an_empty_map_means_no_variants() {
        let r = resolve_in(r#", "catalog_format": "omniroute""#, r#", "variants": {}"#);
        assert!(effective_variants(&r, &tiers(&["low", "high"])).is_empty());
    }

    #[test]
    fn without_a_catalog_format_nothing_is_generated() {
        let r = resolve_in("", "");
        assert!(effective_variants(&r, &tiers(&["low", "high"])).is_empty());
    }

    #[test]
    fn generation_never_picks_a_default() {
        let r = resolve_in(r#", "catalog_format": "omniroute""#, "");
        assert_eq!(r.model.default_variant, None);
        assert!(effective_variants(&r, &tiers(&["low", "high"])).len() == 2);
    }

    #[test]
    fn thinking_is_inert_and_names_the_variant_to_write() {
        let cfg = ModelsConfig::parse(&config_with("", r#", "thinking": "high""#)).unwrap();
        let m = &cfg.models["p/mm"];
        assert_eq!(m.default_variant, None);
        assert_eq!(m.variants, None);
        let notes = cfg.deprecations();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("deprecated")
                && notes[0].contains(r#""default_variant": "high""#)
                && notes[0].contains("'p/mm'"),
            "{}",
            notes[0]
        );
        let off = ModelsConfig::parse(&config_with("", r#", "thinking": "off""#)).unwrap();
        assert!(off.deprecations()[0].contains("remove it"));
        let none = ModelsConfig::parse(&config_with("", "")).unwrap();
        assert!(none.deprecations().is_empty());
    }

    #[test]
    fn repo_models_json_has_no_deprecated_setting() {
        let cfg = ModelsConfig::parse(include_str!("../../models.example.json")).unwrap();
        assert!(cfg.deprecations().is_empty(), "{:?}", cfg.deprecations());
    }
}
