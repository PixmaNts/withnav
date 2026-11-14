//! How every local client reaches the one server that owns the database: find it, prove it is
//! the owner, check it is compatible, or start it in the background.
//!
//! Interface agreed between the clients and the owner; the bodies are filled by the owner
//! implementation.
use std::path::{Path, PathBuf};

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
        let _ = models;
        todo!("owner implementation")
    }
}

/// An authenticated, compatible owner.
#[derive(Clone, PartialEq, Eq)]
pub struct Owner {
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    pub token: String,
    pub instance: String,
    pub record: crate::server::owner::Record,
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

/// The running owner, after the hello proof and the protocol and configuration checks; starts
/// `nav serve --detach` and waits for it (bounded) when none runs. Never replaces a live owner.
pub async fn ensure_server(wanted: &Wanted) -> anyhow::Result<Owner> {
    let _ = wanted;
    todo!("owner implementation")
}

/// Like [`ensure_server`] but never starts one: `None` when no owner holds the lock.
pub async fn find_server(wanted: &Wanted) -> anyhow::Result<Option<Owner>> {
    let _ = wanted;
    todo!("owner implementation")
}
