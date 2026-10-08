//! The production role guard of a session: Jev, the System One model behind
//! the configured instinct layer in config.json.

use std::path::Path;
use std::sync::Arc;

use crate::agent::guard::{JevGuard, RoleGuard};
use crate::provider::config::load_models;
use crate::provider::openai::ClientCache;

pub(super) fn permission_judge(
    models_path: Option<&Path>,
    workspace: &Path,
    config_dir: &Path,
) -> Option<Arc<dyn crate::agent::permission_gate::PermissionJudge>> {
    let (models, _) = load_models(models_path, workspace, config_dir)
        .map_err(|error| tracing::warn!(%error, "permission judge disabled: catalog unavailable"))
        .ok()?;
    let client = crate::provider::layers::available_instinct(
        &models,
        config_dir,
        Arc::new(ClientCache::new()),
    )?;
    Some(Arc::new(crate::agent::permission_gate::JevJudge(client)))
}

/// Builds the guard for a session, or `None` when instinct is unavailable.
/// Then a guarded agent's exceptions go to the Captain
/// with "Jev is not available" as the reason: the session still works.
pub(super) fn jev_guard(
    models_path: Option<&Path>,
    workspace: &Path,
    config_dir: &Path,
) -> Option<Arc<dyn RoleGuard>> {
    let (models, _) = load_models(models_path, workspace, config_dir)
        .map_err(|error| tracing::warn!(%error, "role guard disabled: catalog unavailable"))
        .ok()?;
    let client = crate::provider::layers::available_instinct(
        &models,
        config_dir,
        Arc::new(ClientCache::new()),
    )?;
    Some(Arc::new(JevGuard(client)))
}
