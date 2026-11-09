//! MCP apps served as documents. An app's HTML is parked behind a single-use unguessable path
//! (`POST /v1/mcp/{server}/app`, bearer-protected) and fetched once by the client's sandboxed
//! iframe (`GET /mcp-app/{token}`), with a Content-Security-Policy of its own: the app's inline
//! scripts and styles run, and the network is limited to the domains the resource declares in
//! `_meta.ui.csp`, none by default. The client's own page CSP is never loosened for an app.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::mcp::MAX_APP_HTML_BYTES;
use crate::server::session::AppState;

/// How long a minted document waits to be loaded.
const TTL: Duration = Duration::from_secs(60);
/// Documents waiting to be loaded at once; a client opening more is refused, never evicted.
const MAX_PENDING: usize = 32;
/// Origins kept per CSP directive, and the longest one.
const MAX_SOURCES: usize = 32;
const MAX_SOURCE_LEN: usize = 255;
/// The sandbox the document is served with, so that it is an opaque origin even when opened
/// outside the client's own iframe.
const SANDBOX: &str = "allow-scripts allow-forms";

struct Pending {
    html: String,
    csp: String,
    expires: Instant,
}

/// Documents waiting for their one load.
#[derive(Default)]
pub(crate) struct McpAppStore {
    pending: Mutex<HashMap<String, Pending>>,
}

impl std::fmt::Debug for McpAppStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpAppStore").finish_non_exhaustive()
    }
}

impl McpAppStore {
    /// Parks a document and returns its token; `None` when too many wait already.
    fn insert(&self, html: String, csp: String) -> Option<String> {
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, entry| entry.expires > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let token = format!("{}{}", ulid::Ulid::generate(), ulid::Ulid::generate()).to_lowercase();
        pending.insert(
            token.clone(),
            Pending {
                html,
                csp,
                expires: now + TTL,
            },
        );
        Some(token)
    }

    /// Removes and returns the document: a token works once, and never after its expiry.
    fn take(&self, token: &str) -> Option<(String, String)> {
        let entry = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token)?;
        (entry.expires > Instant::now()).then_some((entry.html, entry.csp))
    }
}

/// One origin a resource declared, in the form a CSP source list takes: a scheme, a host
/// (optionally with a leading `*.`) and an optional port or `*`. Anything else, a path, a
/// quote, a space or a `;` included, is dropped, so metadata cannot add a directive.
fn source(entry: &Value, schemes: &[&str]) -> Option<String> {
    let entry = entry.as_str()?.to_ascii_lowercase();
    if entry.is_empty() || entry.len() > MAX_SOURCE_LEN {
        return None;
    }
    let (scheme, rest) = entry.split_once("://")?;
    if !schemes.contains(&scheme) {
        return None;
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    let name = host.strip_prefix("*.").unwrap_or(host);
    let label_ok = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '.';
    if name.is_empty()
        || name.starts_with('.')
        || name.contains("..")
        || !name.chars().all(label_ok)
    {
        return None;
    }
    if let Some(port) = port
        && port != "*"
        && !(!port.is_empty() && port.len() <= 5 && port.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(entry)
}

fn sources(meta: &Value, key: &str, schemes: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in meta[key].as_array().into_iter().flatten() {
        if out.len() == MAX_SOURCES {
            break;
        }
        if let Some(origin) = source(item, schemes)
            && !out.contains(&origin)
        {
            out.push(origin);
        }
    }
    out
}

/// The Content-Security-Policy of one app, from the `_meta.ui.csp` its resource declared
/// (`null` or malformed means nothing is declared). Inline scripts and styles run; every
/// network and embedding directive is `'none'` unless the matching list names origins.
pub(crate) fn build_csp(declared: &Value) -> String {
    const WEB: &[&str] = &["http", "https"];
    const WEB_AND_SOCKETS: &[&str] = &["http", "https", "ws", "wss"];
    let list = |origins: Vec<String>, base: &[&str]| -> String {
        let mut all: Vec<String> = base.iter().map(|s| (*s).to_string()).collect();
        all.extend(origins);
        if all.is_empty() {
            "'none'".to_string()
        } else {
            all.join(" ")
        }
    };
    let resources = sources(declared, "resourceDomains", WEB);
    let connect = sources(declared, "connectDomains", WEB_AND_SOCKETS);
    let frames = sources(declared, "frameDomains", WEB);
    let base_uri = sources(declared, "baseUriDomains", WEB);
    let with_resources = |base: &[&str]| list(resources.clone(), base);
    format!(
        "default-src 'none'; script-src {scripts}; style-src {styles}; \
         img-src {images}; font-src {fonts}; media-src {media}; connect-src {connect}; \
         frame-src {frames}; base-uri {base_uri}; form-action 'none'; object-src 'none'; \
         sandbox {SANDBOX}",
        scripts = list(resources.clone(), &["'unsafe-inline'"]),
        styles = list(resources.clone(), &["'unsafe-inline'"]),
        images = with_resources(&["data:", "blob:"]),
        fonts = with_resources(&["data:"]),
        media = with_resources(&["data:", "blob:"]),
        connect = list(connect, &[]),
        frames = list(frames, &[]),
        base_uri = list(base_uri, &[]),
    )
}

#[derive(Deserialize)]
pub(super) struct MintBody {
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    html: Option<String>,
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// `POST /v1/mcp/{server}/app`: parks the app's document, from its `ui://` resource or from
/// inline HTML (exactly one), and answers the path to load it from. A server without a bearer
/// token never mints one: the path is the only thing between a page and the document.
pub(super) async fn mint(
    State(state): State<AppState>,
    Path(server): Path<String>,
    body: Result<Json<MintBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.http.bearer_token.is_none() {
        return refuse(
            StatusCode::FORBIDDEN,
            "MCP apps need a server with a bearer token",
        );
    }
    let Ok(Json(body)) = body else {
        return refuse(StatusCode::BAD_REQUEST, "malformed request body");
    };
    let (html, declared) = match (body.uri, body.html) {
        (Some(uri), None) => match state.service().mcp_read_app(&server, &uri).await {
            Ok(found) => found,
            Err(message) => return refuse(StatusCode::BAD_REQUEST, &message),
        },
        (None, Some(html)) if html.len() <= MAX_APP_HTML_BYTES => (html, Value::Null),
        (None, Some(_)) => return refuse(StatusCode::BAD_REQUEST, "MCP app document is too large"),
        _ => return refuse(StatusCode::BAD_REQUEST, "give exactly one of uri and html"),
    };
    match state.mcp_apps.insert(html, build_csp(&declared)) {
        Some(token) => Json(json!({
            "path": format!("/mcp-app/{token}"),
            "expires_in_ms": TTL.as_millis() as u64,
        }))
        .into_response(),
        None => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many MCP apps are waiting to load",
        ),
    }
}

/// True for the one request that needs no bearer: a `GET` of `/mcp-app/{token}`.
pub(super) fn is_app_load(method: &Method, path: &str) -> bool {
    method == Method::GET
        && path
            .strip_prefix("/mcp-app/")
            .is_some_and(|token| !token.is_empty() && !token.contains('/'))
}

/// True when `Host` names this server's own loopback address and port, which keeps a page that
/// resolved another name to 127.0.0.1 from loading a document.
fn host_is_ours(headers: &HeaderMap, port: u16) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

/// `GET /mcp-app/{token}`: the parked document, once. Only a `GET` consumes the token, and only
/// after the request has been checked, so a `HEAD` or a wrong `Host` leaves it for the client.
pub(super) async fn load(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if !host_is_ours(&headers, state.http.bind.port()) {
        return StatusCode::MISDIRECTED_REQUEST.into_response();
    }
    let Some((html, csp)) = state.mcp_apps.take(&token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = html.into_response();
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    if let Ok(csp) = HeaderValue::from_str(&csp) {
        out.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    out.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    out.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_declared_means_no_network_and_inline_code_only() {
        let csp = build_csp(&Value::Null);
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("script-src 'unsafe-inline';"), "{csp}");
        for directive in ["connect-src 'none'", "frame-src 'none'", "base-uri 'none'"] {
            assert!(csp.contains(directive), "{directive} in {csp}");
        }
        assert!(csp.contains("img-src data: blob:;"));
        assert!(csp.contains("sandbox allow-scripts allow-forms"));
    }

    #[test]
    fn declared_origins_open_only_their_directive() {
        let csp = build_csp(&json!({
            "connectDomains": ["https://api.example.com", "wss://live.example.com:8443", "http://*.cdn.test:*"],
            "resourceDomains": ["https://cdn.example.com"],
        }));
        assert!(csp.contains(
            "connect-src https://api.example.com wss://live.example.com:8443 http://*.cdn.test:*;"
        ));
        assert!(csp.contains("script-src 'unsafe-inline' https://cdn.example.com;"));
        assert!(csp.contains("img-src data: blob: https://cdn.example.com;"));
        assert!(csp.contains("frame-src 'none'"));
    }

    #[test]
    fn metadata_cannot_add_a_directive_or_a_scheme() {
        let csp = build_csp(&json!({
            "connectDomains": [
                "https://ok.example.com",
                "https://a.com; script-src *",
                "https://a.com 'unsafe-eval'",
                "https://a.com/path",
                "https://a.com,https://b.com",
                "ftp://a.com",
                "*",
                "data:",
                "https://",
                "https://..",
                "https://a.com:12x",
                7,
                null,
            ],
        }));
        assert!(csp.contains("connect-src https://ok.example.com;"), "{csp}");
        assert!(!csp.contains("unsafe-eval"));
        assert!(!csp.contains("script-src *"));
        // websocket schemes belong to connect-src only
        let csp =
            build_csp(&json!({"resourceDomains": ["wss://a.com"], "frameDomains": ["ws://a.com"]}));
        assert!(!csp.contains("wss://") && !csp.contains("ws://"), "{csp}");
    }

    #[test]
    fn lists_are_bounded() {
        let many: Vec<String> = (0..100)
            .map(|n| format!("https://h{n}.example.com"))
            .collect();
        let long = format!("https://{}.com", "a".repeat(300));
        let csp = build_csp(&json!({"connectDomains": many, "frameDomains": [long]}));
        assert_eq!(csp.matches("https://h").count(), MAX_SOURCES);
        assert!(csp.contains("frame-src 'none'"));
    }

    #[test]
    fn a_document_loads_once_and_expires() {
        let store = McpAppStore::default();
        let token = store.insert("<p>a</p>".into(), "csp".into()).unwrap();
        assert!(store.take("nope").is_none());
        assert_eq!(store.take(&token).unwrap().0, "<p>a</p>");
        assert!(store.take(&token).is_none(), "single use");
        let token = store.insert("<p>b</p>".into(), "csp".into()).unwrap();
        store
            .pending
            .lock()
            .unwrap()
            .get_mut(&token)
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert!(store.take(&token).is_none(), "expired");
    }

    #[test]
    fn waiting_documents_are_capped_and_expired_ones_make_room() {
        let store = McpAppStore::default();
        let tokens: Vec<_> = (0..MAX_PENDING)
            .map(|_| store.insert(String::new(), String::new()).unwrap())
            .collect();
        assert!(store.insert(String::new(), String::new()).is_none());
        store
            .pending
            .lock()
            .unwrap()
            .get_mut(&tokens[0])
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert!(store.insert(String::new(), String::new()).is_some());
    }

    #[test]
    fn only_a_get_of_one_token_segment_skips_the_bearer() {
        assert!(is_app_load(&Method::GET, "/mcp-app/abc"));
        assert!(!is_app_load(&Method::HEAD, "/mcp-app/abc"));
        assert!(!is_app_load(&Method::POST, "/mcp-app/abc"));
        assert!(!is_app_load(&Method::GET, "/mcp-app/"));
        assert!(!is_app_load(&Method::GET, "/mcp-app/a/b"));
        assert!(!is_app_load(&Method::GET, "/v1/mcp/x/app"));
    }
}
