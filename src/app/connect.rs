//! How every local client reaches the one server that owns the database: find it, prove it is
//! the owner, check it is compatible, or start it in the background.
use std::path::{Path, PathBuf};

use crate::server::owner::{OwnerLock, OwnerPaths, PROTOCOL, Record};

/// What a client needs from the owner it joins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub history_db: PathBuf,
    /// An explicit `--models`; the owner must have been started with the same one.
    pub models: Option<PathBuf>,
    /// The `nav` binary started as `nav serve --detach` when no owner runs. Tests pass
    /// `CARGO_BIN_EXE_nav`; the binaries pass `std::env::current_exe()`.
    pub exe: PathBuf,
}

impl Wanted {
    /// The user's directories and this executable, with an optional explicit `--models`.
    pub fn from_env(models: Option<&Path>) -> anyhow::Result<Self> {
        let config_dir = crate::provider::auth::default_config_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve config directory"))?;
        let data_dir = crate::paths::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve data directory"))?;
        let history_db = crate::paths::history_db()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve history database path"))?;
        let models = models
            .map(|m| {
                if m.is_absolute() {
                    Ok(m.to_path_buf())
                } else {
                    std::env::current_dir().map(|cwd| cwd.join(m))
                }
            })
            .transpose()?;
        let exe = std::env::current_exe()?;
        Ok(Self {
            config_dir,
            data_dir,
            history_db,
            models,
            exe,
        })
    }
}

/// An authenticated, compatible owner.
#[derive(Clone, PartialEq, Eq)]
pub struct Owner {
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    pub token: String,
    pub instance: String,
    pub record: Record,
}

impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Owner")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("instance", &self.instance)
            .finish()
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.len() != b_bytes.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a_bytes.iter().zip(b_bytes) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(unix)]
fn spawn_detached(wanted: &Wanted, paths: &OwnerPaths) -> anyhow::Result<()> {
    paths.ensure_dir()?;
    let mut log_options = std::fs::OpenOptions::new();
    log_options.create(true).append(true);
    use std::os::unix::fs::OpenOptionsExt;
    log_options.mode(0o600);
    let log_file = log_options.open(&paths.log)?;

    let mut cmd = std::process::Command::new(&wanted.exe);
    cmd.arg("serve");
    cmd.arg("--detach");
    if let Some(ref models) = wanted.models {
        cmd.arg("--models");
        cmd.arg(models);
    }
    cmd.env("WITHNAV_DATA_DIR", &wanted.data_dir);
    cmd.env("WITHNAV_CONFIG_DIR", &wanted.config_dir);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(log_file.try_clone()?);
    cmd.stderr(log_file);

    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if nix::libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            nix::libc::signal(nix::libc::SIGHUP, nix::libc::SIG_IGN);
            Ok(())
        });
    }

    cmd.spawn()?;
    Ok(())
}

#[cfg(windows)]
fn spawn_detached(wanted: &Wanted, paths: &OwnerPaths) -> anyhow::Result<()> {
    paths.ensure_dir()?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)?;

    let mut cmd = std::process::Command::new(&wanted.exe);
    cmd.arg("serve");
    cmd.arg("--detach");
    if let Some(ref models) = wanted.models {
        cmd.arg("--models");
        cmd.arg(models);
    }
    cmd.env("WITHNAV_DATA_DIR", &wanted.data_dir);
    cmd.env("WITHNAV_CONFIG_DIR", &wanted.config_dir);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(log_file.try_clone()?);
    cmd.stderr(log_file);

    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);

    match cmd.spawn() {
        Ok(_) => Ok(()),
        Err(err) => {
            anyhow::bail!(
                "detached server spawn failed on Windows (job object may forbid breakaway: {err}); start `nav serve` manually"
            );
        }
    }
}

/// The running owner, after the hello proof and the protocol and configuration checks; starts
/// `nav serve --detach` and waits for it (bounded) when none runs. Never replaces a live owner.
pub async fn ensure_server(wanted: &Wanted) -> anyhow::Result<Owner> {
    if let Some(owner) = find_server(wanted).await? {
        return Ok(owner);
    }

    let paths = OwnerPaths::resolve(&wanted.history_db)?;
    spawn_detached(wanted, &paths)?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut interval = std::time::Duration::from_millis(50);
    let max_interval = std::time::Duration::from_millis(500);

    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        if let Some(owner) = find_server(wanted).await? {
            return Ok(owner);
        }
        interval = (interval * 2).min(max_interval);
    }

    anyhow::bail!("timed out waiting for background owner to start (15s); `nav serve status`");
}

/// Like [`ensure_server`] but never starts one: `None` when no owner holds the lock.
pub async fn find_server(wanted: &Wanted) -> anyhow::Result<Option<Owner>> {
    // Looking never creates anything: without a data directory there is no owner, and a
    // read-only command such as `nav doctor` must leave the disk untouched.
    if !wanted
        .history_db
        .parent()
        .is_some_and(std::path::Path::exists)
    {
        return Ok(None);
    }
    let paths = OwnerPaths::resolve(&wanted.history_db)?;
    if !OwnerLock::is_held(&paths)? {
        return Ok(None);
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut record = None;
    while tokio::time::Instant::now() < deadline {
        match Record::read_from(&paths) {
            Ok(r) => {
                record = Some(r);
                break;
            }
            Err(_) => {
                if !OwnerLock::is_held(&paths)? {
                    return Ok(None);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }

    let record = record
        .ok_or_else(|| anyhow::anyhow!("a server is starting or stuck; `nav serve status`"))?;

    if record.protocol != PROTOCOL {
        anyhow::bail!(
            "server protocol mismatch (server build {}, client build {}); run `nav serve stop`",
            record.build,
            env!("WITHNAV_VERSION")
        );
    }

    let wanted_cfg =
        std::fs::canonicalize(&wanted.config_dir).unwrap_or_else(|_| wanted.config_dir.clone());
    let server_cfg =
        std::fs::canonicalize(&record.config_dir).unwrap_or_else(|_| record.config_dir.clone());
    if wanted_cfg != server_cfg {
        anyhow::bail!(
            "server config dir mismatch (server uses {}, wanted {}); run `nav serve stop`",
            record.config_dir.display(),
            wanted.config_dir.display()
        );
    }

    if let Some(ref wanted_models) = wanted.models {
        let matches_models = record.models_path.as_ref().is_some_and(|server_models| {
            std::fs::canonicalize(wanted_models).ok() == std::fs::canonicalize(server_models).ok()
                || wanted_models == server_models
        });
        if !matches_models {
            anyhow::bail!(
                "server models path mismatch (server uses {:?}, wanted {:?}); run `nav serve stop`",
                record.models_path,
                wanted.models
            );
        }
    }

    let mut nonce_bytes = [0u8; 32];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| anyhow::anyhow!("failed to read OS entropy: {e}"))?;
    let nonce = hex::encode(nonce_bytes);

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let hello_url = format!("http://127.0.0.1:{}/v1/hello?nonce={nonce}", record.port);
    let resp = client.get(&hello_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("server returned status {} on hello probe", resp.status());
    }

    let hello: crate::server::owner::Hello = resp.json().await?;
    if hello.instance != record.instance {
        anyhow::bail!(
            "server instance mismatch: expected {}, got {}",
            record.instance,
            hello.instance
        );
    }

    let expected_proof = crate::server::owner::hello_proof(&record.token, &nonce, &record.instance);
    if !constant_time_eq(&hello.proof, &expected_proof) {
        anyhow::bail!("owner proof verification failed: invalid hello answer");
    }

    Ok(Some(Owner {
        url: format!("http://127.0.0.1:{}", record.port),
        token: record.token.clone(),
        instance: record.instance.clone(),
        record,
    }))
}
