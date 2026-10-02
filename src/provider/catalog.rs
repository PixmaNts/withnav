//! Lazy, metadata-only provider model catalog with a durable stale fallback.
use crate::{
    error::ProviderError,
    paths,
    provider::{
        client::ModelClient,
        config::{ApiKind, ResolvedModel},
        openai::ClientCache,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

const TTL: u64 = 86_400;
const MAX_ENTRIES: usize = 10_000;
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Limits {
    pub context_length: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    fetched_at: u64,
    models: HashMap<String, Limits>,
}
#[derive(Debug, Clone)]
pub struct CatalogData {
    pub limits: Option<Limits>,
    pub stale: bool,
    pub age_seconds: Option<u64>,
    pub error: Option<String>,
}
#[derive(Clone, Default, Debug)]
pub struct ModelCatalog {
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    data_dir: Option<PathBuf>,
}
impl ModelCatalog {
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        Self {
            locks: Arc::default(),
            data_dir,
        }
    }
    pub fn system() -> Self {
        Self::new(paths::data_dir())
    }
    fn file(&self, endpoint: &str) -> Option<PathBuf> {
        let key = format!(
            "{:x}",
            endpoint
                .bytes()
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64)
                    .wrapping_mul(0x100000001b3))
        );
        self.data_dir
            .as_ref()
            .map(|d| d.join("model-catalog").join(format!("{key}.json")))
    }
    /// What the cache file holds for this model, whatever its age; never
    /// touches the network. `stale` says the entry is past the TTL.
    pub fn cached(&self, resolved: &ResolvedModel) -> CatalogData {
        let endpoint = resolved.provider.base_url.trim_end_matches('/');
        let old = self.file(endpoint).as_deref().and_then(read_cache);
        let age = old.as_ref().map(|c| now().saturating_sub(c.fetched_at));
        CatalogData {
            limits: old
                .as_ref()
                .and_then(|c| c.models.get(&resolved.model.model).cloned()),
            stale: age.is_some_and(|a| a >= TTL),
            age_seconds: age,
            error: None,
        }
    }
    pub async fn lookup(
        &self,
        resolved: &ResolvedModel,
        key: &str,
        clients: &ClientCache,
    ) -> CatalogData {
        let endpoint = resolved.provider.base_url.trim_end_matches('/').to_string();
        let path = self.file(&endpoint);
        let old = path.as_deref().and_then(read_cache);
        let timestamp = now();
        if let Some(c) = old
            .as_ref()
            .filter(|c| c.version == 1 && timestamp.saturating_sub(c.fetched_at) < TTL)
        {
            return CatalogData {
                limits: c.models.get(&resolved.model.model).cloned(),
                stale: false,
                age_seconds: Some(timestamp.saturating_sub(c.fetched_at)),
                error: None,
            };
        }
        let lock = {
            let mut m = self.locks.lock().unwrap();
            m.entry(endpoint.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;
        let old = path.as_deref().and_then(read_cache);
        let timestamp = now();
        if let Some(c) = old
            .as_ref()
            .filter(|c| c.version == 1 && timestamp.saturating_sub(c.fetched_at) < TTL)
        {
            return CatalogData {
                limits: c.models.get(&resolved.model.model).cloned(),
                stale: false,
                age_seconds: Some(timestamp.saturating_sub(c.fetched_at)),
                error: None,
            };
        }
        let result = async {
            let client = ModelClient::new(
                &resolved.provider_name,
                &resolved.provider,
                key.into(),
                clients,
            )?;
            let bytes = client
                .get_json("models", Duration::from_secs(3), &CancellationToken::new())
                .await?;
            let models = parse_models(&bytes)?;
            Ok::<_, ProviderError>(CacheFile {
                version: 1,
                fetched_at: timestamp,
                models,
            })
        }
        .await;
        match result {
            Ok(c) => {
                if let Some(p) = path {
                    if let Some(parent) = p.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Ok(raw) = serde_json::to_vec(&c) {
                        let _ = std::fs::write(p, raw);
                    }
                }
                CatalogData {
                    limits: c.models.get(&resolved.model.model).cloned(),
                    stale: false,
                    age_seconds: Some(0),
                    error: None,
                }
            }
            Err(e) => CatalogData {
                limits: old
                    .as_ref()
                    .and_then(|c| c.models.get(&resolved.model.model).cloned()),
                stale: old.is_some(),
                age_seconds: old.as_ref().map(|c| timestamp.saturating_sub(c.fetched_at)),
                error: Some(e.to_string()),
            },
        }
    }
}
/// Parses an OpenAI-style `/models` body into limits by id. Zero, null and
/// missing limits are "not reported". An id listed twice with conflicting
/// values is ambiguous: it is dropped (and warned about) rather than guessed,
/// so the alias on it needs an explicit `context_window`; other ids are kept.
/// Duplicates that agree, or fill each other's gaps, are merged.
pub(crate) fn parse_models(bytes: &[u8]) -> Result<HashMap<String, Limits>, ProviderError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| ProviderError::MalformedResponse("invalid model catalog JSON".into()))?;
    let entries = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ProviderError::MalformedResponse("model catalog lacks data array".into()))?;
    if entries.len() > MAX_ENTRIES {
        return Err(ProviderError::MalformedResponse(
            "model catalog has too many entries".into(),
        ));
    }
    let mut models: HashMap<String, Limits> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    for e in entries {
        let Some(id) = e.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if ambiguous.contains(id) {
            continue;
        }
        let positive = |k: &str| e.get(k).and_then(|v| v.as_u64()).filter(|n| *n > 0);
        let incoming = Limits {
            context_length: positive("context_length"),
            max_input_tokens: positive("max_input_tokens"),
            max_output_tokens: positive("max_output_tokens"),
        };
        let Some(existing) = models.get_mut(id) else {
            models.insert(id.to_string(), incoming);
            continue;
        };
        let conflicts = |a: Option<u64>, b: Option<u64>| a.zip(b).is_some_and(|(x, y)| x != y);
        if conflicts(existing.context_length, incoming.context_length)
            || conflicts(existing.max_input_tokens, incoming.max_input_tokens)
            || conflicts(existing.max_output_tokens, incoming.max_output_tokens)
        {
            tracing::warn!(
                id,
                "model catalog lists this id twice with conflicting limits; ignoring it"
            );
            models.remove(id);
            ambiguous.insert(id.to_string());
            continue;
        }
        existing.context_length = existing.context_length.or(incoming.context_length);
        existing.max_input_tokens = existing.max_input_tokens.or(incoming.max_input_tokens);
        existing.max_output_tokens = existing.max_output_tokens.or(incoming.max_output_tokens);
    }
    Ok(models)
}
fn read_cache(path: &Path) -> Option<CacheFile> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The version of the saved [`ModelResolution`] JSON.
pub const RESOLUTION_VERSION: u32 = 1;

/// What a session resolved its model to, saved in `sessions.model_resolution`
/// so a resumed conversation keeps its bounds when discovery is down and is
/// never silently moved to another model. Carries no credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelResolution {
    pub version: u32,
    pub provider: String,
    pub model: String,
    pub api: String,
    /// The provider's base URL without credentials, query or fragment.
    pub endpoint: String,
    pub bounds: Bounds,
    pub history_policy: String,
    pub provenance: Provenance,
}
/// The effective bounds of a model, in tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub context_window: u64,
    pub max_input_tokens: Option<u64>,
    pub reported_output: Option<u64>,
}
/// Where each bound came from: `models.json`, a `discovered.*` field, or `none`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub context_window: String,
    pub max_input_tokens: String,
    pub reported_output: String,
}
impl ModelResolution {
    /// `provider 'p', model 'm', api chat`: the identity a retargeted alias
    /// is compared on.
    pub fn identity(&self) -> String {
        format!(
            "provider '{}', model '{}', api {}",
            self.provider, self.model, self.api
        )
    }
    /// Whether `other` is the same provider, model and api.
    pub fn same_identity(&self, other: &ModelResolution) -> bool {
        self.provider == other.provider && self.model == other.model && self.api == other.api
    }
    /// Parses the saved column. A newer version than this build knows is an
    /// error, never a guess.
    pub fn from_saved(raw: &str) -> Result<Self, String> {
        let saved: Self = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        if saved.version != RESOLUTION_VERSION {
            return Err(format!(
                "version {} is not supported (this build reads version {RESOLUTION_VERSION})",
                saved.version
            ));
        }
        Ok(saved)
    }
}
/// The api name saved in a resolution: `chat`, `fim` or `systemone`.
pub fn api_name(api: ApiKind) -> String {
    format!("{api:?}").to_lowercase()
}
/// A base URL without userinfo, query or fragment, so no credential reaches
/// the database.
fn endpoint_without_credentials(base_url: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(base_url.trim_end_matches('/')) else {
        return "(unparseable endpoint)".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.as_str().trim_end_matches('/').into()
}
/// An explicit `context_window` that differs from what the provider reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discrepancy {
    pub configured: u64,
    pub discovered: u64,
}
impl std::fmt::Display for Discrepancy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "models.json context_window {} differs from discovered context_length {}",
            self.configured, self.discovered
        )
    }
}
#[derive(Debug, Clone)]
pub struct ResolvedChat {
    pub window: u64,
    pub window_source: String,
    pub max_input_tokens: Option<u64>,
    pub reported_output: Option<u64>,
    pub discrepancy: Option<Discrepancy>,
    pub snapshot: ModelResolution,
}
/// A chat alias with no window in models.json and none reported by the
/// provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoContextWindow {
    pub alias: String,
    pub provider: String,
    pub model: String,
}
/// Resolves the context window of a model, in this order:
/// 1. `context_window` in models.json (authoritative; no network call, the
///    cache is only read to report a [`Discrepancy`]);
/// 2. the provider's reported `context_length`;
/// 3. the provider's reported `max_input_tokens` (labelled as a fallback);
/// 4. none: a chat alias is refused with [`NoContextWindow`].
///
/// FIM and SystemOne aliases never consult the catalog and get a placeholder
/// window of 1 (they have no conversation to budget).
pub async fn resolve_chat(
    catalog: &ModelCatalog,
    resolved: &ResolvedModel,
    key: &str,
    clients: &ClientCache,
) -> Result<ResolvedChat, NoContextWindow> {
    let is_chat = resolved.model.api == ApiKind::Chat;
    let explicit = resolved.model.context_window.filter(|n| *n > 0);
    let found = if !is_chat {
        None
    } else if explicit.is_some() {
        catalog.cached(resolved).limits
    } else {
        catalog.lookup(resolved, key, clients).await.limits
    };
    let limits = found.unwrap_or_default();
    let (window, source) = if let Some(n) = explicit {
        (n, "models.json")
    } else if let Some(n) = limits.context_length {
        (n, "discovered.context_length")
    } else if let Some(n) = limits.max_input_tokens {
        (n, "discovered.max_input_tokens (fallback)")
    } else if !is_chat {
        (1, "non-chat")
    } else {
        return Err(NoContextWindow {
            alias: resolved.alias.clone(),
            provider: resolved.provider_name.clone(),
            model: resolved.model.model.clone(),
        });
    };
    let discrepancy = explicit
        .zip(limits.context_length)
        .filter(|(a, b)| a != b)
        .map(|(configured, discovered)| Discrepancy {
            configured,
            discovered,
        });
    let max_input_tokens = resolved.model.max_input_tokens.or(limits.max_input_tokens);
    let snapshot = ModelResolution {
        version: RESOLUTION_VERSION,
        provider: resolved.provider_name.clone(),
        model: resolved.model.model.clone(),
        api: api_name(resolved.model.api),
        endpoint: endpoint_without_credentials(&resolved.provider.base_url),
        bounds: Bounds {
            context_window: window,
            max_input_tokens,
            reported_output: limits.max_output_tokens,
        },
        history_policy: format!("{:?}", resolved.model.reasoning_history).to_lowercase(),
        provenance: Provenance {
            context_window: source.into(),
            max_input_tokens: match (resolved.model.max_input_tokens, limits.max_input_tokens) {
                (Some(_), _) => "models.json",
                (None, Some(_)) => "discovered.max_input_tokens",
                (None, None) => "none",
            }
            .into(),
            reported_output: if limits.max_output_tokens.is_some() {
                "discovered.max_output_tokens"
            } else {
                "none"
            }
            .into(),
        },
    };
    Ok(ResolvedChat {
        window,
        window_source: source.into(),
        max_input_tokens,
        reported_output: limits.max_output_tokens,
        discrepancy,
        snapshot,
    })
}

#[cfg(test)]
mod tests;
