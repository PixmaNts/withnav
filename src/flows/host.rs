//! Workspace-owned execution, shared by session admission and Studio.
use super::{
    client::NavaiDecisionClient,
    records::Records,
    runtime::{FlowRuntime, NoEffects},
    store::FlowStore,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub struct Host {
    pub config: PathBuf,
    pub data: PathBuf,
    models: Option<PathBuf>,
    workspaces: Mutex<BTreeMap<PathBuf, Workspace>>,
}
struct Workspace {
    store: Arc<FlowStore>,
    runtime: Option<Arc<FlowRuntime>>,
}
impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FlowHost")
    }
}
impl Host {
    pub fn new(config: PathBuf, data: PathBuf, models: Option<PathBuf>) -> Self {
        Self {
            config,
            data,
            models,
            workspaces: Mutex::default(),
        }
    }
    fn workspace(&self, path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
    fn entry(&self, workspace: PathBuf) -> Workspace {
        let store = Arc::new(FlowStore::new(workspace.clone(), self.config.clone()));
        let runtime =
            crate::provider::config::load_models(self.models.as_deref(), &workspace, &self.config)
                .ok()
                .and_then(|(models, _)| {
                    NavaiDecisionClient::new(
                        &models,
                        &self.config,
                        Arc::new(crate::provider::openai::ClientCache::new()),
                    )
                    .ok()
                })
                .map(|client| {
                    Arc::new(
                        FlowRuntime::new(store.clone(), Arc::new(client), Arc::new(NoEffects))
                            .with_records(Arc::new(Records::new(&self.data, &workspace))),
                    )
                });
        Workspace { store, runtime }
    }
    pub fn store(&self, path: PathBuf) -> Arc<FlowStore> {
        let workspace = self.workspace(&path);
        self.workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(workspace.clone())
            .or_insert_with(|| self.entry(workspace))
            .store
            .clone()
    }
    pub fn runtime(&self, path: &Path) -> Option<Arc<FlowRuntime>> {
        let workspace = self.workspace(path);
        self.workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(workspace.clone())
            .or_insert_with(|| self.entry(workspace))
            .runtime
            .clone()
    }
    #[cfg(test)]
    pub(crate) fn inject_client(
        &self,
        path: &Path,
        client: Arc<dyn instinct_flow::DecisionClient>,
    ) {
        let workspace = self.workspace(path);
        let store = self.store(workspace.clone());
        let runtime = Arc::new(
            FlowRuntime::new(store.clone(), client, Arc::new(NoEffects))
                .with_records(Arc::new(Records::new(&self.data, &workspace))),
        );
        self.workspaces.lock().unwrap().insert(
            workspace,
            Workspace {
                store,
                runtime: Some(runtime),
            },
        );
    }
}
