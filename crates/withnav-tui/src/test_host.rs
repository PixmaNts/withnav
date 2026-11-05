//! A host that keeps the theme and the favourite models in `<config_dir>/config.json`, so the
//! tests can watch what the client saves, and make a save fail by putting a directory where the
//! file should be.
use std::{future::Future, path::Path, path::PathBuf, pin::Pin};

use serde_json::{Value, json};

use crate::{GitStatus, TuiHost};

#[derive(Debug, Clone, Copy, Default)]
pub struct FileHost;

fn read(config_dir: &Path) -> Result<Value, String> {
    match std::fs::read(config_dir.join("config.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.to_string()),
    }
}

fn update(config_dir: &Path, key: &str, value: Value) -> Result<(), String> {
    let mut root = read(config_dir)?;
    root["tui"][key] = value;
    let path = config_dir.join("config.json");
    let tmp = config_dir.join("config.json.tmp");
    std::fs::write(&tmp, root.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// The theme saved under `config_dir`, as [`FileHost`] reads it.
pub fn saved_theme(config_dir: &Path) -> Option<String> {
    FileHost.load_theme(config_dir).unwrap()
}

impl TuiHost for FileHost {
    fn load_theme(&self, config_dir: &Path) -> Result<Option<String>, String> {
        Ok(read(config_dir)?["tui"]["theme"]
            .as_str()
            .map(str::to_owned))
    }
    fn save_theme(&self, config_dir: &Path, id: &str) -> Result<(), String> {
        update(config_dir, "theme", json!(id))
    }
    fn load_favorite_models(&self, config_dir: &Path) -> Result<Vec<String>, String> {
        Ok(read(config_dir)?["tui"]["favorite_models"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }
    fn save_favorite_models(&self, config_dir: &Path, names: &[String]) -> Result<(), String> {
        update(config_dir, "favorite_models", json!(names))
    }
    fn read_git(&self, _: &Path) -> Pin<Box<dyn Future<Output = Option<GitStatus>> + Send + '_>> {
        Box::pin(async { None })
    }
    fn home_dir(&self) -> Option<PathBuf> {
        None
    }
}
