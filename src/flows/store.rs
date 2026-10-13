//! Disk documents and atomic, immutable activation snapshots.
use super::{catalogue::catalogue, fs::Directory};
use instinct_flow::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    Builtin,
    Global,
    Workspace,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum Content {
    Toml {
        toml: String,
        templates: BTreeMap<String, String>,
    },
    Graph {
        definition: Box<Definition>,
        templates: BTreeMap<String, String>,
    },
}
#[derive(Debug, Clone, Serialize)]
pub struct Validation {
    pub valid: bool,
    pub toml: Option<String>,
    pub definition: Option<Definition>,
    pub templates: BTreeMap<String, String>,
    pub errors: Vec<Diagnostic>,
}
pub fn validate(content: Content) -> Validation {
    let (toml, definition, templates, mut errors) = match content {
        Content::Toml { toml, templates } => {
            let parsed = Definition::parse(&toml);
            let (definition, errors) = match parsed {
                Ok(d) => (Some(d), vec![]),
                Err(e) => (None, e),
            };
            (Some(toml), definition, templates, errors)
        }
        Content::Graph {
            definition,
            templates,
        } => {
            let (toml, errors) = match definition.to_toml() {
                Ok(t) => (Some(t), vec![]),
                Err(e) => (None, e),
            };
            (toml, Some(*definition), templates, errors)
        }
    };
    if let Some(d) = &definition {
        if let Err(e) = d.validate(&catalogue(), &Capabilities { images: false }, &templates) {
            errors.extend(e);
        }
    }
    Validation {
        valid: errors.is_empty(),
        toml,
        definition,
        templates,
        errors,
    }
}
fn load_toml(dir: &Directory, toml: String) -> (Validation, Option<CompiledDefinition>) {
    let mut templates = BTreeMap::new();
    let (definition, mut errors) = match Definition::parse(&toml) {
        Ok(d) => (Some(d), vec![]),
        Err(e) => (None, e),
    };
    if let Some(definition) = &definition {
        for node in &definition.node {
            if let Some(name) = node.kind.template() {
                match dir.read(name, MAX_TEMPLATE_BYTES, false) {
                    Ok(text) => {
                        templates.insert(name.into(), text);
                    }
                    Err(e) => errors.push(Diagnostic::new("io", e.to_string(), name)),
                }
            }
        }
    }
    let compiled = definition.as_ref().and_then(|d| {
        match d.validate(&catalogue(), &Capabilities { images: false }, &templates) {
            Ok(compiled) => Some(compiled),
            Err(e) => {
                errors.extend(e);
                None
            }
        }
    });
    (
        Validation {
            valid: errors.is_empty(),
            toml: Some(toml),
            definition,
            templates,
            errors,
        },
        compiled,
    )
}
#[derive(Debug, Clone, Serialize)]
pub struct Document {
    pub name: String,
    pub location: Location,
    pub path: String,
    pub revision: String,
    pub toml: String,
    pub definition: Option<Definition>,
    pub templates: BTreeMap<String, String>,
    pub errors: Vec<Diagnostic>,
    pub saved_since_reload: bool,
    pub read_only: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct FlowSummary {
    pub name: String,
    pub trigger: Option<String>,
    pub location: Location,
    pub enabled: Option<bool>,
    pub blocking: Option<bool>,
    pub saved_since_reload: bool,
    pub errors: Vec<Diagnostic>,
}
impl Document {
    pub fn summary(&self) -> FlowSummary {
        FlowSummary {
            name: self.name.clone(),
            trigger: self.definition.as_ref().map(|d| d.flow.on.clone()),
            location: self.location,
            enabled: self.definition.as_ref().map(|d| d.flow.enabled),
            blocking: self.definition.as_ref().map(|d| d.flow.blocking),
            saved_since_reload: self.saved_since_reload,
            errors: self.errors.clone(),
        }
    }
}
#[derive(Debug)]
pub struct LoadedFlow {
    pub location: Location,
    pub compiled: CompiledDefinition,
}
pub type Snapshot = BTreeMap<String, Arc<LoadedFlow>>;
#[derive(Debug)]
pub struct FlowStore {
    pub workspace: PathBuf,
    pub config_dir: PathBuf,
    data_dir: PathBuf,
    loaded: RwLock<Arc<Snapshot>>,
    revisions: RwLock<BTreeMap<(Location, String), String>>,
    write: std::sync::Mutex<()>,
    reload_marker: Option<ReloadMarker>,
    reload_errors: RwLock<Vec<Diagnostic>>,
    migration_errors: Vec<(Location, Diagnostic)>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    generation: u64,
    reloaded_at: String,
}
struct ReloadMarker {
    path: PathBuf,
    name: String,
    state: std::sync::Mutex<MarkerState>,
}
#[derive(Default)]
struct MarkerState {
    directory: Option<Directory>,
    observed: u64,
}
impl std::fmt::Debug for ReloadMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReloadMarker")
    }
}
impl ReloadMarker {
    fn new(data: PathBuf, workspace: &std::path::Path) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            path: data.join("flows/reload"),
            name: hex::encode(Sha256::digest(workspace.as_os_str().as_encoded_bytes())),
            state: std::sync::Mutex::new(MarkerState::default()),
        }
    }
    fn read(&self, state: &mut MarkerState) -> Result<u64, StoreError> {
        if state.directory.is_none() {
            state.directory = Some(Directory::private(&self.path, true)?);
        }
        match state
            .directory
            .as_ref()
            .unwrap()
            .read_private(&self.name, 1024)
        {
            Ok(text) => {
                let marker = serde_json::from_str::<Generation>(&text)
                    .map_err(|_| StoreError::reason("io"))?;
                time::OffsetDateTime::parse(
                    &marker.reloaded_at,
                    &time::format_description::well_known::Rfc3339,
                )
                .map_err(|_| StoreError::reason("io"))?;
                if marker.generation == 0 {
                    return Err(StoreError::reason("io"));
                }
                Ok(marker.generation)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(test)]
    fn publish(&self) -> Result<(), StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::reason("io"))?;
        self.publish_locked(&mut state).map(|_| ())
    }
    fn publish_locked(&self, state: &mut MarkerState) -> Result<u64, StoreError> {
        self.read(state)?;
        let dir = state.directory.as_ref().unwrap();
        let _lock = dir.lock(&format!(".{}.lock", self.name))?;
        let generation = self
            .read(state)?
            .checked_add(1)
            .ok_or_else(|| StoreError::reason("io"))?;
        state.directory.as_ref().unwrap().write(
            &self.name,
            &serde_json::to_vec(&Generation {
                generation,
                reloaded_at: timestamp(),
            })
            .map_err(|_| StoreError::reason("io"))?,
        )?;
        Ok(generation)
    }
}
#[derive(Debug, Clone)]
pub struct StoreError {
    pub reason: &'static str,
    pub errors: Vec<Diagnostic>,
}
impl StoreError {
    pub fn reason(reason: &'static str) -> Self {
        Self {
            reason,
            errors: vec![],
        }
    }
}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self {
            reason: "io",
            errors: vec![Diagnostic::new("io", e.to_string(), "")],
        }
    }
}
/// Validate local documents with the runtime's no-follow access and byte/entry limits.
/// Reading a document for inspection does not activate it.
pub fn inspect_directory(
    path: &std::path::Path,
) -> Result<Vec<(String, bool, String)>, StoreError> {
    let dir = match Directory::open(path, false) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut results = vec![];
    let mut names = dir.names()?;
    names.sort();
    for file in names.into_iter().filter(|n| n.ends_with(".toml")) {
        let toml = dir.read(&file, instinct_flow::MAX_FLOW_BYTES, false)?;
        let validation = load_toml(&dir, toml).0;
        let hash = revision(
            validation.toml.as_deref().unwrap_or_default(),
            &validation.templates,
        );
        results.push((file, validation.valid, hash));
    }
    Ok(results)
}
impl FlowStore {
    #[cfg(test)]
    pub(crate) fn validate_test_documents(&self) {
        for document in self.documents(true).unwrap() {
            if document.location != Location::Builtin && document.errors.is_empty() {
                self.validate_version(&document.name, document.location, &document.revision)
                    .unwrap();
            }
        }
    }
    pub fn new(workspace: PathBuf, config_dir: PathBuf) -> Self {
        #[cfg(test)]
        let data = config_dir.join("test-data");
        #[cfg(not(test))]
        let data = crate::paths::data_dir().unwrap_or_else(|| config_dir.join("data"));
        Self::construct(workspace, config_dir, data, None)
    }
    pub fn with_data(workspace: PathBuf, config_dir: PathBuf, data: PathBuf) -> Self {
        let workspace = workspace.canonicalize().unwrap_or(workspace);
        let marker = ReloadMarker::new(data.clone(), &workspace);
        Self::construct(workspace, config_dir, data, Some(marker))
    }
    fn construct(
        workspace: PathBuf,
        config_dir: PathBuf,
        data_dir: PathBuf,
        reload_marker: Option<ReloadMarker>,
    ) -> Self {
        if let Some(marker) = &reload_marker {
            let mut state = marker.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Ok(generation) = marker.read(&mut state) {
                state.observed = generation;
            }
        }
        let mut migration_errors = vec![];
        for (location, source, target) in [
            (
                Location::Workspace,
                crate::paths::workspace_dir(&workspace),
                crate::paths::workspace_dir(&workspace).join("flows"),
            ),
            (
                Location::Global,
                config_dir.join("navigator"),
                config_dir.join("flows"),
            ),
        ] {
            if let Err(error) = super::migrate::migrate_directory(&source, &target) {
                let path = source.join("pipeline.toml").display().to_string();
                let message =
                    format!("{path}: legacy flow conversion failed: {error:#}; original retained");
                tracing::warn!(%message);
                migration_errors.push((location, Diagnostic::new("migration", message, path)));
            }
        }
        let store = Self {
            workspace,
            config_dir,
            data_dir,
            loaded: RwLock::new(Arc::new(BTreeMap::new())),
            revisions: RwLock::new(BTreeMap::new()),
            write: std::sync::Mutex::new(()),
            reload_marker,
            reload_errors: RwLock::new(vec![]),
            migration_errors,
        };
        if let Err(e) = store.reload_local() {
            store.record_reload_error(&e);
            tracing::warn!(
                diagnostics = e.errors.len(),
                "flow startup rejected; builtins only"
            );
            let builtins = super::builtin::all_definitions()
                .into_iter()
                .filter_map(|(definition, templates)| {
                    let name = definition.flow.name.clone();
                    definition
                        .validate(&catalogue(), &Capabilities { images: false }, &templates)
                        .ok()
                        .map(|compiled| {
                            (
                                name,
                                Arc::new(LoadedFlow {
                                    location: Location::Builtin,
                                    compiled,
                                }),
                            )
                        })
                })
                .collect();
            *store.loaded.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(builtins);
        }
        store
    }
    pub fn directory(&self, location: Location) -> Result<PathBuf, StoreError> {
        match location {
            Location::Workspace => Ok(crate::paths::workspace_dir(&self.workspace).join("flows")),
            Location::Global => Ok(self.config_dir.join("flows")),
            Location::Builtin => Err(StoreError::reason("read_only")),
        }
    }
    fn permission_store(&self) -> Result<crate::permission::store::PermissionStore, StoreError> {
        crate::permission::store::PermissionStore::open(&self.data_dir).map_err(validation_error)
    }
    pub fn validate_version(
        &self,
        name: &str,
        location: Location,
        expected: &str,
    ) -> Result<crate::permission::validation::Validation, StoreError> {
        if location == Location::Builtin {
            return Err(StoreError::reason("read_only"));
        }
        let document = self.get(name, location)?;
        if document.revision != expected {
            return Err(StoreError::reason("stale"));
        }
        if !document.errors.is_empty() {
            return Err(StoreError {
                reason: "invalid",
                errors: document.errors,
            });
        }
        let validation = self
            .permission_store()?
            .validate(
                crate::permission::validation::ValidationKind::Flow,
                name,
                &document.path,
                &document.revision,
            )
            .map_err(validation_error)?;
        Ok(validation)
    }
    pub fn validated(&self, document: &Document) -> Result<bool, StoreError> {
        if document.location == Location::Builtin {
            return Ok(true);
        }
        self.permission_store()?
            .is_valid(
                crate::permission::validation::ValidationKind::Flow,
                &document.path,
                &document.revision,
            )
            .map_err(validation_error)
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn get(&self, name: &str, location: Location) -> Result<Document, StoreError> {
        self.document_with_compiled(name, location).map(|(d, _)| d)
    }
    fn document_with_compiled(
        &self,
        name: &str,
        location: Location,
    ) -> Result<(Document, Option<CompiledDefinition>), StoreError> {
        check_name(name)?;
        if location == Location::Builtin {
            let (definition, templates) = super::builtin::all_definitions()
                .into_iter()
                .find(|(d, _)| d.flow.name == name)
                .ok_or_else(|| StoreError::reason("not_found"))?;
            let toml = definition.to_toml().map_err(|errors| StoreError {
                reason: "invalid",
                errors,
            })?;
            return Ok((
                Document {
                    name: name.into(),
                    location,
                    path: format!("builtin/{name}.toml"),
                    revision: revision(&toml, &templates),
                    toml,
                    definition: Some(definition),
                    templates,
                    errors: vec![],
                    saved_since_reload: false,
                    read_only: true,
                },
                None,
            ));
        }
        let path = self.directory(location)?.canonicalize().map_err(missing)?;
        let dir = Directory::open(&path, false).map_err(missing)?;
        let toml = dir
            .read(
                &format!("{name}.toml"),
                instinct_flow::MAX_FLOW_BYTES,
                false,
            )
            .map_err(missing)?;
        let (v, compiled) = load_toml(&dir, toml.clone());
        let revision = revision(&toml, &v.templates);
        let saved_since_reload = self
            .revisions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(location, name.into()))
            != Some(&revision);
        Ok((
            Document {
                name: name.into(),
                location,
                path: path
                    .join(format!("{name}.toml"))
                    .to_string_lossy()
                    .into_owned(),
                revision,
                toml,
                definition: v.definition,
                templates: v.templates,
                errors: v.errors,
                saved_since_reload,
                read_only: false,
            },
            compiled,
        ))
    }
    pub fn list(&self) -> Result<Vec<FlowSummary>, StoreError> {
        let mut summaries: Vec<_> = self
            .documents(true)?
            .iter()
            .map(Document::summary)
            .collect();
        summaries.extend(
            self.migration_errors
                .iter()
                .map(|(location, error)| FlowSummary {
                    name: "legacy-pipeline".into(),
                    trigger: None,
                    location: *location,
                    enabled: Some(false),
                    blocking: None,
                    saved_since_reload: false,
                    errors: vec![error.clone()],
                }),
        );
        Ok(summaries)
    }
    fn documents(&self, include_workspace: bool) -> Result<Vec<Document>, StoreError> {
        self.documents_with_compiled(include_workspace)
            .map(|documents| documents.into_iter().map(|(d, _)| d).collect())
    }
    fn documents_with_compiled(
        &self,
        include_workspace: bool,
    ) -> Result<Vec<(Document, Option<CompiledDefinition>)>, StoreError> {
        let mut documents = super::builtin::all_definitions()
            .into_iter()
            .map(|(d, _)| self.document_with_compiled(&d.flow.name, Location::Builtin))
            .collect::<Result<Vec<_>, _>>()?;
        for location in [Location::Global, Location::Workspace] {
            if location == Location::Workspace && !include_workspace {
                continue;
            }
            let path = self.directory(location)?;
            let dir = match Directory::open(&path, false) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for file in dir.names()? {
                if let Some(name) = file.strip_suffix(".toml") {
                    documents.push(self.document_with_compiled(name, location)?);
                }
            }
        }
        documents.sort_by(|(a, _), (b, _)| {
            (
                &a.definition.as_ref().map(|d| &d.flow.on),
                &a.name,
                a.location,
            )
                .cmp(&(
                    &b.definition.as_ref().map(|d| &d.flow.on),
                    &b.name,
                    b.location,
                ))
        });
        Ok(documents)
    }
    pub fn reload(&self) -> Result<(), StoreError> {
        let result = (|| {
            if let Some(marker) = &self.reload_marker {
                let mut state = marker.state.lock().map_err(|_| StoreError::reason("io"))?;
                self.reload_local()?;
                state.observed = marker.publish_locked(&mut state)?;
                Ok(())
            } else {
                self.reload_local()
            }
        })();
        if let Err(e) = &result {
            self.publish_builtins();
            self.record_reload_error(e);
        }
        result
    }
    fn record_reload_error(&self, error: &StoreError) {
        // Filesystem diagnostics may contain sensitive paths; keep propagation failures fixed.
        *self
            .reload_errors
            .write()
            .unwrap_or_else(|e| e.into_inner()) = if error.errors.is_empty() || error.reason == "io"
        {
            vec![Diagnostic::new(
                "reload",
                "flow reload or propagation failed",
                "",
            )]
        } else {
            error.errors.clone()
        };
    }
    pub fn reload_diagnostics(&self) -> Vec<Diagnostic> {
        let mut errors = self
            .reload_errors
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        errors.extend(self.migration_diagnostics());
        errors
    }
    pub fn migration_diagnostics(&self) -> Vec<Diagnostic> {
        self.migration_errors
            .iter()
            .map(|(_, error)| error.clone())
            .collect()
    }
    /// A marker, not flow-file watching: external edits remain inactive until explicit reload.
    pub fn refresh(&self) {
        let Some(marker) = &self.reload_marker else {
            return;
        };
        let mut state = marker.state.lock().unwrap_or_else(|e| e.into_inner());
        match marker.read(&mut state) {
            Ok(generation) if generation > state.observed => {
                if let Err(e) = self.reload_local() {
                    self.record_reload_error(&e);
                } else {
                    state.observed = generation;
                }
            }
            Ok(_) => {}
            Err(e) => {
                self.publish_builtins();
                self.record_reload_error(&e);
            }
        }
    }
    fn reload_local(&self) -> Result<(), StoreError> {
        // Never retain a custom admission from a previous attempt on an early
        // filesystem or database error. Existing evaluations own their Arc.
        self.publish_builtins();
        let _guard = self.write.lock().map_err(|_| StoreError::reason("io"))?;
        let documents = self.documents_with_compiled(true)?;
        let mut candidate = BTreeMap::new();
        let mut revisions = BTreeMap::new();
        let mut errors = vec![];
        let mut diagnostics = vec![];
        for (document, compiled) in documents {
            if !self.validated(&document)? {
                diagnostics.push(Diagnostic::new(
                    "validation",
                    "flow version is not validated; not loaded",
                    &document.path,
                ));
                continue;
            }
            errors.extend(document.errors.clone());
            if let Some(definition) = document.definition {
                if definition.flow.name != document.name {
                    errors.push(Diagnostic::new(
                        "schema",
                        "flow name must match filename",
                        "flow.name",
                    ));
                }
                let compiled = if document.location == Location::Builtin {
                    match definition.validate(
                        &catalogue(),
                        &Capabilities { images: false },
                        &document.templates,
                    ) {
                        Ok(compiled) => Some(compiled),
                        Err(e) => {
                            errors.extend(e);
                            None
                        }
                    }
                } else {
                    compiled
                };
                if let Some(compiled) = compiled {
                    revisions.insert(
                        (document.location, document.name.clone()),
                        document.revision,
                    );
                    let flow = Arc::new(LoadedFlow {
                        location: document.location,
                        compiled,
                    });
                    if candidate
                        .get(&document.name)
                        .is_none_or(|old: &Arc<LoadedFlow>| old.location < document.location)
                    {
                        candidate.insert(document.name, flow);
                    }
                }
            }
        }
        if candidate.len() > 64 {
            errors.push(Diagnostic::new("limit", "more than 64 loaded flows", ""));
        }
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(candidate);
        *self.revisions.write().unwrap_or_else(|e| e.into_inner()) = revisions;
        if !errors.is_empty() {
            return Err(StoreError {
                reason: "reload_invalid",
                errors,
            });
        }
        *self
            .reload_errors
            .write()
            .unwrap_or_else(|e| e.into_inner()) = diagnostics;
        Ok(())
    }
    fn publish_builtins(&self) {
        let snapshot = super::builtin::definitions()
            .into_iter()
            .filter_map(|(definition, templates)| {
                let name = definition.flow.name.clone();
                definition
                    .validate(&catalogue(), &Capabilities { images: false }, &templates)
                    .ok()
                    .map(|compiled| {
                        (
                            name,
                            Arc::new(LoadedFlow {
                                location: Location::Builtin,
                                compiled,
                            }),
                        )
                    })
            })
            .collect();
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(snapshot);
        self.revisions
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    pub fn save(
        &self,
        name: &str,
        location: Location,
        expected: Option<&str>,
        content: Content,
    ) -> Result<Document, StoreError> {
        check_name(name)?;
        let _guard = self.write.lock().map_err(|_| StoreError::reason("io"))?;
        if location == Location::Builtin {
            return Err(StoreError::reason("read_only"));
        }
        let directory = self.directory(location)?;
        let dir = Directory::open(&directory, true)?;
        let resource = super::fs::resource_lock(&directory.join(".flows.lock"));
        let _resource_guard = resource.lock().map_err(|_| StoreError::reason("io"))?;
        let _file_lock = dir.lock(".flows.lock")?;
        match (self.get(name, location), expected) {
            (Ok(_), None) => return Err(StoreError::reason("already_exists")),
            (Ok(d), Some(r)) if d.revision != r => return Err(StoreError::reason("stale")),
            (Err(e), Some(_)) => return Err(e),
            (Err(e), None) if e.reason != "not_found" => return Err(e),
            _ => {}
        }
        let graph = matches!(&content, Content::Graph { .. });
        let v = validate(content);
        if graph && !v.valid {
            return Err(StoreError {
                reason: "invalid",
                errors: v.errors,
            });
        }
        if v.definition.as_ref().is_some_and(|d| d.flow.name != name) {
            return Err(StoreError::reason("invalid"));
        }
        let text = v.toml.ok_or_else(|| StoreError::reason("invalid"))?;
        if text.len() > instinct_flow::MAX_FLOW_BYTES
            || v.templates
                .values()
                .any(|t| t.len() > instinct_flow::MAX_TEMPLATE_BYTES)
        {
            return Err(StoreError::reason("invalid"));
        }
        // Validate the entire write set before opening or writing any destination.
        for template in v.templates.keys() {
            if template.is_empty()
                || template.len() > 128
                || template.starts_with('.')
                || template.ends_with(".toml")
                || !template
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                return Err(StoreError::reason("invalid"));
            }
        }
        let previous = self.get(name, location).ok();
        for (template, body) in &v.templates {
            match dir.read(template, instinct_flow::MAX_TEMPLATE_BYTES, false) {
                Ok(current) if &current != body => {
                    // The document revision covers only its original referenced templates.
                    // Never overwrite an existing file outside that checked write set.
                    if !previous
                        .as_ref()
                        .is_some_and(|d| d.templates.contains_key(template))
                    {
                        return Err(StoreError::reason("stale"));
                    }
                    for file in dir.names()? {
                        if let Some(other) = file.strip_suffix(".toml") {
                            if other == name {
                                continue;
                            }
                            let document = self.get(other, location)?;
                            if document.templates.contains_key(template) {
                                return Err(StoreError::reason("stale"));
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        for (template, body) in &v.templates {
            dir.write(template, body.as_bytes())?;
        }
        dir.write(&format!("{name}.toml"), text.as_bytes())?;
        self.get(name, location)
    }
    pub fn delete(&self, name: &str, location: Location, expected: &str) -> Result<(), StoreError> {
        let _guard = self.write.lock().map_err(|_| StoreError::reason("io"))?;
        let directory = self.directory(location)?;
        let dir = Directory::open(&directory, false)?;
        let resource = super::fs::resource_lock(&directory.join(".flows.lock"));
        let _resource_guard = resource.lock().map_err(|_| StoreError::reason("io"))?;
        let _file_lock = dir.lock(".flows.lock")?;
        let document = self.get(name, location)?;
        if document.revision != expected {
            return Err(StoreError::reason("stale"));
        }
        dir.remove(&format!("{name}.toml"))?;
        Ok(())
    }
}
fn validation_error(error: crate::permission::store::Error) -> StoreError {
    StoreError {
        reason: "io",
        errors: vec![Diagnostic::new("validation", error.to_string(), "")],
    }
}
fn missing(e: std::io::Error) -> StoreError {
    if e.kind() == std::io::ErrorKind::NotFound {
        StoreError::reason("not_found")
    } else {
        e.into()
    }
}
pub fn check_name(name: &str) -> Result<(), StoreError> {
    if name.len() > 64
        || name.is_empty()
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        Err(StoreError::reason("invalid"))
    } else {
        Ok(())
    }
}
fn revision(text: &str, templates: &BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update((text.len() as u64).to_le_bytes());
    h.update(text.as_bytes());
    for (name, body) in templates {
        h.update((name.len() as u64).to_le_bytes());
        h.update(name);
        h.update((body.len() as u64).to_le_bytes());
        h.update(body);
    }
    format!("sha256:{}", hex::encode(h.finalize()))
}
pub(crate) fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_reload_drops_revoked_admissions_and_refresh_retries() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let config = temp.path().join("config");
        let writer = FlowStore::with_data(temp.path().into(), config.clone(), data.clone());
        let reader = FlowStore::with_data(temp.path().into(), config.clone(), data.clone());
        let doc = writer
            .save(
                "guard",
                Location::Global,
                None,
                content("guard", "no_objection"),
            )
            .unwrap();
        let approval = writer
            .validate_version("guard", Location::Global, &doc.revision)
            .unwrap();
        writer.reload().unwrap();
        reader.refresh();
        assert!(reader.snapshot().contains_key("guard"));
        writer
            .permission_store()
            .unwrap()
            .revoke_validation(approval.id)
            .unwrap();
        let broken = config.join("flows/broken.toml");
        std::fs::write(&broken, "x".repeat(MAX_FLOW_BYTES + 1)).unwrap();
        assert!(writer.reload().is_err());
        assert!(writer
            .snapshot()
            .values()
            .all(|flow| flow.location == Location::Builtin));
        writer.reload_marker.as_ref().unwrap().publish().unwrap();
        reader.refresh();
        assert!(!reader.snapshot().contains_key("guard"));
        assert!(!reader.reload_diagnostics().is_empty());
        std::fs::remove_file(broken).unwrap();
        writer
            .validate_version("guard", Location::Global, &doc.revision)
            .unwrap();
        // No new generation: the failed refresh must retry the previous one.
        reader.refresh();
        assert!(reader.snapshot().contains_key("guard"));
        writer
            .permission_store()
            .unwrap()
            .db()
            .execute_batch("DROP TABLE validations; CREATE TABLE validations (invalid TEXT)")
            .unwrap();
        assert!(reader.reload().is_err());
        assert!(reader
            .snapshot()
            .values()
            .all(|flow| flow.location == Location::Builtin));
    }
    #[test]
    fn approvals_bind_loaded_toml_templates_and_location_and_revoke_on_reload() {
        let temp = tempfile::tempdir().unwrap();
        let store = FlowStore::with_data(
            temp.path().into(),
            temp.path().join("config"),
            temp.path().join("data"),
        );
        let builtins = store.snapshot().len();
        for location in [Location::Global, Location::Workspace] {
            let document = store.save("check", location, None, Content::Toml {
                toml: "[flow]\nversion=1\nname='check'\non='save'\nstart=['a']\n[[node]]\nid='a'\ntype='noul'\ninstructions='test'\nthreshold=0.5\nstate_template='state.txt'\n".into(),
                templates: BTreeMap::from([("state.txt".into(), "original".into())]),
            }).unwrap();
            store.reload().unwrap();
            assert!(!store.snapshot().contains_key("check"));
            assert!(!store.reload_diagnostics().is_empty());
            let approval = store
                .validate_version("check", location, &document.revision)
                .unwrap();
            store.reload().unwrap();
            assert_eq!(store.snapshot()["check"].location, location);
            let directory = store.directory(location).unwrap();
            std::fs::write(directory.join("state.txt"), "changed").unwrap();
            // The displayed snapshot remains immutable even after disk edits.
            assert_eq!(
                revision(&document.toml, &document.templates),
                document.revision
            );
            assert_eq!(
                store
                    .validate_version("check", location, &document.revision)
                    .unwrap_err()
                    .reason,
                "stale"
            );
            store.reload().unwrap();
            assert!(!store.snapshot().contains_key("check"));
            let changed = store.get("check", location).unwrap();
            store
                .validate_version("check", location, &changed.revision)
                .unwrap();
            store.reload().unwrap();
            assert!(store.snapshot().contains_key("check"));
            std::fs::write(
                directory.join("check.toml"),
                format!("{}\n# changed", changed.toml),
            )
            .unwrap();
            store.reload().unwrap();
            assert!(!store.snapshot().contains_key("check"));
            let changed = store.get("check", location).unwrap();
            let current = store
                .validate_version("check", location, &changed.revision)
                .unwrap();
            store.reload().unwrap();
            store
                .permission_store()
                .unwrap()
                .revoke_validation(current.id)
                .unwrap();
            store
                .permission_store()
                .unwrap()
                .revoke_validation(approval.id)
                .unwrap();
            store.reload().unwrap();
            assert!(!store.snapshot().contains_key("check"));
        }
        assert_eq!(store.snapshot().len(), builtins);
    }
    #[test]
    fn concurrent_markers_advance_monotonically_and_reject_unsafe_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let markers: Vec<_> = (0..16)
            .map(|_| ReloadMarker::new(data.clone(), temp.path()))
            .collect();
        std::thread::scope(|scope| {
            for marker in &markers {
                scope.spawn(move || marker.publish().unwrap());
            }
        });
        let marker = &markers[0];
        let mut state = marker.state.lock().unwrap();
        assert_eq!(marker.read(&mut state).unwrap(), 16);
        let path = marker.path.join(&marker.name);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(marker.read(&mut state).is_err());
        std::fs::remove_file(&path).unwrap();
        symlink(temp.path().join("external"), &path).unwrap();
        assert!(marker.read(&mut state).is_err());
    }
    #[test]
    fn template_write_set_refuses_definitions_and_unchecked_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = FlowStore::new(temp.path().into(), temp.path().join("config"));
        let mut draft = content("a", "no_objection");
        if let Content::Toml { templates, .. } = &mut draft {
            templates.insert("b.toml".into(), "overwrite".into());
        }
        assert_eq!(
            store
                .save("a", Location::Global, None, draft)
                .unwrap_err()
                .reason,
            "invalid"
        );
        assert!(!temp.path().join("config/flows/b.toml").exists());
        let dir = Directory::open(&temp.path().join("config/flows"), true).unwrap();
        dir.write("shared.txt", b"external change").unwrap();
        let mut draft = content("a", "no_objection");
        if let Content::Toml { templates, .. } = &mut draft {
            templates.insert("shared.txt".into(), "old".into());
        }
        assert_eq!(
            store
                .save("a", Location::Global, None, draft)
                .unwrap_err()
                .reason,
            "stale"
        );
        assert_eq!(
            dir.read("shared.txt", 100, false).unwrap(),
            "external change"
        );
    }
    #[test]
    fn shared_template_modification_requires_an_unshared_write_set() {
        let temp = tempfile::tempdir().unwrap();
        let store = FlowStore::new(temp.path().into(), temp.path().join("config"));
        let draft = |name: &str, body: &str| {
            Content::Toml {
            toml: format!("[flow]\nversion=1\nname='{name}'\non='save'\nstart=['a']\n[[node]]\nid='a'\ntype='noul'\ninstructions='test'\nthreshold=0.5\nstate_template='shared.txt'\n"),
            templates: BTreeMap::from([("shared.txt".into(), body.into())]),
        }
        };
        let a = store
            .save("a", Location::Global, None, draft("a", "original"))
            .unwrap();
        store
            .save("b", Location::Global, None, draft("b", "original"))
            .unwrap();
        assert_eq!(
            store
                .save(
                    "a",
                    Location::Global,
                    Some(&a.revision),
                    draft("a", "changed")
                )
                .unwrap_err()
                .reason,
            "stale"
        );
        let dir = Directory::open(&store.directory(Location::Global).unwrap(), false).unwrap();
        dir.write("shared.txt", b"external").unwrap();
        assert_eq!(
            store
                .save(
                    "a",
                    Location::Global,
                    Some(&a.revision),
                    draft("a", "original")
                )
                .unwrap_err()
                .reason,
            "stale"
        );
        assert_eq!(dir.read("shared.txt", 100, false).unwrap(), "external");
    }
    fn content(name: &str, action: &str) -> Content {
        Content::Toml {toml:format!("[flow]\nversion=1\nname='{name}'\non='tool.pre_call'\nblocking=true\nbudget_ms=500\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='{action}'\n[node.params]\n"),templates:BTreeMap::new()}
    }
    #[test]
    fn precedence_revocation_and_failed_reload_keep_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path().join("repo");
        let config = temp.path().join("config");
        std::fs::create_dir(&ws).unwrap();
        let store = FlowStore::new(ws, config);
        store
            .save(
                "guard",
                Location::Global,
                None,
                content("guard", "no_objection"),
            )
            .unwrap();
        store
            .save(
                "guard",
                Location::Workspace,
                None,
                content("guard", "no_objection"),
            )
            .unwrap();
        let doc = store.get("guard", Location::Global).unwrap();
        store
            .validate_version("guard", Location::Global, &doc.revision)
            .unwrap();
        store.reload().unwrap();
        assert_eq!(store.snapshot()["guard"].location, Location::Global);
        let doc = store.get("guard", Location::Workspace).unwrap();
        store
            .validate_version("guard", Location::Workspace, &doc.revision)
            .unwrap();
        store.reload().unwrap();
        assert_eq!(store.snapshot()["guard"].location, Location::Workspace);
        let doc = store.get("guard", Location::Workspace).unwrap();
        let approval = store
            .validate_version("guard", Location::Workspace, &doc.revision)
            .unwrap();
        let permissions = store.permission_store().unwrap();
        for validation in permissions.list_validations().unwrap() {
            if validation.location == approval.location {
                permissions.revoke_validation(validation.id).unwrap();
            }
        }
        store.reload().unwrap();
        assert_eq!(store.snapshot()["guard"].location, Location::Global);
        assert!(!store.reload_diagnostics().is_empty());
    }
}
