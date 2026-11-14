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
        let raw_cfg = crate::provider::auth::default_config_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve config directory"))?;
        let config_dir = std::fs::canonicalize(&raw_cfg).unwrap_or(raw_cfg);
        let data_dir = crate::paths::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve data directory"))?;
        let history_db = crate::paths::history_db()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve history database path"))?;
        let models = models
            .map(|m| {
                let abs = if m.is_absolute() {
                    m.to_path_buf()
                } else {
                    std::env::current_dir()?.join(m)
                };
                Ok::<PathBuf, anyhow::Error>(std::fs::canonicalize(&abs).unwrap_or(abs))
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

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct CompatibilityError(String);

async fn probe_live_owner(wanted: &Wanted, record: &Record) -> anyhow::Result<Owner> {
    let mut nonce_bytes = [0u8; 32];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| anyhow::anyhow!("failed to read OS entropy: {e}"))?;
    let nonce = hex::encode(nonce_bytes);

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(3))
        .build()?;

    let hello_url = format!("http://127.0.0.1:{}/v1/hello?nonce={nonce}", record.port);
    let mut resp = client.get(&hello_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("server returned status {} on hello probe", resp.status());
    }

    let mut body_bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body_bytes.len() + chunk.len() > 4096 {
            anyhow::bail!("hello response exceeded 4 KiB limit");
        }
        body_bytes.extend_from_slice(&chunk);
    }

    let hello: crate::server::owner::Hello = serde_json::from_slice(&body_bytes)?;
    if hello.instance != record.instance {
        anyhow::bail!(
            "server instance mismatch: expected {}, got {}",
            record.instance,
            hello.instance
        );
    }

    let expected_proof = crate::server::owner::hello_proof(&record.token, &nonce, &record.instance);
    if !crate::server::owner::constant_time_eq(&hello.proof, &expected_proof) {
        anyhow::bail!("owner proof verification failed: invalid hello answer");
    }

    if record.protocol != PROTOCOL {
        return Err(CompatibilityError(format!(
            "server protocol mismatch (server build {}, client build {}); run `nav serve stop`",
            record.build,
            env!("WITHNAV_VERSION")
        ))
        .into());
    }

    if record.build != env!("WITHNAV_VERSION") {
        return Err(CompatibilityError(format!(
            "server build mismatch (server {}, client {}); run `nav serve stop`",
            record.build,
            env!("WITHNAV_VERSION")
        ))
        .into());
    }

    let wanted_cfg = std::fs::canonicalize(&wanted.config_dir)
        .map_err(|e| CompatibilityError(format!("cannot canonicalize wanted config dir: {e}")))?;
    let server_cfg = std::fs::canonicalize(&record.config_dir)
        .map_err(|e| CompatibilityError(format!("cannot canonicalize server config dir: {e}")))?;
    if wanted_cfg != server_cfg {
        return Err(CompatibilityError(format!(
            "server config dir mismatch (server uses {}, wanted {}); run `nav serve stop`",
            record.config_dir.display(),
            wanted.config_dir.display()
        ))
        .into());
    }

    if let Some(ref wanted_models) = wanted.models {
        let Some(ref server_models) = record.models_path else {
            return Err(CompatibilityError(format!(
                "server models path mismatch (server uses None, wanted {:?}); run `nav serve stop`",
                wanted.models
            ))
            .into());
        };
        let wanted_canon = std::fs::canonicalize(wanted_models).map_err(|e| {
            CompatibilityError(format!("cannot canonicalize wanted models path: {e}"))
        })?;
        let server_canon = std::fs::canonicalize(server_models).map_err(|e| {
            CompatibilityError(format!("cannot canonicalize server models path: {e}"))
        })?;
        if wanted_canon != server_canon {
            return Err(CompatibilityError(format!(
                "server models path mismatch (server uses {:?}, wanted {:?}); run `nav serve stop`",
                record.models_path, wanted.models
            ))
            .into());
        }
    }

    Ok(Owner {
        url: format!("http://127.0.0.1:{}", record.port),
        token: record.token.clone(),
        instance: record.instance.clone(),
        record: record.clone(),
    })
}

/// Like [`ensure_server`] but never starts one: `None` when no owner holds the lock.
pub async fn find_server(wanted: &Wanted) -> anyhow::Result<Option<Owner>> {
    if !wanted
        .history_db
        .parent()
        .is_some_and(std::path::Path::exists)
    {
        return Ok(None);
    }
    let paths = OwnerPaths::resolve(&wanted.history_db)?;
    let held = OwnerLock::is_held(&paths)?;
    if !held {
        return Ok(None);
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if !OwnerLock::is_held(&paths)? {
            return Ok(None);
        }
        if let Ok(record) = Record::read_from(&paths) {
            match probe_live_owner(wanted, &record).await {
                Ok(owner) => return Ok(Some(owner)),
                Err(err) => {
                    if let Some(incompat) = err.downcast_ref::<CompatibilityError>() {
                        return Err(anyhow::anyhow!("{incompat}"));
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    if !OwnerLock::is_held(&paths)? {
        return Ok(None);
    }
    anyhow::bail!("a server is starting or stuck; `nav serve status`");
}
