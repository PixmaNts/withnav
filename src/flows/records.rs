//! Private bounded captures and independent, code-free hourly health aggregates.
use super::fs::Directory;
use instinct_flow::RunRecord;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Mutex, OnceLock,
    },
};
pub struct Records {
    path: PathBuf,
    write: Arc<Mutex<()>>,
    worker: OnceLock<SyncSender<RunRecord>>,
    dropped: Arc<AtomicU64>,
}
#[derive(Default, Serialize, Deserialize)]
struct Health {
    hours: BTreeMap<i64, BTreeMap<String, HealthRow>>,
    capture_failures: BTreeMap<i64, u64>,
}
#[derive(Default, Serialize, Deserialize)]
struct HealthRow {
    runs: u64,
    failures: u64,
    fallbacks_applied: u64,
    cost_usd: f64,
    cost_unknown_runs: u64,
}
impl Records {
    /// Nonblocking admission: bounded queue, no private filesystem access on the caller.
    pub fn enqueue(&self, record: RunRecord) {
        let sender = self.worker.get_or_init(|| {
            let (sender, receiver) = sync_channel::<RunRecord>(32);
            let writer = Self {
                path: self.path.clone(),
                write: self.write.clone(),
                worker: OnceLock::new(),
                dropped: self.dropped.clone(),
            };
            let dropped = self.dropped.clone();
            let result = std::thread::Builder::new()
                .name("flow-captures".into())
                .spawn(move || {
                    while let Ok(record) = receiver.recv() {
                        let failed = writer.append(&record).is_err();
                        if failed {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        if writer.update_health(&record, failed).is_err() {
                            tracing::warn!("private flow health update failed");
                        }
                    }
                });
            if result.is_err() {
                tracing::warn!("private flow capture worker unavailable");
            }
            sender
        });
        if sender.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn read_health(&self) -> std::io::Result<Health> {
        let dir = match Directory::private(&self.path, false) {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Health::default()),
            Err(e) => return Err(e),
        };
        match dir.read("health.json", 4 * 1024 * 1024, true) {
            Ok(text) => serde_json::from_str(&text).map_err(std::io::Error::other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Health::default()),
            Err(e) => Err(e),
        }
    }
    /// Called only after host application; samples and replay never contribute.
    pub fn update_health(&self, record: &RunRecord, capture_failed: bool) -> std::io::Result<()> {
        if record.mode != "live" {
            return Ok(());
        }
        let _guard = self
            .write
            .lock()
            .map_err(|_| std::io::Error::other("health writer poisoned"))?;
        let directory = Directory::private(&self.path, true)?;
        let _file_lock = directory.lock(".records.lock")?;
        let mut health = self.read_health()?;
        let hour = time::OffsetDateTime::now_utc().unix_timestamp() / 3600;
        health.hours.retain(|h, _| *h > hour - 24);
        health.capture_failures.retain(|h, _| *h > hour - 24);
        let row = health
            .hours
            .entry(hour)
            .or_default()
            .entry(record.flow_name.clone())
            .or_default();
        row.runs += 1;
        row.failures += u64::from(record.status == "failure");
        row.fallbacks_applied += u64::from(
            !record.fallback_actions.is_empty() && record.application.status == "applied",
        );
        if let Some(cost) = record.cost_usd {
            row.cost_usd += cost;
        } else {
            row.cost_unknown_runs += 1;
        }
        *health.capture_failures.entry(hour).or_default() += u64::from(capture_failed);
        Directory::private(&self.path, true)?.write(
            "health.json",
            &serde_json::to_vec(&health).map_err(std::io::Error::other)?,
        )
    }
    pub fn health(&self) -> std::io::Result<serde_json::Value> {
        let end = time::OffsetDateTime::now_utc();
        let hour = end.unix_timestamp() / 3600;
        let health = self.read_health()?;
        let mut rows: BTreeMap<String, HealthRow> = BTreeMap::new();
        for (_, entries) in health.hours.range((hour - 23)..=hour) {
            for (name, row) in entries {
                let sum = rows.entry(name.clone()).or_default();
                sum.runs += row.runs;
                sum.failures += row.failures;
                sum.fallbacks_applied += row.fallbacks_applied;
                sum.cost_usd += row.cost_usd;
                sum.cost_unknown_runs += row.cost_unknown_runs;
            }
        }
        let failures: u64 = health
            .capture_failures
            .range((hour - 23)..=hour)
            .map(|(_, n)| n)
            .sum();
        let flows: Vec<_> = rows.into_iter().map(|(name, r)| serde_json::json!({"name":name,"runs":r.runs,"failures":r.failures,"fallbacks_applied":r.fallbacks_applied,"cost_usd":if r.runs==r.cost_unknown_runs { None } else { Some(r.cost_usd) },"cost_unknown_runs":r.cost_unknown_runs})).collect();
        let start = time::OffsetDateTime::from_unix_timestamp((hour - 23) * 3600)
            .map_err(std::io::Error::other)?;
        Ok(
            serde_json::json!({"window_start":start.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),"window_end":end.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),"capture_failures":failures,"capture_drops":self.dropped.load(Ordering::Relaxed),"flows":flows}),
        )
    }
    pub fn new(data: &Path, workspace: &Path) -> Self {
        let path = data.join("flows/runs").join(format!(
            "{:x}",
            Sha256::digest(workspace.as_os_str().as_encoded_bytes())
        ));
        Self {
            write: super::fs::resource_lock(&path.join("runs.jsonl")),
            path,
            worker: OnceLock::new(),
            dropped: Arc::new(AtomicU64::new(0)),
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
                .filter_map(|line| serde_json::from_str::<RunRecord>(line).ok())
                .filter(retained)
                .map(|mut record| {
                    super::redaction::record(&mut record);
                    record
                })
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e),
        }
    }
    pub fn append(&self, record: &RunRecord) -> std::io::Result<()> {
        let mut record = record.clone();
        super::redaction::record(&mut record);
        let record = &record;
        let _guard = self
            .write
            .lock()
            .map_err(|_| std::io::Error::other("capture writer poisoned"))?;
        let directory = Directory::private(&self.path, true)?;
        let _file_lock = directory.lock(".records.lock")?;
        let size = serde_json::to_vec(record)
            .map_err(std::io::Error::other)?
            .len();
        if size > 32 * 1024 * 1024 {
            return Err(std::io::Error::other("capture exceeds byte limit"));
        }
        let mut records = self.read()?;
        records.push(record.clone());
        records.retain(retained);
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
fn retained(record: &RunRecord) -> bool {
    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(7);
    time::OffsetDateTime::parse(
        &record.started_at,
        &time::format_description::well_known::Rfc3339,
    )
    .is_ok_and(|t| t >= cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stalled_capture_writer_never_blocks_admission_and_counts_overflow() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path(), temp.path());
        let guard = records.write.lock().unwrap();
        let started = std::time::Instant::now();
        for _ in 0..100 {
            records.enqueue(fixture());
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        assert!(records.dropped.load(Ordering::Relaxed) >= 67);
        drop(guard);
    }
    pub(crate) fn fixture() -> RunRecord {
        serde_json::from_value(serde_json::json!({"version":1,"run_id":"test","flow_name":"test","definition_digest":"test","mode":"live","started_at":super::super::store::timestamp(),"duration_ms":0,"budget_ms":100,"event":{"trigger":"save","payload":{}},"identity":{"model":"test","variant":null},"inputs":{},"nodes":[],"batches":[],"status":"success","failure":null,"actions":[],"fallback_actions":[],"usage":null,"cost_usd":null,"application":{"status":"not_applied","actions":[]}})).unwrap()
    }
    #[test]
    fn independent_record_instances_keep_concurrent_appends() {
        let temp = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for i in 0..24 {
                let path = temp.path();
                scope.spawn(move || {
                    let records = Records::new(path, path);
                    let mut record = fixture();
                    record.run_id = i.to_string();
                    records.append(&record).unwrap();
                });
            }
        });
        assert_eq!(
            Records::new(temp.path(), temp.path()).read().unwrap().len(),
            24
        );
    }
    #[test]
    fn read_excludes_expired_capture_without_an_append() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path(), temp.path());
        let mut record = fixture();
        record.started_at = "2020-01-01T00:00:00Z".into();
        Directory::private(&records.path, true)
            .unwrap()
            .write(
                "runs.jsonl",
                serde_json::to_string(&record).unwrap().as_bytes(),
            )
            .unwrap();
        assert!(records.read().unwrap().is_empty());
    }
    #[test]
    fn health_is_independent_of_retained_captures_and_expires() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path(), temp.path());
        let hour = time::OffsetDateTime::now_utc().unix_timestamp() / 3600;
        let mut health = Health::default();
        health.hours.entry(hour).or_default().insert(
            "guard".into(),
            HealthRow {
                runs: 7,
                failures: 2,
                fallbacks_applied: 1,
                cost_usd: 0.2,
                cost_unknown_runs: 3,
            },
        );
        health.hours.entry(hour - 24).or_default().insert(
            "expired".into(),
            HealthRow {
                runs: 99,
                ..Default::default()
            },
        );
        health.capture_failures.insert(hour, 4);
        Directory::private(&records.path, true)
            .unwrap()
            .write("health.json", &serde_json::to_vec(&health).unwrap())
            .unwrap();
        assert!(records.read().unwrap().is_empty());
        let result = records.health().unwrap();
        assert_eq!(result["capture_failures"], 4);
        assert_eq!(result["flows"].as_array().unwrap().len(), 1);
        assert_eq!(result["flows"][0]["runs"], 7);
        let text = Directory::private(&records.path, false)
            .unwrap()
            .read("health.json", 4096, true)
            .unwrap();
        assert!(!text.contains("state"));
        assert!(!text.contains("toml"));
    }
}
