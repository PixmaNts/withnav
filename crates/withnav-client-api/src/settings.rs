//! Wire contract of the settings endpoints (`GET`/`PUT /v1/settings`, gateway test).
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Every value `tui.theme` may hold in `config.json`, in the terminal client's picker order. The
/// settings route validates against it and the terminal client's theme list must match it, so a
/// theme offered on the settings screen is never refused by the server.
pub const TUI_THEME_IDS: [&str; 8] = [
    "ayu-dark",
    "ayu-light",
    "nord-dark",
    "nord-light",
    "tide-dark",
    "tide-light",
    "moss-dark",
    "moss-light",
];

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
