//! Disk documents and atomic, immutable activation snapshots.
use super::{catalogue::catalogue, fs::Directory};
use instinct_flow::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
            name: format!(
                "{:x}",
                Sha256::digest(workspace.as_os_str().as_encoded_bytes())
            ),
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
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    version: u32,
    workspaces: BTreeMap<String, Grant>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    trusted: bool,
    granted_at: String,
}
/// Read-only trust inspection: does not instantiate a store or create reload state.
pub fn inspect_trust(
    config: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<bool, StoreError> {
    let dir = match Directory::open(config, false) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let text = match dir.read("flows-trust.json", 262144, true) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let file: TrustFile = serde_json::from_str(&text).map_err(|_| StoreError::reason("invalid"))?;
    if file.version != 1 {
        return Err(StoreError::reason("invalid"));
    }
    Ok(file
        .workspaces
        .get(&workspace.to_string_lossy().to_string())
        .is_some_and(|g| g.trusted))
}

/// Validate local documents with the runtime's no-follow access and byte/entry limits.
/// The caller must not invoke this for an untrusted workspace directory.
pub fn inspect_directory(path: &std::path::Path) -> Result<Vec<(String, bool)>, StoreError> {
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
        results.push((file, load_toml(&dir, toml).0.valid));
    }
    Ok(results)
}
impl FlowStore {
    pub fn new(workspace: PathBuf, config_dir: PathBuf) -> Self {
        Self::construct(workspace, config_dir, None)
    }
    pub fn with_data(workspace: PathBuf, config_dir: PathBuf, data: PathBuf) -> Self {
        let workspace = workspace.canonicalize().unwrap_or(workspace);
        let marker = ReloadMarker::new(data, &workspace);
        Self::construct(workspace, config_dir, Some(marker))
    }
    fn construct(
        workspace: PathBuf,
        config_dir: PathBuf,
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
            let builtins = super::builtin::definitions()
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
    fn trust_file(&self) -> Result<TrustFile, StoreError> {
        let text =
            Directory::open(&self.config_dir, false)?.read("flows-trust.json", 262144, true)?;
        let file: TrustFile = serde_json::from_str(&text).map_err(|_| StoreError {
            reason: "io",
            errors: vec![Diagnostic::new("trust", "malformed trust file", "")],
        })?;
        if file.version != 1 {
            return Err(StoreError::reason("io"));
        }
        Ok(file)
    }
    pub fn trusted(&self) -> bool {
        self.trust_file()
            .ok()
            .and_then(|f| {
                f.workspaces
                    .get(&self.workspace.to_string_lossy().to_string())
                    .map(|g| g.trusted)
            })
            .unwrap_or(false)
    }
    pub fn set_trust(&self, trusted: bool) -> Result<(), StoreError> {
        let dir = Directory::open(&self.config_dir, true)?;
        let lock = super::fs::resource_lock(&self.config_dir.join("flows-trust.json"));
        let _guard = lock.lock().map_err(|_| StoreError::reason("io"))?;
        let _file_lock = dir.lock(".flows-trust.lock")?;
        let mut file = match dir.read("flows-trust.json", 262144, true) {
            Ok(_) => self.trust_file()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => TrustFile {
                version: 1,
                workspaces: BTreeMap::new(),
            },
            Err(e) => return Err(e.into()),
        };
        file.workspaces.insert(
            self.workspace.to_string_lossy().into_owned(),
            Grant {
                trusted,
                granted_at: timestamp(),
            },
        );
        dir.write(
            "flows-trust.json",
            &serde_json::to_vec(&file).map_err(|_| StoreError::reason("io"))?,
        )?;
        if let Some(marker) = &self.reload_marker {
            marker.publish()?;
        }
        Ok(())
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
            let (definition, templates) = super::builtin::definitions()
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
        let path = self.directory(location)?;
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
        let mut documents = super::builtin::definitions()
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
                state.observed = generation;
                if let Err(e) = self.reload_local() {
                    self.record_reload_error(&e);
                }
            }
            Ok(_) => {}
            Err(e) => self.record_reload_error(&e),
        }
    }
    fn reload_local(&self) -> Result<(), StoreError> {
        let _guard = self.write.lock().map_err(|_| StoreError::reason("io"))?;
        let trusted = self.trusted();
        let documents = self.documents_with_compiled(trusted)?;
        let mut candidate = BTreeMap::new();
        let mut revisions = BTreeMap::new();
        let mut errors = vec![];
        for (document, compiled) in documents {
            if document.location == Location::Workspace && !trusted {
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
        if !errors.is_empty() {
            return Err(StoreError {
                reason: "reload_invalid",
                errors,
            });
        }
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(candidate);
        *self.revisions.write().unwrap_or_else(|e| e.into_inner()) = revisions;
        self.reload_errors
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(())
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
    format!("sha256:{:x}", h.finalize())
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
    fn untrusted_files_are_excluded_before_enumeration() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let store = FlowStore::new(temp.path().into(), temp.path().join("config"));
        store
            .save(
                "guard",
                Location::Global,
                None,
                content("guard", "no_objection"),
            )
            .unwrap();
        std::fs::create_dir_all(temp.path().join(".withnav")).unwrap();
        symlink("/not-readable", temp.path().join(".withnav/flows")).unwrap();
        store.reload().unwrap();
        assert!(store.snapshot().contains_key("guard"));
        let restarted = FlowStore::new(temp.path().into(), temp.path().join("config"));
        assert!(restarted.snapshot().contains_key("guard"));
    }
    #[test]
    fn independent_stores_serialize_trust_updates() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let stores: Vec<_> = (0..16)
            .map(|i| FlowStore::new(temp.path().join(format!("repo-{i}")), config.clone()))
            .collect();
        std::thread::scope(|scope| {
            for store in &stores {
                scope.spawn(move || {
                    store.set_trust(true).unwrap();
                });
            }
        });
        assert!(stores.iter().all(FlowStore::trusted));
        std::thread::scope(|scope| {
            for store in &stores {
                scope.spawn(move || {
                    store.set_trust(false).unwrap();
                });
            }
        });
        assert!(stores.iter().all(|s| !s.trusted()));
        assert_eq!(stores[0].trust_file().unwrap().workspaces.len(), 16);
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
        store.reload().unwrap();
        assert_eq!(store.snapshot()["guard"].location, Location::Global);
        store.set_trust(true).unwrap();
        store.reload().unwrap();
        assert_eq!(store.snapshot()["guard"].location, Location::Workspace);
        let old = store.snapshot();
        let doc = store.get("guard", Location::Global).unwrap();
        store
            .save(
                "guard",
                Location::Global,
                Some(&doc.revision),
                Content::Toml {
                    toml: "not toml".into(),
                    templates: BTreeMap::new(),
                },
            )
            .unwrap();
        assert!(store.reload().is_err());
        assert!(Arc::ptr_eq(&old, &store.snapshot()));
        let doc = store.get("guard", Location::Global).unwrap();
        store
            .save(
                "guard",
                Location::Global,
                Some(&doc.revision),
                content("guard", "unknown_action"),
            )
            .unwrap();
        let diagnostics = store.get("guard", Location::Global).unwrap().errors;
        assert!(!diagnostics.is_empty());
        let failed = store.reload().unwrap_err();
        assert_eq!(failed.reason, "reload_invalid");
        assert_eq!(failed.errors.len(), diagnostics.len());
        for diagnostic in diagnostics {
            assert_eq!(
                failed.errors.iter().filter(|e| **e == diagnostic).count(),
                1,
                "each validation diagnostic must appear once"
            );
        }
        assert!(Arc::ptr_eq(&old, &store.snapshot()));
        store.set_trust(false).unwrap();
        assert!(!store.trusted());
    }
    #[test]
    fn trust_is_fail_closed_and_ancestor_links_are_refused() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let store = FlowStore::new(temp.path().into(), config.clone());
        assert!(!store.trusted());
        store.set_trust(true).unwrap();
        assert!(store.trusted());
        std::fs::set_permissions(
            config.join("flows-trust.json"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(!store.trusted());
        std::fs::set_permissions(
            config.join("flows-trust.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(config.join("flows-trust.json"), "oops").unwrap();
        assert!(!store.trusted());
        symlink(&config, temp.path().join("link")).unwrap();
        assert!(Directory::open(&temp.path().join("link/flows"), true).is_err());
    }
}
