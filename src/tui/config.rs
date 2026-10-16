//! Settings wire contracts and pure editing state; never reads configuration files.
use super::{
    protocol::{AgentEntry, ApiKind, Effect, ModelEntry},
    state::{State, UiState},
    theme::ThemeId,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Session,
    Config,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingField {
    pub key: String,
    pub value: Value,
    pub default: Value,
    pub set: bool,
    pub error: Option<String>,
    pub applies: String,
}
impl SettingField {
    pub fn effective(&self) -> &Value {
        if self.set { &self.value } else { &self.default }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsPaths {
    pub config: String,
    pub models: Option<String>,
    pub agents: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayInfo {
    pub name: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub key_source: String,
    pub catalog_format: Option<String>,
    pub connect_timeout_ms: Option<u64>,
    pub request_timeout_ms: Option<u64>,
    pub headers: Vec<String>,
    pub model_count: Option<u64>,
    pub used_by: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsBody {
    pub revision: String,
    pub paths: SettingsPaths,
    pub fields: Vec<SettingField>,
    pub gateways: Vec<GatewayInfo>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayTest {
    pub ok: bool,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub model_count: Option<u64>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WriteRequest {
    pub revision: String,
    pub key: String,
    pub value: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("config.json changed elsewhere")]
    Stale,
    #[error("{message}")]
    Invalid { key: String, message: String },
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Gateways,
    Layers,
    Agents,
    Permissions,
    Subagents,
    Appearance,
}
impl Section {
    pub const ALL: [Self; 6] = [
        Self::Gateways,
        Self::Layers,
        Self::Agents,
        Self::Permissions,
        Self::Subagents,
        Self::Appearance,
    ];
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
    pub fn label(self) -> &'static str {
        [
            "Gateways",
            "Layers",
            "Agents",
            "Permissions",
            "Subagents",
            "Appearance",
        ][self.index()]
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Model(ApiKind),
    Choice,
    Integer { min: i64, max: i64 },
    Bool,
    StringList,
    ReadOnly,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRow {
    /// Display key; `write_key` is the whole assignment for model/variant rows.
    pub key: String,
    pub write_key: String,
    pub label: String,
    pub group: String,
    pub section: Section,
    pub kind: RowKind,
    pub allowed: Vec<Value>,
    pub saved: Value,
    pub applies: String,
    pub gateway: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsTarget {
    pub key: String,
    pub api: ApiKind,
    pub title: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SaveStatus {
    #[default]
    UpToDate,
    Unsaved(usize),
    Writing,
    Written,
    StaleReloaded(usize),
    Error {
        key: String,
        message: String,
    },
}
impl SaveStatus {
    pub fn text(&self) -> String {
        match self {
            Self::UpToDate => "up to date".into(),
            Self::Unsaved(n) => format!("{n} unsaved"),
            Self::Writing => "writing…".into(),
            Self::Written => "written".into(),
            Self::StaleReloaded(n) => {
                format!("config.json changed elsewhere; reloaded · {n} still staged")
            }
            Self::Error { message, .. } => format!("Not saved: {message}"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaveChoice {
    Write,
    Discard,
    Stay,
}
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigAction {
    Jump(Section),
    CycleSection(i32),
    Move(i32),
    Step(i32),
    Toggle,
    Edit,
    Stage { key: String, value: Value },
    Revert,
    Write,
    Leave,
    ResolveLeave(LeaveChoice),
    StartFilter,
    Filter(String),
    FilterDone,
    StartList,
    ListText(String),
    CommitList,
    CancelList,
    PickModel(String),
    TestGateway,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConfigState {
    pub body: Option<SettingsBody>,
    pub section: Section,
    /// Index into `visible_rows`, not the unfiltered row list.
    pub row: usize,
    pub filter: String,
    pub filtering: bool,
    /// Sorted keys are the write order; one request at a time carries the latest revision.
    pub staged: BTreeMap<String, Value>,
    pub status: SaveStatus,
    pub field_errors: BTreeMap<String, String>,
    pub gateway_tests: BTreeMap<String, Result<GatewayTest, String>>,
    pub testing_gateway: Option<String>,
    pub leave_prompt: bool,
    pub leave_after_write: bool,
    pub list_editor: Option<String>,
    pub writing: Option<WriteRequest>,
    /// The cached revision cannot be used until a fetch succeeds.
    pub stale_reload: bool,
    /// Only the latest fetch may replace the cached settings.
    pub loading: Option<u64>,
    pub request: u64,
}

impl ConfigState {
    pub fn value(&self, key: &str) -> Value {
        if let Some(v) = self.staged.get(key) {
            if !v.is_null() {
                return v.clone();
            }
            return self
                .body
                .as_ref()
                .and_then(|b| b.fields.iter().find(|f| f.key == key))
                .map(|f| f.default.clone())
                .unwrap_or_default();
        }
        self.body
            .as_ref()
            .and_then(|b| b.fields.iter().find(|f| f.key == key))
            .map(|f| f.effective().clone())
            .unwrap_or_default()
    }
    pub fn stage(&mut self, key: String, value: Value) {
        if self.writing.is_some() {
            return;
        }
        let Some(field) = self
            .body
            .as_ref()
            .and_then(|b| b.fields.iter().find(|f| f.key == key))
        else {
            return;
        };
        if (value.is_null() && !field.set) || (field.set && field.value == value) {
            self.staged.remove(&key);
        } else {
            self.staged.insert(key.clone(), value);
        }
        self.field_errors.remove(&key);
        self.status = if self.staged.is_empty() {
            SaveStatus::UpToDate
        } else {
            SaveStatus::Unsaved(self.staged.len())
        };
    }
    pub fn rows(&self, catalog: &[ModelEntry], agents: &[AgentEntry]) -> Vec<ConfigRow> {
        let Some(body) = &self.body else {
            return vec![];
        };
        let mut rows = Vec::new();
        for g in &body.gateways {
            for (key, label, value) in [
                ("base_url", "Base URL", json!(g.base_url)),
                ("api_key_env", "API key env", json!(g.api_key_env)),
                ("key_source", "Key source", json!(g.key_source)),
                ("catalog_format", "Catalog format", json!(g.catalog_format)),
                (
                    "connect_timeout_ms",
                    "Connect timeout",
                    json!(g.connect_timeout_ms),
                ),
                (
                    "request_timeout_ms",
                    "Request timeout",
                    json!(g.request_timeout_ms),
                ),
            ] {
                rows.push(ConfigRow {
                    key: format!("gateways.{}.{key}", g.name),
                    write_key: String::new(),
                    label: label.into(),
                    group: g.name.clone(),
                    section: Section::Gateways,
                    kind: RowKind::ReadOnly,
                    allowed: vec![],
                    saved: value,
                    applies: String::new(),
                    gateway: Some(g.name.clone()),
                });
            }
        }
        for field in &body.fields {
            let key = &field.key;
            let (section, label, kind, allowed) = match key.as_str() {
                "layers.reflex" => (
                    Section::Layers,
                    "Model",
                    RowKind::Model(ApiKind::Fim),
                    vec![],
                ),
                "layers.instinct" => (
                    Section::Layers,
                    "Model",
                    RowKind::Model(ApiKind::SystemOne),
                    vec![],
                ),
                "layers.metacognition" => (
                    Section::Layers,
                    "Model",
                    RowKind::Model(ApiKind::Chat),
                    vec![],
                ),
                "default_agent" => (
                    Section::Layers,
                    "Default agent",
                    RowKind::Choice,
                    agents
                        .iter()
                        .filter(|a| matches!(a.mode.as_str(), "primary" | "all"))
                        .map(|a| json!(a.id))
                        .collect(),
                ),
                "permissions.scratch_dirs" => (
                    Section::Permissions,
                    "Scratch directories",
                    RowKind::StringList,
                    vec![],
                ),
                "subagents.max_concurrent" => (
                    Section::Subagents,
                    "Max concurrent per session",
                    RowKind::Integer { min: 1, max: 64 },
                    (1..=64).map(|n| json!(n)).collect(),
                ),
                "subagents.watchdog_interval_secs" => (
                    Section::Subagents,
                    "Subagent watchdog",
                    RowKind::Choice,
                    [0, 60, 300, 900, 3600].map(|n| json!(n)).to_vec(),
                ),
                "agent.watchdog_interval_secs" => (
                    Section::Subagents,
                    "Root agent watchdog",
                    RowKind::Choice,
                    [0, 60, 300, 900, 3600].map(|n| json!(n)).to_vec(),
                ),
                "tui.theme" => (
                    Section::Appearance,
                    "Theme",
                    RowKind::Choice,
                    ThemeId::ALL.map(|t| json!(t.id())).to_vec(),
                ),
                _ if key.starts_with("agents.") => (
                    Section::Agents,
                    "Model",
                    RowKind::Model(ApiKind::Chat),
                    vec![],
                ),
                _ => continue,
            };
            let group = key
                .strip_prefix("layers.")
                .or_else(|| key.strip_prefix("agents."))
                .unwrap_or(section.label())
                .to_string();
            let model = matches!(kind, RowKind::Model(_));
            rows.push(ConfigRow {
                key: if model {
                    format!("{key}.model")
                } else {
                    key.clone()
                },
                write_key: key.clone(),
                label: label.into(),
                group: group.clone(),
                section,
                kind,
                allowed,
                saved: if model {
                    model_name(field.effective())
                        .map(|name| json!(name))
                        .unwrap_or_default()
                } else {
                    field.effective().clone()
                },
                applies: field.applies.clone(),
                gateway: None,
            });
            if key == "layers.metacognition" || key.starts_with("agents.") {
                let value = self.value(key);
                let name = model_name(&value);
                let mut allowed = vec![Value::Null, json!("off")];
                if let Some(m) = catalog.iter().find(|m| Some(m.name.as_str()) == name) {
                    allowed.extend(
                        m.variants
                            .iter()
                            .filter(|v| v.as_str() != "off")
                            .map(|v| json!(v)),
                    );
                }
                rows.push(ConfigRow {
                    key: format!("{key}.variant"),
                    write_key: key.clone(),
                    label: "Variant".into(),
                    group: group.clone(),
                    section,
                    kind: RowKind::Choice,
                    allowed,
                    saved: field
                        .effective()
                        .get("variant")
                        .cloned()
                        .unwrap_or_default(),
                    applies: field.applies.clone(),
                    gateway: None,
                });
            }
            if let Some(id) = key.strip_prefix("agents.")
                && let Some(a) = agents.iter().find(|a| a.id == id)
            {
                rows.push(ConfigRow {
                    key: format!("{key}.mode"),
                    write_key: String::new(),
                    label: "Mode".into(),
                    group,
                    section,
                    kind: RowKind::ReadOnly,
                    allowed: vec![],
                    saved: json!(a.mode),
                    applies: String::new(),
                    gateway: None,
                });
            }
        }
        rows.sort_by_key(|r| {
            (
                r.section.index(),
                match r.write_key.as_str() {
                    "layers.reflex" => 0,
                    "layers.instinct" => 1,
                    "layers.metacognition" => 2,
                    "default_agent" => 3,
                    _ => 4,
                },
            )
        });
        rows
    }
    pub fn visible_rows(&self, catalog: &[ModelEntry], agents: &[AgentEntry]) -> Vec<ConfigRow> {
        let words: Vec<_> = self
            .filter
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        self.rows(catalog, agents)
            .into_iter()
            .filter(|r| {
                r.section == self.section
                    && words.iter().all(|w| {
                        format!("{} {} {} {}", r.key, r.label, r.group, self.row_value(r))
                            .to_lowercase()
                            .contains(w)
                    })
            })
            .collect()
    }
    pub fn row_value(&self, row: &ConfigRow) -> Value {
        if row.write_key.is_empty() {
            return row.saved.clone();
        }
        let value = self.value(&row.write_key);
        if row.key.ends_with(".variant") {
            value.get("variant").cloned().unwrap_or_default()
        } else if row.key.ends_with(".model") {
            model_name(&value).map(|v| json!(v)).unwrap_or_default()
        } else {
            value
        }
    }
    pub fn summaries(&self) -> [String; 6] {
        let Some(body) = &self.body else {
            return Default::default();
        };
        let mut layers: Vec<_> = ["reflex", "instinct", "metacognition"]
            .into_iter()
            .filter(|l| model_name(&self.value(&format!("layers.{l}"))).is_some())
            .map(|l| if l == "metacognition" { "meta" } else { l })
            .collect();
        layers.push("cognition");
        [
            if body.gateways.is_empty() {
                "none in models.json".into()
            } else {
                body.gateways
                    .iter()
                    .map(|g| g.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" · ")
            },
            layers.join(" · "),
            self.value("default_agent")
                .as_str()
                .unwrap_or("build")
                .into(),
            match self
                .value("permissions.scratch_dirs")
                .as_array()
                .map(Vec::len)
            {
                Some(0) => "no scratch dir".into(),
                Some(1) => "1 scratch dir".into(),
                Some(n) => format!("{n} scratch dirs"),
                None => "rules".into(),
            },
            format!(
                "max {} per session",
                self.value("subagents.max_concurrent").as_u64().unwrap_or(4)
            ),
            self.value("tui.theme")
                .as_str()
                .and_then(ThemeId::from_id)
                .map(|t| t.name())
                .unwrap_or("not set")
                .into(),
        ]
    }
    pub fn unsaved_in(
        &self,
        section: Section,
        catalog: &[ModelEntry],
        agents: &[AgentEntry],
    ) -> usize {
        self.staged
            .keys()
            .filter(|key| {
                self.rows(catalog, agents)
                    .iter()
                    .any(|r| r.section == section && &r.write_key == *key)
            })
            .count()
    }
    pub fn fetch(&mut self) -> Effect {
        self.request += 1;
        self.loading = Some(self.request);
        Effect::FetchConfig(self.request)
    }
    pub fn next_write(&mut self) -> Vec<Effect> {
        if self.writing.is_some() || self.loading.is_some() {
            return vec![];
        }
        if self.stale_reload {
            return vec![self.fetch()];
        }
        let Some((key, value)) = self.staged.first_key_value() else {
            return vec![];
        };
        let Some(body) = &self.body else {
            return vec![];
        };
        let request = WriteRequest {
            revision: body.revision.clone(),
            key: key.clone(),
            value: value.clone(),
        };
        self.writing = Some(request.clone());
        self.request += 1;
        self.status = SaveStatus::Writing;
        vec![Effect::WriteConfig(request)]
    }
}

pub fn model_name(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("model").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

pub fn handle(s: &mut State, ui: &mut UiState, action: ConfigAction) -> Vec<Effect> {
    let row = s
        .config
        .visible_rows(&s.catalog, &s.agents)
        .get(s.config.row)
        .cloned();
    let row_count = s.config.visible_rows(&s.catalog, &s.agents).len();
    let config = &mut s.config;
    match action {
        ConfigAction::Jump(section) => {
            config.section = section;
            config.row = 0;
        }
        ConfigAction::CycleSection(delta) => {
            config.section =
                Section::ALL[(config.section.index() as i32 + delta).rem_euclid(6) as usize];
            config.row = 0;
        }
        ConfigAction::Move(delta) => {
            config.row =
                (config.row as i32 + delta).clamp(0, row_count.saturating_sub(1) as i32) as usize;
        }
        ConfigAction::Stage { key, value } => config.stage(key, value),
        ConfigAction::Revert => {
            if config.writing.is_none()
                && let Some(row) = row
            {
                config.staged.remove(&row.write_key);
                config.field_errors.remove(&row.write_key);
                config.status = if config.staged.is_empty() {
                    SaveStatus::UpToDate
                } else {
                    SaveStatus::Unsaved(config.staged.len())
                };
            }
        }
        ConfigAction::Step(delta) => {
            if let Some(row) = row {
                let current = config.row_value(&row);
                let value = match row.kind {
                    RowKind::Integer { min, max } => {
                        json!((current.as_i64().unwrap_or(min) + i64::from(delta)).clamp(min, max))
                    }
                    RowKind::Choice if !row.allowed.is_empty() => {
                        let at = row.allowed.iter().position(|v| v == &current).unwrap_or(0);
                        row.allowed
                            [(at as i32 + delta).rem_euclid(row.allowed.len() as i32) as usize]
                            .clone()
                    }
                    _ => return vec![],
                };
                if row.key.ends_with(".variant") {
                    let old = config.value(&row.write_key);
                    let Some(model) = model_name(&old) else {
                        return vec![];
                    };
                    config.stage(row.write_key, json!({"model": model, "variant": value}));
                } else {
                    config.stage(row.write_key, value);
                }
            }
        }
        ConfigAction::Toggle => {
            if let Some(row) = row.filter(|r| r.kind == RowKind::Bool) {
                let value = !config.value(&row.write_key).as_bool().unwrap_or(false);
                config.stage(row.write_key, json!(value));
            }
        }
        ConfigAction::Edit => {
            if config.writing.is_some() || config.stale_reload {
                return vec![];
            }
            if let Some(row) = row {
                match row.kind {
                    RowKind::Model(api) => {
                        ui.settings_target = Some(SettingsTarget {
                            title: format!("model for {}", row.write_key),
                            key: row.write_key,
                            api,
                        });
                        ui.overlay = Some(super::protocol::Overlay::Models);
                        ui.filter.clear();
                        ui.selection.insert("Models".into(), 0);
                    }
                    RowKind::StringList => {
                        config.list_editor = Some(
                            config
                                .value(&row.write_key)
                                .as_array()
                                .map(|v| {
                                    v.iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_default(),
                        );
                    }
                    _ => {}
                }
            }
        }
        ConfigAction::PickModel(name) => {
            if let Some(target) = ui.settings_target.take() {
                let Some(model) = s
                    .catalog
                    .iter()
                    .find(|m| m.name == name && m.api == target.api)
                else {
                    return vec![];
                };
                let old = config.value(&target.key);
                let variant = old.get("variant").and_then(Value::as_str).filter(|v| {
                    model.variants.iter().any(|m| m == v)
                        || (*v == "off" && !model.variants.is_empty())
                });
                config.stage(target.key, json!({"model": name, "variant": variant}));
                ui.overlay = None;
                ui.filter.clear();
            }
        }
        ConfigAction::Write => return config.next_write(),
        ConfigAction::Leave => {
            if config.writing.is_some() {
                return vec![];
            }
            if config.staged.is_empty() {
                ui.screen = Screen::Session;
            } else {
                config.leave_prompt = true;
            }
        }
        ConfigAction::ResolveLeave(choice) => {
            config.leave_prompt = false;
            match choice {
                LeaveChoice::Stay => config.leave_after_write = false,
                LeaveChoice::Discard if config.writing.is_none() => {
                    config.leave_after_write = false;
                    for key in config.staged.keys() {
                        config.field_errors.remove(key);
                    }
                    config.staged.clear();
                    config.status = SaveStatus::UpToDate;
                    ui.screen = Screen::Session;
                }
                LeaveChoice::Write => {
                    config.leave_after_write = true;
                    if config.staged.is_empty() {
                        ui.screen = Screen::Session;
                    } else {
                        return config.next_write();
                    }
                }
                _ => {}
            }
        }
        ConfigAction::StartFilter => config.filtering = true,
        ConfigAction::Filter(text) => {
            config.filter = text;
            config.row = 0;
        }
        ConfigAction::FilterDone => config.filtering = false,
        ConfigAction::StartList => return handle(s, ui, ConfigAction::Edit),
        ConfigAction::ListText(text) => config.list_editor = Some(text),
        ConfigAction::CancelList => config.list_editor = None,
        ConfigAction::CommitList => {
            if let Some(text) = &config.list_editor {
                let paths: Vec<_> = text
                    .lines()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .collect();
                if paths.iter().any(|p| !std::path::Path::new(p).is_absolute()) {
                    config.field_errors.insert(
                        "permissions.scratch_dirs".into(),
                        "Scratch directories must be absolute paths".into(),
                    );
                } else {
                    config.stage("permissions.scratch_dirs".into(), json!(paths));
                    config.list_editor = None;
                }
            }
        }
        ConfigAction::TestGateway => {
            if let Some(name) = row.and_then(|r| r.gateway)
                && config.testing_gateway.is_none()
            {
                config.testing_gateway = Some(name.clone());
                return vec![Effect::TestGateway(name)];
            }
        }
    }
    vec![]
}

pub fn loaded(
    config: &mut ConfigState,
    request: u64,
    result: Result<SettingsBody, String>,
) -> Vec<Effect> {
    if config.loading != Some(request) || request != config.request {
        return vec![];
    }
    config.loading = None;
    match result {
        Ok(body) => {
            config.body = Some(body);
            if config.stale_reload {
                config.status = SaveStatus::StaleReloaded(config.staged.len());
                config.stale_reload = false;
            }
            if config.leave_after_write {
                return config.next_write();
            }
        }
        Err(message) => {
            config.stale_reload = true;
            config.status = SaveStatus::Error {
                key: String::new(),
                message,
            }
        }
    }
    vec![]
}
pub fn written(
    config: &mut ConfigState,
    key: String,
    value: Value,
    result: Result<SettingsBody, SettingsError>,
) -> Vec<Effect> {
    if !config
        .writing
        .as_ref()
        .is_some_and(|r| r.key == key && r.value == value)
    {
        return vec![];
    }
    config.writing = None;
    match result {
        Ok(body) => {
            config.request += 1;
            config.loading = None;
            config.stale_reload = false;
            config.body = Some(body);
            config.staged.remove(&key);
            config.field_errors.remove(&key);
            let mut effects = vec![];
            if key == "tui.theme"
                && let Some(theme) = config.value(&key).as_str().and_then(ThemeId::from_id)
            {
                effects.push(Effect::ApplyConfigTheme(theme));
            }
            if config.staged.is_empty() {
                config.status = SaveStatus::Written;
                if config.leave_after_write {
                    config.leave_after_write = false;
                    effects.push(Effect::LeaveConfig);
                }
            } else {
                effects.extend(config.next_write());
            }
            effects
        }
        Err(SettingsError::Stale) => {
            config.stale_reload = true;
            config.leave_after_write = false;
            vec![config.fetch()]
        }
        Err(error) => {
            let (key, message) = match error {
                SettingsError::Invalid { key, message } => (key, message),
                other => (key, other.to_string()),
            };
            config.field_errors.insert(key.clone(), message.clone());
            config.status = SaveStatus::Error { key, message };
            config.leave_after_write = false;
            vec![]
        }
    }
}
