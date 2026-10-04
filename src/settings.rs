//! User settings in `<config_dir>/config.json`: a JSON object whose keys belong to the clients
//! that read them (`tui` for the terminal client). Saving keeps every key it does not know, so
//! clients of different versions can share the file.
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub const FILE_NAME: &str = "config.json";

/// How many children one session may have running at once when the settings
/// say nothing.
pub const DEFAULT_MAX_SUBAGENTS: usize = 4;

/// The most the settings may allow: more children than this is a mistake,
/// not a plan.
pub const MAX_SUBAGENTS_LIMIT: usize = 64;

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
    pub fn permissions_scratch_dirs(&self) -> Result<Vec<PathBuf>, String> {
        match self
            .root
            .get("permissions")
            .and_then(|p| p.get("scratch_dirs"))
        {
            None => {
                let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute() && path.is_dir());
                let private = runtime.map(|path| path.join("navai")).unwrap_or_else(|| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_default()
                        .join(".cache/navai/scratch")
                });
                Ok(vec![private, std::env::temp_dir().join("navai")])
            }
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(PathBuf::from)
                        .filter(|path| path.is_absolute())
                        .ok_or_else(|| {
                            "permissions.scratch_dirs must contain absolute paths".into()
                        })
                })
                .collect(),
            _ => Err("permissions.scratch_dirs must be a list of absolute paths".into()),
        }
    }
    /// Root progress-check interval; zero disables supervision.
    pub fn agent_watchdog_interval_secs(&self) -> Result<u64, String> {
        let Some(value) = self
            .root
            .get("agent")
            .and_then(|s| s.get("watchdog_interval_secs"))
        else {
            return Ok(300);
        };
        value
            .as_u64()
            .filter(|n| *n <= 365 * 24 * 60 * 60)
            .ok_or_else(|| {
                "agent.watchdog_interval_secs must be an integer from 0 to 31536000".into()
            })
    }
    /// Automatic advisory review; disabling it asks for every mutating call.
    pub fn permissions_automatic_review(&self) -> Result<bool, String> {
        let Some(permissions) = self.root.get("permissions") else {
            return Ok(true);
        };
        if !permissions.is_object() {
            return Err("permissions must be an object".into());
        }
        let Some(value) = permissions.get("automatic_review") else {
            return Ok(true);
        };
        value
            .as_bool()
            .ok_or_else(|| "permissions.automatic_review must be a boolean".into())
    }
    /// Progress-check interval; zero disables child supervision.
    pub fn subagents_watchdog_interval_secs(&self) -> Result<u64, String> {
        let Some(value) = self
            .root
            .get("subagents")
            .and_then(|s| s.get("watchdog_interval_secs"))
        else {
            return Ok(300);
        };
        value
            .as_u64()
            .filter(|n| *n <= 365 * 24 * 60 * 60)
            .ok_or_else(|| {
                "subagents.watchdog_interval_secs must be an integer from 0 to 31536000".into()
            })
    }
    /// The theme id the terminal client saved, `tui.theme`.
    pub fn tui_theme(&self) -> Option<&str> {
        self.root.get("tui")?.get("theme")?.as_str()
    }

    /// `subagents.max_concurrent`: how many children one session may have at
    /// once (starting, running or waiting for an answer). Absent means
    /// [`DEFAULT_MAX_SUBAGENTS`]; anything but an integer from 1 to
    /// [`MAX_SUBAGENTS_LIMIT`] is an error, never a silent default.
    pub fn subagents_max_concurrent(&self) -> Result<usize, String> {
        let Some(value) = self
            .root
            .get("subagents")
            .and_then(|subagents| subagents.get("max_concurrent"))
        else {
            return Ok(DEFAULT_MAX_SUBAGENTS);
        };
        value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=MAX_SUBAGENTS_LIMIT).contains(n))
            .ok_or_else(|| {
                format!(
                    "subagents.max_concurrent must be an integer from 1 to {MAX_SUBAGENTS_LIMIT}, \
                     not {value}"
                )
            })
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

/// Writes the settings through a synced temporary file renamed over the old one, then syncs the
/// directory: atomic (a crash never leaves a half-written file) and durable (the rename itself
/// survives a crash).
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
    std::fs::File::open(config_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(write_err)?;
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
    fn scratch_setting_disables_or_selects_absolute_paths() {
        let settings = super::Settings {
            root: serde_json::json!({"permissions":{"scratch_dirs":[]}})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert!(settings.permissions_scratch_dirs().unwrap().is_empty());
        let settings = super::Settings {
            root: serde_json::json!({"permissions":{"scratch_dirs":["/tmp/custom"]}})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert_eq!(
            settings.permissions_scratch_dirs().unwrap(),
            vec![std::path::PathBuf::from("/tmp/custom")]
        );
    }
    #[test]
    fn watchdog_interval_defaults_disables_and_rejects_invalid_values() {
        let mut settings = Settings::default();
        assert_eq!(settings.subagents_watchdog_interval_secs(), Ok(300));
        for n in [0, 1, 300, 31536000] {
            settings.root.insert(
                "subagents".into(),
                serde_json::json!({"watchdog_interval_secs":n}),
            );
            assert_eq!(settings.subagents_watchdog_interval_secs(), Ok(n));
        }
        for value in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("300"),
            serde_json::Value::Null,
            serde_json::json!(31536001),
        ] {
            settings.root.insert(
                "subagents".into(),
                serde_json::json!({"watchdog_interval_secs":value}),
            );
            assert!(settings.subagents_watchdog_interval_secs().is_err());
        }
    }

    #[test]
    fn root_watchdog_interval_is_validated() {
        let mut settings = Settings::default();
        assert_eq!(settings.agent_watchdog_interval_secs(), Ok(300));
        for n in [0, 1, 300, 31536000] {
            settings.root.insert(
                "agent".into(),
                serde_json::json!({"watchdog_interval_secs":n}),
            );
            assert_eq!(settings.agent_watchdog_interval_secs(), Ok(n));
        }
        for bad in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("300"),
            serde_json::json!(null),
            serde_json::json!(31536001),
        ] {
            settings.root.insert(
                "agent".into(),
                serde_json::json!({"watchdog_interval_secs":bad}),
            );
            assert!(settings.agent_watchdog_interval_secs().is_err());
        }
    }

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
    fn the_subagent_limit_defaults_and_is_validated() {
        let mut settings = Settings::default();
        assert_eq!(settings.subagents_max_concurrent(), Ok(4));
        for (raw, want) in [("2", Ok(2)), ("64", Ok(64)), ("1", Ok(1))] {
            settings.root.insert(
                "subagents".into(),
                serde_json::json!({ "max_concurrent": serde_json::from_str::<Value>(raw).unwrap() }),
            );
            assert_eq!(settings.subagents_max_concurrent(), want, "{raw}");
        }
        for bad in ["0", "-1", "65", "2.5", "\"4\"", "null", "true"] {
            settings.root.insert(
                "subagents".into(),
                serde_json::json!({ "max_concurrent": serde_json::from_str::<Value>(bad).unwrap() }),
            );
            let error = settings.subagents_max_concurrent().unwrap_err();
            assert!(error.contains("subagents.max_concurrent"), "{bad}: {error}");
        }
        // Other keys under `subagents` change nothing.
        settings
            .root
            .insert("subagents".into(), serde_json::json!({ "other": 1 }));
        assert_eq!(settings.subagents_max_concurrent(), Ok(4));
    }

    #[test]
    fn a_non_object_tui_key_is_replaced() {
        let mut settings = Settings::default();
        settings.root.insert("tui".into(), Value::Bool(true));
        settings.set_tui_theme("nord-dark");
        assert_eq!(settings.tui_theme(), Some("nord-dark"));
    }
}
