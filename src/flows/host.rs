//! Workspace-owned execution, shared by session admission and Studio.
use super::{
    client::WithNavDecisionClient,
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
    records: Arc<Records>,
    diagnostic: Option<instinct_flow::Diagnostic>,
}
impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FlowHost")
    }
}
impl Host {
    pub fn shutdown(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        for entry in self
            .workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            entry
                .records
                .shutdown(deadline.saturating_duration_since(std::time::Instant::now()));
        }
    }
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
        let store = Arc::new(FlowStore::with_data(
            workspace.clone(),
            self.config.clone(),
            self.data.clone(),
        ));
        let records = Arc::new(Records::new(&self.data, &workspace));
        let client =
            crate::provider::config::load_models(self.models.as_deref(), &workspace, &self.config)
                .map_err(anyhow::Error::from)
                .and_then(|(models, _)| {
                    WithNavDecisionClient::new(
                        &models,
                        &self.config,
                        Arc::new(crate::provider::openai::ClientCache::new()),
                    )
                });
        let (client, diagnostic): (Arc<dyn instinct_flow::DecisionClient>, _) = match client {
            Ok(client) => (Arc::new(client), None),
            Err(_) => {
                // Provider errors can contain destinations or credentials; expose a fixed message.
                tracing::warn!(
                    "instinct client unavailable; flow decisions use provider-error fallback"
                );
                (
                    Arc::new(super::client::UnavailableClient),
                    Some(instinct_flow::Diagnostic::new(
                        "io",
                        "instinct client unavailable; decisions use provider-error fallback",
                        "instinct",
                    )),
                )
            }
        };
        let runtime = Some(Arc::new(
            FlowRuntime::new(store.clone(), client, Arc::new(NoEffects))
                .with_records(records.clone()),
        ));
        Workspace {
            store,
            runtime,
            records,
            diagnostic,
        }
    }
    pub fn diagnostics(&self, path: &Path) -> Vec<instinct_flow::Diagnostic> {
        let workspace = self.workspace(path);
        let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
        let entry = workspaces
            .entry(workspace.clone())
            .or_insert_with(|| self.entry(workspace));
        entry
            .diagnostic
            .clone()
            .into_iter()
            .chain(entry.store.reload_diagnostics())
            .collect()
    }
    pub fn records(&self, path: &Path) -> Arc<Records> {
        let workspace = self.workspace(path);
        self.workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(workspace.clone())
            .or_insert_with(|| self.entry(workspace))
            .records
            .clone()
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
        let records = self.records(&workspace);
        let runtime = Arc::new(
            FlowRuntime::new(store.clone(), client, Arc::new(NoEffects))
                .with_records(records.clone()),
        );
        self.workspaces.lock().unwrap().insert(
            workspace,
            Workspace {
                store,
                runtime: Some(runtime),
                records,
                diagnostic: None,
            },
        );
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn content(action: &str) -> super::super::store::Content {
        let params = if action == "no_objection" {
            ""
        } else {
            "note='guard'\n"
        };
        super::super::store::Content::Toml {
            toml: format!("[flow]\nversion=1\nname='guard'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='{action}'\n[node.params]\n{params}"),
            templates: BTreeMap::new(),
        }
    }
    async fn verdict(runtime: &FlowRuntime) -> super::super::runtime::VerdictLike {
        runtime
            .tool_pre_call(
                super::super::runtime::ToolCallContextLike {
                    call_id: "x".into(),
                    tool: "read".into(),
                    args: serde_json::json!({}),
                    agent: "build".into(),
                    workspace: runtime.store.workspace.clone(),
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
    }
    #[tokio::test]
    async fn independent_hosts_reload_on_next_event_and_keep_invalid_snapshot() {
        use super::super::{
            runtime::VerdictLike,
            store::{Content, Location},
        };
        let temp = tempfile::tempdir().unwrap();
        let a = Host::new(temp.path().join("config"), temp.path().join("data"), None);
        let b = Host::new(a.config.clone(), a.data.clone(), None);
        let store = a.store(temp.path().into());
        let runtime = b.runtime(temp.path()).unwrap();
        let doc = store
            .save("guard", Location::Global, None, content("deny"))
            .unwrap();
        assert_eq!(verdict(&runtime).await, VerdictLike::NoObjection);
        store.validate_test_documents();
        store.reload().unwrap();
        assert!(matches!(verdict(&runtime).await, VerdictLike::Deny { .. }));
        let next = store
            .save(
                "guard",
                Location::Global,
                Some(&doc.revision),
                content("no_objection"),
            )
            .unwrap();
        assert!(matches!(verdict(&runtime).await, VerdictLike::Deny { .. }));
        store.validate_test_documents();
        store.reload().unwrap();
        // A publishes a valid set; an external editor corrupts the file before B evaluates.
        store
            .save(
                "guard",
                Location::Global,
                Some(&next.revision),
                Content::Toml {
                    toml: "invalid".into(),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        assert_eq!(verdict(&runtime).await, VerdictLike::NoObjection);
        assert!(!b.diagnostics(temp.path()).is_empty());
        store.reload().unwrap();
        let dir =
            super::super::fs::Directory::private(&a.data.join("flows/reload"), false).unwrap();
        let marker = dir
            .names()
            .unwrap()
            .into_iter()
            .find(|n| !n.starts_with('.'))
            .unwrap();
        let generation: serde_json::Value =
            serde_json::from_str(&dir.read(&marker, 1024, true).unwrap()).unwrap();
        assert_eq!(generation["generation"], 3);
        assert!(time::OffsetDateTime::parse(
            generation["reloaded_at"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339
        )
        .is_ok());
    }
    #[tokio::test]
    async fn independent_hosts_revoke_workspace_runs_even_when_reload_is_invalid() {
        use super::super::{
            runtime::VerdictLike,
            store::{Content, Location},
        };
        let temp = tempfile::tempdir().unwrap();
        let a = Host::new(temp.path().join("config"), temp.path().join("data"), None);
        let b = Host::new(a.config.clone(), a.data.clone(), None);
        let store = a.store(temp.path().into());
        let runtime = b.runtime(temp.path()).unwrap();
        store
            .save("guard", Location::Workspace, None, content("deny"))
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
        assert!(matches!(verdict(&runtime).await, VerdictLike::Deny { .. }));
        store
            .save(
                "broken",
                Location::Global,
                None,
                Content::Toml {
                    toml: "invalid".into(),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        let permissions = crate::permission::store::PermissionStore::open(&a.data).unwrap();
        for approval in permissions.list_validations().unwrap() {
            permissions.revoke_validation(approval.id).unwrap();
        }
        store.reload().unwrap();
        assert_eq!(verdict(&runtime).await, VerdictLike::NoObjection);
        assert!(!runtime.store.reload_diagnostics().is_empty());
    }
    #[test]
    fn editor_adapter_observes_shared_host_reload() {
        let temp = tempfile::tempdir().unwrap();
        let host = Host::new(temp.path().join("config"), temp.path().join("data"), None);
        let runtime = host.runtime(temp.path()).unwrap();
        let editor = runtime.with_effects_adapter(Arc::new(NoEffects));
        let store = host.store(temp.path().canonicalize().unwrap());
        assert!(Arc::ptr_eq(&editor.store, &store));
        let text = "[flow]\nversion=1\nname='editor'\non='save'\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='log'\n[node.params]\ntext='test'\n";
        let saved = store
            .save(
                "editor",
                super::super::store::Location::Global,
                None,
                super::super::store::Content::Toml {
                    toml: text.into(),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
        assert!(
            editor.store.snapshot()["editor"]
                .compiled
                .definition()
                .flow
                .enabled
        );
        store
            .save(
                "editor",
                super::super::store::Location::Global,
                Some(&saved.revision),
                super::super::store::Content::Toml {
                    toml: text.replace("on='save'", "on='save'\nenabled=false"),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        store.validate_test_documents();
        store.reload().unwrap();
        assert!(
            !editor.store.snapshot()["editor"]
                .compiled
                .definition()
                .flow
                .enabled
        );
    }
    #[tokio::test]
    async fn unavailable_client_keeps_deterministic_rules_and_decision_fallbacks() {
        let temp = tempfile::tempdir().unwrap();
        let host = Host::new(
            temp.path().join("config"),
            temp.path().join("data"),
            Some(temp.path().join("missing-models.toml")),
        );
        let runtime = host.runtime(temp.path()).unwrap();
        assert!(!host.diagnostics(temp.path()).is_empty());
        for (name, node) in [
            (
                "deterministic",
                "type='action'\naction='deny'\n[node.params]\nnote='denied'",
            ),
            (
                "decision",
                "type='noul'\ninstructions='test'\nthreshold=0.5\n[node.inputs]\nx='session.agent'",
            ),
        ] {
            runtime.store.save(name, super::super::store::Location::Global, None, super::super::store::Content::Toml {
                toml: format!("[flow]\nversion=1\nname='{name}'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\n{node}\n"), templates: BTreeMap::new()
            }).unwrap();
        }
        runtime.store.validate_test_documents();
        runtime.store.reload().unwrap();
        let verdict = runtime
            .tool_pre_call(
                super::super::runtime::ToolCallContextLike {
                    call_id: "x".into(),
                    tool: "read".into(),
                    args: serde_json::json!({}),
                    agent: "build".into(),
                    workspace: temp.path().into(),
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            verdict,
            super::super::runtime::VerdictLike::Deny { .. }
        ));
        assert_eq!(
            runtime
                .client()
                .decide(
                    instinct_flow::protocol::DecisionRequest {
                        state: "",
                        questions: &BTreeMap::new()
                    },
                    &tokio_util::sync::CancellationToken::new(),
                    std::time::Instant::now()
                )
                .await
                .unwrap_err(),
            instinct_flow::DecisionError::ProviderError
        );
    }
}
