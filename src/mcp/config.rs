//! Global stdio configuration; resolved environment values never enter settings.
use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(rename = "type")]
    pub transport: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    pub startup_timeout_ms: u64,
    pub call_timeout_ms: u64,
    pub tools: BTreeMap<String, bool>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: "stdio".into(),
            command: vec![],
            env: BTreeMap::new(),
            enabled: true,
            startup_timeout_ms: 10_000,
            call_timeout_ms: 60_000,
            tools: BTreeMap::new(),
        }
    }
}

pub fn parse(value: Option<&Value>) -> Vec<(String, Result<ServerConfig, String>)> {
    let Some(value) = value else {
        return vec![];
    };
    let Some(servers) = value.as_object() else {
        return vec![("configuration".into(), Err("mcp must be an object".into()))];
    };
    servers
        .iter()
        .map(|(id, value)| {
            let config = (|| {
                if id.is_empty()
                    || id.contains("__")
                    || id.len() > 24
                    || !id
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
                {
                    return Err("server id must match [a-z0-9_-]{1,24} and contain no __".into());
                }
                let config: ServerConfig = serde_json::from_value(value.clone()).map_err(|_| {
                    "invalid stdio server configuration (values omitted)".to_string()
                })?;
                if config.transport != "stdio" {
                    return Err("only stdio transport is supported".into());
                }
                if config.command.is_empty()
                    || config.command[0].is_empty()
                    || config
                        .command
                        .iter()
                        .any(|v| v.contains("{env:") || v.contains('\0'))
                {
                    return Err(
                        "command must be a nonempty argv array without environment interpolation"
                            .into(),
                    );
                }
                if config.startup_timeout_ms == 0 || config.call_timeout_ms == 0 {
                    return Err("timeouts must be positive milliseconds".into());
                }
                for pattern in config.tools.keys() {
                    globset::Glob::new(pattern).map_err(|_| "invalid tools glob".to_string())?;
                }
                for (key, value) in &config.env {
                    if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                        return Err("invalid environment entry".into());
                    }
                    interpolate(value, |_| Some(String::new()))?;
                }
                Ok(config)
            })();
            (id.clone(), config)
        })
        .collect()
}

pub fn interpolate(value: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut result = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("{env:") {
        result.push_str(&rest[..start]);
        rest = &rest[start + 5..];
        let end = rest
            .find('}')
            .ok_or("unterminated environment interpolation")?;
        let key = &rest[..end];
        if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return Err("invalid environment interpolation name".into());
        }
        result.push_str(
            &lookup(key).ok_or_else(|| format!("environment variable {key} is unavailable"))?,
        );
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

pub fn child_env(config: &ServerConfig) -> Result<(BTreeMap<String, String>, Vec<String>), String> {
    #[cfg(unix)]
    let mut env: BTreeMap<_, _> = std::env::vars()
        .filter(|(key, _)| {
            matches!(key.as_str(), "PATH" | "HOME" | "LANG" | "TMPDIR")
                || key.starts_with("LC_")
                || key.starts_with("XDG_")
        })
        .collect();
    #[cfg(windows)]
    let mut env: BTreeMap<_, _> = std::env::vars()
        .filter(|(key, _)| windows_child_variable(key))
        .collect();
    let mut secrets = vec![];
    for (key, value) in &config.env {
        let parts = std::cell::RefCell::new(vec![]);
        let resolved = interpolate(value, |name| {
            let value = std::env::var(name).ok()?;
            parts.borrow_mut().push(value.clone());
            Some(value)
        })?;
        secrets.extend(parts.into_inner());
        secrets.push(resolved.clone());
        #[cfg(windows)]
        env.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
        env.insert(key.clone(), resolved);
    }
    secrets.retain(|value| !value.is_empty());
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    Ok((env, secrets))
}

#[cfg(windows)]
fn windows_child_variable(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    matches!(
        key.as_str(),
        "PATH"
            | "PATHEXT"
            | "SYSTEMROOT"
            | "SYSTEMDRIVE"
            | "WINDIR"
            | "COMSPEC"
            | "TEMP"
            | "TMP"
            | "USERPROFILE"
            | "APPDATA"
            | "LOCALAPPDATA"
            | "PROGRAMDATA"
            | "PROGRAMFILES"
            | "PROGRAMFILES(X86)"
            | "PROGRAMW6432"
            | "COMMONPROGRAMFILES"
            | "NUMBER_OF_PROCESSORS"
            | "PROCESSOR_ARCHITECTURE"
            | "HOME"
            | "LANG"
            | "TMPDIR"
    ) || key.starts_with("LC_")
        || key.starts_with("XDG_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(windows)]
    #[test]
    fn windows_environment_names_are_case_insensitive_and_secrets_are_excluded() {
        for key in [
            "Path",
            "pAtHeXt",
            "SystemRoot",
            "windir",
            "ComSpec",
            "USERPROFILE",
            "AppData",
            "lc_messages",
            "xdg_cache_home",
        ] {
            assert!(windows_child_variable(key), "{key}");
        }
        for key in ["OMNIROUTE_API_KEY", "TOKEN", "SECRET", "SYSTEMROOT_SECRET"] {
            assert!(!windows_child_variable(key), "{key}");
        }
    }
    #[cfg(windows)]
    #[test]
    fn configured_windows_environment_replaces_inherited_names_case_insensitively() {
        let config = ServerConfig {
            env: BTreeMap::from([("pAtH".into(), "fixture-path-sentinel".into())]),
            ..ServerConfig::default()
        };
        let (env, _) = child_env(&config).unwrap();
        assert_eq!(
            env.keys()
                .filter(|key| key.eq_ignore_ascii_case("PATH"))
                .count(),
            1
        );
        assert_eq!(env.get("pAtH").unwrap(), "fixture-path-sentinel");
        assert!(env.keys().any(|key| key.eq_ignore_ascii_case("SystemRoot")));
    }
    #[test]
    fn interpolation_is_explicit_and_missing_values_fail() {
        assert_eq!(
            interpolate("prefix {env:TOKEN}", |key| (key == "TOKEN")
                .then(|| "secret".into()))
            .unwrap(),
            "prefix secret"
        );
        assert!(interpolate("{env:MISSING}", |_| None).is_err());
        assert!(interpolate("{env:X", |_| Some("x".into())).is_err());
        assert!(interpolate("{env:X-Y}", |_| Some("x".into())).is_err());
    }
    #[test]
    fn invalid_ids_commands_transports_and_globs_are_reported() {
        for config in [
            json!({"UPPER":{"command":["x"]}}),
            json!({"good":{"command":["{env:X}"]}}),
            json!({"good":{"type":"http","command":["x"]}}),
            json!({"good":{"command":["x"],"tools":{"[":false}}}),
        ] {
            assert!(parse(Some(&config))[0].1.is_err());
        }
        assert!(parse(Some(&json!({"good":{"command":["x"]}})))[0].1.is_ok());
    }
}
