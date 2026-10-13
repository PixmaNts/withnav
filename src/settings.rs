//! User settings in `<config_dir>/config.json`: a JSON object whose keys belong to the clients
//! that read them (`tui` for the terminal client). Saving keeps every key it does not know, so
//! clients of different versions can share the file.
//! Cooperating writers use `update` and a stable advisory lock. Manual edits by other programs
//! are outside this guarantee; PUT revision checks still detect edits made before its read.
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

mod assignments;
pub use assignments::{AssignmentVariant, ModelAssignment};

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
    /// The parsed settings snapshot, including unknown keys.
    pub(crate) fn as_object(&self) -> &Map<String, Value> {
        &self.root
    }

    pub fn compaction_writer(&self) -> Result<&str, String> {
        match self.get_dotted("compaction.writer").and_then(Value::as_str) {
            Some(value @ ("metacognition" | "session")) => Ok(value),
            None if self.get_dotted("compaction.writer").is_none() => Ok("metacognition"),
            _ => Err("compaction.writer must be metacognition or session".into()),
        }
    }

    pub fn compaction_chunk_tokens(&self) -> Result<u64, String> {
        match self.get_dotted("compaction.chunk_tokens") {
            None => Ok(64_000),
            Some(value) => value
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| "compaction.chunk_tokens must be a positive integer".into()),
        }
    }
    /// Reads a dotted settings key without dropping unknown sibling keys.
    pub fn get_dotted(&self, key: &str) -> Option<&Value> {
        let mut parts = key.split('.');
        let mut value = self.root.get(parts.next()?)?;
        for part in parts {
            value = value.get(part)?;
        }
        Some(value)
    }

    /// Null removes just the selected key; writes preserve unrelated objects.
    pub fn set_dotted(&mut self, key: &str, value: Value) {
        fn set(map: &mut Map<String, Value>, parts: &[&str], value: Value) {
            if parts.len() == 1 {
                if value.is_null() {
                    map.remove(parts[0]);
                } else {
                    map.insert(parts[0].into(), value);
                }
                return;
            }
            if value.is_null() && !map.contains_key(parts[0]) {
                return;
            }
            let child = map
                .entry(parts[0])
                .or_insert_with(|| Value::Object(Map::new()));
            if !child.is_object() {
                if value.is_null() {
                    return;
                }
                *child = Value::Object(Map::new());
            }
            set(
                child.as_object_mut().expect("object created above"),
                &parts[1..],
                value,
            );
        }
        set(&mut self.root, &key.split('.').collect::<Vec<_>>(), value);
    }
    /// Assignment-mode detection from the same parsed object as its typed getters.
    pub fn has_assignment_configuration(&self) -> bool {
        self.root.contains_key("agents") || self.root.contains_key("default_agent")
    }
    pub fn permissions_scratch_dirs(&self) -> Result<Vec<PathBuf>, String> {
        match self
            .root
            .get("permissions")
            .and_then(|p| p.get("scratch_dirs"))
        {
            None => {
                let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                let private = private_scratch_root(runtime.as_deref(), &home);
                Ok(unique_roots(vec![
                    private,
                    std::env::temp_dir().join("withnav"),
                ]))
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
                .collect::<Result<Vec<_>, String>>()
                .map(unique_roots),
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
    /// Top-level `yolo`: disables all permission control for new sessions.
    pub fn yolo(&self) -> Result<bool, String> {
        match self.root.get("yolo") {
            None => Ok(false),
            Some(value) => value
                .as_bool()
                .ok_or_else(|| "yolo must be a boolean".into()),
        }
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

    /// The models the terminal client marked as favorites, `tui.favorite_models`. Entries that
    /// are not strings are ignored.
    pub fn tui_favorite_models(&self) -> Vec<String> {
        self.root
            .get("tui")
            .and_then(|tui| tui.get("favorite_models"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
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

    /// Stores the favorite models; an empty list removes the key. Other `tui` keys stay.
    pub fn set_tui_favorite_models(&mut self, names: &[String]) {
        let tui = self
            .root
            .entry("tui")
            .or_insert_with(|| Value::Object(Map::new()));
        if !tui.is_object() {
            *tui = Value::Object(Map::new());
        }
        if let Value::Object(tui) = tui {
            if names.is_empty() {
                tui.remove("favorite_models");
            } else {
                tui.insert(
                    "favorite_models".into(),
                    Value::Array(names.iter().cloned().map(Value::String).collect()),
                );
            }
        }
    }
}

fn private_scratch_root(runtime: Option<&Path>, home: &Path) -> PathBuf {
    let usable = runtime.filter(|path| crate::agent::scratch::private_runtime_usable(path));
    usable
        .map(|path| path.join("withnav"))
        .unwrap_or_else(|| home.join(".cache/withnav/scratch"))
}

fn unique_roots(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        let path: PathBuf = path.components().collect();
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join(FILE_NAME)
}

/// Reads the settings; a missing file gives the defaults.
pub fn load(config_dir: &Path) -> Result<Settings, SettingsError> {
    load_with_revision(config_dir).map(|(settings, _)| settings)
}

/// Parse and fingerprint exactly the same snapshot of the file.
pub fn load_with_revision(config_dir: &Path) -> Result<(Settings, String), SettingsError> {
    let path = path(config_dir);
    let bytes = match std::fs::read(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Settings::default(), "absent".into()))
        }
        Err(source) => return Err(SettingsError::Read { path, source }),
    };
    let revision = format!(
        "fnv:{:016x}",
        bytes
            .iter()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b))
                .wrapping_mul(0x100000001b3))
    );
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(root)) => Ok((Settings { root }, revision)),
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

/// Writes settings atomically through a synced temporary file. Unix also syncs the
/// directory to make the rename durable; Windows does not provide that guarantee.
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
    #[cfg(unix)]
    std::fs::File::open(config_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(write_err)?;
    Ok(())
}

/// Reads, edits and writes the settings while holding an exclusive advisory lock on
/// `config.json.lock`, so two WithNav processes (a TUI saving its theme, a WebUI saving a
/// setting) cannot lose each other's changes. `edit` gets the settings and the revision of the
/// bytes they were parsed from; an error from it leaves the file untouched. Programs that edit
/// `config.json` without taking the lock are outside this guarantee.
pub fn update<T, E: From<SettingsError>>(
    config_dir: &Path,
    edit: impl FnOnce(&mut Settings, &str) -> Result<T, E>,
) -> Result<T, E> {
    let lock_path = config_dir.join("config.json.lock");
    let error = |source| SettingsError::Write {
        path: lock_path.clone(),
        source,
    };
    std::fs::create_dir_all(config_dir)
        .map_err(error)
        .map_err(E::from)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(error)
        .map_err(E::from)?;
    #[cfg(unix)]
    let _lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| error(std::io::Error::from(e)))
        .map_err(E::from)?;
    #[cfg(windows)]
    let _lock = {
        file.lock().map_err(error).map_err(E::from)?;
        file
    };
    let (mut settings, revision) = load_with_revision(config_dir).map_err(E::from)?;
    let result = edit(&mut settings, &revision)?;
    save(config_dir, &settings).map_err(E::from)?;
    Ok(result)
}

/// Saves the terminal client's theme. An unreadable file is left untouched rather than replaced.
pub fn save_tui_theme(config_dir: &Path, id: &str) -> Result<(), SettingsError> {
    update(config_dir, |settings, _| {
        settings.set_tui_theme(id);
        Ok(())
    })
}

/// Saves the terminal client's favorite models. An unreadable file is left untouched.
pub fn save_tui_favorite_models(config_dir: &Path, names: &[String]) -> Result<(), SettingsError> {
    update(config_dir, |settings, _| {
        settings.set_tui_favorite_models(names);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_revision_matches_parsed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        save_tui_theme(dir.path(), "nord-dark").unwrap();
        let (snapshot, revision) = load_with_revision(dir.path()).unwrap();
        save_tui_theme(dir.path(), "ayu-light").unwrap();
        assert_eq!(snapshot.tui_theme(), Some("nord-dark"));
        assert_ne!(revision, load_with_revision(dir.path()).unwrap().1);
    }
    #[test]
    fn concurrent_updates_preserve_keys_despite_stale_views() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            for key in ["first", "second"] {
                let barrier = barrier.clone();
                let path = dir.path();
                scope.spawn(move || {
                    let stale = load(path).unwrap();
                    assert!(stale.get_dotted(key).is_none());
                    barrier.wait();
                    update::<_, SettingsError>(path, |settings, _| {
                        settings.set_dotted(key, Value::Bool(true));
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        let settings = load(dir.path()).unwrap();
        assert_eq!(settings.get_dotted("first"), Some(&Value::Bool(true)));
        assert_eq!(settings.get_dotted("second"), Some(&Value::Bool(true)));
    }
    #[test]
    fn assignment_mode_uses_captured_settings_not_replaced_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            path(dir.path()),
            r#"{"default_agent":"plan","agents":{"plan":{"model":"p/model","variant":"named"}}}"#,
        )
        .unwrap();
        let captured = load(dir.path()).unwrap();
        std::fs::write(path(dir.path()), "{}").unwrap();
        assert!(captured.has_assignment_configuration());
        assert_eq!(captured.default_agent().unwrap(), "plan");
        assert_eq!(
            captured.agent_assignment("plan").unwrap().unwrap().variant,
            AssignmentVariant::Named("named".into())
        );
        assert!(!load(dir.path()).unwrap().has_assignment_configuration());
    }
    #[test]
    #[cfg(unix)]
    fn private_scratch_uses_only_owned_private_runtime_directory() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        let home = temp.path().join("home");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            private_scratch_root(Some(&runtime), &home),
            runtime.join("withnav")
        );
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert_eq!(
            private_scratch_root(Some(&runtime), &home),
            home.join(".cache/withnav/scratch")
        );
        assert_eq!(
            private_scratch_root(None, &home),
            home.join(".cache/withnav/scratch")
        );
        assert_eq!(
            private_scratch_root(Some(Path::new("relative")), &home),
            home.join(".cache/withnav/scratch")
        );
        assert_eq!(
            private_scratch_root(Some(&temp.path().join("missing")), &home),
            home.join(".cache/withnav/scratch")
        );
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
        assert_eq!(
            private_scratch_root(Some(&alias.join("runtime")), &home),
            home.join(".cache/withnav/scratch")
        );
        assert_eq!(
            unique_roots(vec![runtime.join("withnav"), runtime.join("withnav/")]),
            vec![runtime.join("withnav")]
        );
    }
    #[test]
    fn scratch_setting_disables_or_selects_absolute_paths() {
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("custom");
        let settings = super::Settings {
            root: serde_json::json!({"permissions":{"scratch_dirs":[]}})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert!(settings.permissions_scratch_dirs().unwrap().is_empty());
        let settings = super::Settings {
            root: serde_json::json!({"permissions":{"scratch_dirs":[custom]}})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert_eq!(settings.permissions_scratch_dirs().unwrap(), vec![custom]);
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
    fn yolo_defaults_to_false_and_must_be_a_boolean() {
        let mut settings = Settings::default();
        assert_eq!(settings.yolo(), Ok(false));
        settings.root.insert("yolo".into(), serde_json::json!(true));
        assert_eq!(settings.yolo(), Ok(true));
        settings
            .root
            .insert("yolo".into(), serde_json::json!(false));
        assert_eq!(settings.yolo(), Ok(false));
        for bad in [serde_json::json!("true"), serde_json::json!(1), Value::Null] {
            settings.root.insert("yolo".into(), bad);
            assert_eq!(settings.yolo(), Err("yolo must be a boolean".into()));
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
        assert_eq!(leftovers.len(), 2, "only config and its stable lock remain");
        assert!(dir.path().join("config.json.lock").is_file());
    }

    #[test]
    fn saving_creates_the_directory_and_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("withnav");
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
    fn favorite_models_round_trip_and_keep_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            path(dir.path()),
            r#"{"editor":{"font":"mono"},"tui":{"theme":"ayu-dark","sidebar":false},"n":3}"#,
        )
        .unwrap();
        assert!(load(dir.path()).unwrap().tui_favorite_models().is_empty());
        let names = vec!["a/one".to_string(), "b/two".to_string()];
        save_tui_favorite_models(dir.path(), &names).unwrap();
        let settings = load(dir.path()).unwrap();
        assert_eq!(settings.tui_favorite_models(), names);
        assert_eq!(settings.tui_theme(), Some("ayu-dark"));
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(path(dir.path())).unwrap()).unwrap();
        assert_eq!(raw["editor"]["font"], "mono");
        assert_eq!(raw["tui"]["sidebar"], false);
        assert_eq!(raw["n"], 3);
        // Saving the theme afterwards keeps the favorites; an empty list drops the key only.
        save_tui_theme(dir.path(), "nord-dark").unwrap();
        assert_eq!(load(dir.path()).unwrap().tui_favorite_models(), names);
        save_tui_favorite_models(dir.path(), &[]).unwrap();
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(path(dir.path())).unwrap()).unwrap();
        assert!(raw["tui"].get("favorite_models").is_none());
        assert_eq!(raw["tui"]["theme"], "nord-dark");
    }

    #[test]
    fn malformed_favorite_models_are_ignored_not_fatal() {
        let mut settings = Settings::default();
        settings.root.insert(
            "tui".into(),
            serde_json::json!({"favorite_models": ["ok", 3, null, "also"]}),
        );
        assert_eq!(settings.tui_favorite_models(), ["ok", "also"]);
        settings
            .root
            .insert("tui".into(), serde_json::json!({"favorite_models": "x"}));
        assert!(settings.tui_favorite_models().is_empty());
    }

    #[test]
    fn a_non_object_tui_key_is_replaced() {
        let mut settings = Settings::default();
        settings.root.insert("tui".into(), Value::Bool(true));
        settings.set_tui_theme("nord-dark");
        assert_eq!(settings.tui_theme(), Some("nord-dark"));
    }
}
