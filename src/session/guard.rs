//! The production role guard of a session: Jev, the System One model behind
//! the `instinct` alias of the session's models.json.

use std::path::Path;
use std::sync::Arc;

use crate::agent::guard::{JevGuard, RoleGuard};
use crate::provider::config::load_models;
use crate::provider::openai::ClientCache;
use crate::provider::systemone::SystemOneClient;

/// The alias the guard's model is read from.
pub const GUARD_ALIAS: &str = "instinct";

pub(super) fn permission_judge(
    models_path: Option<&Path>,
    workspace: &Path,
    config_dir: &Path,
) -> Option<Arc<dyn crate::agent::permission_gate::PermissionJudge>> {
    let (models, _) = load_models(models_path, workspace, config_dir).ok()?;
    let client = SystemOneClient::from_alias(
        &models,
        GUARD_ALIAS,
        config_dir,
        Arc::new(ClientCache::new()),
    )
    .ok()?;
    Some(Arc::new(crate::agent::permission_gate::JevJudge(client)))
}

/// Builds the guard for a session, or `None` when there is no `instinct`
/// alias or no key for it. Then a guarded agent's exceptions go to the driver
/// with "Jev is not available" as the reason: the session still works.
pub(super) fn jev_guard(
    models_path: Option<&Path>,
    workspace: &Path,
    config_dir: &Path,
) -> Option<Arc<dyn RoleGuard>> {
    let (models, _) = load_models(models_path, workspace, config_dir).ok()?;
    let client = SystemOneClient::from_alias(
        &models,
        GUARD_ALIAS,
        config_dir,
        Arc::new(ClientCache::new()),
    )
    .ok()?;
    Some(Arc::new(JevGuard(client)))
}
