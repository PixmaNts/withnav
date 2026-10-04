//! Read-only assignment projections. No provider credentials or request overrides cross this seam.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{Service, SessionId};
use crate::agent::definitions::{AgentDefinition, AgentMode, AgentRoots, Agents};
use crate::error::ServerError;
use crate::provider::assignments::{
    AgentModel, AssignedModel, AssignmentResolver, Layer, LayerAssignment,
};
use crate::provider::config::{effective_variants, load_models, ApiKind, ModelsConfig};
use crate::provider::model_name::ModelName;
use crate::session::store::{AgentOverride, AgentVariantOverride, StoredSession};
use crate::settings::{AssignmentVariant, Settings};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentSource {
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
                if let Ok(name) = ModelName::parse(&raw, &catalog) {
                    if let Ok(resolved) = catalog.resolve_name(&name) {
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
                LayerAssignment::Disabled(reason) => {
                    entry.reason = Some(safe_reason(
                        &reason.to_string(),
                        requested.as_ref().map(|a| a.model.as_str()),
                        &snapshot.catalog,
                    ))
                }
                LayerAssignment::Enabled(assigned) => {
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
        let snapshot = self.assignment_snapshot(workspace).await?;
        let saved = match session {
            Some(id) => {
                let row = self
                    .history()
                    .meta(id)
                    .await
                    .map_err(ServerError::History)?
                    .ok_or_else(|| ServerError::SessionNotFound(id.to_string()))?;
                if Path::new(&row.workspace).canonicalize().ok().as_ref()
                    != Some(&snapshot.workspace)
                {
                    return Err(diagnostic("session_id belongs to a different workspace"));
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
        crate::agent::definitions::AgentId::parse(
            snapshot.settings.default_agent().map_err(diagnostic)?,
        )
        .map_err(|_| diagnostic("default_agent is not a valid agent id"))?;
        let wanted = snapshot.settings.default_agent().map_err(diagnostic)?;
        if let Some(definition) = snapshot.agents.get(wanted) {
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
        let default_agent = resolver
            .default_agent_check(&known)
            .map_err(|error| diagnostic(error.to_string()))?;
        let default = snapshot
            .agents
            .get(&default_agent)
            .ok_or_else(|| diagnostic("default_agent definition unavailable"))?;
        let fallback = self
            .base_assignment(&snapshot, default)?
            .0
            .ok_or_else(|| diagnostic("default_agent has no usable model"))?;
        if let Some(row) = &saved {
            let current = row.primary_agent.as_deref().unwrap_or("build");
            if snapshot.agents.get(current).is_none() {
                return Err(diagnostic(format!(
                    "saved session agent '{current}' is unavailable"
                )));
            }
            for agent in row.agent_overrides.keys() {
                if snapshot.agents.get(agent).is_none() {
                    return Err(diagnostic(format!(
                        "saved override agent '{agent}' is unavailable"
                    )));
                }
            }
        }
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
            let assignment = if inherits_parent {
                None
            } else {
                Some(base.unwrap_or_else(|| {
                    let mut selected = fallback.clone();
                    selected.source = AssignmentSource::DefaultAgent;
                    if !matches!(selected.variant_source, AssignmentSource::ModelDefault) {
                        selected.variant_source = AssignmentSource::DefaultAgent;
                    }
                    selected
                }))
            };
            let mut effective = assignment.clone();
            let mut selection_override = None;
            if let Some(row) = &saved {
                if let Some(over) = row.agent_overrides.get(definition.id.as_str()) {
                    effective =
                        Some(self.override_assignment(&snapshot, assignment.as_ref(), over)?);
                    selection_override = Some(override_view(over));
                }
                if definition.id.as_str() == row.primary_agent.as_deref().unwrap_or("build") {
                    effective = Some(saved_selection(row, effective.as_ref(), &snapshot.catalog)?);
                } else if inherits_parent && selection_override.is_none() {
                    let mut parent = saved_selection(row, None, &snapshot.catalog)?;
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
                Some(safe_reason(
                    &warning.to_string(),
                    Some(&warning.model),
                    &snapshot.catalog,
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
        if let Some(name) = &variant {
            if !offered.contains_key(name) {
                return Err(if !snapshot.offers.contains_key(&model.name.to_string()) {
                    "variant offer unavailable: no cached provider effort tiers".into()
                } else {
                    format!("variant '{name}' is not offered by model '{}'", model.name)
                });
            }
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

fn safe_reason(reason: &str, requested: Option<&str>, catalog: &ModelsConfig) -> String {
    if requested.is_some_and(|raw| canonical_name(raw, catalog).is_err()) {
        "assignment has an invalid canonical model name".into()
    } else {
        reason.into()
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

fn saved_selection(
    row: &StoredSession,
    candidate: Option<&AssignmentSelection>,
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
    let same = candidate.filter(|s| s.model == model && s.variant == row.variant);
    Ok(AssignmentSelection {
        model,
        variant: row.variant.clone(),
        source: same.map_or(AssignmentSource::Override, |s| s.source),
        variant_source: same.map_or(AssignmentSource::Override, |s| s.variant_source),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn fixture(settings: Value) -> (tempfile::TempDir, crate::server::AppState) {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("cfg");
        std::fs::create_dir_all(config_dir.join("agents")).unwrap();
        std::fs::write(config_dir.join("config.json"), settings.to_string()).unwrap();
        let models = json!({"providers":{"mock":{"base_url":"https://url-secret.invalid/v1", "api_key_env":"AUTH_SECRET", "headers":{"Authorization":"header-secret"}}},"models":{
            "legacy-chat":{"provider":"mock","model":"combo/chat","api":"chat","context_window":128000,"supports_tools":true,"variants":{"low":{"reasoning_effort":"low"},"high":{"reasoning_effort":"high"}},"default_variant":"high"},
            "legacy-fim":{"provider":"mock","model":"fim","api":"fim"},
            "legacy-instinct":{"provider":"mock","model":"jev","api":"systemone"}
        }});
        std::fs::write(config_dir.join("models.json"), models.to_string()).unwrap();
        for (name, header) in [
            ("oracle", "mode: primary\nmodel: mock/combo/chat"),
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
        let uri = format!("/v1/{route}?workspace={}{}", root.display(), extra);
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
        assert!(body["layers"][1]["reason"]
            .as_str()
            .unwrap()
            .contains("systemone"));
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
        assert_eq!(agent(&body, "oracle")["assignment"]["source"], "header");
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
        let id = ulid::Ulid::new();
        state
            .history()
            .record_session(id, root.path().to_str().unwrap(), "legacy-chat", None)
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
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("different workspace"));
        let (status, _) = get(
            &state,
            root.path(),
            "agents",
            &format!("&session_id={}", ulid::Ulid::new()),
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
        let id = ulid::Ulid::new();
        state
            .history()
            .record_session(id, root.path().to_str().unwrap(), "legacy-chat", None)
            .await
            .unwrap();
        let (status, body) = get(&state, root.path(), "agents", &format!("&session_id={id}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("no durable model identity"));
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
        std::fs::write(state.config.config_dir.join("config.json"), json!({"agents":{"build":{"model":"mock/combo/chat"},"oracle":{"model":"mock/combo/chat","variant":null}}}).to_string()).unwrap();
        let (_, body) = get(&state, root.path(), "agents", "").await;
        assert_eq!(agent(&body, "oracle")["assignment"]["source"], "config");
        assert_eq!(
            agent(&body, "oracle")["assignment"]["variant_source"],
            "config"
        );
        assert!(agent(&body, "oracle")["assignment"]["variant"].is_null());
    }
}
