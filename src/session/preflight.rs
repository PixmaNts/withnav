//! Startup admission for processes that own cognition assignments.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::agent::definitions::{AgentRoots, Agents};
use crate::agent::instructions::RulesRoots;
use crate::provider::assignments::{AgentModel, AssignmentResolver, DefaultAgentError};
use crate::provider::catalog::{ModelCatalog, resolve_chat};
use crate::provider::config::{ModelsConfig, load_models};
use crate::provider::openai::ClientCache;
use crate::settings::Settings;

/// One settings/catalog snapshot captured after the owning workspace is known.
pub struct Preflight {
    settings: Settings,
    models: ModelsConfig,
    models_path: PathBuf,
    config_dir: PathBuf,
    roots: AgentRoots,
    catalog: ModelCatalog,
    clients: ClientCache,
}

impl Preflight {
    /// The catalog admitted for startup, reused by the editor's model layers.
    pub(crate) fn models(&self) -> &ModelsConfig {
        &self.models
    }

    /// The selected catalog file, pinned for the editor's cognition server.
    pub(crate) fn models_path(&self) -> &Path {
        &self.models_path
    }
    /// Captures assignments and catalog once, without provider requests.
    pub async fn capture(
        config_dir: &Path,
        models_path: Option<&Path>,
        workspace: &Path,
        rules_roots: &RulesRoots,
    ) -> anyhow::Result<Self> {
        let config_dir = config_dir.to_path_buf();
        let models_path = models_path.map(Path::to_path_buf);
        let workspace = workspace.to_path_buf();
        let (settings, models, path) = tokio::task::spawn_blocking({
            let config_dir = config_dir.clone();
            move || -> anyhow::Result<_> {
                let settings = crate::settings::load(&config_dir)
                    .context("cannot load startup config.json")?;
                let (models, path) = load_models(models_path.as_deref(), &workspace, &config_dir)
                    .context("cannot load startup models.json")?;
                Ok((settings, models, path))
            }
        })
        .await
        .context("startup configuration task failed")??;
        Ok(Self {
            settings,
            models,
            models_path: path,
            roots: AgentRoots {
                home: rules_roots.home.clone(),
                config_dir: Some(config_dir.clone()),
            },
            config_dir,
            catalog: ModelCatalog::system(),
            clients: ClientCache::new(),
        })
    }

    /// Validates the default against the actual workspace's primary agents.
    /// Only bounded catalog metadata discovery is allowed; no inference is sent.
    pub async fn check(&self, workspace: &Path) -> anyhow::Result<String> {
        self.check_scope(workspace, false).await?.ok_or_else(|| {
            anyhow::anyhow!("default_agent is unresolved in the initialized workspace")
        })
    }

    /// Checks a tentative scope without rejecting an agent unknown in that scope.
    /// Editor startup must instead capture and check its actual initialize workspace.
    pub async fn check_provisional(&self, workspace: &Path) -> anyhow::Result<Option<String>> {
        self.check_scope(workspace, true).await
    }

    async fn check_scope(
        &self,
        workspace: &Path,
        defer_unknown: bool,
    ) -> anyhow::Result<Option<String>> {
        let workspace = std::fs::canonicalize(workspace).with_context(|| {
            format!(
                "startup workspace {} is not accessible",
                workspace.display()
            )
        })?;
        anyhow::ensure!(workspace.is_dir(), "startup workspace is not a directory");
        let roots = self.roots.clone();
        let agents = tokio::task::spawn_blocking(move || Agents::load(&roots, &workspace))
            .await
            .context("startup agent discovery task failed")?;
        let known: Vec<_> = agents
            .all()
            .map(|a| (a.id.as_str(), a.model.as_deref(), a.mode.is_primary()))
            .collect();
        let resolver = AssignmentResolver::new(&self.models, &self.settings);
        let default = match resolver.default_agent_check(&known) {
            Ok(default) => default,
            Err(DefaultAgentError::Unknown { .. }) if defer_unknown => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let header = agents.get(&default).and_then(|a| a.model.as_deref());
        let AgentModel::Assigned(assigned) = resolver
            .agent_model(&default, header)
            .map_err(anyhow::Error::msg)?
        else {
            anyhow::bail!("default_agent '{default}' has no usable assigned model");
        };
        let unusable = |reason: String| DefaultAgentError::NoUsableModel {
            name: default.clone(),
            reason,
        };
        let key = crate::provider::auth::resolve_key(
            &self.config_dir,
            &assigned.resolved.provider_name,
            assigned.resolved.provider.api_key_env.as_deref(),
        )
        .map_err(|_| {
            unusable(format!(
                "model '{}' authentication unavailable for provider '{}'; check auth.json permissions and API key configuration",
                assigned.name, assigned.resolved.provider_name
            ))
        })?;
        anyhow::ensure!(
            !key.trim().is_empty(),
            "default_agent '{default}' has no usable model: missing API key for provider '{}'",
            assigned.resolved.provider_name
        );
        let mut metadata_model = assigned.resolved.clone();
        // No selected variant needs effort discovery. A configured window is
        // sufficient admission evidence; the session can discover its offer later.
        if assigned.variant.is_none() && metadata_model.model.variants.is_none() {
            metadata_model.model.variants = Some(Default::default());
        }
        let chat = resolve_chat(&self.catalog, &metadata_model, &key, &self.clients)
            .await
            .map_err(|_| {
                unusable(format!(
                    "model '{}' has no known context window; set context_window in {}",
                    assigned.name,
                    self.models_path.display()
                ))
            })?;
        if let Some(variant) = &assigned.variant
            && !chat.variants.contains_key(variant)
        {
            return Err(unusable(format!(
                "model '{}' has invalid variant '{variant}'; offered variants: {}",
                assigned.name,
                chat.variants.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
            .into());
        }
        crate::agent::context::ContextLimits::from_resolution(
            &assigned.name.to_string(),
            assigned.variant.as_deref(),
            &chat.snapshot,
            assigned.resolved.model.max_output_tokens,
        )
        .map_err(|error| unusable(error.to_string()))?;
        Ok(Some(default))
    }
}

/// Validates a process's default before any listener, terminal or session starts.
pub async fn check(
    config_dir: &Path,
    models_path: Option<&Path>,
    workspace: &Path,
    rules_roots: &RulesRoots,
) -> anyhow::Result<()> {
    Preflight::capture(config_dir, models_path, workspace, rules_roots)
        .await?
        .check(workspace)
        .await?;
    Ok(())
}
