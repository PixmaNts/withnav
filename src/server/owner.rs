//! The one process that owns a WithNav database: its lock, its published record and the proof a
//! client asks before it sends the owner's token.
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The record format and the frozen `/v1/hello` + `/v1/shutdown` shapes. A client refuses an
/// owner with another protocol, except `nav serve stop`, which only uses the frozen shapes.
pub const PROTOCOL: u32 = 1;

/// Where the owner of one database keeps its lock, record and log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerPaths {
    /// The canonical identity of the database: the resolved target of `withnav.db`.
    pub database: PathBuf,
    /// `<database>.owner.lock`: never unlinked, held by the owner for its whole life.
    pub lock: PathBuf,
    /// `<data dir>/server/`: mode 0700.
    pub dir: PathBuf,
    /// `<dir>/owner.json`: mode 0600, written and removed only by the lock holder.
    pub record: PathBuf,
    /// `<dir>/server.log`: the detached owner's stdout and stderr, mode 0600.
    pub log: PathBuf,
}

impl OwnerPaths {
    /// Resolves the canonical paths for the database owner.
    /// Follows symlinks to canonicalize `withnav.db`; if absent, canonicalizes the parent directory.
    pub fn resolve(history_db: &Path) -> anyhow::Result<Self> {
        let database = if history_db.exists() {
            std::fs::canonicalize(history_db)?
        } else {
            let parent = history_db.parent().unwrap_or(Path::new("."));
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
            let canonical_parent = std::fs::canonicalize(parent)?;
            let file_name = history_db
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("history database path has no file name"))?;
            canonical_parent.join(file_name)
        };

        let mut lock_name = database.as_os_str().to_os_string();
        lock_name.push(".owner.lock");
        let lock = PathBuf::from(lock_name);

        let data_dir = database
            .parent()
            .ok_or_else(|| anyhow::anyhow!("database has no parent directory"))?;
        let dir = data_dir.join("server");
        let record = dir.join("owner.json");
        let log = dir.join("server.log");

        Ok(Self {
            database,
            lock,
            dir,
            record,
            log,
        })
    }

    /// Ensures the `<data dir>/server` directory exists with mode 0700, owned by the current user.
    /// If permissions or ownership do not match, the directory is refused without repairing.
    pub fn ensure_dir(&self) -> anyhow::Result<()> {
        if !self.dir.exists() {
            // Two clients starting the first owner at once both get here; the one that loses
            // the creation race checks the directory the winner made, like any existing one.
            #[cfg(unix)]
            let created = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(&self.dir)
            };
            #[cfg(not(unix))]
            let created = std::fs::create_dir(&self.dir);
            match created {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::symlink_metadata(&self.dir)?;
            if !meta.file_type().is_dir() {
                anyhow::bail!("server directory {:?} is not a directory", self.dir);
            }
            let current_uid = nix::unistd::getuid().as_raw();
            if meta.uid() != current_uid {
                anyhow::bail!(
                    "server directory {:?} is owned by uid {}, expected {}",
                    self.dir,
                    meta.uid(),
                    current_uid
                );
            }
            let mode = meta.mode() & 0o777;
            if mode != 0o700 {
                anyhow::bail!(
                    "server directory {:?} has mode {:04o}, expected 0700; refused",
                    self.dir,
                    mode
                );
            }
        }
        #[cfg(windows)]
        {
            withnav_provider::auth::protect_private(&self.dir)
                .map_err(|e| anyhow::anyhow!("cannot protect server directory: {e}"))?;
            withnav_provider::auth::check_private(&self.dir)
                .map_err(|e| anyhow::anyhow!("server directory ACL check failed: {e}"))?;
        }

        Ok(())
    }
}

/// The exclusive non-blocking lock held by the database owner process.
pub struct OwnerLock {
    #[cfg(unix)]
    _lock: Flock<File>,
    #[cfg(windows)]
    _file: File,
}

impl std::fmt::Debug for OwnerLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerLock").finish()
    }
}

impl OwnerLock {
    /// Attempts to acquire an exclusive lock on `<database>.owner.lock`.
    /// The file is opened with `O_CLOEXEC` to prevent child tools from inheriting it.
    ///
    /// Never creates the data directory: an owner whose directory was deleted must not bring it
    /// back by probing its own lock, and the starting owner creates it before anything else.
    pub fn try_acquire(paths: &OwnerPaths) -> io::Result<Option<Self>> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(nix::libc::O_CLOEXEC);
        }
        let file = options.open(&paths.lock)?;

        #[cfg(unix)]
        {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) => Ok(Some(OwnerLock { _lock: lock })),
                Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
                Err((_, err)) => Err(io::Error::from_raw_os_error(err as i32)),
            }
        }
        #[cfg(windows)]
        {
            match file.try_lock() {
                Ok(()) => Ok(Some(OwnerLock { _file: file })),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(std::fs::TryLockError::Error(err)) => Err(err),
            }
        }
    }

    /// Probes whether the lock is currently held by attempting to lock and immediately release.
    pub fn is_held(paths: &OwnerPaths) -> io::Result<bool> {
        if !paths.lock.exists() {
            return Ok(false);
        }
        match Self::try_acquire(paths)? {
            Some(lock) => {
                drop(lock);
                Ok(false)
            }
            None => Ok(true),
        }
    }
}

/// What the owner publishes once its listener is bound and authentication is live.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub protocol: u32,
    /// `WITHNAV_VERSION` of the owner's binary.
    pub build: String,
    /// Random per owner start; part of the hello proof.
    pub instance: String,
    pub pid: u32,
    /// Always on 127.0.0.1.
    pub port: u16,
    /// 256 bits from the OS, hex. Never logged, never in a URL or argv.
    pub token: String,
    pub config_dir: PathBuf,
    pub models_path: Option<PathBuf>,
    pub started_at: String,
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("protocol", &self.protocol)
            .field("build", &self.build)
            .field("instance", &self.instance)
            .field("pid", &self.pid)
            .field("port", &self.port)
            .field("token", &"<redacted>")
            .field("config_dir", &self.config_dir)
            .field("models_path", &self.models_path)
            .field("started_at", &self.started_at)
            .finish()
    }
}

impl Record {
    /// Writes the record to a temporary file (0600) and atomically renames it to `owner.json`.
    pub fn write_to(&self, paths: &OwnerPaths) -> anyhow::Result<()> {
        paths.ensure_dir()?;
        let temp_path = paths
            .dir
            .join(format!("owner.json.tmp.{}", ulid::Ulid::generate()));
        {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            use std::io::Write;
            let mut file = options.open(&temp_path)?;
            let json_bytes = serde_json::to_vec_pretty(self)?;
            file.write_all(&json_bytes)?;
            file.sync_all()?;
        }
        #[cfg(windows)]
        {
            withnav_provider::auth::protect_private(&temp_path)
                .map_err(|e| anyhow::anyhow!("cannot protect owner record: {e}"))?;
        }
        std::fs::rename(&temp_path, &paths.record)?;
        Ok(())
    }

    /// Reads and validates the record file from disk.
    /// Checks regular file status, ownership, 0600-safe permissions (& 077 == 0) and bounded size (<= 4 KiB).
    pub fn read_from(paths: &OwnerPaths) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::symlink_metadata(&paths.record)?;
            if !meta.file_type().is_file() {
                anyhow::bail!("owner record {:?} is not a regular file", paths.record);
            }
            let current_uid = nix::unistd::getuid().as_raw();
            if meta.uid() != current_uid {
                anyhow::bail!("owner record {:?} not owned by current user", paths.record);
            }
            if meta.mode() & 0o077 != 0 {
                anyhow::bail!(
                    "owner record {:?} permissions {:04o} allow group or other access",
                    paths.record,
                    meta.mode() & 0o777
                );
            }
            if meta.len() > 4096 {
                anyhow::bail!("owner record {:?} exceeds 4 KiB size limit", paths.record);
            }
        }
        #[cfg(windows)]
        {
            let meta = std::fs::symlink_metadata(&paths.record)?;
            if !meta.file_type().is_file() {
                anyhow::bail!("owner record {:?} is not a regular file", paths.record);
            }
            if meta.len() > 4096 {
                anyhow::bail!("owner record {:?} exceeds 4 KiB size limit", paths.record);
            }
            withnav_provider::auth::check_private(&paths.record).map_err(|e| {
                anyhow::anyhow!("owner record {:?} ACL check failed: {e}", paths.record)
            })?;
        }
        let bytes = std::fs::read(&paths.record)?;
        if bytes.len() > 4096 {
            anyhow::bail!("owner record {:?} exceeds 4 KiB size limit", paths.record);
        }
        let record: Self = serde_json::from_slice(&bytes)?;
        Ok(record)
    }

    /// Removes the published record file under `<dir>/owner.json`.
    pub fn remove(paths: &OwnerPaths) -> io::Result<()> {
        if paths.record.exists() {
            std::fs::remove_file(&paths.record)
        } else {
            Ok(())
        }
    }
}

/// Generates 32 random bytes from OS entropy, returned in hex representation.
pub fn generate_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("failed to read OS entropy: {e}"))?;
    Ok(hex::encode(bytes))
}

/// Generates 16 random bytes from OS entropy, returned in hex representation.
pub fn generate_instance() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("failed to read OS entropy: {e}"))?;
    Ok(hex::encode(bytes))
}

/// Computes HMAC-SHA256 according to RFC 2104.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut k = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hash = Sha256::digest(key);
        k[..hash.len()].copy_from_slice(&hash);
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let outer_hash = outer.finalize();

    hex::encode(outer_hash)
}

/// `GET /v1/hello?nonce=<hex>` answer (public route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub build: String,
    pub instance: String,
    /// Hex `HMAC-SHA256(token, nonce || instance)`.
    pub proof: String,
}

/// Generates the hello proof: `HMAC-SHA256(token, nonce || instance)` in hex.
pub fn hello_proof(token: &str, nonce: &str, instance: &str) -> String {
    let mut message = Vec::with_capacity(nonce.len() + instance.len());
    message.extend_from_slice(nonce.as_bytes());
    message.extend_from_slice(instance.as_bytes());
    hmac_sha256(token.as_bytes(), &message)
}

/// Compares two strings in constant time.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
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

/// `POST /v1/shutdown` body (authenticated). Without `force` a server with running turns
/// answers 409 and keeps running.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownRequest {
    #[serde(default)]
    pub force: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_rfc4231_test_case_1() {
        let key = [0x0bu8; 20];
        let data = b"Hi There";
        let expected = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        assert_eq!(hmac_sha256(&key, data), expected);
    }

    #[test]
    fn hmac_sha256_rfc4231_test_case_2() {
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let expected = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        assert_eq!(hmac_sha256(key, data), expected);
    }

    #[test]
    fn hello_proof_combines_nonce_and_instance() {
        let token = "deadbeef";
        let nonce = "1234";
        let instance = "5678";
        let proof = hello_proof(token, nonce, instance);
        let expected = hmac_sha256(token.as_bytes(), b"12345678");
        assert_eq!(proof, expected);
    }

    #[test]
    fn record_debug_redacts_token() {
        let record = Record {
            protocol: PROTOCOL,
            build: "0.8.4".into(),
            instance: "inst1".into(),
            pid: 1234,
            port: 7310,
            token: "secret_token_12345".into(),
            config_dir: PathBuf::from("/etc/withnav"),
            models_path: None,
            started_at: "2026-11-14T00:00:00Z".into(),
        };
        let debug = format!("{record:?}");
        assert!(!debug.contains("secret_token_12345"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn owner_lock_and_paths_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("data/withnav.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::write(&db_path, []).unwrap();

        let paths = OwnerPaths::resolve(&db_path).unwrap();
        assert_eq!(paths.database, std::fs::canonicalize(&db_path).unwrap());
        assert_eq!(
            paths.lock,
            paths
                .database
                .parent()
                .unwrap()
                .join("withnav.db.owner.lock")
        );
        assert_eq!(paths.dir, paths.database.parent().unwrap().join("server"));
        assert_eq!(paths.record, paths.dir.join("owner.json"));
        assert_eq!(paths.log, paths.dir.join("server.log"));

        assert!(!OwnerLock::is_held(&paths).unwrap());

        let lock = OwnerLock::try_acquire(&paths)
            .unwrap()
            .expect("acquire lock");
        assert!(OwnerLock::is_held(&paths).unwrap());
        assert!(OwnerLock::try_acquire(&paths).unwrap().is_none());

        let record = Record {
            protocol: PROTOCOL,
            build: "0.8.4".into(),
            instance: "inst1".into(),
            pid: std::process::id(),
            port: 8080,
            token: generate_token().unwrap(),
            config_dir: temp.path().join("config"),
            models_path: None,
            started_at: "2026-11-14T00:00:00Z".into(),
        };

        record.write_to(&paths).unwrap();
        let read = Record::read_from(&paths).unwrap();
        assert_eq!(read, record);

        Record::remove(&paths).unwrap();
        assert!(!paths.record.exists());

        drop(lock);
        assert!(!OwnerLock::is_held(&paths).unwrap());
    }
}
