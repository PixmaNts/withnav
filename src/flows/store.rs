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
impl FlowStore {
    pub fn new(workspace: PathBuf, config_dir: PathBuf) -> Self {
        let store = Self {
            workspace,
            config_dir,
            loaded: RwLock::new(Arc::new(BTreeMap::new())),
            revisions: RwLock::new(BTreeMap::new()),
            write: std::sync::Mutex::new(()),
        };
        if let Err(e) = store.reload() {
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
            Location::Workspace => Ok(self.workspace.join(".navai/flows")),
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
        Ok(())
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn get(&self, name: &str, location: Location) -> Result<Document, StoreError> {
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
            return Ok(Document {
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
            });
        }
        let path = self.directory(location)?;
        let dir = Directory::open(&path, false).map_err(missing)?;
        let toml = dir
            .read(&format!("{name}.toml"), 262144, false)
            .map_err(missing)?;
        let parsed = Definition::parse(&toml);
        let mut templates = BTreeMap::new();
        let mut extra = vec![];
        if let Ok(d) = &parsed {
            for node in &d.node {
                if let Some(name) = node.kind.template() {
                    match dir.read(name, 65536, false) {
                        Ok(text) => {
                            templates.insert(name.into(), text);
                        }
                        Err(e) => extra.push(Diagnostic::new("io", e.to_string(), name)),
                    }
                }
            }
        }
        let v = validate(Content::Toml {
            toml: toml.clone(),
            templates: templates.clone(),
        });
        let revision = revision(&toml, &templates);
        let saved_since_reload = self
            .revisions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(location, name.into()))
            != Some(&revision);
        let mut errors = v.errors;
        errors.extend(extra);
        Ok(Document {
            name: name.into(),
            location,
            path: path
                .join(format!("{name}.toml"))
                .to_string_lossy()
                .into_owned(),
            revision,
            toml,
            definition: v.definition,
            templates,
            errors,
            saved_since_reload,
            read_only: false,
        })
    }
    pub fn list(&self) -> Result<Vec<FlowSummary>, StoreError> {
        Ok(self
            .documents(true)?
            .iter()
            .map(Document::summary)
            .collect())
    }
    fn documents(&self, include_workspace: bool) -> Result<Vec<Document>, StoreError> {
        let mut documents = super::builtin::definitions()
            .into_iter()
            .map(|(d, _)| self.get(&d.flow.name, Location::Builtin))
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
                    documents.push(self.get(name, location)?);
                }
            }
        }
        documents.sort_by(|a, b| {
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
        let _guard = self.write.lock().map_err(|_| StoreError::reason("io"))?;
        let trusted = self.trusted();
        let documents = self.documents(trusted)?;
        let mut candidate = BTreeMap::new();
        let mut revisions = BTreeMap::new();
        let mut errors = vec![];
        for document in documents {
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
                match definition.validate(
                    &catalogue(),
                    &Capabilities { images: false },
                    &document.templates,
                ) {
                    Ok(compiled) => {
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
                    Err(e) => errors.extend(e),
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
        if text.len() > 262144 || v.templates.values().any(|t| t.len() > 65536) {
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
            match dir.read(template, 65536, false) {
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
        std::fs::create_dir_all(temp.path().join(".navai")).unwrap();
        symlink("/not-readable", temp.path().join(".navai/flows")).unwrap();
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
