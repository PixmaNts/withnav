//! How the terminal client finds the session server an editor's `nav lsp` already runs for the
//! same workspace, so both clients share one server and therefore the same conversations.
//!
//! `nav lsp` publishes one file per process, `<runtime dir>/withnav-servers/<key>-<instance>.json`,
//! holding the loopback port and the bearer token of its in-process server. `<key>` is derived
//! from the canonical workspace path and `<instance>` is a fresh ULID, so a file name is never
//! reused: two editors on one workspace each own their own file, and removing a stale file can
//! never remove a newer one. The file goes away when the LSP exits normally; after a crash it
//! stays until a lookup proves it stale.
//!
//! The token is a credential: the directory is `0700` and the file `0600`, both owned by the
//! user, and discovery is only offered inside a private `$XDG_RUNTIME_DIR` (cleared at logout,
//! never shared between hosts). Without one, and on Windows, nothing is published and the
//! terminal client starts its own server. Not under `$XDG_RUNTIME_DIR/withnav`: that is the
//! agent's default scratch root, which the agent may write to without asking.
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The directory name under `$XDG_RUNTIME_DIR`.
pub const DIR_NAME: &str = "withnav-servers";
/// The format of a discovery file; a file with another version is ignored.
const VERSION: u32 = 1;
/// A discovery file is a few hundred bytes; anything larger is not one of ours.
const MAX_ENTRY_BYTES: u64 = 4096;
/// One request to a published server: it is on loopback, so a slower answer means it is busy.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// The whole lookup, so a few unresponsive entries cannot hold the terminal client's start-up.
const LOOKUP_BUDGET: Duration = Duration::from_secs(3);

/// What `nav lsp` publishes about its session server.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub version: u32,
    /// The ULID in the file name: unique per publication and ordered by its millisecond.
    pub instance: String,
    /// The loopback port; the URL is always built from it, never read from the file.
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// The canonical workspace path the LSP was initialized with.
    pub workspace: PathBuf,
    pub started_at: String,
}

impl Entry {
    /// The server's base URL, always on the IPv4 loopback address.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("instance", &self.instance)
            .field("port", &self.port)
            .field("token", &"<redacted>")
            .field("pid", &self.pid)
            .field("workspace", &self.workspace)
            .field("started_at", &self.started_at)
            .finish()
    }
}

/// The discovery directory, when this machine has a private runtime directory.
pub fn dir() -> Option<PathBuf> {
    dir_in(
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .as_deref(),
    )
}

#[cfg(unix)]
fn dir_in(runtime: Option<&Path>) -> Option<PathBuf> {
    let runtime = runtime.filter(|path| crate::agent::scratch::private_runtime_usable(path))?;
    Some(runtime.join(DIR_NAME))
}

#[cfg(not(unix))]
fn dir_in(_runtime: Option<&Path>) -> Option<PathBuf> {
    None
}

/// The file-name prefix of a workspace: the first 16 hex characters of the SHA-256 of its
/// canonical path, so the path itself never appears in a file name.
pub fn key(workspace: &Path) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    hex::encode(&digest[..8])
}

/// The canonical form of a workspace, as `nav tui` resolves its own; `None` when it cannot be
/// resolved, in which case nothing is published.
pub fn canonical_workspace(path: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(path).ok()?;
    crate::tools::canonical_path(&resolved).ok()
}

/// A published discovery file, removed when dropped.
#[derive(Debug)]
pub struct Published {
    path: PathBuf,
}

impl Published {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Published {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Publishes the server `nav lsp` runs for `workspace` in [`dir`]. Failures are logged without
/// the token and give `None`: the editor's chat works the same without discovery.
pub fn publish_for(workspace: &Path, port: u16, token: &str) -> Option<Published> {
    let dir = dir()?;
    let Some(workspace) = canonical_workspace(workspace) else {
        tracing::warn!(
            "server discovery off: workspace {} cannot be resolved",
            workspace.display()
        );
        return None;
    };
    match publish(&dir, &workspace, port, token) {
        Ok(published) => Some(published),
        Err(err) => {
            tracing::warn!("server discovery off: {err}");
            None
        }
    }
}

/// Writes a discovery file for `workspace` (already canonical) into `dir`, creating `dir` as
/// a private directory. The file is written under a temporary name with mode `0600` and renamed
/// into place, so a reader never sees half of it.
pub fn publish(dir: &Path, workspace: &Path, port: u16, token: &str) -> std::io::Result<Published> {
    use std::io::Write as _;
    private_dir(dir, true)?;
    let instance = ulid::Ulid::generate().to_string().to_lowercase();
    let entry = Entry {
        version: VERSION,
        instance: instance.clone(),
        port,
        token: token.to_owned(),
        pid: std::process::id(),
        workspace: workspace.to_path_buf(),
        started_at: withnav_client_api::event::now_rfc3339(),
    };
    let name = format!("{}-{instance}.json", key(workspace));
    let path = dir.join(&name);
    let temporary = dir.join(format!(".{name}.tmp"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let written = options.open(&temporary).and_then(|mut file| {
        file.write_all(&serde_json::to_vec(&entry).map_err(std::io::Error::other)?)?;
        file.sync_all()
    });
    if let Err(err) = written.and_then(|()| std::fs::rename(&temporary, &path)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(err);
    }
    Ok(Published { path })
}

/// The newest live server published for `workspace` (canonical) in `dir`, if any.
///
/// An entry is live when its process exists, `GET /v1/health` answers and an authenticated
/// `GET /v1/mcp` with its token is accepted. An entry proven stale (its process is gone, nothing
/// listens on its port, or its token is refused) is removed; one that only timed out or failed
/// is kept, since its editor may still be busy starting.
pub async fn find(dir: &Path, workspace: &Path) -> Option<Entry> {
    let mut entries = candidates(dir, workspace);
    // The instance is a ULID: the greatest one was published last.
    entries.sort_by(|(a, _), (b, _)| b.instance.cmp(&a.instance));
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let lookup = async {
        for (entry, path) in entries {
            match probe(&client, &entry).await {
                Probe::Live => return Some(entry),
                Probe::Stale => {
                    let _ = std::fs::remove_file(&path);
                }
                Probe::Unknown => {}
            }
        }
        None
    };
    tokio::time::timeout(LOOKUP_BUDGET, lookup)
        .await
        .ok()
        .flatten()
}

/// The well-formed entries of `workspace` in a private `dir`, with their paths.
fn candidates(dir: &Path, workspace: &Path) -> Vec<(Entry, PathBuf)> {
    if private_dir(dir, false).is_err() {
        return Vec::new();
    }
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let prefix = format!("{}-", key(workspace));
    listing
        .filter_map(Result::ok)
        .map(|item| item.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"))
        })
        .filter_map(|path| read_entry(&path).map(|entry| (entry, path)))
        .filter(|(entry, _)| entry.version == VERSION && entry.workspace == workspace)
        .collect()
}

/// Reads one entry only if it is a small regular file, not a link, owned by the user and
/// readable by nobody else.
fn read_entry(path: &Path) -> Option<Entry> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_ENTRY_BYTES {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != nix::unistd::Uid::current().as_raw() || metadata.mode() & 0o077 != 0 {
            return None;
        }
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// Makes sure `dir` is a directory, not a link, owned by the user with mode `0700`; creates it
/// first when `create` is set. An existing directory with other permissions is refused, never
/// repaired: someone else may have prepared it.
fn private_dir(dir: &Path, create: bool) -> std::io::Result<()> {
    if create {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != nix::unistd::Uid::current().as_raw()
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(std::io::Error::other(format!(
                "{} must be a 0700 directory owned by the current user",
                dir.display()
            )));
        }
    }
    Ok(())
}

enum Probe {
    Live,
    Stale,
    Unknown,
}

async fn probe(client: &reqwest::Client, entry: &Entry) -> Probe {
    if !process_exists(entry.pid) {
        return Probe::Stale;
    }
    let base = entry.url();
    match client.get(format!("{base}/v1/health")).send().await {
        Ok(response) if response.status().is_success() => {}
        Ok(_) => return Probe::Unknown,
        Err(err) if err.is_connect() => return Probe::Stale,
        Err(_) => return Probe::Unknown,
    }
    match client
        .get(format!("{base}/v1/mcp"))
        .bearer_auth(&entry.token)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => Probe::Live,
        Ok(response)
            if matches!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) =>
        {
            Probe::Stale
        }
        Ok(_) | Err(_) => Probe::Unknown,
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// A loopback server answering health, and `/v1/mcp` only with `token`.
    async fn server(token: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::get;
        let app = axum::Router::new()
            .route("/v1/health", get(|| async { "{\"status\":\"ok\"}" }))
            .route(
                "/v1/mcp",
                get(move |headers: HeaderMap| async move {
                    let expected = format!("Bearer {token}");
                    if headers
                        .get("authorization")
                        .is_some_and(|value| value.as_bytes() == expected.as_bytes())
                    {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, task)
    }

    /// A port nothing listens on any more.
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    struct Fixture {
        _root: tempfile::TempDir,
        dir: PathBuf,
        workspace: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let workspace = canonical_workspace(root.path()).unwrap();
        let dir = workspace.join(DIR_NAME);
        Fixture {
            dir,
            workspace,
            _root: root,
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn json_files(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|item| {
                item.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "json")
            })
            .count()
    }

    // Given a workspace and a server
    // When it is published and the guard is dropped
    // Then the file is owner-only in an owner-only directory, and is gone after the drop
    #[test]
    fn a_published_file_is_private_and_removed_on_drop() {
        let f = fixture();
        let published = publish(&f.dir, &f.workspace, 4242, "secret-token").unwrap();
        assert_eq!(mode(&f.dir), 0o700);
        assert_eq!(mode(published.path()), 0o600);
        let entry = read_entry(published.path()).unwrap();
        assert_eq!(entry.port, 4242);
        assert_eq!(entry.token, "secret-token");
        assert_eq!(entry.pid, std::process::id());
        assert_eq!(entry.workspace, f.workspace);
        let path = published.path().to_path_buf();
        drop(published);
        assert!(!path.exists());
        assert_eq!(json_files(&f.dir), 0, "no temporary file is left behind");
    }

    // Given a running server published for the workspace
    // When the terminal client looks it up
    // Then it gets that server's port and token
    #[tokio::test]
    async fn a_live_server_is_found() {
        let f = fixture();
        let (port, task) = server("good").await;
        let _published = publish(&f.dir, &f.workspace, port, "good").unwrap();
        let found = find(&f.dir, &f.workspace).await.expect("found");
        assert_eq!(found.port, port);
        assert_eq!(found.token, "good");
        assert_eq!(found.url(), format!("http://127.0.0.1:{port}"));
        task.abort();
    }

    // Given two live servers for one workspace
    // When the terminal client looks them up
    // Then the one published last is chosen
    #[tokio::test]
    async fn the_newest_live_server_wins() {
        let f = fixture();
        let (first, a) = server("one").await;
        let (second, b) = server("two").await;
        let _old = publish(&f.dir, &f.workspace, first, "one").unwrap();
        // A ULID orders by millisecond; within one millisecond the order is random.
        std::thread::sleep(Duration::from_millis(5));
        let _new = publish(&f.dir, &f.workspace, second, "two").unwrap();
        assert_eq!(find(&f.dir, &f.workspace).await.unwrap().port, second);
        a.abort();
        b.abort();
    }

    // Given entries whose port is closed, whose token is refused, or whose process is gone
    // When the terminal client looks them up
    // Then none is returned and each file is removed
    #[tokio::test]
    async fn stale_entries_are_ignored_and_removed() {
        let f = fixture();
        let (port, task) = server("right").await;
        let closed = publish(&f.dir, &f.workspace, closed_port().await, "x").unwrap();
        let refused = publish(&f.dir, &f.workspace, port, "wrong").unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        let dead = publish(&f.dir, &f.workspace, port, "right").unwrap();
        let mut entry = read_entry(dead.path()).unwrap();
        entry.pid = dead_pid;
        std::fs::write(dead.path(), serde_json::to_vec(&entry).unwrap()).unwrap();

        assert!(find(&f.dir, &f.workspace).await.is_none());
        for published in [&closed, &refused, &dead] {
            assert!(!published.path().exists(), "{:?}", published.path());
        }
        task.abort();
    }

    // Given a server that answers health but fails the authenticated request
    // When the terminal client looks it up
    // Then it is not used, and its file is kept: it may be busy, not gone
    #[tokio::test]
    async fn an_uncertain_entry_is_skipped_but_kept() {
        use axum::http::StatusCode;
        use axum::routing::get;
        let f = fixture();
        let app = axum::Router::new()
            .route("/v1/health", get(|| async { "ok" }))
            .route("/v1/mcp", get(|| async { StatusCode::SERVICE_UNAVAILABLE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let published = publish(&f.dir, &f.workspace, port, "t").unwrap();
        assert!(find(&f.dir, &f.workspace).await.is_none());
        assert!(published.path().exists());
        task.abort();
    }

    // Given a live server published for another workspace
    // When the terminal client looks up its own
    // Then nothing is found and the other file is untouched
    #[tokio::test]
    async fn another_workspace_is_not_joined() {
        let f = fixture();
        let other = f.workspace.join("other");
        std::fs::create_dir(&other).unwrap();
        let (port, task) = server("t").await;
        let published = publish(&f.dir, &other, port, "t").unwrap();
        assert!(find(&f.dir, &f.workspace).await.is_none());
        assert!(published.path().exists());
        task.abort();
    }

    // Given entries that are readable by others, a symbolic link, or in a shared directory
    // When the terminal client looks them up
    // Then none of them is trusted
    #[tokio::test]
    async fn entries_that_are_not_private_are_not_trusted() {
        let f = fixture();
        let (port, task) = server("t").await;
        let published = publish(&f.dir, &f.workspace, port, "t").unwrap();

        std::fs::set_permissions(published.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(find(&f.dir, &f.workspace).await.is_none());
        std::fs::set_permissions(published.path(), std::fs::Permissions::from_mode(0o600)).unwrap();

        let target = f.workspace.join("elsewhere.json");
        std::fs::rename(published.path(), &target).unwrap();
        std::os::unix::fs::symlink(&target, published.path()).unwrap();
        assert!(find(&f.dir, &f.workspace).await.is_none());
        std::fs::remove_file(published.path()).unwrap();
        std::fs::rename(&target, published.path()).unwrap();
        assert!(find(&f.dir, &f.workspace).await.is_some());

        std::fs::set_permissions(&f.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(find(&f.dir, &f.workspace).await.is_none());
        assert!(
            publish(&f.dir, &f.workspace, port, "t").is_err(),
            "a shared directory is refused, not repaired"
        );
        std::fs::set_permissions(&f.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        task.abort();
    }

    // Given a runtime directory that is private, shared, or absent
    // When the discovery directory is resolved
    // Then only the private one offers discovery
    #[test]
    fn discovery_needs_a_private_runtime_directory() {
        let root = tempfile::tempdir().unwrap();
        let runtime = canonical_workspace(root.path()).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(dir_in(Some(&runtime)), Some(runtime.join(DIR_NAME)));
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(dir_in(Some(&runtime)), None);
        assert_eq!(dir_in(None), None);
    }

    // Given an entry
    // When it is formatted for a log
    // Then the token does not appear
    #[test]
    fn the_token_never_reaches_debug_output() {
        let entry = Entry {
            version: VERSION,
            instance: "i".into(),
            port: 1,
            token: "do-not-print".into(),
            pid: 1,
            workspace: "/w".into(),
            started_at: String::new(),
        };
        assert!(!format!("{entry:?}").contains("do-not-print"));
    }
}
