//! Global stdio and Streamable HTTP configuration; resolved environment values never enter
//! settings.
use std::collections::{BTreeMap, HashMap};

use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(rename = "type")]
    pub transport: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
    pub startup_timeout_ms: u64,
    pub call_timeout_ms: u64,
    pub tools: BTreeMap<String, bool>,
}

/// Headers the HTTP transport writes itself; rmcp refuses a request that also sets one of
/// them, so the declaration is refused up front with a clear message instead.
const TRANSPORT_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "content-length",
    "host",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: "stdio".into(),
            command: vec![],
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
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
                let config: ServerConfig = serde_json::from_value(value.clone())
                    .map_err(|_| "invalid MCP server configuration (values omitted)".to_string())?;
                if config.transport == "http" {
                    validate_http(&config)?;
                } else if config.transport != "stdio" {
                    return Err("type must be \"stdio\" or \"http\"".into());
                } else if !config.url.is_empty() || !config.headers.is_empty() {
                    return Err("url and headers apply to \"type\": \"http\" only".into());
                } else if config.command.is_empty()
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

fn validate_http(config: &ServerConfig) -> Result<(), String> {
    if !config.command.is_empty() || !config.env.is_empty() {
        return Err("command and env apply to \"type\": \"stdio\" only".into());
    }
    // The URL is shown nowhere and never interpolated: a credential belongs in a header,
    // where it is resolved at connect time and redacted.
    let valid = reqwest::Url::parse(&config.url).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
    });
    if !valid || config.url.contains("{env:") {
        return Err(
            "url must be an http(s) URL without credentials or environment interpolation".into(),
        );
    }
    for (name, value) in &config.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "invalid header name".to_string())?;
        if TRANSPORT_HEADERS.contains(&name.as_str()) {
            return Err(format!("header {name} is set by the MCP transport"));
        }
        HeaderValue::from_str(&interpolate(value, |_| Some(String::new()))?)
            .map_err(|_| format!("invalid value for header {name} (value omitted)"))?;
    }
    Ok(())
}

/// The declared headers of an HTTP server, interpolated now, and the values to redact: each
/// interpolated environment value and each whole header value. Every value is marked
/// sensitive, so no debug output of the request prints it.
pub fn http_headers(
    config: &ServerConfig,
) -> Result<(HashMap<HeaderName, HeaderValue>, Vec<String>), String> {
    let mut headers = HashMap::new();
    let mut secrets = vec![];
    for (name, value) in &config.headers {
        let parts = std::cell::RefCell::new(vec![]);
        let resolved = interpolate(value, |key| {
            let value = std::env::var(key).ok()?;
            parts.borrow_mut().push(value.clone());
            Some(value)
        })?;
        secrets.extend(parts.into_inner());
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "invalid header name".to_string())?;
        let mut header = HeaderValue::from_str(&resolved).map_err(|_| {
            format!("header {name} has an invalid value after interpolation (value omitted)")
        })?;
        header.set_sensitive(true);
        // Any header value may be a credential, even `1` or `no`. A `<scheme> <credential>`
        // value also has its credential tracked alone, so a server echoing only the token
        // is redacted too.
        if let Some((_, credential)) = resolved.split_once(' ') {
            secrets.push(credential.trim().to_owned());
        }
        secrets.push(resolved);
        headers.insert(name, header);
    }
    Ok((headers, sorted_secrets(secrets)))
}

fn is_non_secret_flag(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "1" | "true" | "false" | "yes" | "no"
    )
}

/// Longest first, so a shorter secret never splits a longer one when redacted.
fn sorted_secrets(mut secrets: Vec<String>) -> Vec<String> {
    secrets.retain(|value| !value.is_empty());
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    secrets
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
        let is_public_url = (key.ends_with("_URL") || key.ends_with("_URI"))
            && (resolved.starts_with("http://") || resolved.starts_with("https://"))
            && !resolved.contains('@');
        if !is_non_secret_flag(&resolved) && !is_public_url {
            secrets.push(resolved.clone());
        }
        #[cfg(windows)]
        env.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
        env.insert(key.clone(), resolved);
    }
    Ok((env, sorted_secrets(secrets)))
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
            json!({"good":{"type":"sse","url":"http://h/mcp"}}),
            json!({"good":{"command":["x"],"tools":{"[":false}}}),
        ] {
            assert!(parse(Some(&config))[0].1.is_err());
        }
        assert!(parse(Some(&json!({"good":{"command":["x"]}})))[0].1.is_ok());
    }
    #[test]
    fn http_declarations_need_a_plain_url_and_ordinary_headers() {
        let accepted = json!({"web":{"type":"http","url":"https://mcp.example/mcp",
            "headers":{"Authorization":"Bearer {env:TOKEN}","X-Team":"core"}}});
        assert!(parse(Some(&accepted))[0].1.is_ok());
        for config in [
            json!({"web":{"type":"http"}}),
            json!({"web":{"type":"http","url":"ftp://mcp.example/"}}),
            json!({"web":{"type":"http","url":"https://user:pw@mcp.example/"}}),
            json!({"web":{"type":"http","url":"https://mcp.example/?k={env:K}"}}),
            json!({"web":{"type":"http","url":"https://h/","command":["x"]}}),
            json!({"web":{"type":"http","url":"https://h/","env":{"A":"b"}}}),
            json!({"web":{"type":"http","url":"https://h/","headers":{"bad name":"x"}}}),
            json!({"web":{"type":"http","url":"https://h/","headers":{"Mcp-Session-Id":"x"}}}),
            json!({"web":{"type":"http","url":"https://h/","headers":{"X":"a\r\nInjected: b"}}}),
            json!({"web":{"type":"http","url":"https://h/","headers":{"X":"{env:"}}}),
            json!({"web":{"command":["x"],"url":"https://h/"}}),
            json!({"web":{"command":["x"],"headers":{"X":"y"}}}),
        ] {
            assert!(parse(Some(&config))[0].1.is_err(), "{config}");
        }
    }
    #[test]
    fn http_headers_are_sensitive_and_their_secrets_tracked() {
        let config = ServerConfig {
            transport: "http".into(),
            url: "https://h/".into(),
            headers: BTreeMap::from([
                ("Authorization".into(), "Bearer {env:PATH}".into()),
                ("X-Literal".into(), "Token literal-sentinel".into()),
                ("X-Key".into(), "1".into()),
            ]),
            ..ServerConfig::default()
        };
        let (headers, secrets) = http_headers(&config).unwrap();
        let path = std::env::var("PATH").unwrap();
        assert!(headers.values().all(HeaderValue::is_sensitive));
        assert_eq!(
            headers[&HeaderName::from_static("authorization")],
            format!("Bearer {path}").as_str()
        );
        for secret in [
            format!("Bearer {path}"),
            path,
            "literal-sentinel".into(),
            "1".into(),
        ] {
            assert!(secrets.contains(&secret), "{secret}");
        }
        let missing = ServerConfig {
            headers: BTreeMap::from([("X".into(), "{env:WITHNAV_SURELY_UNSET_VAR}".into())]),
            ..config
        };
        assert!(http_headers(&missing).is_err());
    }
}
