//! Browser pairing: a single-use code minted with the owner's token, exchanged once by the
//! browser for its own expiring credential. The owner's token never reaches a browser.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;

const MAX_CODES: usize = 64;
const MAX_CREDENTIALS: usize = 256;
const CODE_TTL_SECONDS: i64 = 60;
const CREDENTIAL_TTL_HOURS: i64 = 12;

/// Time source for pairing code and browser credential expiration.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> time::OffsetDateTime;
}

/// Standard system wall clock (UTC).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }
}

struct CodeEntry {
    code: String,
    launch_workspaces: Vec<PathBuf>,
    expires_at: time::OffsetDateTime,
}

struct CredentialEntry {
    credential: String,
    launch_workspaces: Vec<PathBuf>,
    expires_at: time::OffsetDateTime,
}

#[derive(Default)]
struct StoreInner {
    codes: Vec<CodeEntry>,
    credentials: Vec<CredentialEntry>,
}

impl StoreInner {
    fn prune(&mut self, now: time::OffsetDateTime) {
        self.codes.retain(|entry| entry.expires_at > now);
        self.credentials.retain(|entry| entry.expires_at > now);
    }
}

/// Pairing codes and browser credentials, in memory only.
pub struct Store {
    inner: Mutex<StoreInner>,
    clock: Arc<dyn Clock>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (codes_count, creds_count) = if let Ok(guard) = self.inner.lock() {
            (guard.codes.len(), guard.credentials.len())
        } else {
            (0, 0)
        };
        f.debug_struct("Store")
            .field("codes_count", &codes_count)
            .field("credentials_count", &creds_count)
            .finish()
    }
}

impl Store {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Mutex::new(StoreInner::default()),
            clock,
        }
    }

    /// Mints a single-use pairing code with 32 random bytes (64 hex characters) valid for 60 seconds.
    pub fn mint(&self, launch_workspaces: Vec<PathBuf>) -> String {
        let now = self.clock.now();
        let code = random_hex_32();
        let expires_at = now + time::Duration::seconds(CODE_TTL_SECONDS);
        let mut inner = self.inner.lock().expect("pairing store mutex poisoned");
        inner.prune(now);
        if inner.codes.len() >= MAX_CODES {
            inner.codes.remove(0);
        }
        inner.codes.push(CodeEntry {
            code: code.clone(),
            launch_workspaces,
            expires_at,
        });
        code
    }

    /// Atomically consumes a pairing code, returning a newly minted browser credential and its
    /// expiration timestamp in RFC 3339 format. A second exchange of the same code fails.
    pub fn exchange(&self, code: &str) -> Option<(String, String)> {
        let now = self.clock.now();
        let mut inner = self.inner.lock().expect("pairing store mutex poisoned");
        inner.prune(now);
        let pos = inner
            .codes
            .iter()
            .position(|entry| constant_time_eq(&entry.code, code))?;
        let entry = inner.codes.remove(pos);
        let credential = random_hex_32();
        let expires_at = now + time::Duration::hours(CREDENTIAL_TTL_HOURS);
        let expires_at_str = format_rfc3339(expires_at);
        if inner.credentials.len() >= MAX_CREDENTIALS {
            inner.credentials.remove(0);
        }
        inner.credentials.push(CredentialEntry {
            credential: credential.clone(),
            launch_workspaces: entry.launch_workspaces,
            expires_at,
        });
        Some((credential, expires_at_str))
    }

    /// True when `credential` is a live browser credential. Checked via constant-time comparison.
    pub fn accepts(&self, credential: &str) -> bool {
        let now = self.clock.now();
        let mut inner = self.inner.lock().expect("pairing store mutex poisoned");
        inner.prune(now);
        let mut valid = false;
        for entry in &inner.credentials {
            if constant_time_eq(&entry.credential, credential) {
                valid = true;
            }
        }
        valid
    }

    /// Returns the launch workspaces associated with the given browser credential, or empty if unknown.
    pub fn launch_workspaces(&self, credential: &str) -> Vec<PathBuf> {
        let now = self.clock.now();
        let mut inner = self.inner.lock().expect("pairing store mutex poisoned");
        inner.prune(now);
        let mut result = None;
        for entry in &inner.credentials {
            if constant_time_eq(&entry.credential, credential) {
                result = Some(entry.launch_workspaces.clone());
            }
        }
        result.unwrap_or_default()
    }

    /// Revokes a browser credential. Returns true if the credential was found and removed.
    pub fn revoke(&self, credential: &str) -> bool {
        let now = self.clock.now();
        let mut inner = self.inner.lock().expect("pairing store mutex poisoned");
        inner.prune(now);
        let pos = inner
            .credentials
            .iter()
            .position(|entry| constant_time_eq(&entry.credential, credential));
        if let Some(idx) = pos {
            inner.credentials.remove(idx);
            true
        } else {
            false
        }
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.len() != b_bytes.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a_bytes.iter().zip(b_bytes.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

fn format_rfc3339(dt: time::OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn random_hex_32() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS entropy source must not fail");
    hex::encode(bytes)
}

#[derive(Debug, Deserialize)]
struct PairRequest {
    #[serde(default)]
    workspaces: Vec<PathBuf>,
}

#[derive(Serialize)]
struct PairResponse {
    code: String,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct ExchangeRequest {
    code: String,
}

#[derive(Serialize)]
struct ExchangeResponse {
    credential: String,
    expires_at: String,
}

async fn mint_pair(State(state): State<AppState>, Json(payload): Json<PairRequest>) -> Response {
    for ws in &payload.workspaces {
        if !ws.is_absolute() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "all workspaces must be absolute paths" })),
            )
                .into_response();
        }
    }
    let code = state.pairing.mint(payload.workspaces);
    (
        StatusCode::OK,
        Json(PairResponse {
            code,
            expires_in: CODE_TTL_SECONDS as u64,
        }),
    )
        .into_response()
}

async fn exchange_pair(
    State(state): State<AppState>,
    Json(payload): Json<ExchangeRequest>,
) -> Response {
    match state.pairing.exchange(&payload.code) {
        Some((credential, expires_at)) => {
            let mut response = (
                StatusCode::OK,
                Json(ExchangeResponse {
                    credential,
                    expires_at,
                }),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        None => {
            let mut response = (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "invalid or expired pairing code" })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
    }
}

/// `POST /v1/pair` (owner token) and `POST /v1/pair/exchange` (public).
pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/pair", post(mint_pair))
        .route("/v1/pair/exchange", post(exchange_pair))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct MockClock {
        offset_seconds: AtomicI64,
        base: time::OffsetDateTime,
    }

    impl MockClock {
        fn new() -> Self {
            Self {
                offset_seconds: AtomicI64::new(0),
                base: time::OffsetDateTime::now_utc(),
            }
        }

        fn advance(&self, seconds: i64) {
            self.offset_seconds.fetch_add(seconds, Ordering::SeqCst);
        }
    }

    impl Clock for MockClock {
        fn now(&self) -> time::OffsetDateTime {
            let offset = self.offset_seconds.load(Ordering::SeqCst);
            self.base + time::Duration::seconds(offset)
        }
    }

    #[test]
    fn single_use_consumption() {
        let store = Store::new();
        let code = store.mint(vec![PathBuf::from("/workspace/test")]);
        let first = store.exchange(&code);
        assert!(first.is_some());
        let (cred, _) = first.unwrap();
        assert!(store.accepts(&cred));

        // A second exchange of the same code must fail
        let second = store.exchange(&code);
        assert!(second.is_none());
    }

    #[test]
    fn expiry_with_injected_clock() {
        let clock = Arc::new(MockClock::new());
        let store = Store::with_clock(clock.clone());

        let code = store.mint(vec![PathBuf::from("/workspace/alpha")]);
        // Advance clock past 60s pairing code TTL
        clock.advance(61);
        assert!(store.exchange(&code).is_none());

        // Mint fresh code, exchange it, verify credential expires after 12 hours
        let code2 = store.mint(vec![PathBuf::from("/workspace/beta")]);
        let (cred, _) = store.exchange(&code2).expect("exchange");
        assert!(store.accepts(&cred));
        assert_eq!(
            store.launch_workspaces(&cred),
            vec![PathBuf::from("/workspace/beta")]
        );

        // Advance 12 hours + 1 second
        clock.advance(12 * 3600 + 1);
        assert!(!store.accepts(&cred));
        assert!(store.launch_workspaces(&cred).is_empty());
    }

    #[test]
    fn bounded_codes_evicts_oldest() {
        let store = Store::new();
        let mut codes = Vec::new();
        for i in 0..65 {
            codes.push(store.mint(vec![PathBuf::from(format!("/workspace/{i}"))]));
        }
        // First code was evicted when 65th was minted
        assert!(store.exchange(&codes[0]).is_none());
        // 65th code is valid
        assert!(store.exchange(&codes[64]).is_some());
    }

    #[test]
    fn bounded_credentials_evicts_oldest() {
        let store = Store::new();
        let mut creds = Vec::new();
        for i in 0..257 {
            let code = store.mint(vec![PathBuf::from(format!("/workspace/{i}"))]);
            let (cred, _) = store.exchange(&code).expect("exchange");
            creds.push(cred);
        }
        // First credential was evicted when 257th was stored
        assert!(!store.accepts(&creds[0]));
        // 257th credential is valid
        assert!(store.accepts(&creds[256]));
    }

    #[test]
    fn revoke_removes_credential() {
        let store = Store::new();
        let code = store.mint(vec![PathBuf::from("/workspace/foo")]);
        let (cred, _) = store.exchange(&code).expect("exchange");
        assert!(store.accepts(&cred));
        assert_eq!(
            store.launch_workspaces(&cred),
            vec![PathBuf::from("/workspace/foo")]
        );

        assert!(store.revoke(&cred));
        assert!(!store.accepts(&cred));
        assert!(store.launch_workspaces(&cred).is_empty());
        // Second revoke returns false
        assert!(!store.revoke(&cred));
    }

    #[test]
    fn constant_time_comparison_behavior() {
        let store = Store::new();
        let code = store.mint(vec![]);
        let (cred, _) = store.exchange(&code).expect("exchange");

        assert!(store.accepts(&cred));
        // Wrong credential
        assert!(!store.accepts("invalid-token"));
        // Tampered byte
        let mut tampered = cred.clone();
        let last = if tampered.ends_with('a') { 'b' } else { 'a' };
        tampered.pop();
        tampered.push(last);
        assert!(!store.accepts(&tampered));
    }
}
