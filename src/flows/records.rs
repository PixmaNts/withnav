//! Private bounded captures; health is derived only from live records.
use super::fs::Directory;
use instinct_flow::RunRecord;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};
pub struct Records {
    path: PathBuf,
    write: Mutex<()>,
}
impl Records {
    pub fn new(data: &Path, workspace: &Path) -> Self {
        Self {
            path: data.join("flows/runs").join(format!(
                "{:x}",
                Sha256::digest(workspace.as_os_str().as_encoded_bytes())
            )),
            write: Mutex::new(()),
        }
    }
    pub fn read(&self) -> std::io::Result<Vec<RunRecord>> {
        let dir = match Directory::private(&self.path, false) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        match dir.read("runs.jsonl", 32 * 1024 * 1024, true) {
            Ok(text) => Ok(text
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e),
        }
    }
    pub fn append(&self, record: &RunRecord) -> std::io::Result<()> {
        let _guard = self
            .write
            .lock()
            .map_err(|_| std::io::Error::other("capture writer poisoned"))?;
        let size = serde_json::to_vec(record)
            .map_err(std::io::Error::other)?
            .len();
        if size > 32 * 1024 * 1024 {
            return Err(std::io::Error::other("capture exceeds byte limit"));
        }
        let mut records = self.read()?;
        records.push(record.clone());
        let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(7);
        records.retain(|r| {
            time::OffsetDateTime::parse(
                &r.started_at,
                &time::format_description::well_known::Rfc3339,
            )
            .is_ok_and(|t| t >= cutoff)
        });
        let mut lines: Vec<String> = records
            .into_iter()
            .filter_map(|r| serde_json::to_string(&r).ok())
            .collect();
        let mut bytes = lines.iter().map(|l| l.len() + 1).sum::<usize>();
        while lines.len() > 1000 || bytes > 32 * 1024 * 1024 {
            if lines.is_empty() {
                break;
            }
            bytes = bytes.saturating_sub(lines.remove(0).len() + 1);
        }
        Directory::private(&self.path, true)?
            .write("runs.jsonl", format!("{}\n", lines.join("\n")).as_bytes())
    }
}
