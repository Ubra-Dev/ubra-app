//! Read/write seams used by MCP orchestrators: an Agent's final answer from
//! its transcript, and folding a child's committed branch into the parent.
use std::path::{Path, PathBuf};

use serde_json::Value;
use ubra_proto::{
    AgentKind, ControlError, ReadTranscriptParams, ReadTranscriptResult, SessionRecord,
    TranscriptTurnRecord, WorktreeIntegrateParams,
};

use super::{decode, encode, io_control_error, poisoned};

const DEFAULT_TURNS: u32 = 1;
const MAX_TURNS: u32 = 100;

impl super::ControlServer {
    fn record_for(&self, id: &str) -> Result<SessionRecord, ControlError> {
        self.registry
            .lock()
            .map_err(poisoned)?
            .records()
            .into_iter()
            .find(|record| record.id.0 == id)
            .ok_or_else(|| ControlError::not_found(id.to_owned()))
    }

    pub(super) fn session_usage(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: ubra_proto::SessionIdParams = decode(params)?;
        let record = self.record_for(&p.session_id.0)?;
        let mut ledger = self.session_usage_ledger.lock().map_err(poisoned)?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        encode(&usage_for_record(
            &record,
            &mut ledger,
            std::time::SystemTime::now(),
            home.as_deref(),
        ))
    }

    pub(super) fn session_read_transcript(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        let p: ReadTranscriptParams = decode(params)?;
        let record = self.record_for(&p.session_id.0)?;
        let wanted = p.turns.unwrap_or(DEFAULT_TURNS).clamp(1, MAX_TURNS) as usize;
        encode(&match transcript_turns(&record) {
            Ok(mut turns) => {
                let excess = turns.len().saturating_sub(wanted);
                turns.drain(..excess);
                ReadTranscriptResult {
                    available: true,
                    reason: None,
                    turns,
                }
            }
            Err(reason) => ReadTranscriptResult {
                available: false,
                reason: Some(reason),
                turns: Vec::new(),
            },
        })
    }

    pub(super) fn worktree_integrate(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: WorktreeIntegrateParams = decode(params)?;
        let source = self.record_for(&p.source_session_id.0)?;
        let target = self.record_for(&p.target_session_id.0)?;
        if source.host.is_some() || target.host.is_some() {
            return Err(ControlError::bad_request(
                "integration is available for local sessions only",
            ));
        }
        if source.project_id != target.project_id {
            return Err(ControlError::bad_request(
                "source and target sessions belong to different projects",
            ));
        }
        let branch = crate::git::branch(Path::new(&source.cwd))
            .or(source.git_branch.clone())
            .ok_or_else(|| {
                ControlError::bad_request("the source session is not on a named branch")
            })?;
        let result = crate::git::integrate(
            Path::new(&target.cwd),
            Path::new(&source.cwd),
            &branch,
            p.strategy,
            p.message.as_deref(),
        )
        .map_err(io_control_error)?;
        encode(&result)
    }
}

/// Which agent's transcript a record reads: the session's own conversation.
/// Only a shell borrows its foreground agent's — an agent session's own
/// transcript stays readable no matter what holds its reclaimed shell.
fn transcript_kind(record: &SessionRecord) -> &AgentKind {
    if record.kind == AgentKind::SHELL {
        record.effective_kind()
    } else {
        &record.kind
    }
}

/// Only local Claude Code and Codex sessions keep a transcript Ubra can
/// validate. Everything else reports why, so callers fall back to the screen.
fn transcript_turns(record: &SessionRecord) -> Result<Vec<TranscriptTurnRecord>, String> {
    if record.host.is_some() {
        return Err("remote session transcripts are not readable locally".into());
    }
    let kind = transcript_kind(record);
    if !matches!(kind.id(), AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID) {
        return Err(format!(
            "{} sessions have no readable transcript",
            kind.id()
        ));
    }
    let path = record
        .transcript_path
        .as_deref()
        .ok_or("this session has not reported a transcript yet")?;
    let agent_id = record
        .agent_session_id
        .as_deref()
        .ok_or("this session has no provider conversation identity yet")?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is unset")?;
    let snapshot =
        crate::transcript::load(&home, Path::new(path), kind, agent_id, &record.cwd, None)
            .map_err(|error| format!("transcript unavailable: {error}"))?
            .ok_or("transcript unavailable")?;
    Ok(snapshot
        .document
        .turns
        .into_iter()
        .map(|turn| TranscriptTurnRecord {
            role: if turn.role == "You" { "user" } else { "agent" }.into(),
            text: turn.text,
        })
        .collect())
}

fn usage_for_record(
    record: &SessionRecord,
    ledger: &mut ubra_usage::transcripts::TranscriptUsageStore,
    observed: std::time::SystemTime,
    home: Option<&Path>,
) -> ubra_proto::SessionUsageResult {
    use ubra_proto::{SessionUsageAvailability as Availability, SessionUsageScope};
    let mut result = ubra_proto::SessionUsageResult {
        session_id: record.id.clone(), availability: Availability::Unavailable, reason: None,
        provider: None, conversation_id: None, scope: SessionUsageScope::ConversationLifetime,
        scope_note: "Provider conversation lifetime, including history before resume; not this Ubra incarnation or account spend".into(),
        tokens: None, context: None, context_reason: None, pricing: None,
        observed_at: observed.into(), source_updated_at: None, stale: false,
    };
    if record.host.is_some() {
        result.availability = Availability::Unsupported;
        result.reason = Some(
            "Remote transport provides account aggregates, not selected-session accounting".into(),
        );
        return result;
    }
    let kind = transcript_kind(record);
    let provider = match kind.id() {
        AgentKind::CLAUDE_CODE_ID => ubra_usage::transcripts::UsageProvider::Claude,
        AgentKind::CODEX_ID => ubra_usage::transcripts::UsageProvider::Codex,
        _ => {
            result.availability = Availability::Unsupported;
            result.reason =
                Some("This provider has no supported local transcript accounting".into());
            return result;
        }
    };
    result.provider = Some(kind.id().to_owned());
    let Some(conversation) = record.agent_session_id.as_deref() else {
        result.reason = Some("Session has no provider conversation identity yet".into());
        return result;
    };
    let Some(path) = record.transcript_path.as_deref() else {
        result.reason = Some("Session has not reported a transcript yet".into());
        return result;
    };
    let Some(home) = home else {
        result.reason = Some("HOME is unset".into());
        return result;
    };
    let usage =
        crate::transcript::open_validated(home, Path::new(path), kind, conversation, &record.cwd)
            .and_then(|file| {
                ledger.refresh(&format!("{}:{conversation}", kind.id()), provider, &file)
            });
    match usage {
        Ok(usage) => {
            result.availability = Availability::Available;
            result.conversation_id = Some(conversation.to_owned());
            result.tokens = usage.has_reported_usage.then_some(usage.tokens);
            result.context = usage.context;
            result.context_reason = match result.context {
                Some(context) if context.window.is_none() => {
                    Some("Provider did not report a context window".into())
                }
                None if kind.id() == AgentKind::CODEX_ID => Some(
                    "No current last-request context reported, or invalidated by compaction".into(),
                ),
                None => {
                    Some("Provider transcript does not report current context occupancy".into())
                }
                _ => None,
            };
            result.pricing = usage.has_reported_usage.then_some(usage.pricing);
            result.source_updated_at = usage.source_updated_at.map(Into::into);
            result.stale = usage.stale;
            if !usage.has_reported_usage {
                result.reason =
                    Some("Validated conversation has not reported token usage yet".into());
            }
            // A freshly read immutable/idle transcript is current evidence;
            // age alone is not proof the provider has newer unseen usage.
        }
        Err(error) => {
            result.reason = Some(format!("Validated transcript unavailable: {error}"));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ubra_proto::{DateMillis, ProjectId, Resumability, SessionId, SessionStatus, TitleSource};

    fn record(kind: AgentKind, foreground_agent: Option<AgentKind>) -> SessionRecord {
        SessionRecord {
            attention_state: None,
            id: SessionId("s_test".into()),
            kind,
            cwd: "/tmp".into(),
            project_id: ProjectId("p".into()),
            worktree_path: None,
            git_branch: None,
            title: "test".into(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Idle,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::NotResumable,
            capabilities: None,
            parent: None,
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
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
            foreground_agent,
            terminal_cwd: None,
            note_id: None,
            note_workspace: None,
            foreground_ports: None,
            terminal_progress: None,
            scheduled_run: None,
        }
    }

    #[test]
    fn transcripts_follow_identity_except_in_shells() {
        // A shell reads its foreground agent's transcript.
        assert_eq!(
            transcript_kind(&record(AgentKind::SHELL, Some(AgentKind::CODEX))),
            &AgentKind::CODEX
        );
        assert_eq!(
            transcript_kind(&record(AgentKind::SHELL, None)),
            &AgentKind::SHELL
        );
        // An agent session reads its own, no matter what holds its
        // reclaimed login shell.
        assert_eq!(
            transcript_kind(&record(AgentKind::CLAUDE_CODE, Some(AgentKind::new("pi")))),
            &AgentKind::CLAUDE_CODE
        );
        assert_eq!(
            transcript_kind(&record(AgentKind::CODEX, None)),
            &AgentKind::CODEX
        );
    }

    fn usage_fixture(home: &Path, conversation: &str) -> SessionRecord {
        let path = home
            .join(".codex/sessions/2026/07/22")
            .join(format!("{conversation}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines = [
            serde_json::json!({"type":"session_meta","payload":{"id":conversation,"cwd":"/tmp"}}),
            serde_json::json!({"type":"turn_context","payload":{"model":"gpt-5.4"}}),
            serde_json::json!({"timestamp":"2026-07-22T11:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{
                "last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10,"total_tokens":110},
                "total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10},"model_context_window":200_000}}}),
        ];
        std::fs::write(
            &path,
            lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let mut record = record(AgentKind::CODEX, None);
        record.agent_session_id = Some(conversation.into());
        record.transcript_path = Some(path.to_string_lossy().into_owned());
        record
    }

    #[test]
    fn session_usage_is_validated_conversation_scope_not_incarnation_or_account_cost() {
        let home = tempfile::tempdir().unwrap();
        let mut first = usage_fixture(home.path(), "conversation_a");
        let _unrelated = usage_fixture(home.path(), "conversation_b");
        let mut ledger = ubra_usage::transcripts::TranscriptUsageStore::default();
        let observed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(123);
        let usage = usage_for_record(&first, &mut ledger, observed, Some(home.path()));
        assert_eq!(
            usage.availability,
            ubra_proto::SessionUsageAvailability::Available
        );
        assert_eq!(
            usage.scope,
            ubra_proto::SessionUsageScope::ConversationLifetime
        );
        assert_eq!(usage.tokens.unwrap().input, 80);
        assert_eq!(usage.conversation_id.as_deref(), Some("conversation_a"));
        assert_eq!(usage.observed_at, DateMillis(123_000.0));
        assert!(!usage.stale);
        first.id = SessionId("resumed_incarnation".into());
        first.created_at = DateMillis(99_000.0);
        let resumed = usage_for_record(&first, &mut ledger, observed, Some(home.path()));
        assert_eq!(resumed.tokens, usage.tokens);
        assert_eq!(resumed.pricing, usage.pricing);
        assert_eq!(resumed.scope, usage.scope);
        assert_eq!(resumed.session_id.0, "resumed_incarnation");
    }

    #[test]
    fn session_usage_rejects_malformed_or_mismatched_provider_binding() {
        let home = tempfile::tempdir().unwrap();
        let mut session = usage_fixture(home.path(), "conversation");
        let mut ledger = ubra_usage::transcripts::TranscriptUsageStore::default();
        let observed = std::time::UNIX_EPOCH;
        session.agent_session_id = Some("../conversation".into());
        let malformed = usage_for_record(&session, &mut ledger, observed, Some(home.path()));
        assert_eq!(
            malformed.availability,
            ubra_proto::SessionUsageAvailability::Unavailable
        );
        assert!(malformed.tokens.is_none());
        assert!(malformed.pricing.is_none());
        session.agent_session_id = Some("wrong_conversation".into());
        assert!(
            usage_for_record(&session, &mut ledger, observed, Some(home.path()))
                .tokens
                .is_none()
        );
        session.agent_session_id = Some("conversation".into());
        session.cwd = "/different-project".into();
        assert!(
            usage_for_record(&session, &mut ledger, observed, Some(home.path()))
                .tokens
                .is_none()
        );
    }

    #[test]
    fn remote_usage_is_unsupported_without_opening_or_scanning_transcripts() {
        let mut session = record(AgentKind::CODEX, None);
        session.host = Some("remote".into());
        session.transcript_path = Some("/should/not/be/read".into());
        let mut ledger = ubra_usage::transcripts::TranscriptUsageStore::default();
        let usage = usage_for_record(&session, &mut ledger, std::time::UNIX_EPOCH, None);
        assert_eq!(
            usage.availability,
            ubra_proto::SessionUsageAvailability::Unsupported
        );
        assert!(usage.tokens.is_none());
        assert!(usage.pricing.is_none());
        assert!(usage.reason.unwrap().contains("account aggregates"));
    }
}
