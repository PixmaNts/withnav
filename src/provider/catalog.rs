//! Lazy, metadata-only provider model catalog with a durable stale fallback.
use crate::{
    error::ProviderError,
    paths,
    provider::{
        client::ModelClient,
        config::{effective_variants, ApiKind, ResolvedModel, UnknownVariant, Variant, Variants},
        openai::ClientCache,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

const TTL: u64 = 86_400;
/// Version 2 added the effort tiers; a version 1 file is read as absent and
/// refetched.
const CACHE_VERSION: u32 = 2;
const MAX_ENTRIES: usize = 10_000;
const FETCH_DEADLINE: Duration = Duration::from_secs(3);
/// After a failed refresh the endpoint is not asked again for this long, so
/// callers queued behind the failure do not each repeat it.
const FAILURE_BACKOFF: Duration = Duration::from_secs(5);
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Limits {
    pub context_length: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// The reasoning-effort tiers the provider advertises (`low`, `high`, ...;
    /// `none` included when reported). Empty: none reported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effort_tiers: Vec<String>,
}
impl Limits {
    /// Every reported limit is positive.
    fn is_sane(&self) -> bool {
        [
            self.context_length,
            self.max_input_tokens,
            self.max_output_tokens,
        ]
        .into_iter()
        .flatten()
        .all(|n| n > 0)
    }
}
/// What is written to `<data_dir>/model-catalog/<endpoint hash>.json`:
/// limits by model id and when they were fetched. Never a key or a header.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    fetched_at: u64,
    models: HashMap<String, Limits>,
    /// For a model whose latest refresh could not renew every limit and kept
    /// an earlier value: when the oldest kept value was fetched. Absent means
    /// everything was renewed at `fetched_at`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    carried: HashMap<String, u64>,
}
impl CacheFile {
    /// Seconds since the values of `model` were last confirmed by the provider.
    fn age_of(&self, model: &str) -> u64 {
        let since = self.carried.get(model).copied().unwrap_or(self.fetched_at);
        now().saturating_sub(since.min(self.fetched_at))
    }
}
#[derive(Debug, Clone)]
pub struct CatalogData {
    pub limits: Option<Limits>,
    /// The values are past the 24 h TTL (a refresh failed or has not run).
    pub stale: bool,
    pub age_seconds: Option<u64>,
    /// Why the last refresh failed: short, no secrets.
    pub error: Option<String>,
}
/// One endpoint: the lock that makes its refresh single-flight, and what
/// this process knows beyond the file (it is all there is when the cache
/// directory cannot be written).
#[derive(Debug, Default)]
struct Endpoint {
    flight: tokio::sync::Mutex<()>,
    mem: Mutex<Mem>,
}
impl Endpoint {
    fn error(&self) -> Option<String> {
        self.mem
            .lock()
            .expect("catalog state poisoned")
            .error
            .clone()
    }
}
#[derive(Debug, Default)]
struct Mem {
    cache: Option<CacheFile>,
    error: Option<String>,
    failed_at: Option<Instant>,
}
#[derive(Clone, Debug)]
pub struct ModelCatalog {
    endpoints: Arc<Mutex<HashMap<String, Arc<Endpoint>>>>,
    data_dir: Option<PathBuf>,
    deadline: Duration,
    write_failures: Arc<AtomicUsize>,
}
impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new(None)
    }
}
impl ModelCatalog {
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        Self {
            endpoints: Arc::default(),
            data_dir,
            deadline: FETCH_DEADLINE,
            write_failures: Arc::default(),
        }
    }
    pub fn system() -> Self {
        Self::new(paths::data_dir())
    }
    fn endpoint(&self, key: &str) -> Arc<Endpoint> {
        self.endpoints
            .lock()
            .expect("catalog endpoints poisoned")
            .entry(key.to_string())
            .or_default()
            .clone()
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
    /// The newest of the file and this process's copy. A corrupt or
    /// unknown-version file counts as absent.
    fn current(&self, ep: &Endpoint, path: Option<&Path>) -> Option<CacheFile> {
        let file = path.and_then(read_cache);
        let mem = ep.mem.lock().expect("catalog state poisoned").cache.clone();
        match (file, mem) {
            (Some(f), Some(m)) => Some(if m.fetched_at > f.fetched_at { m } else { f }),
            (f, m) => f.or(m),
        }
    }
    /// What is known for this model, whatever its age; never touches the
    /// network. `stale` says the entry is past the TTL.
    pub fn cached(&self, resolved: &ResolvedModel) -> CatalogData {
        let endpoint = endpoint_key(&resolved.provider.base_url);
        let ep = self.endpoint(&endpoint);
        let old = self.current(&ep, self.file(&endpoint).as_deref());
        let age = old.as_ref().map(|c| c.age_of(&resolved.model.model));
        let error = ep.error();
        CatalogData {
            limits: old
                .as_ref()
                .and_then(|c| c.models.get(&resolved.model.model).cloned()),
            stale: age.is_some_and(|a| a >= TTL),
            age_seconds: age,
            error,
        }
    }
    /// The model's limits, refreshing the endpoint's list when the cache is
    /// missing or past the TTL. Concurrent callers share one request; a
    /// failed refresh returns the last known values marked stale.
    pub async fn lookup(
        &self,
        resolved: &ResolvedModel,
        key: &str,
        clients: &ClientCache,
    ) -> CatalogData {
        let endpoint = endpoint_key(&resolved.provider.base_url);
        let ep = self.endpoint(&endpoint);
        let path = self.file(&endpoint);
        let model = &resolved.model.model;
        if let Some(d) = self.fresh(&ep, path.as_deref(), model) {
            return d;
        }
        let _flight = ep.flight.lock().await;
        if let Some(d) = self.fresh(&ep, path.as_deref(), model) {
            return d;
        }
        let backing_off = ep
            .mem
            .lock()
            .expect("catalog state poisoned")
            .failed_at
            .is_some_and(|t| t.elapsed() < FAILURE_BACKOFF);
        if !backing_off {
            match self.fetch(resolved, key, clients).await {
                Ok(fetched) => {
                    let old = self.current(&ep, path.as_deref());
                    let (models, carried) = merge(old.as_ref(), fetched);
                    let file = CacheFile {
                        version: CACHE_VERSION,
                        fetched_at: now(),
                        models,
                        carried,
                    };
                    // The file is the source of truth; the process keeps its
                    // own copy only when there is no file to keep it in.
                    let saved = path.as_ref().is_some_and(|p| self.persist(p, &file));
                    {
                        let mut mem = ep.mem.lock().expect("catalog state poisoned");
                        mem.cache = (!saved).then(|| file.clone());
                        mem.error = None;
                        mem.failed_at = None;
                    }
                    let age = file.age_of(model);
                    return CatalogData {
                        limits: file.models.get(model).cloned(),
                        stale: age >= TTL,
                        age_seconds: Some(age),
                        error: None,
                    };
                }
                Err(e) => {
                    let mut mem = ep.mem.lock().expect("catalog state poisoned");
                    mem.error = Some(short(&e));
                    mem.failed_at = Some(Instant::now());
                }
            }
        }
        let old = self.current(&ep, path.as_deref());
        let error = ep.error();
        CatalogData {
            limits: old.as_ref().and_then(|c| c.models.get(model).cloned()),
            stale: old.is_some(),
            age_seconds: old.as_ref().map(|c| c.age_of(model)),
            error,
        }
    }
    fn fresh(&self, ep: &Endpoint, path: Option<&Path>, model: &str) -> Option<CatalogData> {
        let c = self.current(ep, path)?;
        // The list is current, so it is not asked again; a model whose kept
        // values are older than the TTL still reports itself as stale.
        (now().saturating_sub(c.fetched_at) < TTL).then(|| {
            let age = c.age_of(model);
            CatalogData {
                limits: c.models.get(model).cloned(),
                stale: age >= TTL,
                age_seconds: Some(age),
                error: None,
            }
        })
    }
    async fn fetch(
        &self,
        resolved: &ResolvedModel,
        key: &str,
        clients: &ClientCache,
    ) -> Result<HashMap<String, Limits>, ProviderError> {
        let client = ModelClient::for_catalog(
            &resolved.provider_name,
            &resolved.provider,
            key.into(),
            clients,
        )?;
        let bytes = client
            .get_json("models", self.deadline, &CancellationToken::new())
            .await?;
        parse_models(&bytes)
    }
    /// Writes the cache file atomically. A directory that cannot be written
    /// leaves the process on its memory copy, with one warning per catalog.
    /// Returns whether the file was written.
    fn persist(&self, path: &Path, file: &CacheFile) -> bool {
        let write = || -> std::io::Result<()> {
            let raw = serde_json::to_vec(file).map_err(std::io::Error::other)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&tmp, raw)
                .and_then(|()| std::fs::rename(&tmp, path))
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&tmp);
                })
        };
        let Err(e) = write() else { return true };
        if self.write_failures.fetch_add(1, Ordering::SeqCst) == 0 {
            tracing::warn!(error = %e, "cannot write the model catalog cache; keeping it in memory only");
        }
        false
    }
}
/// The endpoint as it is keyed and hashed: no userinfo, query or fragment.
fn endpoint_key(base_url: &str) -> String {
    endpoint_without_credentials(base_url)
}
/// New values over the previous ones: a limit the provider now reports as
/// null, zero or malformed keeps the value already known for that model. The
/// second map dates the oldest value kept for each such model, so a kept
/// number does not pass for a freshly confirmed one.
#[allow(clippy::type_complexity)]
fn merge(
    old: Option<&CacheFile>,
    fetched: HashMap<String, Limits>,
) -> (HashMap<String, Limits>, HashMap<String, u64>) {
    let Some(old) = old else {
        return (fetched, HashMap::new());
    };
    let mut carried = HashMap::new();
    let models = fetched
        .into_iter()
        .map(|(id, mut l)| {
            if let Some(p) = old.models.get(&id) {
                let kept = (l.context_length.is_none() && p.context_length.is_some())
                    || (l.max_input_tokens.is_none() && p.max_input_tokens.is_some())
                    || (l.max_output_tokens.is_none() && p.max_output_tokens.is_some());
                if kept {
                    let since = old.carried.get(&id).copied().unwrap_or(old.fetched_at);
                    carried.insert(id.clone(), since.min(old.fetched_at));
                }
                l.context_length = l.context_length.or(p.context_length);
                l.max_input_tokens = l.max_input_tokens.or(p.max_input_tokens);
                l.max_output_tokens = l.max_output_tokens.or(p.max_output_tokens);
            }
            (id, l)
        })
        .collect();
    (models, carried)
}
/// A short, secret-free reason for a failed refresh. Never the response
/// body, a URL or a header.
fn short(e: &ProviderError) -> String {
    match e {
        ProviderError::Api {
            status: 0, message, ..
        } => message.clone(),
        ProviderError::Api {
            status, message, ..
        } => format!("{message} (HTTP {status})"),
        ProviderError::MalformedResponse(m) => m.clone(),
        ProviderError::Http(e) if e.is_timeout() => "request timed out".into(),
        ProviderError::Http(e) if e.is_connect() => "connection failed".into(),
        ProviderError::Http(_) => "request failed".into(),
        ProviderError::Canceled { .. } => "canceled".into(),
        _ => "refresh failed".into(),
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
            effort_tiers: effort_tiers(e),
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
        if existing.effort_tiers.is_empty() {
            existing.effort_tiers = incoming.effort_tiers;
        }
    }
    Ok(models)
}

/// At most this many tiers of one model are kept.
const MAX_TIERS: usize = 16;

/// `capabilities.effort_tiers` of one model entry: the usable names, in the
/// order reported, without duplicates. A tier becomes a variant name and a
/// request value, so anything that is not a short plain word is dropped.
fn effort_tiers(entry: &serde_json::Value) -> Vec<String> {
    let mut tiers: Vec<String> = Vec::new();
    let reported = entry
        .pointer("/capabilities/effort_tiers")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str());
    for tier in reported {
        let plain = !tier.is_empty()
            && tier.len() <= 32
            && tier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if plain && !tiers.iter().any(|t| t == tier) && tiers.len() < MAX_TIERS {
            tiers.push(tier.to_string());
        }
    }
    tiers
}
/// A cache file this build can trust: known version, and no limit of zero
/// (the parser never writes one, so it means the file was damaged).
fn read_cache(path: &Path) -> Option<CacheFile> {
    serde_json::from_slice::<CacheFile>(&std::fs::read(path).ok()?)
        .ok()
        .filter(|c| c.version == CACHE_VERSION && c.models.values().all(Limits::is_sane))
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
    /// The provider's base URL without credentials, query or fragment. Empty
    /// in a resolution saved before endpoints were recorded: it is accepted
    /// on resume and filled in then.
    #[serde(default)]
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
    /// `provider 'p', model 'm', api chat, endpoint <url>`: the identity a
    /// retargeted alias is compared on. The endpoint is omitted when unknown.
    pub fn identity(&self) -> String {
        let base = format!(
            "provider '{}', model '{}', api {}",
            self.provider, self.model, self.api
        );
        if self.endpoint.is_empty() {
            base
        } else {
            format!("{base}, endpoint {}", self.endpoint)
        }
    }
    /// Whether `other` is the same provider, model, api and endpoint. A
    /// resolution saved without an endpoint matches any: that gap is filled
    /// on resume rather than refused.
    pub fn same_identity(&self, other: &ModelResolution) -> bool {
        self.provider == other.provider
            && self.model == other.model
            && self.api == other.api
            && (self.endpoint.is_empty()
                || other.endpoint.is_empty()
                || self.endpoint == other.endpoint)
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
        if saved.bounds.context_window == 0 {
            return Err("context_window must be positive".into());
        }
        if saved.bounds.max_input_tokens == Some(0) {
            return Err("max_input_tokens must be positive".into());
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
pub(crate) fn endpoint_without_credentials(base_url: &str) -> String {
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
    /// The discovered values used are past the TTL: the refresh failed.
    pub stale: bool,
    pub stale_age_secs: Option<u64>,
    /// Why the last refresh failed (short, no secrets), if it did.
    pub refresh_error: Option<String>,
    pub snapshot: ModelResolution,
    /// The variants the alias offers: its own, or generated from the
    /// discovered effort tiers.
    pub variants: Variants,
    /// The alias's `default_variant` as configured. It may name a generated
    /// tier the provider no longer reports: check with [`Self::default_override`].
    pub default_variant: Option<String>,
}
impl ResolvedChat {
    /// The default variant's name and override object; `None` when the alias
    /// sets no default, an error when the default names no variant now on offer.
    pub fn default_override(
        &self,
        alias: &str,
    ) -> Result<Option<(String, Variant)>, UnknownVariant> {
        let Some(name) = &self.default_variant else {
            return Ok(None);
        };
        self.variants
            .get(name)
            .map(|v| Some((name.clone(), v.clone())))
            .ok_or_else(|| UnknownVariant {
                alias: alias.to_string(),
                variant: name.clone(),
            })
    }
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
    // Generated variants need the provider's tiers even when the window is
    // configured.
    let needs_tiers = resolved.model.variants.is_none() && resolved.provider.catalog_format.is_some();
    let data = if !is_chat {
        None
    } else if explicit.is_some() && !needs_tiers {
        Some(catalog.cached(resolved))
    } else {
        Some(catalog.lookup(resolved, key, clients).await)
    };
    // Freshness only describes values that came from the provider's list.
    let used = data.as_ref().filter(|d| d.limits.is_some());
    let stale = used.is_some_and(|d| d.stale);
    let stale_age_secs = used.filter(|d| d.stale).and_then(|d| d.age_seconds);
    let refresh_error = used.and_then(|d| d.error.clone());
    let limits = data.and_then(|d| d.limits).unwrap_or_default();
    let variants = effective_variants(resolved, &limits.effort_tiers);
    let default_variant = resolved.model.default_variant.clone();
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
    // A zero in models.json means "not set": a saved resolution with a zero
    // bound is refused on resume, so it must never be written.
    let configured_input = resolved.model.max_input_tokens.filter(|n| *n > 0);
    let max_input_tokens = configured_input.or(limits.max_input_tokens);
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
            max_input_tokens: match (configured_input, limits.max_input_tokens) {
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
        stale,
        stale_age_secs,
        refresh_error,
        snapshot,
        variants,
        default_variant,
    })
}

#[cfg(test)]
mod tests;
