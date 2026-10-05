//! Bounded durable history of authoritative Session lifecycle outcomes.
//!
//! Callers present complete `SessionRecord`s at the existing event-publication
//! seam. This module alone maps statuses to activity vocabulary, collapses
//! repeats, snapshots display identity, and maintains the JSONL file.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ubra_proto::{
    ActivityEntry, ActivityKind, ActivitySource, DateMillis, SessionRecord, SessionStatus,
};

const MAX_ENTRIES: usize = 300;
const COMPACT_AT_PHYSICAL_LINES: usize = MAX_ENTRIES * 2;

pub struct ActivityLog {
    path: PathBuf,
    entries: Vec<ActivityEntry>,
    physical_lines: usize,
}

impl ActivityLog {
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let physical_lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count();
        let mut entries = Vec::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let Ok(entry) = serde_json::from_slice::<ActivityEntry>(line) else {
                continue;
            };
            append_collapsing(&mut entries, entry);
            trim(&mut entries);
        }
        Ok(Self {
            path,
            entries,
            physical_lines,
        })
    }

    pub fn recent(&self, limit: usize) -> Vec<ActivityEntry> {
        self.entries.iter().rev().take(limit).cloned().collect()
    }

    pub fn for_session(&self, session_id: Option<&str>, limit: usize) -> Vec<ActivityEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| session_id.is_none_or(|id| entry.session_id.0 == id))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Append only factual producer receipts. Replayed/older revisions are ignored.
    pub fn record(
        &mut self,
        record: &SessionRecord,
        kind: ActivityKind,
        source: ActivitySource,
        at: DateMillis,
    ) -> io::Result<bool> {
        let replayed = self.entries.iter().rev().find_map(|entry| {
            (entry.session_id == record.id)
                .then_some(entry.source.as_ref())
                .flatten()
                .filter(|old| old.producer == source.producer && old.id == source.id)
        });
        if replayed.is_some_and(|previous| previous.revision >= source.revision) {
            return Ok(false);
        }
        let entry = ActivityEntry {
            id: activity_id(),
            session_id: record.id.clone(),
            kind,
            at,
            title: record.title.clone(),
            agent_id: record.effective_kind().id().to_owned(),
            project_id: record.project_id.clone(),
            cwd: record.cwd.clone(),
            host: record.host.clone(),
            source: Some(source),
        };
        self.persist(entry)
    }

    pub fn observe(&mut self, record: &SessionRecord) -> io::Result<bool> {
        let kind = match record.status {
            SessionStatus::Starting => ActivityKind::Started,
            SessionStatus::Working => ActivityKind::Working,
            SessionStatus::NeedsInput(_) => ActivityKind::NeedsInput,
            SessionStatus::Idle => ActivityKind::Idle,
            SessionStatus::Exited(_) => ActivityKind::Exited,
            SessionStatus::Unknown => return Ok(false),
        };
        self.append(record, kind)
    }

    pub fn observe_removed(&mut self, record: &SessionRecord) -> io::Result<bool> {
        self.append(record, ActivityKind::Exited)
    }

    fn append(&mut self, record: &SessionRecord, kind: ActivityKind) -> io::Result<bool> {
        if self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.session_id == record.id && entry.source.is_none())
            .is_some_and(|entry| {
                entry.kind == kind
                    && entry.title == record.title
                    && entry.agent_id == record.effective_kind().id()
                    && entry.project_id == record.project_id
                    && entry.cwd == record.cwd
                    && entry.host == record.host
            })
        {
            return Ok(false);
        }
        let entry = ActivityEntry {
            id: activity_id(),
            session_id: record.id.clone(),
            kind,
            at: DateMillis::from(std::time::SystemTime::now()),
            title: record.title.clone(),
            agent_id: record.effective_kind().id().to_owned(),
            project_id: record.project_id.clone(),
            cwd: record.cwd.clone(),
            host: record.host.clone(),
            source: None,
        };
        self.persist(entry)
    }

    fn persist(&mut self, entry: ActivityEntry) -> io::Result<bool> {
        append_line(&self.path, &entry)?;
        self.physical_lines += 1;
        let appended = append_collapsing(&mut self.entries, entry);
        trim(&mut self.entries);
        if self.physical_lines >= COMPACT_AT_PHYSICAL_LINES {
            compact(&self.path, &self.entries)?;
            self.physical_lines = self.entries.len();
        }
        Ok(appended)
    }
}

fn append_collapsing(entries: &mut Vec<ActivityEntry>, entry: ActivityEntry) -> bool {
    if let Some(source) = entry.source.as_ref()
        && let Some(previous) = entries.iter_mut().rev().find_map(|candidate| {
            (candidate.session_id == entry.session_id)
                .then_some(candidate.source.as_mut())
                .flatten()
                .filter(|previous| previous.producer == source.producer && previous.id == source.id)
        })
    {
        if previous.revision >= source.revision {
            return false;
        }
        if source.producer == ubra_proto::ActivityProducer::Task
            && source.state.is_some()
            && previous.state == source.state
        {
            // Retain the durable high-water mark without inventing a transition.
            previous.revision = source.revision;
            return false;
        }
    }
    if let Some(previous) = entries.iter().rposition(|candidate| {
        candidate.session_id == entry.session_id && candidate.source.is_none()
    }) && entry.source.is_none()
        && entries[previous].kind == entry.kind
    {
        entries.remove(previous);
    }
    entries.push(entry);
    true
}

fn trim(entries: &mut Vec<ActivityEntry>) {
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
}

fn append_line(path: &Path, entry: &ActivityEntry) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    serde_json::to_writer(&mut file, entry)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

fn compact(path: &Path, entries: &[ActivityEntry]) -> io::Result<()> {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("activity-log.jsonl");
    let temporary = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    for entry in entries {
        serde_json::to_writer(&mut file, entry)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    drop(file);
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}

fn activity_id() -> String {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let fallback = DateMillis::from(std::time::SystemTime::now()).0.to_bits();
        bytes = fallback.to_be_bytes();
    }
    format!(
        "a_{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ubra_proto::{AgentKind, ProjectId, Resumability, SessionId, TitleSource};

    pub(crate) fn record(id: &str, status: SessionStatus) -> SessionRecord {
        SessionRecord {
            attention_state: None,
            id: SessionId::new(id),
            kind: AgentKind::CODEX,
            cwd: "/tmp/project".into(),
            project_id: ProjectId::new("p_project"),
            worktree_path: None,
            git_branch: None,
            title: format!("Session {id}"),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            capabilities: None,
            parent: None,
            created_at: DateMillis(1.0),
            updated_at: DateMillis(1.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            remote_connection: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
            terminal_cwd: None,
            note_id: None,
            note_workspace: None,
            foreground_ports: None,
            terminal_progress: None,
            scheduled_run: None,
        }
    }

    #[test]
    fn lifecycle_outcomes_are_durable_and_repeat_collapsed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("activity.jsonl");
        let mut log = ActivityLog::load(&path).expect("load");
        let working = record("s_one", SessionStatus::Working);
        assert!(log.observe(&working).expect("working"));
        assert!(!log.observe(&working).expect("duplicate"));
        let idle = record("s_one", SessionStatus::Idle);
        assert!(log.observe(&idle).expect("idle"));

        let reloaded = ActivityLog::load(&path).expect("reload");
        assert_eq!(
            reloaded
                .recent(10)
                .into_iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            [ActivityKind::Idle, ActivityKind::Working]
        );
    }

    #[test]
    fn snapshots_survive_a_removed_record() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("activity.jsonl");
        let mut log = ActivityLog::load(&path).expect("load");
        let record = record("s_removed", SessionStatus::Working);
        log.observe(&record).expect("started");
        log.observe_removed(&record).expect("removed");

        let entries = ActivityLog::load(path).expect("reload").recent(1);
        assert_eq!(entries[0].kind, ActivityKind::Exited);
        assert_eq!(entries[0].title, "Session s_removed");
        assert_eq!(entries[0].cwd, "/tmp/project");
    }

    #[test]
    fn malformed_lines_do_not_hide_later_history() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("activity.jsonl");
        let valid = ActivityEntry {
            id: "a_valid".into(),
            session_id: SessionId::new("s_valid"),
            kind: ActivityKind::Finished,
            at: DateMillis(1.0),
            title: "Valid".into(),
            agent_id: "codex".into(),
            project_id: ProjectId::new("p"),
            cwd: "/tmp".into(),
            host: None,
            source: None,
        };
        let mut bytes = b"not json\n".to_vec();
        bytes.extend(serde_json::to_vec(&valid).expect("encode"));
        bytes.push(b'\n');
        std::fs::write(&path, bytes).expect("fixture");

        assert_eq!(ActivityLog::load(path).expect("load").recent(10), [valid]);
    }

    #[test]
    fn session_filter_precedes_limit_and_global_retention_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = ActivityLog::load(dir.path().join("activity.jsonl")).unwrap();
        log.observe(&record("target", SessionStatus::Working))
            .unwrap();
        for i in 0..299 {
            log.observe(&record(&format!("other_{i}"), SessionStatus::Working))
                .unwrap();
        }
        assert_eq!(log.for_session(Some("target"), 1).len(), 1);
        assert_eq!(log.recent(400).len(), 300);
        log.observe(&record("overflow", SessionStatus::Working))
            .unwrap();
        assert!(log.for_session(Some("target"), 1).is_empty());
    }

    #[test]
    fn typed_source_replays_and_stale_revisions_do_not_duplicate_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.jsonl");
        let mut log = ActivityLog::load(&path).unwrap();
        let record = record("one", SessionStatus::Working);
        let source = |revision, state: &str| ActivitySource {
            producer: ubra_proto::ActivityProducer::Task,
            id: "task_1".into(),
            revision,
            state: Some(state.into()),
        };
        assert!(
            log.record(
                &record,
                ActivityKind::TaskTransition,
                source(2, "blocked"),
                DateMillis(2.0)
            )
            .unwrap()
        );
        assert!(
            !log.record(
                &record,
                ActivityKind::TaskTransition,
                source(2, "blocked"),
                DateMillis(3.0)
            )
            .unwrap()
        );
        assert!(
            !log.record(
                &record,
                ActivityKind::TaskTransition,
                source(1, "acknowledged"),
                DateMillis(1.0)
            )
            .unwrap()
        );
        assert!(
            log.record(
                &record,
                ActivityKind::TaskTransition,
                source(3, "acknowledged"),
                DateMillis(4.0)
            )
            .unwrap()
        );
        assert!(
            !log.record(
                &record,
                ActivityKind::TaskTransition,
                source(10, "acknowledged"),
                DateMillis(5.0)
            )
            .unwrap()
        );
        assert!(
            !log.record(
                &record,
                ActivityKind::TaskTransition,
                source(8, "blocked"),
                DateMillis(5.5)
            )
            .unwrap()
        );
        assert!(
            log.record(
                &record,
                ActivityKind::TaskTransition,
                source(11, "blocked"),
                DateMillis(6.0)
            )
            .unwrap()
        );
        let mut log = ActivityLog::load(path).unwrap();
        assert_eq!(
            log.recent(10)
                .iter()
                .map(|entry| {
                    let source = entry.source.as_ref().unwrap();
                    (source.revision, source.state.as_deref().unwrap())
                })
                .collect::<Vec<_>>(),
            [(11, "blocked"), (10, "acknowledged"), (2, "blocked")],
        );
        assert!(
            !log.record(
                &record,
                ActivityKind::TaskTransition,
                source(11, "blocked"),
                DateMillis(7.0)
            )
            .unwrap()
        );
    }
}
