//! The one process that owns a WithNav database: its lock, its published record and the proof a
//! client asks before it sends the owner's token.
//!
//! Interface agreed between the server, the clients and the editor bridge; the bodies are filled
//! by the owner implementation.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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

/// `GET /v1/hello?nonce=<hex>` answer (public route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub build: String,
    pub instance: String,
    /// Hex `HMAC-SHA256(token, nonce || instance)`.
    pub proof: String,
}

/// `POST /v1/shutdown` body (authenticated). Without `force` a server with running turns
/// answers 409 and keeps running.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownRequest {
    #[serde(default)]
    pub force: bool,
}
