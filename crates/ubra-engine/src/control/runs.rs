//! A bounded, non-PTY local command producer with durable execution evidence.
//!
//! The registry is used only to copy a target record. Process waiting, output
//! draining, SQLite and fsync never hold the global session registry lock.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use ubra_proto::runs::*;
use ubra_proto::{
    ActivityKind, ActivityProducer, ActivitySource, ControlError, DateMillis, SessionRecord,
};

const MAX_REQUESTS: i64 = 100_000;
const FRAME_HEADER: usize = 13;
const BUFFER_BYTES: usize = 16 * 1024;
const OUTPUT_REFRESH: Duration = Duration::from_millis(100);
const MAX_READ_PARTS: usize = 256;
const MAX_POST_EXIT_DRAIN: Duration = Duration::from_millis(500);

pub(super) struct RunStore {
    inner: Arc<Inner>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Clone)]
pub(super) struct RunLiveness(Arc<Inner>);
impl RunLiveness {
    pub(super) fn active_count(&self) -> usize {
        let active = self
            .0
            .active
            .lock()
            .map(|active| active.len())
            .unwrap_or(MAX_ACTIVE_RUNS);
        // Admission is live work too: do not retire Engine during the durable
        // reservation -> spawn boundary before the worker is registered.
        if active == 0 && self.0.admission.try_lock().is_err() {
            1
        } else {
            active
        }
    }
    /// Freeze admission at the actual idle-exit decision. Never wait behind a
    /// spawn/durable reservation and never stop admission while a run is live.
    pub(super) fn try_stop_idle(&self) -> bool {
        let Ok(_admission) = self.0.admission.try_lock() else {
            return false;
        };
        let idle = self.0.active.lock().is_ok_and(|active| active.is_empty());
        if idle {
            self.0.stopping.store(true, Ordering::Release);
        }
        idle
    }
}

struct RunProcess {
    child: Child,
    guard: RunGuard,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    output: File,
}
struct ActiveRun {
    started: Instant,
    observed_duration_ms: Option<u64>,
}
struct Inner {
    root: PathBuf,
    events: crate::events::EventBus,
    admission: Mutex<()>,
    active: Mutex<HashMap<String, ActiveRun>>,
    stopping: AtomicBool,
    recovered: Mutex<Vec<RunRecord>>,
}

impl RunStore {
    pub(super) fn open(root: PathBuf, events: crate::events::EventBus) -> io::Result<Self> {
        private_dir(&root)?;
        private_dir(&root.join("output"))?;
        let inner = Arc::new(Inner {
            root,
            events,
            admission: Mutex::new(()),
            active: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
            recovered: Mutex::new(Vec::new()),
        });
        let db = inner.db().map_err(control_io)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs_v1 (
                run_id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                terminal INTEGER NOT NULL, record TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS run_sessions_v1 ON runs_v1(session_id);
             CREATE TABLE IF NOT EXISTS run_requests_v1 (
                session_id TEXT NOT NULL, request_id TEXT NOT NULL,
                fingerprint TEXT NOT NULL, run_id TEXT NOT NULL,
                PRIMARY KEY(session_id, request_id)
             );",
        )
        .map_err(io::Error::other)?;
        let mut statement = db
            .prepare("SELECT record FROM runs_v1 WHERE terminal=0")
            .map_err(io::Error::other)?;
        let unsettled = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(io::Error::other)?;
        let mut recovered = Vec::new();
        for raw in unsettled {
            let mut record: RunRecord =
                serde_json::from_str(&raw.map_err(io::Error::other)?).map_err(io::Error::other)?;
            record.status = RunStatus::Interrupted;
            record.finished_at = Some(now());
            record.duration_ms = None;
            record.exit_code = None;
            record.signal = None;
            record.error = Some("Engine restarted before this run settled".into());
            record.revision += 1;
            // A crash can happen between an output append and its metadata
            // checkpoint. Only complete retained frames are made readable.
            record.output_bytes = recover_output(&inner.output_path(&record.run_id))?;
            record.output_available = record.output_bytes > 0;
            inner.save(&db, &record).map_err(control_io)?;
            inner.publish(&record);
            recovered.push(record);
        }
        drop(statement);
        *inner.recovered.lock().expect("recovered runs") = recovered;
        let sessions = {
            let mut query = db
                .prepare("SELECT DISTINCT session_id FROM runs_v1")
                .map_err(io::Error::other)?;
            query
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(io::Error::other)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(io::Error::other)?
        };
        for session in sessions {
            inner.prune(&db, &session).map_err(control_io)?;
        }
        Ok(Self {
            inner,
            workers: Mutex::new(Vec::new()),
        })
    }

    /// Called once after construction; snapshots are copied before Activity's
    /// durable append, avoiding the registry -> activity lock ordering.
    pub(super) fn publish_recovery_activity(
        &self,
        registry: &Arc<Mutex<crate::registry::Registry>>,
    ) {
        let recovered = std::mem::take(&mut *self.inner.recovered.lock().expect("recovered runs"));
        let snapshots = registry
            .lock()
            .ok()
            .map(|registry| {
                recovered
                    .iter()
                    .filter_map(|run| {
                        registry
                            .record(&run.session_id.0)
                            .map(|session| (session, run))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (session, record) in snapshots {
            self.inner.activity(&session, record);
        }
    }

    fn start(&self, session: SessionRecord, p: RunStartParams) -> Result<RunRecord, ControlError> {
        validate_start(&session, &p)?;
        let _admission = self.inner.admission.lock().map_err(super::poisoned)?;
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(ControlError::new("engine_stopping", "Engine is stopping"));
        }
        let mut db = self.inner.db()?;
        let fingerprint = fingerprint(&p)?;
        let existing: Option<(String, String)> = db.query_row(
            "SELECT fingerprint, run_id FROM run_requests_v1 WHERE session_id=?1 AND request_id=?2",
            params![p.session_id.0, p.request_id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(storage)?;
        if let Some((previous, id)) = existing {
            if previous != fingerprint {
                return Err(ControlError::new(
                    "request_conflict",
                    "request_id already identifies a different command",
                ));
            }
            return self.inner.get(&db, &p.session_id.0, &id).map_err(|error| {
                if error.code == "not_found" {
                    ControlError::new(
                        "run_retention_expired",
                        "This request already executed; its run exceeded the retention limit",
                    )
                } else {
                    error
                }
            });
        }
        let active_count: i64 = db
            .query_row("SELECT COUNT(*) FROM runs_v1 WHERE terminal=0", [], |row| {
                row.get(0)
            })
            .map_err(storage)?;
        let session_count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM runs_v1 WHERE terminal=0 AND session_id=?1",
                [&p.session_id.0],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let supervisors = self.inner.active.lock().map_err(super::poisoned)?.len();
        if active_count >= MAX_ACTIVE_RUNS as i64
            || supervisors >= MAX_ACTIVE_RUNS
            || session_count >= MAX_ACTIVE_RUNS_PER_SESSION as i64
        {
            return Err(ControlError::new(
                "run_busy",
                "Local run concurrency limit reached (8 total, 4 per session)",
            ));
        }
        let requests: i64 = db
            .query_row("SELECT COUNT(*) FROM run_requests_v1", [], |row| row.get(0))
            .map_err(storage)?;
        if requests >= MAX_REQUESTS {
            return Err(ControlError::new(
                "run_request_capacity",
                "Durable run request identity limit reached",
            ));
        }
        let run_id = next_run_id()?;
        let output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.inner.output_path(&run_id))
            .map_err(storage)?;
        output.sync_all().map_err(storage)?;
        File::open(self.inner.root.join("output"))
            .and_then(|dir| dir.sync_all())
            .map_err(storage)?;
        let started = Instant::now();
        let mut record = RunRecord {
            run_id: run_id.clone(),
            session_id: p.session_id.clone(),
            request_id: p.request_id.clone(),
            kind: p.kind,
            argv: p.argv.clone(),
            cwd: session.cwd.clone(),
            producer: RunProducer::LocalCommand,
            started_at: now(),
            finished_at: None,
            duration_ms: Some(0),
            revision: 1,
            status: RunStatus::Running,
            exit_code: None,
            signal: None,
            error: None,
            output_bytes: 0,
            output_available: false,
            output_truncated: false,
        };
        let tx = db.transaction().map_err(storage)?;
        let raw = serde_json::to_string(&record).map_err(storage)?;
        tx.execute(
            "INSERT INTO runs_v1 VALUES (?1,?2,0,?3)",
            params![run_id, p.session_id.0, raw],
        )
        .map_err(storage)?;
        tx.execute(
            "INSERT INTO run_requests_v1 VALUES (?1,?2,?3,?4)",
            params![p.session_id.0, p.request_id, fingerprint, run_id],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;

        // Registration happens in pre_exec before exec, so even SIGKILL of
        // Engine between spawn and supervisor creation cannot orphan a group.
        let spawned = spawn_command(&record);
        let (mut child, guard) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                record.status = RunStatus::Failed;
                record.finished_at = Some(now());
                record.duration_ms = Some(elapsed_ms(started));
                record.error = Some(format!("Could not start local command: {error}"));
                record.revision += 1;
                self.inner.save(&db, &record)?;
                self.inner.publish(&record);
                self.inner.activity(&session, &record);
                self.inner.prune(&db, &session.id.0)?;
                return Ok(record);
            }
        };
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        if let Err(error) =
            nonblocking(stdout.as_raw_fd()).and_then(|()| nonblocking(stderr.as_raw_fd()))
        {
            guard.finish();
            let status = child.wait().map_err(storage)?;
            record.status = RunStatus::Failed;
            record.finished_at = Some(now());
            record.duration_ms = Some(elapsed_ms(started));
            record.exit_code = status.code();
            record.signal = status.signal();
            record.error = Some(format!("Could not capture command output: {error}"));
            record.revision += 1;
            self.inner.save(&db, &record)?;
            self.inner.publish(&record);
            self.inner.activity(&session, &record);
            self.inner.prune(&db, &session.id.0)?;
            return Ok(record);
        }
        self.inner.active.lock().map_err(super::poisoned)?.insert(
            run_id.clone(),
            ActiveRun {
                started,
                observed_duration_ms: None,
            },
        );
        self.inner.publish(&record);
        self.inner.activity(&session, &record);
        let initial = record.clone();
        let inner = Arc::clone(&self.inner);
        // Builder::spawn can fail without running its closure: the group's
        // guard and child must be retained outside the moved closure then.
        let job = Arc::new(Mutex::new(Some(RunProcess {
            child,
            guard,
            stdout,
            stderr,
            output,
        })));
        let thread_job = Arc::clone(&job);
        let worker_session = session.clone();
        let worker = std::thread::Builder::new()
            .name(format!("ubra-run-{run_id}"))
            .spawn(move || {
                let process = thread_job.lock().expect("run job").take().expect("run job");
                supervise(&inner, worker_session, record, started, process);
            });
        match worker {
            Ok(worker) => {
                let mut workers = self.workers.lock().map_err(super::poisoned)?;
                workers.retain(|worker| !worker.is_finished());
                workers.push(worker);
            }
            Err(error) => {
                let RunProcess {
                    mut child, guard, ..
                } = job
                    .lock()
                    .map_err(super::poisoned)?
                    .take()
                    .expect("unstarted run job");
                guard.finish();
                let status = child.wait().map_err(storage)?;
                let mut failed = initial;
                failed.status = RunStatus::Failed;
                failed.finished_at = Some(now());
                failed.duration_ms = Some(elapsed_ms(started));
                failed.exit_code = status.code();
                failed.signal = status.signal();
                failed.error = Some(format!("Could not start output supervisor: {error}"));
                failed.revision += 1;
                self.inner.save(&db, &failed)?;
                self.inner
                    .active
                    .lock()
                    .map_err(super::poisoned)?
                    .remove(&run_id);
                self.inner.publish(&failed);
                self.inner.activity(&session, &failed);
                self.inner.prune(&db, &failed.session_id.0)?;
                return Ok(failed);
            }
        }
        self.inner.prune(&db, &initial.session_id.0)?;
        Ok(initial)
    }

    fn list(&self, p: &RunListParams) -> Result<RunListResult, ControlError> {
        identity(&p.session_id.0)?;
        let db = self.inner.db()?;
        let before = match &p.cursor {
            Some(id) => {
                run_identity(id)?;
                db.query_row(
                    "SELECT rowid FROM runs_v1 WHERE run_id=?1 AND session_id=?2",
                    params![id, p.session_id.0],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(storage)?
                .ok_or_else(|| {
                    ControlError::new(
                        "invalid_cursor",
                        "Run cursor does not belong to this retained session",
                    )
                })?
            }
            None => i64::MAX,
        };
        let limit = p
            .limit
            .unwrap_or(50)
            .clamp(1, MAX_RETAINED_RUNS_PER_SESSION as u32) as usize;
        let mut query = db.prepare("SELECT record FROM runs_v1 WHERE session_id=?1 AND rowid<?2 ORDER BY rowid DESC LIMIT ?3")
            .map_err(storage)?;
        let rows = query
            .query_map(params![p.session_id.0, before, limit as i64 + 1], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage)?;
        let mut runs: Vec<RunRecord> = Vec::with_capacity(limit.min(32));
        let mut encoded_bytes = 0_usize;
        let mut more = false;
        for raw in rows {
            let raw = raw.map_err(storage)?;
            // Large, structured argv still must fit the bounded control line.
            // Stored JSON size accounts for escaping without reserializing it.
            if runs.len() == limit
                || (!runs.is_empty()
                    && encoded_bytes + raw.len() > ubra_proto::control::MAX_CONTROL_LINE_BYTES / 2)
            {
                more = true;
                break;
            }
            encoded_bytes += raw.len();
            runs.push(serde_json::from_str(&raw).map_err(storage)?);
        }
        let next_cursor = if more {
            runs.last().map(|run| run.run_id.clone())
        } else {
            None
        };
        for run in &mut runs {
            self.inner.live_duration(run);
        }
        Ok(RunListResult {
            runs,
            next_cursor,
            retention_limit: MAX_RETAINED_RUNS_PER_SESSION,
        })
    }

    fn get(&self, p: &RunGetParams) -> Result<RunRecord, ControlError> {
        identity(&p.session_id.0)?;
        run_identity(&p.run_id)?;
        self.inner
            .get(&self.inner.db()?, &p.session_id.0, &p.run_id)
    }

    fn read_output(&self, p: &RunReadOutputParams) -> Result<RunOutputChunk, ControlError> {
        identity(&p.session_id.0)?;
        run_identity(&p.run_id)?;
        let run = self
            .inner
            .get(&self.inner.db()?, &p.session_id.0, &p.run_id)?;
        if p.offset > run.output_bytes {
            return Err(ControlError::bad_request(
                "Output offset exceeds retained output",
            ));
        }
        let limit = p
            .max_bytes
            .unwrap_or(MAX_RUN_READ_BYTES)
            .clamp(1, MAX_RUN_READ_BYTES) as u64;
        let end = p.offset.saturating_add(limit).min(run.output_bytes);
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.inner.output_path(&p.run_id))
            .map_err(storage)?;
        let mut logical = 0_u64;
        let mut next_offset = p.offset;
        let mut parts = Vec::new();
        while logical < end && parts.len() < MAX_READ_PARTS {
            let (stream, sequence, length) =
                read_header(&mut file).map_err(storage)?.ok_or_else(|| {
                    ControlError::internal("Run output ended before its durable metadata")
                })?;
            let finish = logical + u64::from(length);
            if finish <= p.offset {
                file.seek(SeekFrom::Current(i64::from(length)))
                    .map_err(storage)?;
            } else {
                let skip = p.offset.saturating_sub(logical);
                file.seek(SeekFrom::Current(skip as i64)).map_err(storage)?;
                let count = (finish.min(end) - (logical + skip)) as usize;
                let mut bytes = vec![0_u8; count];
                file.read_exact(&mut bytes).map_err(storage)?;
                next_offset = logical + skip + count as u64;
                parts.push(RunOutputPart {
                    sequence,
                    stream,
                    offset: logical + skip,
                    byte_len: count as u32,
                    bytes,
                });
                if finish > end {
                    break;
                }
                file.seek(SeekFrom::Current(
                    (u64::from(length) - skip - count as u64) as i64,
                ))
                .map_err(storage)?;
            }
            logical = finish;
        }
        Ok(RunOutputChunk {
            session_id: run.session_id,
            run_id: run.run_id,
            revision: run.revision,
            offset: p.offset,
            next_offset,
            parts,
            eof: run.status.is_terminal() && next_offset == run.output_bytes,
            truncated: run.output_truncated,
        })
    }

    /// Call before explicit process::exit paths; Drop covers normal ownership
    /// teardown and the pipe guards cover signals/crashes, including SIGKILL.
    pub(super) fn shutdown(&self) {
        self.inner.stopping.store(true, Ordering::Release);
        let _admission = self.inner.admission.lock().ok();
        if let Ok(mut workers) = self.workers.lock() {
            for worker in workers.drain(..) {
                let _ = worker.join();
            }
        }
    }
    pub(super) fn liveness(&self) -> RunLiveness {
        RunLiveness(Arc::clone(&self.inner))
    }
    pub(super) fn active_count(&self) -> usize {
        self.liveness().active_count()
    }
}
impl Drop for RunStore {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Inner {
    fn db(&self) -> Result<Connection, ControlError> {
        let path = self.root.join("metadata.sqlite");
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(storage)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(storage)?;
        drop(file);
        let db = Connection::open(path).map_err(storage)?;
        db.busy_timeout(Duration::from_secs(2)).map_err(storage)?;
        // DELETE journaling prevents permissive sidecars; the owner-only parent
        // also protects SQLite's short-lived rollback journal.
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
            .map_err(storage)?;
        Ok(db)
    }
    fn output_path(&self, id: &str) -> PathBuf {
        self.root.join("output").join(format!("{id}.bin"))
    }
    fn save(&self, db: &Connection, run: &RunRecord) -> Result<(), ControlError> {
        let raw = serde_json::to_string(run).map_err(storage)?;
        db.execute(
            "UPDATE runs_v1 SET terminal=?1, record=?2 WHERE run_id=?3",
            params![run.status.is_terminal(), raw, run.run_id],
        )
        .map_err(storage)?;
        Ok(())
    }
    fn get(&self, db: &Connection, session: &str, id: &str) -> Result<RunRecord, ControlError> {
        let raw: Option<String> = db
            .query_row(
                "SELECT record FROM runs_v1 WHERE run_id=?1 AND session_id=?2",
                params![id, session],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        let mut run: RunRecord = serde_json::from_str(
            &raw.ok_or_else(|| ControlError::not_found("Run not found in requested session"))?,
        )
        .map_err(storage)?;
        self.live_duration(&mut run);
        Ok(run)
    }
    fn live_duration(&self, run: &mut RunRecord) {
        if run.status == RunStatus::Running
            && let Ok(active) = self.active.lock()
            && let Some(active) = active.get(&run.run_id)
        {
            run.duration_ms = Some(
                active
                    .observed_duration_ms
                    .unwrap_or_else(|| elapsed_ms(active.started)),
            );
        }
    }
    fn publish(&self, record: &RunRecord) {
        self.events.publish_encoded(
            "run.updated",
            &RunUpdatedEvent {
                session_id: record.session_id.clone(),
                run_id: record.run_id.clone(),
                revision: record.revision,
            },
            Some(&record.session_id.0),
        );
    }
    fn activity(&self, session: &SessionRecord, record: &RunRecord) {
        let state = match record.status {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Interrupted => "interrupted",
        };
        self.events.record_activity(
            session,
            ActivityKind::RunTransition,
            ActivitySource {
                producer: ActivityProducer::Run,
                id: record.run_id.clone(),
                revision: record.revision,
                state: Some(state.into()),
            },
            record.finished_at.unwrap_or(record.started_at),
        );
    }
    fn prune(&self, db: &Connection, session: &str) -> Result<(), ControlError> {
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM runs_v1 WHERE session_id=?1",
                [session],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let excess = count
            .saturating_sub(MAX_RETAINED_RUNS_PER_SESSION as i64)
            .max(0);
        if excess == 0 {
            return Ok(());
        }
        let ids = {
            let mut query = db.prepare("SELECT run_id FROM runs_v1 WHERE session_id=?1 AND terminal=1 ORDER BY rowid ASC LIMIT ?2")
                .map_err(storage)?;
            query
                .query_map(params![session, excess], |row| row.get::<_, String>(0))
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        for id in ids {
            // Keep request tombstones: an evicted request never executes again.
            db.execute("DELETE FROM runs_v1 WHERE run_id=?1 AND terminal=1", [&id])
                .map_err(storage)?;
            match std::fs::remove_file(self.output_path(&id)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage(error)),
            }
        }
        Ok(())
    }
}

fn supervise(
    inner: &Inner,
    session: SessionRecord,
    mut run: RunRecord,
    started: Instant,
    process: RunProcess,
) {
    let RunProcess {
        mut child,
        guard,
        mut stdout,
        mut stderr,
        mut output,
    } = process;
    let mut open = [true, true];
    let fds = [stdout.as_raw_fd(), stderr.as_raw_fd()];
    let mut buffer = [0_u8; BUFFER_BYTES];
    let mut file_bytes = 0_u64;
    let mut sequence = 0_u64;
    let mut checkpoint = Instant::now();
    let mut changed = false;
    let mut failure: Option<String> = None;
    let mut cancelled = false;
    let mut ended = false;
    let mut observed_finish = None;
    let mut drain_started = None;
    let mut guard = Some(guard);
    loop {
        if !ended {
            match exited_unreaped(child.id()) {
                Ok(true) => {
                    ended = true;
                    drain_started = Some(Instant::now());
                    let duration_ms = elapsed_ms(started);
                    observed_finish = Some((now(), duration_ms));
                    if let Ok(mut active) = inner.active.lock()
                        && let Some(active) = active.get_mut(&run.run_id)
                    {
                        active.observed_duration_ms = Some(duration_ms);
                    }
                    // Kill inherited-pipe descendants while the zombie leader
                    // still pins the group id, before reaping or guard release.
                    if let Some(guard) = guard.take() {
                        guard.finish();
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    failure
                        .get_or_insert_with(|| format!("Could not observe command exit: {error}"));
                    if let Some(guard) = guard.take() {
                        guard.finish();
                    }
                    ended = true;
                    drain_started = Some(Instant::now());
                }
            }
        }
        if inner.stopping.load(Ordering::Acquire) && !ended {
            cancelled = true;
            if let Some(guard) = guard.take() {
                guard.finish();
            }
        }
        let mut polls = [
            libc::pollfd {
                fd: if open[0] { fds[0] } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if open[1] { fds[1] } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: the two live owned descriptors and stack pollfd array remain
        // valid through this bounded call; no global locks are held.
        let polled = unsafe { libc::poll(polls.as_mut_ptr(), 2, 25) };
        if polled < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            failure.get_or_insert_with(|| "Could not poll captured output".into());
            if let Some(guard) = guard.take() {
                guard.finish();
            }
            break;
        }
        for index in 0..2 {
            if !open[index] || polls[index].revents == 0 {
                continue;
            }
            // A permanently busy stdout cannot starve stderr or exit detection.
            for _ in 0..4 {
                let read = if index == 0 {
                    stdout.read(&mut buffer)
                } else {
                    stderr.read(&mut buffer)
                };
                match read {
                    Ok(0) => {
                        open[index] = false;
                        break;
                    }
                    Ok(size) => {
                        if failure.is_none() {
                            let available = MAX_RUN_OUTPUT_BYTES
                                .saturating_sub(file_bytes + FRAME_HEADER as u64);
                            let kept = (size as u64).min(available) as usize;
                            if kept < size && !run.output_truncated {
                                run.output_truncated = true;
                                changed = true;
                            }
                            if kept > 0 {
                                sequence += 1;
                                let mut header = [0_u8; FRAME_HEADER];
                                header[0] = index as u8;
                                header[1..9].copy_from_slice(&sequence.to_le_bytes());
                                header[9..13].copy_from_slice(&(kept as u32).to_le_bytes());
                                if let Err(error) = output
                                    .write_all(&header)
                                    .and_then(|()| output.write_all(&buffer[..kept]))
                                {
                                    failure =
                                        Some(format!("Could not persist captured output: {error}"));
                                    run.output_truncated = true;
                                    changed = true;
                                } else {
                                    file_bytes += FRAME_HEADER as u64 + kept as u64;
                                    run.output_bytes += kept as u64;
                                    run.output_available = true;
                                    changed = true;
                                }
                            }
                        }
                        // Always keep draining, including after cap or disk error.
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        failure.get_or_insert_with(|| {
                            format!("Could not read captured output: {error}")
                        });
                        open[index] = false;
                        break;
                    }
                }
            }
        }
        if changed && checkpoint.elapsed() >= OUTPUT_REFRESH {
            let duration_ms = observed_finish
                .as_ref()
                .map_or_else(|| elapsed_ms(started), |(_, duration)| *duration);
            match checkpoint_output(inner, &output, &mut run, duration_ms) {
                Ok(()) => {
                    changed = false;
                    checkpoint = Instant::now();
                }
                Err(error) => {
                    failure.get_or_insert_with(|| {
                        format!("Could not checkpoint run output: {}", error.message)
                    });
                    if let Some(guard) = guard.take() {
                        guard.finish();
                    }
                }
            }
        }
        if ended && !open[0] && !open[1] {
            break;
        }
        if drain_started.is_some_and(|started| started.elapsed() >= MAX_POST_EXIT_DRAIN) {
            run.output_truncated = true;
            failure.get_or_insert_with(|| {
                "Output capture closed: an inherited writer outlived the command".into()
            });
            break;
        }
    }
    if let Some(guard) = guard.take() {
        guard.finish();
    }
    let status = child.wait();
    let (finished_at, duration_ms) =
        observed_finish.unwrap_or_else(|| (now(), elapsed_ms(started)));
    run.finished_at = Some(finished_at);
    run.duration_ms = Some(duration_ms);
    run.error = failure;
    match status {
        Ok(status) => {
            run.exit_code = status.code();
            run.signal = status.signal();
            run.status = if cancelled {
                RunStatus::Cancelled
            } else if status.success() && run.error.is_none() {
                RunStatus::Succeeded
            } else {
                RunStatus::Failed
            };
        }
        Err(error) => {
            run.status = RunStatus::Interrupted;
            run.error = Some(format!("Could not reap local command: {error}"));
        }
    }
    run.revision += 1;
    let settled = output
        .sync_all()
        .map_err(storage)
        .and_then(|()| inner.db())
        .and_then(|db| {
            inner.save(&db, &run)?;
            inner.prune(&db, &run.session_id.0)
        });
    if settled.is_ok() {
        inner.publish(&run);
        inner.activity(&session, &run);
    } else {
        // Do not invent success if storage failed. The last durable running
        // record is conservatively interrupted during the next Engine load.
        eprintln!("ubra-engine: could not settle durable run metadata");
    }
    if let Ok(mut active) = inner.active.lock() {
        active.remove(&run.run_id);
    }
}

fn checkpoint_output(
    inner: &Inner,
    output: &File,
    run: &mut RunRecord,
    duration_ms: u64,
) -> Result<(), ControlError> {
    output.sync_data().map_err(storage)?;
    run.duration_ms = Some(duration_ms);
    run.revision += 1;
    inner.save(&inner.db()?, run)?;
    inner.publish(run);
    Ok(())
}

/// One minimal syscall-only guard, following holder::guard's pipe-EOF/group
/// ownership convention. It inherits no Engine descriptors except its pipe.
/// It owns no PTY, socket, command output, or execution metadata.
struct RunGuard {
    pipe: Option<OwnedFd>,
    pid: libc::pid_t,
}

fn guard_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    #[cfg(target_os = "linux")]
    let result = unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    let result = unsafe { libc::pipe(descriptors.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe returned two new, exclusively owned descriptors.
    let read = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    #[cfg(not(target_os = "linux"))]
    for fd in [read.as_raw_fd(), write.as_raw_fd()] {
        // SAFETY: owned pipe descriptor; only Engine retains the writer after
        // exec, not unrelated command children.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}
impl RunGuard {
    fn new() -> io::Result<Self> {
        let (read, write) = guard_pipe()?;
        let (ready_read, ready_write) = guard_pipe()?;
        // Match the existing PTY/Holder descriptor-isolation convention.
        // Capture the fallback bound before fork, not through inherited libc
        // state in the syscall-only child.
        let maximum = unsafe { libc::getdtablesize() };
        // SAFETY: the child calls only async-signal-safe syscalls and _exit;
        // it never enters Rust allocation, IO abstractions or inherited locks.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            unsafe {
                // Preserve the acknowledgement independently before replacing
                // fd 3/4; callers may have closed a standard descriptor.
                let ready = libc::fcntl(ready_write.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5);
                if ready < 0
                    || libc::setsid() < 0
                    || libc::dup2(read.as_raw_fd(), 3) < 0
                    || libc::dup2(ready, 4) < 0
                {
                    libc::_exit(1);
                }
                libc::close(0);
                libc::close(1);
                libc::close(2);
                #[cfg(target_os = "macos")]
                for fd in 5..maximum {
                    libc::close(fd);
                }
                #[cfg(target_os = "linux")]
                {
                    if libc::syscall(libc::SYS_close_range, 5_u32, u32::MAX, 0_u32) < 0 {
                        // Older-kernel fallback, using syscall-only code.
                        for fd in 5..maximum {
                            libc::close(fd);
                        }
                    }
                }
                let ready = 1_u8;
                if libc::write(4, (&ready as *const u8).cast(), 1) != 1 {
                    libc::_exit(1);
                }
                libc::close(4);
                let mut group_bytes = [0_u8; 4];
                let mut filled = 0_usize;
                while filled < group_bytes.len() {
                    let read = libc::read(
                        3,
                        group_bytes.as_mut_ptr().add(filled).cast(),
                        group_bytes.len() - filled,
                    );
                    if read == 0 {
                        libc::_exit(0);
                    }
                    if read < 0 {
                        continue;
                    }
                    filled += read as usize;
                }
                let group = i32::from_ne_bytes(group_bytes);
                let mut byte = 0_u8;
                loop {
                    let read = libc::read(3, (&mut byte as *mut u8).cast(), 1);
                    if read == 0 {
                        break;
                    }
                    if read < 0 {
                        continue;
                    }
                    // Explicit disarm is used when exec failed and std already
                    // reaped the never-executed leader.
                    if byte == 0 {
                        libc::_exit(0);
                    }
                }
                if group > 1 {
                    libc::kill(-group, libc::SIGKILL);
                }
                libc::_exit(0);
            }
        }
        drop(read);
        drop(ready_write);
        let guard = Self {
            pipe: Some(write),
            pid,
        };
        let mut ready = [0_u8; 1];
        File::from(ready_read).read_exact(&mut ready)?;
        if ready != [1] {
            return Err(io::Error::other("Run liveness guard did not become ready"));
        }
        Ok(guard)
    }
    fn descriptor(&self) -> RawFd {
        self.pipe.as_ref().expect("live guard").as_raw_fd()
    }
    fn finish(mut self) {
        self.close_and_wait();
    }
    fn disarm(mut self) {
        if let Some(pipe) = &self.pipe {
            let byte = 0_u8;
            // SAFETY: one atomic byte to our owned pipe.
            unsafe { libc::write(pipe.as_raw_fd(), (&byte as *const u8).cast(), 1) };
        }
        self.close_and_wait();
    }
    fn close_and_wait(&mut self) {
        drop(self.pipe.take());
        if self.pid > 0 {
            // SAFETY: this is our own forked guard child, never a run leader.
            loop {
                let result = unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
                if result >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
            self.pid = 0;
        }
    }
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        self.close_and_wait();
    }
}

fn spawn_command(run: &RunRecord) -> io::Result<(Child, RunGuard)> {
    let guard = RunGuard::new()?;
    let guard_fd = guard.descriptor();
    let environment = execution_environment();
    let executable = if Path::new(&run.argv[0]).is_absolute() {
        PathBuf::from(&run.argv[0])
    } else if run.argv[0].contains('/') {
        Path::new(&run.cwd).join(&run.argv[0])
    } else {
        crate::agent::resolve_on_path(
            &run.argv[0],
            environment
                .iter()
                .find(|(name, _)| name == "PATH")
                .map(|(_, value)| value.as_str())
                .unwrap_or("/usr/bin:/bin"),
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&run.argv[0]))
    };
    let mut command = Command::new(executable);
    command
        .args(&run.argv[1..])
        .current_dir(&run.cwd)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only setpgid/getpid/write syscalls run between fork and exec.
    // Register before exec; Engine alone retains the CLOEXEC liveness writer.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            let group = libc::getpid().to_ne_bytes();
            if libc::write(guard_fd, group.as_ptr().cast(), group.len()) != group.len() as isize {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match command.spawn() {
        Ok(child) => Ok((child, guard)),
        Err(error) => {
            guard.disarm();
            Err(error)
        }
    }
}

fn execution_environment() -> Vec<(String, String)> {
    // Do not inherit provider credentials, hooks/MCP sockets, or arbitrary
    // daemon secrets. PATH follows the Engine's already-captured login path.
    let mut environment = std::env::vars()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "HOME"
                    | "USER"
                    | "LOGNAME"
                    | "SHELL"
                    | "PATH"
                    | "TMPDIR"
                    | "LANG"
                    | "LC_ALL"
                    | "LC_CTYPE"
                    | "XDG_CACHE_HOME"
                    | "XDG_CONFIG_HOME"
                    | "XDG_DATA_HOME"
                    | "PNPM_HOME"
                    | "CARGO_HOME"
                    | "RUSTUP_HOME"
                    | "JAVA_HOME"
                    | "GOPATH"
            )
        })
        .collect::<Vec<_>>();
    let path = crate::local_path::search_path(None, environment.iter().cloned());
    environment.retain(|(name, _)| name != "PATH");
    environment.push(("PATH".into(), path));
    environment.push(("TERM".into(), "dumb".into()));
    environment.push(("NO_COLOR".into(), "1".into()));
    environment
}

fn validate_start(session: &SessionRecord, p: &RunStartParams) -> Result<(), ControlError> {
    identity(&p.session_id.0)?;
    identity(&p.request_id)?;
    if p.session_id != session.id {
        return Err(ControlError::not_found("Run target not found"));
    }
    if session.host.is_some() {
        return Err(ControlError::new(
            "run_remote_unsupported",
            "Runs execute only for local sessions; remote command capture is not available",
        ));
    }
    if session.is_note() {
        return Err(ControlError::new(
            "session_has_no_terminal",
            "Notes cannot execute commands",
        ));
    }
    if session.is_archived() {
        return Err(ControlError::new(
            "session_archived",
            "Archived sessions cannot execute commands",
        ));
    }
    let cwd = Path::new(&session.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(ControlError::bad_request(
            "Run target cwd must be an existing absolute local directory",
        ));
    }
    if p.argv.is_empty()
        || p.argv[0].is_empty()
        || p.argv.len() > 256
        || p.argv.iter().any(|arg| arg.contains('\0'))
        || p.argv.iter().map(String::len).sum::<usize>() > 64 * 1024
    {
        return Err(ControlError::bad_request(
            "Run argv requires an executable and at most 256 NUL-free arguments, totaling at most 64 KiB",
        ));
    }
    Ok(())
}
fn identity(id: &str) -> Result<(), ControlError> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        Err(ControlError::bad_request(
            "Run/session/request identity must be 1–256 non-control bytes",
        ))
    } else {
        Ok(())
    }
}
fn run_identity(id: &str) -> Result<(), ControlError> {
    if id.len() == 34
        && id.starts_with("r_")
        && id[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(ControlError::bad_request("Invalid run identity"))
    }
}
fn fingerprint(p: &RunStartParams) -> Result<String, ControlError> {
    let bytes = serde_json::to_vec(p).map_err(storage)?;
    let mut encoded = String::with_capacity(64);
    append_hex(&mut encoded, &Sha256::digest(bytes));
    Ok(encoded)
}
fn next_run_id() -> Result<String, ControlError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(storage)?;
    let mut id = String::with_capacity(34);
    id.push_str("r_");
    append_hex(&mut id, &bytes);
    Ok(id)
}
fn append_hex(encoded: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}
fn now() -> DateMillis {
    DateMillis::from(SystemTime::now())
}
fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}
fn storage(error: impl std::fmt::Display) -> ControlError {
    ControlError::internal(format!("Run storage: {error}"))
}
fn control_io(error: ControlError) -> io::Error {
    io::Error::other(error.message)
}
fn private_dir(path: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Run storage must be a real owner-only directory",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}
fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: callers retain the pipe owner through all reads.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn exited_unreaped(pid: u32) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: our child id, valid output storage; WNOWAIT pins its identity
    // until process-group cleanup and pipe draining complete.
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}
fn read_header(file: &mut File) -> io::Result<Option<(RunOutputStream, u64, u32)>> {
    let mut header = [0_u8; FRAME_HEADER];
    let count = file.read(&mut header[..1])?;
    if count == 0 {
        return Ok(None);
    }
    file.read_exact(&mut header[1..])?;
    let stream = match header[0] {
        0 => RunOutputStream::Stdout,
        1 => RunOutputStream::Stderr,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unknown run output source",
            ));
        }
    };
    let sequence = u64::from_le_bytes(header[1..9].try_into().expect("sequence bytes"));
    let length = u32::from_le_bytes(header[9..13].try_into().expect("length bytes"));
    if length == 0 || length as usize > BUFFER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid run output frame length",
        ));
    }
    Ok(Some((stream, sequence, length)))
}
fn recover_output(path: &Path) -> io::Result<u64> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let actual = file.metadata()?.len().min(MAX_RUN_OUTPUT_BYTES);
    let mut complete = 0_u64;
    let mut logical = 0_u64;
    let mut previous = 0_u64;
    loop {
        match read_header(&mut file) {
            Ok(Some((_, sequence, length))) if sequence > previous => {
                let end = file.stream_position()?.saturating_add(u64::from(length));
                if end > actual {
                    break;
                }
                file.seek(SeekFrom::Start(end))?;
                complete = end;
                logical += u64::from(length);
                previous = sequence;
            }
            Ok(None) | Ok(Some(_)) => break,
            Err(error)
                if error.kind() == io::ErrorKind::UnexpectedEof
                    || error.kind() == io::ErrorKind::InvalidData =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    file.set_len(complete)?;
    file.sync_all()?;
    Ok(logical)
}

impl super::ControlServer {
    fn run_store(&self) -> Result<&RunStore, ControlError> {
        self.runs.as_ref().map_err(|error| {
            ControlError::new(
                "run_storage_unavailable",
                format!("Run storage unavailable: {error}"),
            )
        })
    }
    pub(super) fn run_start(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: RunStartParams = super::decode(params)?;
        let session = self
            .registry
            .lock()
            .map_err(super::poisoned)?
            .record(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found("Run target session not found"))?;
        super::encode(&self.run_store()?.start(session, p)?)
    }
    pub(super) fn run_list(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: RunListParams = super::decode(params)?;
        super::encode(&self.run_store()?.list(&p)?)
    }
    pub(super) fn run_get(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: RunGetParams = super::decode(params)?;
        super::encode(&self.run_store()?.get(&p)?)
    }
    pub(super) fn run_read_output(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: RunReadOutputParams = super::decode(params)?;
        super::encode(&self.run_store()?.read_output(&p)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ubra_proto::{AgentKind, SessionId};

    fn fixture() -> (tempfile::TempDir, RunStore, SessionRecord) {
        let directory = tempfile::tempdir().unwrap();
        let store = RunStore::open(
            directory.path().join("runs-v1"),
            crate::events::EventBus::new(),
        )
        .unwrap();
        let session =
            super::super::new_record("s_runs_a", "shell", directory.path().to_str().unwrap());
        (directory, store, session)
    }
    fn command(session: &SessionRecord, request: &str, script: &str) -> RunStartParams {
        RunStartParams {
            session_id: session.id.clone(),
            request_id: request.into(),
            kind: RunKind::Test,
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        }
    }
    fn get(store: &RunStore, run: &RunRecord) -> RunRecord {
        store
            .get(&RunGetParams {
                session_id: run.session_id.clone(),
                run_id: run.run_id.clone(),
            })
            .unwrap()
    }
    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("real run condition did not settle before timeout");
    }
    fn terminal(store: &RunStore, run: &RunRecord) -> RunRecord {
        wait_until(|| get(store, run).status.is_terminal());
        get(store, run)
    }
    fn output(store: &RunStore, run: &RunRecord, stream: RunOutputStream) -> String {
        let mut offset = 0;
        let mut text = String::new();
        let mut decoder = RunOutputDecoder::default();
        loop {
            let chunk = store
                .read_output(&RunReadOutputParams {
                    session_id: run.session_id.clone(),
                    run_id: run.run_id.clone(),
                    offset,
                    max_bytes: Some(MAX_RUN_READ_BYTES),
                })
                .unwrap();
            for part in chunk.parts {
                if part.stream == stream {
                    text.push_str(&decoder.push(stream, &part.bytes));
                }
            }
            if chunk.eof {
                text.push_str(&decoder.finish(stream));
                return text;
            }
            assert!(
                chunk.next_offset > offset,
                "terminal output must advance until EOF"
            );
            offset = chunk.next_offset;
        }
    }
    #[test]
    fn run_output_preserves_utf8_across_single_byte_pages() {
        let (_directory, store, session) = fixture();
        let run = store
            .start(
                session.clone(),
                command(&session, "utf8", "printf '\\303\\251\\360\\237\\246\\200'"),
            )
            .unwrap();
        let run = terminal(&store, &run);
        let mut offset = 0;
        let mut text = String::new();
        let mut decoder = RunOutputDecoder::default();
        loop {
            let chunk = store
                .read_output(&RunReadOutputParams {
                    session_id: run.session_id.clone(),
                    run_id: run.run_id.clone(),
                    offset,
                    max_bytes: Some(1),
                })
                .unwrap();
            for part in &chunk.parts {
                text.push_str(&decoder.push(part.stream, &part.bytes));
            }
            if chunk.eof {
                text.push_str(&decoder.finish(RunOutputStream::Stdout));
                text.push_str(&decoder.finish(RunOutputStream::Stderr));
                break;
            }
            offset = chunk.next_offset;
        }
        assert_eq!(text, "é🦀");
    }
    fn process_running(pid: i32) -> bool {
        // Zombies cannot run or retain output descriptors; a container's pid 1
        // may defer reaping an orphan even though group cleanup succeeded.
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && stat
                .rsplit_once(") ")
                .is_some_and(|(_, tail)| tail.starts_with('Z'))
        {
            return false;
        }
        // SAFETY: signal zero only observes existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }
    fn read_pid(path: &Path) -> i32 {
        wait_until(|| {
            std::fs::read_to_string(path).is_ok_and(|pid| pid.trim().parse::<i32>().is_ok())
        });
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn run_real_delayed_stdout_stderr_exit_and_duplicate_admission() {
        let (directory, store, session) = fixture();
        let params = command(
            &session,
            "delayed",
            "printf 'spawn\\n' >> admissions; printf 'first\\n'; printf 'problem\\n' >&2; while [ ! -f release ]; do /bin/sleep 0.01; done; printf 'last\\n'; exit 7",
        );
        let (first, duplicate) = std::thread::scope(|scope| {
            let a = scope.spawn(|| store.start(session.clone(), params.clone()).unwrap());
            let b = scope.spawn(|| store.start(session.clone(), params.clone()).unwrap());
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_eq!(first.run_id, duplicate.run_id);
        assert_eq!(first.status, RunStatus::Running);
        assert_eq!(first.exit_code, None);
        wait_until(|| get(&store, &first).output_bytes > 0);
        assert_eq!(get(&store, &first).status, RunStatus::Running);
        std::fs::write(directory.path().join("release"), "").unwrap();
        let finished = terminal(&store, &first);
        assert_eq!(finished.status, RunStatus::Failed);
        assert_eq!(finished.exit_code, Some(7));
        assert_eq!(finished.signal, None);
        assert!(finished.duration_ms.unwrap() >= 100);
        assert!(finished.finished_at.is_some());
        assert_eq!(
            output(&store, &finished, RunOutputStream::Stdout),
            "first\nlast\n"
        );
        assert_eq!(
            output(&store, &finished, RunOutputStream::Stderr),
            "problem\n"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("admissions")).unwrap(),
            "spawn\n"
        );
        assert_eq!(store.start(session, params).unwrap().run_id, first.run_id);
        assert_eq!(
            store
                .list(&RunListParams {
                    session_id: first.session_id,
                    limit: Some(20),
                    cursor: None
                })
                .unwrap()
                .runs
                .len(),
            1
        );
    }

    #[test]
    fn run_spawn_failure_has_no_fabricated_exit_and_request_conflict_never_executes() {
        let (directory, store, session) = fixture();
        let params = RunStartParams {
            session_id: session.id.clone(),
            request_id: "missing".into(),
            kind: RunKind::Build,
            argv: vec![
                directory
                    .path()
                    .join("missing-executable")
                    .to_str()
                    .unwrap()
                    .into(),
            ],
        };
        let run = store.start(session.clone(), params.clone()).unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.exit_code, None);
        assert_eq!(run.signal, None);
        assert!(run.error.as_deref().unwrap().contains("Could not start"));
        assert_eq!(
            store.start(session.clone(), params).unwrap().run_id,
            run.run_id
        );
        let conflict = store
            .start(
                session.clone(),
                command(&session, "missing", "touch should-not-exist"),
            )
            .unwrap_err();
        assert_eq!(conflict.code, "request_conflict");
        assert!(!directory.path().join("should-not-exist").exists());
    }

    #[test]
    fn run_real_structured_arguments_are_never_split_or_evaluated_as_shell_text() {
        let (directory, store, session) = fixture();
        let argv = vec![
            "/bin/echo".into(),
            "two words".into(),
            "$(touch forbidden)".into(),
            "; touch forbidden".into(),
        ];
        let run = store
            .start(
                session.clone(),
                RunStartParams {
                    session_id: session.id,
                    request_id: "literal-argv".into(),
                    kind: RunKind::Command,
                    argv: argv.clone(),
                },
            )
            .unwrap();
        let run = terminal(&store, &run);
        assert_eq!(run.argv, argv);
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(
            output(&store, &run, RunOutputStream::Stdout),
            "two words $(touch forbidden) ; touch forbidden\n"
        );
        assert!(!directory.path().join("forbidden").exists());
    }

    #[test]
    fn run_real_signal_exit_is_not_a_numeric_exit_code() {
        let (_directory, store, session) = fixture();
        let run = store
            .start(
                session.clone(),
                command(&session, "signal", "kill -TERM $$"),
            )
            .unwrap();
        let run = terminal(&store, &run);
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.exit_code, None);
        assert_eq!(run.signal, Some(libc::SIGTERM));
        assert!(run.duration_ms.is_some());
    }

    #[test]
    fn run_combined_output_cap_drains_both_streams_and_read_is_bounded() {
        let (_directory, store, session) = fixture();
        let run = store.start(session.clone(), command(&session, "cap",
            "/bin/dd if=/dev/zero bs=65536 count=96 2>/dev/null & /bin/dd if=/dev/zero bs=65536 count=96 >&2 2>/dev/null & wait; exit 3")).unwrap();
        let run = terminal(&store, &run);
        assert_eq!(
            run.exit_code,
            Some(3),
            "both over-cap writers must finish, not block on full pipes"
        );
        assert!(run.output_truncated);
        assert!(run.output_bytes <= MAX_RUN_OUTPUT_BYTES);
        assert!(
            std::fs::metadata(store.inner.output_path(&run.run_id))
                .unwrap()
                .len()
                <= MAX_RUN_OUTPUT_BYTES
        );
        let chunk = store
            .read_output(&RunReadOutputParams {
                session_id: session.id,
                run_id: run.run_id,
                offset: 0,
                max_bytes: Some(u32::MAX),
            })
            .unwrap();
        assert_eq!(chunk.next_offset, u64::from(MAX_RUN_READ_BYTES));
        assert!(chunk.truncated);
        assert!(!chunk.eof);
        assert!(
            chunk
                .parts
                .iter()
                .map(|part| u64::from(part.byte_len))
                .sum::<u64>()
                <= u64::from(MAX_RUN_READ_BYTES)
        );
    }

    #[test]
    fn run_session_scope_and_unsafe_terminal_controls_are_preserved_as_inert_text() {
        let (_directory, store, session) = fixture();
        let run = store
            .start(
                session.clone(),
                command(
                    &session,
                    "safe",
                    "printf '\\033]52;c;private\\007\\033[31mred\\033[0m\\r\\n'",
                ),
            )
            .unwrap();
        let run = terminal(&store, &run);
        let other = SessionId::new("s_runs_b");
        let error = store
            .get(&RunGetParams {
                session_id: other.clone(),
                run_id: run.run_id.clone(),
            })
            .unwrap_err();
        assert_eq!(error.code, "not_found");
        let error = store
            .read_output(&RunReadOutputParams {
                session_id: other,
                run_id: run.run_id.clone(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap_err();
        assert_eq!(error.code, "not_found");
        let text = output(&store, &run, RunOutputStream::Stdout);
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\x07'));
        assert!(!text.contains('\r'));
        assert!(text.contains("\\u{1b}]52;c;private\\u{7}"));
        assert!(text.contains("red"));
    }

    #[test]
    fn run_rejects_remote_note_archived_and_invalid_cwd_before_spawn() {
        let (directory, store, session) = fixture();
        let params = command(&session, "reject", "touch forbidden");
        let mut remote = session.clone();
        remote.host = Some("host_fixture".into());
        assert_eq!(
            store.start(remote, params.clone()).unwrap_err().code,
            "run_remote_unsupported"
        );
        let mut note = session.clone();
        note.kind = AgentKind::new(AgentKind::NOTE_ID);
        assert_eq!(
            store.start(note, params.clone()).unwrap_err().code,
            "session_has_no_terminal"
        );
        let mut archived = session.clone();
        archived.archived_at = Some(now());
        assert_eq!(
            store.start(archived, params.clone()).unwrap_err().code,
            "session_archived"
        );
        let mut invalid = session;
        invalid.cwd = "relative".into();
        assert_eq!(
            store.start(invalid, params).unwrap_err().code,
            "bad_request"
        );
        assert!(!directory.path().join("forbidden").exists());
    }

    #[test]
    fn run_normal_leader_exit_cleans_descendant_group_and_unblocks_inherited_pipes() {
        let (directory, store, session) = fixture();
        let run = store.start(session.clone(), command(&session, "descendant",
            "/bin/sleep 30 & printf '%s\\n' \"$!\" > descendant; printf 'leader finished\\n'; exit 0")).unwrap();
        let pid = read_pid(&directory.path().join("descendant"));
        let run = terminal(&store, &run);
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(
            output(&store, &run, RunOutputStream::Stdout),
            "leader finished\n"
        );
        wait_until(|| !process_running(pid));
    }

    #[test]
    fn run_shutdown_cancels_group_and_settles_real_signal() {
        let (directory, store, session) = fixture();
        let run = store.start(session.clone(), command(&session, "shutdown",
            "printf '%s\\n' \"$$\" > leader; /bin/sleep 30 & printf '%s\\n' \"$!\" > descendant; wait")).unwrap();
        let leader = read_pid(&directory.path().join("leader"));
        let descendant = read_pid(&directory.path().join("descendant"));
        assert_eq!(store.active_count(), 1);
        store.shutdown();
        let run = get(&store, &run);
        assert_eq!(run.status, RunStatus::Cancelled);
        assert_eq!(run.signal, Some(libc::SIGKILL));
        assert_eq!(run.exit_code, None);
        assert_eq!(store.active_count(), 0);
        wait_until(|| !process_running(leader) && !process_running(descendant));
    }

    #[test]
    fn run_concurrency_is_bounded_without_preventing_other_sessions() {
        let (_directory, store, a) = fixture();
        let mut b = a.clone();
        b.id = SessionId::new("s_runs_b");
        let mut c = a.clone();
        c.id = SessionId::new("s_runs_c");
        for index in 0..MAX_ACTIVE_RUNS_PER_SESSION {
            store
                .start(
                    a.clone(),
                    command(&a, &format!("a-{index}"), "/bin/sleep 30"),
                )
                .unwrap();
        }
        assert_eq!(
            store
                .start(a.clone(), command(&a, "a-over", "/bin/true"))
                .unwrap_err()
                .code,
            "run_busy"
        );
        for index in 0..MAX_ACTIVE_RUNS_PER_SESSION {
            store
                .start(
                    b.clone(),
                    command(&b, &format!("b-{index}"), "/bin/sleep 30"),
                )
                .unwrap();
        }
        assert_eq!(
            store
                .start(c.clone(), command(&c, "c-over", "/bin/true"))
                .unwrap_err()
                .code,
            "run_busy"
        );
        assert_eq!(store.active_count(), MAX_ACTIVE_RUNS);
        store.shutdown();
    }

    #[test]
    fn run_metadata_and_output_are_owner_only_and_survive_reopen() {
        let (directory, store, session) = fixture();
        let p = command(&session, "durable", "printf durable");
        let run = store.start(session.clone(), p.clone()).unwrap();
        let run = terminal(&store, &run);
        for path in [&store.inner.root, &store.inner.root.join("output")] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for path in [
            store.inner.root.join("metadata.sqlite"),
            store.inner.output_path(&run.run_id),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(store);
        let reopened = RunStore::open(
            directory.path().join("runs-v1"),
            crate::events::EventBus::new(),
        )
        .unwrap();
        assert_eq!(get(&reopened, &run).exit_code, Some(0));
        assert_eq!(output(&reopened, &run, RunOutputStream::Stdout), "durable");
        assert_eq!(reopened.start(session, p).unwrap().run_id, run.run_id);
    }

    #[test]
    fn run_idle_exit_atomically_freezes_admission_without_stopping_live_runs()
    -> Result<(), ControlError> {
        let (_directory, store, session) = fixture();
        let liveness = store.liveness();
        {
            let _admitting = store
                .inner
                .admission
                .lock()
                .map_err(super::super::poisoned)?;
            assert!(
                !liveness.try_stop_idle(),
                "in-flight admission must cancel idle exit without blocking"
            );
        }
        let run = store
            .start(
                session.clone(),
                command(&session, "keepalive", "/bin/sleep 30"),
            )
            .unwrap();
        assert!(!liveness.try_stop_idle());
        let second = store
            .start(
                session.clone(),
                command(&session, "still-admitted", "/bin/sleep 30"),
            )
            .unwrap();
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(second.status, RunStatus::Running);
        store.shutdown();

        let (_idle_directory, idle_store, idle_session) = fixture();
        assert!(idle_store.liveness().try_stop_idle());
        let rejected = idle_store
            .start(
                idle_session.clone(),
                command(&idle_session, "after-stop", "/bin/true"),
            )
            .unwrap_err();
        assert_eq!(rejected.code, "engine_stopping");
        assert!(
            idle_store
                .list(&RunListParams {
                    session_id: idle_session.id,
                    limit: None,
                    cursor: None
                })
                .unwrap()
                .runs
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn run_retention_paginates_and_never_deletes_active_or_reexecutes_evicted_requests() {
        let (_directory, store, session) = fixture();
        let p = command(&session, "old-real", "printf original");
        let oldest = store.start(session.clone(), p.clone()).unwrap();
        let finished = terminal(&store, &oldest);
        let active = store
            .start(
                session.clone(),
                command(&session, "active", "/bin/sleep 30"),
            )
            .unwrap();
        let db = store.inner.db().unwrap();
        // Metadata retention fixture cloned from an observed terminal run.
        // Only the active record is excluded from GC; outputs are real files.
        for index in 0..MAX_RETAINED_RUNS_PER_SESSION {
            let mut fixture = finished.clone();
            fixture.run_id = next_run_id().unwrap();
            fixture.request_id = format!("retained-{index}");
            fixture.output_bytes = 0;
            fixture.output_available = false;
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(store.inner.output_path(&fixture.run_id))
                .unwrap();
            db.execute(
                "INSERT INTO runs_v1 VALUES (?1,?2,1,?3)",
                params![
                    fixture.run_id,
                    session.id.0,
                    serde_json::to_string(&fixture).unwrap(),
                ],
            )
            .unwrap();
        }
        store.inner.prune(&db, &session.id.0).unwrap();
        assert_eq!(get(&store, &active).status, RunStatus::Running);
        let page = store
            .list(&RunListParams {
                session_id: session.id.clone(),
                limit: Some(17),
                cursor: None,
            })
            .unwrap();
        assert_eq!(page.runs.len(), 17);
        let next = store
            .list(&RunListParams {
                session_id: session.id.clone(),
                limit: Some(200),
                cursor: page.next_cursor,
            })
            .unwrap();
        assert_eq!(next.runs.len(), MAX_RETAINED_RUNS_PER_SESSION - 17);
        assert!(next.next_cursor.is_none());
        assert!(!next.runs.iter().any(|run| run.run_id == oldest.run_id));
        assert_eq!(
            store.start(session, p).unwrap_err().code,
            "run_retention_expired"
        );
        store.shutdown();
    }

    struct EscapedWriterCleanup(PathBuf);
    impl EscapedWriterCleanup {
        fn kill(&self) {
            if let Ok(contents) = std::fs::read_to_string(&self.0)
                && let Ok(pid) = contents.parse::<i32>()
                && pid > 1
            {
                // SAFETY: this private fixture records its live setsid child.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
    }
    impl Drop for EscapedWriterCleanup {
        fn drop(&mut self) {
            self.kill();
        }
    }

    fn escaped_writer_command(
        session: &SessionRecord,
        pid_file: &Path,
        keep_leader: bool,
    ) -> RunStartParams {
        let mut argv = vec![
            "/usr/bin/env".into(),
            format!("UBRA_RUN_ESCAPED_PID_FILE={}", pid_file.display()),
        ];
        if keep_leader {
            argv.push("UBRA_RUN_ESCAPED_LEADER_WAIT=1".into());
        }
        argv.extend([
            std::env::current_exe()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
            "--exact".into(),
            "control::runs::tests::run_escaped_writer_fixture".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ]);
        RunStartParams {
            session_id: session.id.clone(),
            request_id: "escaped-writer".into(),
            kind: RunKind::Test,
            argv,
        }
    }

    #[test]
    #[ignore = "subprocess fixture invoked by escaped-writer regressions"]
    fn run_escaped_writer_fixture() {
        let pid_file = std::env::var_os("UBRA_RUN_ESCAPED_PID_FILE").expect("fixture pid file");
        let mut ready = [0_i32; 2];
        // SAFETY: valid stack descriptors; child performs only syscalls after fork.
        unsafe {
            assert_eq!(libc::pipe(ready.as_mut_ptr()), 0);
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::close(ready[0]);
                if libc::setsid() < 0 {
                    libc::_exit(1);
                }
                if libc::write(ready[1], b"r".as_ptr().cast(), 1) != 1 {
                    libc::_exit(1);
                }
                libc::close(ready[1]);
                loop {
                    libc::pause();
                }
            }
            libc::close(ready[1]);
            let mut signal = 0_u8;
            assert_eq!(libc::read(ready[0], (&mut signal as *mut u8).cast(), 1), 1);
            libc::close(ready[0]);
            assert_eq!(signal, b'r');
            std::fs::write(pid_file, pid.to_string()).unwrap();
        }
        if std::env::var_os("UBRA_RUN_ESCAPED_LEADER_WAIT").is_some() {
            // SAFETY: fixture waits for the run guard's real termination signal.
            loop {
                unsafe {
                    libc::pause();
                }
            }
        }
        std::process::exit(0);
    }

    #[test]
    fn run_escaped_writer_does_not_prevent_settlement() {
        let (directory, store, session) = fixture();
        let pid_file = directory.path().join("escaped-pid");
        let _cleanup = EscapedWriterCleanup(pid_file.clone());
        let run = store
            .start(
                session.clone(),
                escaped_writer_command(&session, &pid_file, false),
            )
            .unwrap();
        let escaped = read_pid(&pid_file);
        assert!(process_running(escaped));
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !get(&store, &run).status.is_terminal() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let settled = get(&store, &run);
        assert!(
            settled.status.is_terminal(),
            "escaped inherited writer kept the run open"
        );
        assert_eq!(settled.status, RunStatus::Failed);
        assert_eq!(settled.exit_code, Some(0));
        assert!(settled.output_truncated);
        assert!(
            settled
                .error
                .as_deref()
                .unwrap()
                .contains("inherited writer")
        );
    }

    #[test]
    fn run_escaped_writer_does_not_block_explicit_shutdown() {
        let (directory, store, session) = fixture();
        let store = Arc::new(store);
        let pid_file = directory.path().join("escaped-pid");
        let cleanup = EscapedWriterCleanup(pid_file.clone());
        let run = store
            .start(
                session.clone(),
                escaped_writer_command(&session, &pid_file, true),
            )
            .unwrap();
        read_pid(&pid_file);
        let shutdown_store = Arc::clone(&store);
        let (sent, received) = std::sync::mpsc::sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            shutdown_store.shutdown();
            sent.send(()).unwrap();
        });
        let result = received.recv_timeout(Duration::from_secs(2));
        if result.is_err() {
            cleanup.kill();
        }
        shutdown.join().unwrap();
        assert!(
            result.is_ok(),
            "escaped inherited writer blocked Engine shutdown"
        );
        let settled = get(&store, &run);
        assert_eq!(settled.status, RunStatus::Cancelled);
        assert_eq!(settled.signal, Some(libc::SIGKILL));
        assert!(settled.output_truncated);
    }

    /// Invoked in a separate test process by the crash-recovery regression.
    #[test]
    fn run_engine_crash_fixture() {
        let Some(root) = std::env::var_os("UBRA_RUN_CRASH_FIXTURE") else {
            return;
        };
        let root = PathBuf::from(root);
        let store = RunStore::open(root.join("runs-v1"), crate::events::EventBus::new()).unwrap();
        let session = super::super::new_record("s_crash", "shell", root.to_str().unwrap());
        let run = store.start(session.clone(), command(&session, "crash",
            "printf '%s\\n' \"$$\" > leader; /bin/sleep 30 & printf '%s\\n' \"$!\" > descendant; printf 'before crash\\n'; wait")).unwrap();
        std::fs::write(root.join("run-id"), &run.run_id).unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn run_engine_sigkill_cleans_group_and_restart_marks_unsettled_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "control::runs::tests::run_engine_crash_fixture",
                "--nocapture",
            ])
            .env("UBRA_RUN_CRASH_FIXTURE", directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let leader = read_pid(&directory.path().join("leader"));
        let descendant = read_pid(&directory.path().join("descendant"));
        let root = directory.path().join("runs-v1");
        wait_until(|| directory.path().join("run-id").is_file());
        let id = std::fs::read_to_string(directory.path().join("run-id")).unwrap();
        let db = Connection::open(root.join("metadata.sqlite")).unwrap();
        wait_until(|| {
            db.query_row("SELECT record FROM runs_v1 WHERE run_id=?1", [&id], |row| {
                row.get::<_, String>(0)
            })
            .ok()
            .and_then(|raw| serde_json::from_str::<RunRecord>(&raw).ok())
            .is_some_and(|run| run.output_bytes > 0)
        });
        engine.kill().unwrap();
        engine.wait().unwrap();
        wait_until(|| !process_running(leader) && !process_running(descendant));
        drop(db);
        let reopened = RunStore::open(root, crate::events::EventBus::new()).unwrap();
        let run = reopened
            .get(&RunGetParams {
                session_id: SessionId::new("s_crash"),
                run_id: id,
            })
            .unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
        assert_eq!(run.exit_code, None);
        assert_eq!(run.signal, None);
        assert_eq!(run.duration_ms, None);
        assert!(run.error.as_deref().unwrap().contains("restarted"));
        assert_eq!(
            output(&reopened, &run, RunOutputStream::Stdout),
            "before crash\n"
        );
    }
}
