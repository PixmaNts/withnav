use std::collections::BTreeMap;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ProviderError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthEntry {
    pub api_key: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthStore {
    pub providers: BTreeMap<String, AuthEntry>,
}

pub fn default_config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", crate::paths::APP_NAME)
        .map(|p| p.config_dir().to_path_buf())
}

fn auth_path(config_dir: &Path) -> PathBuf {
    config_dir.join("auth.json")
}

pub fn load_store(config_dir: &Path) -> Result<AuthStore, ProviderError> {
    let Some(text) = read_store(config_dir)? else {
        return Ok(AuthStore::default());
    };
    parse_store(&text)
}

/// Read one permission-checked snapshot for typed credentials and diagnostics.
pub(crate) fn load_store_with_document(
    config_dir: &Path,
) -> Result<(AuthStore, serde_json::Value), ProviderError> {
    let Some(text) = read_store(config_dir)? else {
        return Ok((AuthStore::default(), serde_json::json!({})));
    };
    let store = parse_store(&text)?;
    let document = serde_json::from_str(&text)
        .map_err(|e| ProviderError::Auth(format!("failed to parse auth.json: {e}")))?;
    Ok((store, document))
}

fn read_store(config_dir: &Path) -> Result<Option<String>, ProviderError> {
    let path = auth_path(config_dir);
    if !path.exists() {
        return Ok(None);
    }
    check_perms_or_fail(&path)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| ProviderError::Auth(format!("failed to read auth.json: {e}")))?;
    Ok(Some(text))
}

fn parse_store(text: &str) -> Result<AuthStore, ProviderError> {
    serde_json::from_str(text)
        .map_err(|e| ProviderError::Auth(format!("failed to parse auth.json: {e}")))
}

#[cfg(unix)]
fn check_perms_or_fail(path: &Path) -> Result<(), ProviderError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| ProviderError::Auth(format!("failed to stat auth.json: {e}")))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(ProviderError::Auth(format!(
            "auth.json at {} has insecure permissions {:o}; run: chmod 600 {}",
            path.display(),
            mode,
            path.display()
        )));
    }
    Ok(())
}

pub fn resolve_key(
    config_dir: &Path,
    provider_name: &str,
    api_key_env: Option<&str>,
) -> Result<String, ProviderError> {
    let store = load_store(config_dir)?;
    resolve_key_from_store(&store, provider_name, api_key_env)
}

pub(crate) fn resolve_key_from_store(
    store: &AuthStore,
    provider_name: &str,
    api_key_env: Option<&str>,
) -> Result<String, ProviderError> {
    if let Some(entry) = store.providers.get(provider_name) {
        return Ok(entry.api_key.clone());
    }
    let env_name = api_key_env.unwrap_or("OMNIROUTE_API_KEY");
    if let Ok(v) = std::env::var(env_name) {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    Err(ProviderError::MissingApiKey)
}

/// Credential provenance only, following exactly the key resolver's precedence.
pub fn key_source(
    config_dir: &Path,
    provider_name: &str,
    api_key_env: Option<&str>,
) -> Result<&'static str, ProviderError> {
    if load_store(config_dir)?
        .providers
        .contains_key(provider_name)
    {
        return Ok("auth_store");
    }
    if std::env::var(api_key_env.unwrap_or("OMNIROUTE_API_KEY")).is_ok_and(|v| !v.is_empty()) {
        return Ok("environment");
    }
    Ok("missing")
}

pub fn remove_key(config_dir: &Path, provider_name: &str) -> Result<(), ProviderError> {
    let mut store = load_store(config_dir)?;
    if let Some(_entry) = store.providers.remove_entry(provider_name) {
        return save_store(config_dir, store);
    }
    Err(ProviderError::MissingApiKey)
}

pub fn set_key(config_dir: &Path, provider_name: &str, api_key: &str) -> Result<(), ProviderError> {
    std::fs::create_dir_all(config_dir).map_err(io_api)?;
    let mut store = load_store(config_dir)?;
    store.providers.insert(
        provider_name.to_string(),
        AuthEntry {
            api_key: api_key.to_string(),
        },
    );
    save_store(config_dir, store)
}

/// Helper function to save a Store to disk.
/// Used for saving the store after adding a key or removing one
#[cfg(unix)]
pub fn save_store(config_dir: &Path, store: AuthStore) -> Result<(), ProviderError> {
    let path = auth_path(config_dir);
    if path.exists() {
        check_perms_or_fail(&path)?;
    }

    let serialized = serde_json::to_string_pretty(&store)
        .map_err(|e| ProviderError::Auth(format!("failed to serialize auth.json: {e}")))?;
    let path = auth_path(config_dir);
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io_api)?;
        f.write_all(serialized.as_bytes()).map_err(io_api)?;
        f.sync_all().map_err(io_api)?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(io_api)?;
    std::fs::rename(&tmp, &path).map_err(io_api)?;
    Ok(())
}

pub fn list_providers(config_dir: &Path) -> Result<Vec<String>, ProviderError> {
    let store = load_store(config_dir)?;
    Ok(store.providers.keys().cloned().collect())
}

fn io_api(e: std::io::Error) -> ProviderError {
    ProviderError::Auth(format!("auth.json I/O error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_key_writes_0600_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        set_key(tmp.path(), "omniroute", "secret-abc").unwrap();
        let path = auth_path(tmp.path());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth.json must be created 0600");
        let key = resolve_key(tmp.path(), "omniroute", None).unwrap();
        assert_eq!(key, "secret-abc");
        set_key(tmp.path(), "other", "second-key").unwrap();
        assert_eq!(resolve_key(tmp.path(), "omniroute", None).unwrap(), key);

        let corrupt = "{\"omniroute\":{\"api_key\":\"secret-abc\"},";
        std::fs::write(&path, corrupt).unwrap();
        assert!(matches!(
            set_key(tmp.path(), "other", "replacement"),
            Err(ProviderError::Auth(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn file_key_wins_over_env() {
        let tmp = tempfile::tempdir().unwrap();
        set_key(tmp.path(), "omniroute", "from-file").unwrap();
        std::env::set_var("WITHNAV_TEST_KEY", "from-env");
        let key = resolve_key(tmp.path(), "omniroute", Some("WITHNAV_TEST_KEY")).unwrap();
        assert_eq!(key, "from-file");
        std::env::remove_var("WITHNAV_TEST_KEY");
    }

    #[test]
    fn env_fallback_when_no_file_entry() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("WITHNAV_TEST_KEY2", "env-only");
        let key = resolve_key(tmp.path(), "someprovider", Some("WITHNAV_TEST_KEY2")).unwrap();
        assert_eq!(key, "env-only");
        std::env::remove_var("WITHNAV_TEST_KEY2");
    }

    #[test]
    fn remove_key_test() {
        let tmp = tempfile::tempdir().unwrap();
        set_key(tmp.path(), "omniroute", "super-secret-value").unwrap();
        assert!(remove_key(tmp.path(), "omniroute").is_ok());
        let e = resolve_key(tmp.path(), "omniroute", None).unwrap_err();
        assert!(matches!(e, ProviderError::MissingApiKey));
    }

    #[test]
    fn missing_everywhere_returns_missing_api_key() {
        let tmp = tempfile::tempdir().unwrap();
        let e = resolve_key(
            tmp.path(),
            "omniroute",
            Some("WITHNAV_DEFINITELY_UNSET_XYZ"),
        )
        .unwrap_err();
        assert!(matches!(e, ProviderError::MissingApiKey));
    }

    #[test]
    fn loose_perms_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = auth_path(tmp.path());
        std::fs::write(&path, "{\"omniroute\":{\"api_key\":\"x\"}}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = load_store(tmp.path()).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("insecure permissions"), "got: {s}");
        assert!(matches!(
            set_key(tmp.path(), "other", "new-key"),
            Err(ProviderError::Auth(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"omniroute\":{\"api_key\":\"x\"}}"
        );
    }

    #[test]
    fn list_returns_provider_names_only() {
        let tmp = tempfile::tempdir().unwrap();
        set_key(tmp.path(), "omniroute", "super-secret-value").unwrap();
        let names = list_providers(tmp.path()).unwrap();
        assert_eq!(names, vec!["omniroute".to_string()]);
        assert!(!names.iter().any(|n| n.contains("super-secret-value")));
    }
}

#[cfg(windows)]
fn check_perms_or_fail(_path: &Path) -> Result<(), ProviderError> {
    Err(ProviderError::Auth(
        "Credential permission validation is not yet supported on Windows".into(),
    ))
}
#[cfg(windows)]
pub fn save_store(_config_dir: &Path, _store: AuthStore) -> Result<(), ProviderError> {
    Err(ProviderError::Auth(
        "Private credential persistence is not yet supported on Windows".into(),
    ))
}
