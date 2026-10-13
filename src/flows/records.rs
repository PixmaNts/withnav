//! Private bounded captures and independent, code-free hourly health aggregates.
use super::fs::Directory;
use instinct_flow::RunRecord;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
};
const MAX_RECORDS: usize = 1000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const COMPACT_BYTES: usize = MAX_BYTES * 3 / 2;
// One maximum-sized capture can be appended before the atomic compaction.
const MAX_STORAGE_BYTES: usize = COMPACT_BYTES + MAX_BYTES;
#[derive(Default)]
struct CaptureState {
    bytes: u64,
    lines: usize,
    checked: Option<std::time::Instant>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub flow_name: String,
    pub started_at: String,
    pub status: String,
    pub duration_ms: u64,
    pub cost_usd: Option<f64>,
    pub mode: String,
}
#[derive(Debug, Serialize)]
pub struct RunPage {
    pub runs: Vec<RunSummary>,
    pub next_cursor: Option<String>,
}
pub struct Records {
    path: PathBuf,
    write: Arc<Mutex<()>>,
    worker: Mutex<Option<Writer>>,
    stopped: Arc<AtomicBool>,
    closed: AtomicBool,
    pending: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    capture: Mutex<CaptureState>,
}
struct Writer {
    sender: SyncSender<RunRecord>,
    done: Receiver<()>,
    join: std::thread::JoinHandle<()>,
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
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if self.closed.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if worker.is_none() {
            let (sender, receiver) = sync_channel::<RunRecord>(32);
            let (done_tx, done) = sync_channel(1);
            let writer = Self {
                path: self.path.clone(),
                write: self.write.clone(),
                worker: Mutex::new(None),
                stopped: Arc::new(AtomicBool::new(false)),
                closed: AtomicBool::new(true),
                pending: self.pending.clone(),
                dropped: self.dropped.clone(),
                capture: Mutex::new(CaptureState::default()),
            };
            let dropped = self.dropped.clone();
            let stopped = self.stopped.clone();
            let result = std::thread::Builder::new()
                .name("flow-captures".into())
                .spawn(move || {
                    while let Ok(record) = receiver.recv() {
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        // fetch_update supports the declared Rust 1.89 minimum; newer compilers rename it.
                        #[allow(deprecated)]
                        let _ = writer.pending.fetch_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |n| Some(n.saturating_sub(1)),
                        );
                        let failed = writer.append(&record).is_err();
                        if failed {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        if writer.update_health(&record, failed).is_err() {
                            tracing::warn!("private flow health update failed");
                        }
                    }
                    let _ = done_tx.send(());
                });
            match result {
                Ok(join) => *worker = Some(Writer { sender, done, join }),
                Err(_) => {
                    tracing::warn!("private flow capture worker unavailable");
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
        self.pending.fetch_add(1, Ordering::Relaxed);
        if worker.as_ref().unwrap().sender.try_send(record).is_err() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// Closing admission is separate from cancellation, so queued records can drain.
    pub fn shutdown(&self, timeout: std::time::Duration) {
        let worker = {
            let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
            self.closed.store(true, Ordering::Release);
            worker.take()
        };
        let Some(Writer { sender, done, join }) = worker else {
            return;
        };
        drop(sender);
        if done.recv_timeout(timeout).is_ok() {
            let _ = join.join();
        } else {
            self.stopped.store(true, Ordering::Release);
            self.dropped
                .fetch_add(self.pending.swap(0, Ordering::Relaxed), Ordering::Relaxed);
            // A filesystem call cannot be cancelled safely; detach the in-flight write.
        }
        self.stopped.store(true, Ordering::Release);
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
        let path = data.join("flows/runs").join(hex::encode(Sha256::digest(
            workspace.as_os_str().as_encoded_bytes(),
        )));
        Self {
            write: super::fs::resource_lock(&path.join("runs.jsonl")),
            path,
            worker: Mutex::new(None),
            stopped: Arc::new(AtomicBool::new(false)),
            closed: AtomicBool::new(false),
            pending: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            capture: Mutex::new(CaptureState::default()),
        }
    }
    pub fn read(&self) -> std::io::Result<Vec<RunRecord>> {
        let dir = match Directory::private(&self.path, false) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        self.read_from(&dir)
    }
    pub fn summaries(
        &self,
        flow_name: &str,
        before: Option<&str>,
        limit: usize,
    ) -> std::io::Result<RunPage> {
        let mut page = RunPage {
            runs: vec![],
            next_cursor: None,
        };
        let directory = match Directory::private(&self.path, false) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(page),
            Err(e) => return Err(e),
        };
        let file = match directory.open_read("runs.jsonl", MAX_STORAGE_BYTES, true, false) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(page),
            Err(e) => return Err(e),
        };
        let limit = limit.clamp(1, 100);
        let mut cursor_found = before.is_none();
        let mut count = 0;
        let mut bytes = 0;
        reverse_lines(file, |line| {
            let Ok(mut summary) = serde_json::from_slice::<RunSummary>(line) else {
                return true;
            };
            if !retained_at(&summary.started_at) {
                return true;
            }
            bytes += line.len() + 1;
            count += 1;
            if bytes > MAX_BYTES || count > MAX_RECORDS {
                return false;
            }
            // Match read()'s redaction without materializing the capture's payload or trace.
            for text in [
                &mut summary.run_id,
                &mut summary.flow_name,
                &mut summary.started_at,
                &mut summary.status,
                &mut summary.mode,
            ] {
                if super::redaction::contains_secret(&serde_json::Value::String(text.clone())) {
                    *text = "<redacted>".into();
                }
            }
            if summary.flow_name != flow_name {
                return true;
            }
            if !cursor_found {
                cursor_found = before == Some(summary.run_id.as_str());
                return true;
            }
            page.runs.push(summary);
            page.runs.len() <= limit
        })?;
        if page.runs.len() > limit {
            page.runs.truncate(limit);
            page.next_cursor = page.runs.last().map(|r| r.run_id.clone());
        }
        Ok(page)
    }
    fn read_from(&self, dir: &Directory) -> std::io::Result<Vec<RunRecord>> {
        match dir.open_read("runs.jsonl", MAX_STORAGE_BYTES, true, false) {
            Ok(file) => {
                let mut records = vec![];
                let mut bytes = 0;
                reverse_lines(file, |line| {
                    let Ok(mut record) = serde_json::from_slice::<RunRecord>(line) else {
                        return true;
                    };
                    if !retained(&record) {
                        return true;
                    }
                    bytes += line.len() + 1;
                    if bytes > MAX_BYTES {
                        return false;
                    }
                    super::redaction::record(&mut record);
                    records.push(record);
                    records.len() < MAX_RECORDS
                })?;
                records.reverse();
                Ok(records)
            }
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
        let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        line.push(b'\n');
        if line.len() > MAX_BYTES {
            return Err(std::io::Error::other("capture exceeds byte limit"));
        }
        let mut state = self
            .capture
            .lock()
            .map_err(|_| std::io::Error::other("capture state poisoned"))?;
        match directory.open_read("runs.jsonl", MAX_STORAGE_BYTES, true, false) {
            Ok(mut file) => {
                let size = file.metadata()?.len();
                if state.checked.is_none() || state.bytes != size {
                    let mut count = 0;
                    let mut buffer = [0; 8192];
                    loop {
                        let n = file.read(&mut buffer)?;
                        if n == 0 {
                            break;
                        }
                        count += buffer[..n].iter().filter(|b| **b == b'\n').count();
                    }
                    state.lines = count;
                    state.bytes = size;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                state.bytes = 0;
                state.lines = 0;
            }
            Err(e) => return Err(e),
        }
        // Expiry is always enforced by readers; reclaim old captures at most daily.
        if state
            .checked
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(86400))
            || state.bytes > COMPACT_BYTES as u64
            || state.lines > MAX_RECORDS * 3 / 2
        {
            self.compact(&directory, &mut state)?;
        }
        directory.append("runs.jsonl", &line)?;
        state.bytes += line.len() as u64;
        state.lines += 1;
        if state.bytes > COMPACT_BYTES as u64 || state.lines > MAX_RECORDS * 3 / 2 {
            self.compact(&directory, &mut state)?;
        }
        Ok(())
    }
    fn compact(&self, directory: &Directory, state: &mut CaptureState) -> std::io::Result<()> {
        let records = self.read_from(directory)?;
        let mut bytes = Vec::new();
        for record in &records {
            serde_json::to_writer(&mut bytes, record).map_err(std::io::Error::other)?;
            bytes.push(b'\n');
        }
        directory.write("runs.jsonl", &bytes)?;
        state.bytes = bytes.len() as u64;
        state.lines = records.len();
        state.checked = Some(std::time::Instant::now());
        Ok(())
    }
}
impl Drop for Records {
    fn drop(&mut self) {
        self.shutdown(std::time::Duration::from_secs(2));
    }
}
fn retained(record: &RunRecord) -> bool {
    retained_at(&record.started_at)
}
fn retained_at(started_at: &str) -> bool {
    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(7);
    time::OffsetDateTime::parse(started_at, &time::format_description::well_known::Rfc3339)
        .is_ok_and(|t| t >= cutoff)
}

/// Read bounded blocks from the tail, never reopening the validated descriptor.
fn reverse_lines(
    mut file: std::fs::File,
    mut visit: impl FnMut(&[u8]) -> bool,
) -> std::io::Result<()> {
    let mut position = file.metadata()?.len();
    let mut buffer = [0; 8192];
    let mut line = Vec::new();
    while position > 0 {
        let length = position.min(buffer.len() as u64) as usize;
        position -= length as u64;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut buffer[..length])?;
        for byte in buffer[..length].iter().rev() {
            if *byte == b'\n' {
                line.reverse();
                if !line.is_empty() && !visit(&line) {
                    return Ok(());
                }
                line.clear();
            } else {
                line.push(*byte);
            }
        }
    }
    line.reverse();
    if !line.is_empty() {
        visit(&line);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shutdown_drains_and_joins_capture_writer() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path(), temp.path());
        for i in 0..8 {
            let mut record = fixture();
            record.run_id = i.to_string();
            records.enqueue(record);
        }
        records.shutdown(std::time::Duration::from_secs(2));
        assert_eq!(records.read().unwrap().len(), 8);
        assert_eq!(records.dropped.load(Ordering::Relaxed), 0);
        records.enqueue(fixture());
        assert_eq!(records.dropped.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn shutdown_timeout_counts_queued_records_without_blocking() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path(), temp.path());
        let guard = records.write.lock().unwrap();
        for _ in 0..8 {
            records.enqueue(fixture());
        }
        let started = std::time::Instant::now();
        records.shutdown(std::time::Duration::from_millis(10));
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert!(records.dropped.load(Ordering::Relaxed) >= 7);
        drop(guard);
    }
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
    fn append_retains_count_and_compacts_only_periodically() {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path(), temp.path());
        let mut record = fixture();
        records.append(&record).unwrap();
        let path = records.path.join("runs.jsonl");
        #[cfg(unix)]
        let inode = std::fs::metadata(&path).unwrap().ino();
        for i in 1..1500 {
            record.run_id = i.to_string();
            records.append(&record).unwrap();
        }
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let kept = records.read().unwrap();
        assert_eq!(kept.len(), 1000);
        assert_eq!(kept[0].run_id, "500");
        record.run_id = "1500".into();
        records.append(&record).unwrap();
        #[cfg(unix)]
        let compacted = std::fs::metadata(&path).unwrap().ino();
        #[cfg(unix)]
        assert_ne!(compacted, inode);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            1000
        );
        records.append(&record).unwrap();
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), compacted);
    }
    #[test]
    fn missing_storage_reads_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(&temp.path().join("missing"), temp.path());
        assert!(!records.path.exists());
        assert!(records.read().unwrap().is_empty());
        assert!(records.summaries("flow", None, 10).unwrap().runs.is_empty());
        records.read_health().unwrap();
        assert!(!records.path.exists());
    }
    #[test]
    fn append_retains_byte_limit_and_preserves_captures_on_oversize() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path(), temp.path());
        let mut record = fixture();
        record.event.payload = serde_json::json!({"text":"x".repeat(12 * 1024 * 1024)});
        for i in 0..4 {
            record.run_id = i.to_string();
            records.append(&record).unwrap();
        }
        let kept = records.read().unwrap();
        assert_eq!(
            kept.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            ["2", "3"]
        );
        assert!(
            std::fs::metadata(records.path.join("runs.jsonl"))
                .unwrap()
                .len()
                <= MAX_BYTES as u64
        );
        record.event.payload = serde_json::json!({"text":"x".repeat(MAX_BYTES)});
        assert!(records.append(&record).is_err());
        assert_eq!(records.read().unwrap().len(), 2);
    }
    #[test]
    fn truncated_tail_does_not_lose_next_capture_and_compaction_redacts() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path(), temp.path());
        let directory = Directory::private(&records.path, true).unwrap();
        let secret = "ghp_123456789012345678901234";
        let mut record = fixture();
        record.event.payload = serde_json::json!({"token":secret});
        let raw = format!("{}\n{{\"run_id\":", serde_json::to_string(&record).unwrap());
        directory.write("runs.jsonl", raw.as_bytes()).unwrap();
        assert_eq!(records.read().unwrap().len(), 1);
        records.capture.lock().unwrap().checked = Some(std::time::Instant::now());
        record.run_id = "after-crash".into();
        records.append(&record).unwrap();
        assert_eq!(records.read().unwrap().len(), 2);
        assert!(std::fs::read_to_string(records.path.join("runs.jsonl"))
            .unwrap()
            .contains("after-crash"));
        // A fresh writer's initial compaction repairs the incomplete row and old raw captures.
        let fresh = Records::new(temp.path(), temp.path());
        record.run_id = "after-restart".into();
        fresh.append(&record).unwrap();
        assert_eq!(fresh.read().unwrap().len(), 3);
        let text = directory.read_private("runs.jsonl", 8192).unwrap();
        assert!(!text.contains(secret));
        assert_eq!(text.lines().count(), 3);
        assert_eq!(
            fresh.read().unwrap()[2].event.payload["token"],
            "<redacted>"
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
        assert!(records.summaries("test", None, 20).unwrap().runs.is_empty());
        record.started_at = super::super::store::timestamp();
        record.event.payload = serde_json::json!({"text":"x".repeat(20_000)});
        for i in 0..6 {
            record.run_id = i.to_string();
            record.flow_name = if i % 2 == 0 { "test" } else { "other" }.into();
            records.append(&record).unwrap();
        }
        let first = records.summaries("test", None, 2).unwrap();
        assert_eq!(
            first
                .runs
                .iter()
                .map(|r| r.run_id.as_str())
                .collect::<Vec<_>>(),
            ["4", "2"]
        );
        assert_eq!(first.next_cursor.as_deref(), Some("2"));
        let last = records.summaries("test", Some("2"), 2).unwrap();
        assert_eq!(last.runs[0].run_id, "0");
        assert!(last.next_cursor.is_none());
        assert!(records
            .summaries("test", Some("missing"), 2)
            .unwrap()
            .runs
            .is_empty());
        assert!(records
            .summaries("test", Some("5"), 2)
            .unwrap()
            .runs
            .is_empty());
        assert_eq!(records.summaries("test", None, 0).unwrap().runs.len(), 1);
        assert_eq!(
            records
                .summaries("test", None, usize::MAX)
                .unwrap()
                .runs
                .len(),
            3
        );
        record.flow_name = "test".into();
        record.run_id = "ghp_123456789012345678901234".into();
        records.append(&record).unwrap();
        assert_eq!(
            records.summaries("test", None, 1).unwrap().runs[0].run_id,
            "<redacted>"
        );
        let file = Directory::private(&records.path, false)
            .unwrap()
            .open_read("runs.jsonl", MAX_STORAGE_BYTES, true, false)
            .unwrap();
        let mut visited = 0;
        reverse_lines(file, |_| {
            visited += 1;
            visited < 3
        })
        .unwrap();
        assert_eq!(
            visited, 3,
            "tail scan stops without traversing older captures"
        );
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
