//! User settings in `<config_dir>/config.json`: a JSON object whose keys belong to the clients
//! that read them (`tui` for the terminal client). Saving keeps every key it does not know, so
//! clients of different versions can share the file.
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub const FILE_NAME: &str = "config.json";

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a JSON object: {reason}")]
    Parse { path: PathBuf, reason: String },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The whole settings object, unknown keys included.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    root: Map<String, Value>,
}

impl Settings {
    /// The theme id the terminal client saved, `tui.theme`.
    pub fn tui_theme(&self) -> Option<&str> {
        self.root.get("tui")?.get("theme")?.as_str()
    }

    pub fn set_tui_theme(&mut self, id: &str) {
        let tui = self
            .root
            .entry("tui")
            .or_insert_with(|| Value::Object(Map::new()));
        if !tui.is_object() {
            *tui = Value::Object(Map::new());
        }
        if let Value::Object(tui) = tui {
            tui.insert("theme".into(), Value::String(id.into()));
        }
    }
}

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join(FILE_NAME)
}

/// Reads the settings; a missing file gives the defaults.
pub fn load(config_dir: &Path) -> Result<Settings, SettingsError> {
    let path = path(config_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(source) => return Err(SettingsError::Read { path, source }),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(root)) => Ok(Settings { root }),
        Ok(_) => Err(SettingsError::Parse {
            path,
            reason: "the top level is not an object".into(),
        }),
        Err(err) => Err(SettingsError::Parse {
            path,
            reason: err.to_string(),
        }),
    }
}

/// Writes the settings through a temporary file renamed over the old one, so a crash never
/// leaves a half-written file.
pub fn save(config_dir: &Path, settings: &Settings) -> Result<(), SettingsError> {
    let path = path(config_dir);
    let write_err = |source| SettingsError::Write {
        path: path.clone(),
        source,
    };
    std::fs::create_dir_all(config_dir).map_err(write_err)?;
    let mut text = serde_json::to_string_pretty(&Value::Object(settings.root.clone()))
        .expect("a JSON map always serializes");
    text.push('\n');
    let mut tmp = tempfile::NamedTempFile::new_in(config_dir).map_err(write_err)?;
    tmp.write_all(text.as_bytes()).map_err(write_err)?;
    tmp.as_file().sync_all().map_err(write_err)?;
    tmp.persist(&path).map_err(|err| write_err(err.error))?;
    Ok(())
}

/// Saves the terminal client's theme. An unreadable file is left untouched rather than replaced.
pub fn save_tui_theme(config_dir: &Path, id: &str) -> Result<(), SettingsError> {
    let mut settings = load(config_dir)?;
    settings.set_tui_theme(id);
    save(config_dir, &settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load(dir.path()).unwrap();
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.tui_theme(), None);
    }

    #[test]
    fn saving_the_theme_keeps_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            path(dir.path()),
            r#"{"editor":{"font":"mono"},"tui":{"theme":"ayu-dark","sidebar":false},"n":3}"#,
        )
        .unwrap();
        save_tui_theme(dir.path(), "nord-light").unwrap();
        let settings = load(dir.path()).unwrap();
        assert_eq!(settings.tui_theme(), Some("nord-light"));
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(path(dir.path())).unwrap()).unwrap();
        assert_eq!(raw["editor"]["font"], "mono");
        assert_eq!(raw["tui"]["sidebar"], false);
        assert_eq!(raw["n"], 3);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn saving_creates_the_directory_and_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("navai");
        save_tui_theme(&nested, "ayu-light").unwrap();
        assert_eq!(load(&nested).unwrap().tui_theme(), Some("ayu-light"));
    }

    #[test]
    fn a_broken_file_is_reported_and_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(dir.path()), "{ not json").unwrap();
        assert!(matches!(load(dir.path()), Err(SettingsError::Parse { .. })));
        assert!(save_tui_theme(dir.path(), "nord-dark").is_err());
        assert_eq!(
            std::fs::read_to_string(path(dir.path())).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn a_non_object_tui_key_is_replaced() {
        let mut settings = Settings::default();
        settings.root.insert("tui".into(), Value::Bool(true));
        settings.set_tui_theme("nord-dark");
        assert_eq!(settings.tui_theme(), Some("nord-dark"));
    }
}
