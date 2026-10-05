//! Explicit task acknowledgements are independent of terminal/session status.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    AwaitingAcknowledgement,
    Acknowledged,
    Blocked,
    Completed,
    Failed,
    /// The sender withdrew the task. Terminal, like completion.
    Cancelled,
}
impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// One entry in a task's bounded audit trail: progress notes, blockers,
/// answers from the sender, and the terminal report.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskUpdate {
    /// `progress`, `status`, `answer`, or `cancel`.
    pub kind: String,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub revision: u64,
}

/// Oldest audit entries are dropped past this bound; the latest status and
/// result on the record itself are never dropped.
pub const MAX_TASK_UPDATES: usize = 32;
/// Explicit display metadata is public receipt data, never derived from the brief.
pub const MAX_TASK_TITLE_BYTES: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskRecord {
    pub task_id: String,
    pub sender_id: String,
    pub session_id: String,
    pub delivery: String,
    pub status: TaskStatus,
    pub result: Option<String>,
    pub revision: u64,
    /// Additive: absent on receipts written before the audit trail existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updates: Vec<TaskUpdate>,
    /// Optional JSON Schema the sender expects the completed result to match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_schema: Option<serde_json::Value>,
    /// Explicitly supplied public label. Legacy receipts remain identity-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSubmitParams {
    pub caller_id: String,
    pub request_id: String,
    pub session_id: String,
    pub text: String,
    #[serde(default)]
    pub result_schema: Option<serde_json::Value>,
    /// Persisted verbatim as public display metadata; never inferred from `text`.
    #[serde(default)]
    pub title: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGetParams {
    pub caller_id: String,
    pub task_id: Option<String>,
    pub request_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReportParams {
    pub caller_id: String,
    pub task_id: String,
    pub status: TaskStatus,
    pub result: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAnswerParams {
    pub caller_id: String,
    pub task_id: String,
    pub text: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCancelParams {
    pub caller_id: String,
    pub task_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskListParams {
    pub caller_id: String,
    /// `sent`, `assigned`, or omitted for both.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub include_terminal: bool,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Owner-authenticated desktop query; does not impersonate an Agent caller.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTasksParams {
    pub session_id: String,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Exclusive task journal row cursor from the previous page.
    #[serde(default)]
    pub cursor: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionTasksResult {
    pub session_id: String,
    /// The selected receipt appears only in `current_task`, not twice in this page.
    pub tasks: Vec<TaskRecord>,
    pub current_task_id: Option<String>,
    /// Included independently of pagination, never chosen by recency.
    pub current_task: Option<TaskRecord>,
    pub next_cursor: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentTaskParams {
    pub session_id: String,
    /// None explicitly clears selection. Only assigned acknowledged/blocked tasks qualify.
    pub task_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskAnswerResult {
    pub ok: bool,
    pub delivery: String,
    pub task: TaskRecord,
}

/// One event is routed to each distinct participant's subscription.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskUpdatedEvent {
    pub task_id: String,
    pub revision: u64,
    pub sender_id: String,
    /// Assigned session, distinct from the event's subscription routing session.
    pub session_id: String,
}
