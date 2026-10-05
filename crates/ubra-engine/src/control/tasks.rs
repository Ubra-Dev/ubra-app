//! Task receipts require explicit acknowledgements. No screen/idle heuristic
//! can complete a task. The journal stays in the local Engine, outside Holders.
use super::message_delivery::{digest, open};
use super::operations::{identity, storage_error};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;
use ubra_proto::tasks::{
    CurrentTaskParams, MAX_TASK_TITLE_BYTES, MAX_TASK_UPDATES, SessionTasksParams,
    SessionTasksResult, TaskAnswerParams, TaskAnswerResult, TaskCancelParams, TaskGetParams,
    TaskListParams, TaskRecord, TaskReportParams, TaskStatus, TaskSubmitParams, TaskUpdate,
    TaskUpdatedEvent,
};
use ubra_proto::{ControlError, DeliverMessageParams, SessionId};

fn database(path: &Path) -> Result<Connection, ControlError> {
    let db = open(path)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks_v1 (
        id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, record TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS tasks_sender_v1 ON tasks_v1(json_extract(record, '$.sender_id'));
    CREATE INDEX IF NOT EXISTS tasks_assignee_v1 ON tasks_v1(json_extract(record, '$.session_id'));
    CREATE TABLE IF NOT EXISTS current_tasks_v1 (
        session_id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks_v1(id)
    );",
    )
    .map_err(storage_error)?;
    Ok(db)
}
fn load(db: &Connection, id: &str) -> Result<TaskRecord, ControlError> {
    let raw: Option<String> = db
        .query_row("SELECT record FROM tasks_v1 WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(storage_error)?;
    serde_json::from_str(&raw.ok_or_else(|| ControlError::not_found("task"))?)
        .map_err(|_| ControlError::internal("invalid stored task receipt"))
}
fn save(db: &Connection, record: &TaskRecord) -> Result<(), ControlError> {
    let raw =
        serde_json::to_string(record).map_err(|_| ControlError::internal("cannot encode task"))?;
    db.execute(
        "UPDATE tasks_v1 SET record=?1 WHERE id=?2",
        params![raw, record.task_id],
    )
    .map_err(storage_error)?;
    if record.status.is_terminal() {
        db.execute(
            "DELETE FROM current_tasks_v1 WHERE task_id=?1",
            [&record.task_id],
        )
        .map_err(storage_error)?;
    }
    Ok(())
}
fn reserve(path: &Path, p: &TaskSubmitParams) -> Result<(TaskRecord, bool), ControlError> {
    for field in [&p.caller_id, &p.request_id, &p.session_id] {
        identity(field)?;
    }
    if p.text.is_empty() || p.text.len() > 1_048_576 {
        return Err(ControlError::bad_request(
            "task text must contain 1–1048576 bytes",
        ));
    }
    if p.title.as_ref().is_some_and(|title| {
        title.trim().is_empty()
            || title.len() > MAX_TASK_TITLE_BYTES
            || title.chars().any(char::is_control)
    }) {
        return Err(ControlError::bad_request(
            "task title must contain 1–256 bytes without control characters",
        ));
    }
    let id = format!("task_{}", digest(&json!([p.caller_id, p.request_id])));
    // Preserve existing identities when no additive display metadata is supplied.
    let fingerprint = match (&p.result_schema, &p.title) {
        (None, None) => digest(&json!([p.session_id, p.text])),
        (Some(schema), None) => digest(&json!([p.session_id, p.text, schema])),
        (schema, Some(title)) => digest(&json!([p.session_id, p.text, schema, title])),
    };
    let mut db = database(path)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let prior: Option<String> = tx
        .query_row(
            "SELECT fingerprint FROM tasks_v1 WHERE id=?1",
            [&id],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    if let Some(prior) = prior {
        if prior != fingerprint {
            return Err(ControlError::new(
                "task_id_conflict",
                "request_id identifies a different task; nothing was sent",
            ));
        }
        return Ok((load(&tx, &id)?, false));
    }
    let count: i64 = tx
        .query_row("SELECT COUNT(*) FROM tasks_v1", [], |row| row.get(0))
        .map_err(storage_error)?;
    if count >= 100_000 {
        return Err(ControlError::new(
            "task_storage_full",
            "task storage is full; nothing was sent",
        ));
    }
    let now = now_ms();
    let record = TaskRecord {
        task_id: id,
        sender_id: p.caller_id.clone(),
        session_id: p.session_id.clone(),
        delivery: "unknown".into(),
        status: TaskStatus::AwaitingAcknowledgement,
        result: None,
        revision: 0,
        updates: Vec::new(),
        result_schema: p.result_schema.clone(),
        title: p.title.clone(),
        created_at_ms: Some(now),
        updated_at_ms: Some(now),
        completed_at_ms: None,
    };
    tx.execute(
        "INSERT INTO tasks_v1 VALUES (?1, ?2, ?3)",
        params![
            record.task_id,
            fingerprint,
            serde_json::to_string(&record).unwrap()
        ],
    )
    .map_err(storage_error)?;
    tx.commit().map_err(storage_error)?;
    Ok((record, true))
}
fn get(path: &Path, p: &TaskGetParams) -> Result<TaskRecord, ControlError> {
    let id = match (&p.task_id, &p.request_id) {
        (Some(id), None) => {
            identity(id)?;
            id.clone()
        }
        (None, Some(request)) => {
            identity(request)?;
            format!("task_{}", digest(&json!([p.caller_id, request])))
        }
        _ => {
            return Err(ControlError::bad_request(
                "provide exactly one of task_id or request_id",
            ));
        }
    };
    let record = load(&database(path)?, &id)?;
    if p.caller_id != record.sender_id && p.caller_id != record.session_id {
        return Err(ControlError::new(
            "forbidden",
            "only task participants may inspect this task",
        ));
    }
    Ok(record)
}
fn report(path: &Path, p: &TaskReportParams) -> Result<TaskRecord, ControlError> {
    if p.result.as_ref().is_some_and(|r| r.len() > 16_384) {
        return Err(ControlError::bad_request("task result exceeds 16384 bytes"));
    }
    let mut db = database(path)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let mut record = load(&tx, &p.task_id)?;
    if p.caller_id != record.session_id {
        return Err(ControlError::new(
            "forbidden",
            "only the assigned Agent may acknowledge or finish this task",
        ));
    }
    if p.status == TaskStatus::AwaitingAcknowledgement {
        return Err(ControlError::bad_request(
            "a task cannot return to unacknowledged",
        ));
    }
    if record.status == p.status && record.result == p.result {
        return Ok(record);
    }
    if record.status.is_terminal() {
        return Err(ControlError::new(
            "task_terminal",
            "a terminal task result is immutable",
        ));
    }
    if record.status == TaskStatus::AwaitingAcknowledgement && p.status != TaskStatus::Acknowledged
    {
        return Err(ControlError::new(
            "task_not_acknowledged",
            "acknowledge this task before reporting progress or completion",
        ));
    }
    if p.status.is_terminal()
        && p.result
            .as_deref()
            .is_none_or(|result| result.trim().is_empty())
    {
        return Err(ControlError::bad_request(
            "a terminal task report requires result evidence",
        ));
    }
    if p.status == TaskStatus::Completed {
        validate_completed_result(&record, p.result.as_deref().unwrap_or_default())?;
    }
    let kind = if record.status == p.status {
        "progress"
    } else {
        "status"
    };
    record.status = p.status.clone();
    record.result = p.result.clone();
    record.revision += 1;
    record.updated_at_ms = Some(now_ms());
    if record.status.is_terminal() {
        record.completed_at_ms = record.updated_at_ms;
    }
    push_update(&mut record, kind, &p.caller_id, p.result.clone());
    save(&tx, &record)?;
    tx.commit().map_err(storage_error)?;
    Ok(record)
}

/// The Engine, not an MCP preflight, owns completed-result schema enforcement.
fn validate_completed_result(record: &TaskRecord, result: &str) -> Result<(), ControlError> {
    let Some(schema) = &record.result_schema else {
        return Ok(());
    };
    let value: Value = serde_json::from_str(result).map_err(|_| {
        ControlError::bad_request("this task has a result_schema: result must be matching JSON")
    })?;
    validate_result_value(&value, schema, "result").map_err(ControlError::bad_request)
}

/// Same supported schema vocabulary as MCP: type/enum/required/properties/items
/// and string/numeric bounds. No external schema fetches or additional semantics.
fn validate_result_value(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let expected = schema["type"].as_str().unwrap_or("any");
    let valid = match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.as_f64().is_some_and(f64::is_finite),
        "integer" => value.is_i64() || value.is_u64(),
        "null" => value.is_null(),
        _ => true,
    };
    if !valid {
        return Err(format!("{path} must be {expected}"));
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(value)
    {
        return Err(format!("{path} is not a supported value"));
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!("missing required argument: {key}"));
                }
            }
        }
        for (key, field) in object {
            if let Some(field_schema) = schema["properties"].get(key) {
                validate_result_value(field, field_schema, &format!("{path}.{key}"))?;
            } else if schema["additionalProperties"] == false {
                return Err(format!("unsupported argument: {key}"));
            }
        }
    }
    if let Some(entries) = value.as_array() {
        for (index, entry) in entries.iter().enumerate() {
            validate_result_value(entry, &schema["items"], &format!("{path}[{index}]"))?;
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if schema["minLength"].as_u64().is_some_and(|min| length < min)
            || schema["maxLength"].as_u64().is_some_and(|max| length > max)
        {
            return Err(format!("{path} has an invalid length"));
        }
    }
    if let Some(number) = value.as_f64()
        && (schema["minimum"].as_f64().is_some_and(|min| number < min)
            || schema["maximum"].as_f64().is_some_and(|max| number > max))
    {
        return Err(format!("{path} is outside the supported range"));
    }
    Ok(())
}

fn push_update(record: &mut TaskRecord, kind: &str, by: &str, text: Option<String>) {
    record.updates.push(TaskUpdate {
        kind: kind.into(),
        by: by.into(),
        status: Some(record.status.clone()),
        text,
        revision: record.revision,
    });
    let excess = record.updates.len().saturating_sub(MAX_TASK_UPDATES);
    record.updates.drain(..excess);
}

/// Sender-side mutation shared by answer and cancel: only the task's sender
/// may act, and a terminal task never changes again.
fn sender_update(
    path: &Path,
    caller: &str,
    task_id: &str,
    apply: impl FnOnce(&mut TaskRecord),
) -> Result<TaskRecord, ControlError> {
    identity(task_id)?;
    let mut db = database(path)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let mut record = load(&tx, task_id)?;
    if caller != record.sender_id {
        return Err(ControlError::new(
            "forbidden",
            "only the session that submitted this task may answer or cancel it",
        ));
    }
    if record.status.is_terminal() {
        return Err(ControlError::new(
            "task_terminal",
            "a terminal task result is immutable",
        ));
    }
    record.revision += 1;
    record.updated_at_ms = Some(now_ms());
    apply(&mut record);
    if record.status.is_terminal() {
        record.completed_at_ms = record.updated_at_ms;
    }
    save(&tx, &record)?;
    tx.commit().map_err(storage_error)?;
    Ok(record)
}

fn answer(path: &Path, p: &TaskAnswerParams) -> Result<TaskRecord, ControlError> {
    if p.text.trim().is_empty() || p.text.len() > 65_536 {
        return Err(ControlError::bad_request(
            "a task answer must contain 1–65536 bytes",
        ));
    }
    sender_update(path, &p.caller_id, &p.task_id, |record| {
        // Answering a blocker resumes the work; the Agent still owns the result.
        if record.status == TaskStatus::Blocked {
            record.status = TaskStatus::Acknowledged;
        }
        push_update(record, "answer", &p.caller_id, Some(p.text.clone()));
    })
}

fn cancel(path: &Path, p: &TaskCancelParams) -> Result<TaskRecord, ControlError> {
    if p.reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 16_384)
    {
        return Err(ControlError::bad_request(
            "cancel reason exceeds 16384 bytes",
        ));
    }
    sender_update(path, &p.caller_id, &p.task_id, |record| {
        record.status = TaskStatus::Cancelled;
        record.result = p.reason.clone();
        push_update(record, "cancel", &p.caller_id, p.reason.clone());
    })
}

/// The newest tasks a caller sent or received. Receipts are small; the table
/// is capped, so a bounded newest-first scan stays cheap.
fn list(path: &Path, p: &TaskListParams) -> Result<Vec<TaskRecord>, ControlError> {
    identity(&p.caller_id)?;
    let (sent, assigned) = match p.role.as_deref() {
        None | Some("all") => (true, true),
        Some("sent") => (true, false),
        Some("assigned") => (false, true),
        Some(_) => {
            return Err(ControlError::bad_request(
                "role must be sent, assigned, or all",
            ));
        }
    };
    let limit = p.limit.unwrap_or(50).clamp(1, 200) as usize;
    let db = database(path)?;
    let mut statement = db
        .prepare("SELECT record FROM tasks_v1 ORDER BY rowid DESC LIMIT 5000")
        .map_err(storage_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(storage_error)?;
    let mut tasks = Vec::new();
    for raw in rows {
        let Ok(record) = serde_json::from_str::<TaskRecord>(&raw.map_err(storage_error)?) else {
            continue;
        };
        let mine = (sent && record.sender_id == p.caller_id)
            || (assigned && record.session_id == p.caller_id);
        if mine && (p.include_terminal || !record.status.is_terminal()) {
            tasks.push(record);
            if tasks.len() == limit {
                break;
            }
        }
    }
    Ok(tasks)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn current_task(db: &Connection, session_id: &str) -> Result<Option<TaskRecord>, ControlError> {
    let id: Option<String> = db
        .query_row(
            "SELECT task_id FROM current_tasks_v1 WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    id.map(|id| load(db, &id)).transpose()
}

/// Participant filtering precedes paging. Unrelated newer tasks cannot hide older work.
fn session_tasks(path: &Path, p: &SessionTasksParams) -> Result<SessionTasksResult, ControlError> {
    identity(&p.session_id)?;
    if p.cursor.is_some_and(|cursor| cursor <= 0) {
        return Err(ControlError::bad_request("task cursor must be positive"));
    }
    let db = database(path)?;
    // Selection and page are one consistent read snapshot.
    let tx = db.unchecked_transaction().map_err(storage_error)?;
    let current = current_task(&tx, &p.session_id)?;
    let limit = p.limit.unwrap_or(50).clamp(1, 200) as usize;
    // Leave envelope headroom and include the independently selected receipt in
    // the same control-line bound. Audit trails can be much larger than labels.
    let current_bytes = serde_json::to_vec(&current)
        .map_err(|_| ControlError::internal("cannot encode current task"))?
        .len();
    if current_bytes.saturating_add(4096) > ubra_proto::control::MAX_CONTROL_LINE_BYTES {
        return Err(ControlError::new(
            "task_response_too_large",
            "current task receipt exceeds the control response limit",
        ));
    }
    let byte_limit =
        ubra_proto::control::MAX_CONTROL_LINE_BYTES.saturating_sub(current_bytes + 4096);
    let mut page_bytes = 0usize;
    let mut statement = tx
        .prepare(
            "SELECT rowid, record FROM tasks_v1
         WHERE (json_extract(record, '$.sender_id')=?1 OR json_extract(record, '$.session_id')=?1)
           AND rowid < ?2 AND (?4 IS NULL OR id != ?4)
         ORDER BY rowid DESC LIMIT ?3",
        )
        .map_err(storage_error)?;
    let rows = statement
        .query_map(
            params![
                p.session_id,
                p.cursor.unwrap_or(i64::MAX),
                limit as i64 + 1,
                current.as_ref().map(|record| record.task_id.as_str())
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(storage_error)?;
    let mut tasks = Vec::new();
    let mut last_row = None;
    let mut next_cursor = None;
    for row in rows {
        let (row_id, raw) = row.map_err(storage_error)?;
        if tasks.len() == limit || page_bytes.saturating_add(raw.len() + 1) > byte_limit {
            if tasks.is_empty() {
                return Err(ControlError::new(
                    "task_response_too_large",
                    "task receipt and current selection exceed the control response limit",
                ));
            }
            next_cursor = last_row;
            break;
        }
        tasks.push(
            serde_json::from_str(&raw)
                .map_err(|_| ControlError::internal("invalid stored task receipt"))?,
        );
        last_row = Some(row_id);
        page_bytes += raw.len() + 1;
    }
    Ok(SessionTasksResult {
        session_id: p.session_id.clone(),
        tasks,
        current_task_id: current.as_ref().map(|record| record.task_id.clone()),
        current_task: current,
        next_cursor,
    })
}

fn set_current_task(path: &Path, p: &CurrentTaskParams) -> Result<(), ControlError> {
    identity(&p.session_id)?;
    let mut db = database(path)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    if let Some(task_id) = &p.task_id {
        identity(task_id)?;
        let record = load(&tx, task_id)?;
        if record.session_id != p.session_id {
            return Err(ControlError::new(
                "forbidden",
                "current task must be assigned to this session",
            ));
        }
        if !matches!(
            record.status,
            TaskStatus::Acknowledged | TaskStatus::Blocked
        ) {
            return Err(ControlError::new(
                "task_not_active",
                "current task must be acknowledged or blocked",
            ));
        }
        tx.execute(
            "INSERT INTO current_tasks_v1(session_id, task_id) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET task_id=excluded.task_id",
            params![p.session_id, task_id],
        )
        .map_err(storage_error)?;
    } else {
        tx.execute(
            "DELETE FROM current_tasks_v1 WHERE session_id=?1",
            [&p.session_id],
        )
        .map_err(storage_error)?;
    }
    tx.commit().map_err(storage_error)?;
    Ok(())
}

fn publish_task_update(events: &crate::events::EventBus, record: &TaskRecord) {
    let params = serde_json::to_value(TaskUpdatedEvent {
        task_id: record.task_id.clone(),
        revision: record.revision,
        sender_id: record.sender_id.clone(),
        session_id: record.session_id.clone(),
    })
    .expect("task event serializes");
    events.publish("task.updated", params.clone(), Some(&record.session_id));
    if record.sender_id != record.session_id {
        events.publish("task.updated", params, Some(&record.sender_id));
    }
}

impl super::ControlServer {
    fn tasks_path(&self) -> std::path::PathBuf {
        self.socket_path.with_file_name("tasks-v1.sqlite")
    }
    fn require_task_session(&self, session_id: &str) -> Result<(), ControlError> {
        identity(session_id)?;
        if self
            .registry
            .lock()
            .map_err(super::poisoned)?
            .record(session_id)
            .is_none()
        {
            return Err(ControlError::not_found("task session"));
        }
        Ok(())
    }

    /// Local desktop query under the private Engine socket's owner authentication.
    pub(super) fn session_tasks(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: SessionTasksParams = super::decode(params)?;
        self.require_task_session(&p.session_id)?;
        super::encode(&session_tasks(&self.tasks_path(), &p)?)
    }

    pub(super) fn session_set_current_task(
        &self,
        params: Option<Value>,
    ) -> Result<Value, ControlError> {
        let p: CurrentTaskParams = super::decode(params)?;
        self.require_task_session(&p.session_id)?;
        set_current_task(&self.tasks_path(), &p)?;
        self.events.publish(
            "task.current_changed",
            json!({"session_id": p.session_id, "current_task_id": p.task_id}),
            Some(&p.session_id),
        );
        super::encode(&session_tasks(
            &self.tasks_path(),
            &SessionTasksParams {
                session_id: p.session_id,
                limit: None,
                cursor: None,
            },
        )?)
    }
    pub(super) fn task_submit(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskSubmitParams = super::decode(params)?;
        // Check existence before reservation, but never infer task state from it.
        if self
            .registry
            .lock()
            .map_err(super::poisoned)?
            .get(&p.session_id)
            .is_none()
        {
            return Err(ControlError::not_found("task target"));
        }
        let path = self.tasks_path();
        let (mut record, fresh) = reserve(&path, &p)?;
        if fresh {
            let message = DeliverMessageParams {
                session_id: SessionId::new(&p.session_id),
                sender_id: p.caller_id,
                message_id: record.task_id.clone(),
                submit: true,
                text: format!(
                    "[Ubra task {} from session {}]\nBefore starting, call report_task with task_id=\"{}\" and status=\"acknowledged\". After verifying this task, call report_task with the same task_id, status=\"completed\" (or \"failed\"), and result describing the outcome and evidence. Use status=\"blocked\" for a blocker. Session idle does not complete this task.\n\n{}",
                    record.task_id, record.sender_id, record.task_id, p.text
                ),
            };
            let receipt =
                self.session_deliver_message(Some(serde_json::to_value(message).unwrap()));
            // A target may acknowledge while delivery is returning. Never write
            // a stale status over that acknowledgement.
            let mut db = database(&path)?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            record = load(&tx, &record.task_id)?;
            record.delivery = receipt
                .ok()
                .and_then(|r| r["delivery"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into());
            record.revision += 1;
            record.updated_at_ms = Some(now_ms());
            save(&tx, &record)?;
            tx.commit().map_err(storage_error)?;
            self.publish_task(&record);
        }
        Ok(json!({"ok": record.delivery == "sent", "duplicate":!fresh, "task":record}))
    }
    pub(super) fn task_get(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskGetParams = super::decode(params)?;
        super::encode(&get(&self.tasks_path(), &p)?)
    }
    pub(super) fn task_report(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskReportParams = super::decode(params)?;
        let record = report(&self.tasks_path(), &p)?;
        self.publish_task(&record);
        super::encode(&record)
    }
    pub(super) fn task_answer(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskAnswerParams = super::decode(params)?;
        let record = answer(&self.tasks_path(), &p)?;
        self.publish_task(&record);
        let receipt = self.notify_task_agent(
            &record,
            "answer",
            &format!(
                "[Ubra task {} — answer from session {}]\n{}\n\nContinue the task, then report_task with this task_id.",
                record.task_id, record.sender_id, p.text
            ),
        );
        super::encode(&TaskAnswerResult {
            ok: receipt == "sent",
            delivery: receipt,
            task: record,
        })
    }
    pub(super) fn task_cancel(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskCancelParams = super::decode(params)?;
        let record = cancel(&self.tasks_path(), &p)?;
        self.publish_task(&record);
        let reason = p
            .reason
            .as_deref()
            .map(|reason| format!("\nReason: {reason}"))
            .unwrap_or_default();
        let receipt = self.notify_task_agent(
            &record,
            "cancel",
            &format!(
                "[Ubra task {} cancelled by session {}]{reason}\nStop working on this task. Do not report it again.",
                record.task_id, record.sender_id
            ),
        );
        Ok(json!({"ok": true, "delivery": receipt, "task": record}))
    }
    pub(super) fn task_list(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: TaskListParams = super::decode(params)?;
        Ok(json!({"tasks": list(&self.tasks_path(), &p)?}))
    }
    fn publish_task(&self, record: &TaskRecord) {
        publish_task_update(&self.events, record);
        let participants = [record.session_id.as_str(), record.sender_id.as_str()];
        let state = serde_json::to_value(&record.status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned));
        for (index, participant) in participants.into_iter().enumerate() {
            if index == 1 && record.sender_id == record.session_id {
                continue;
            }
            let session = self
                .registry
                .lock()
                .ok()
                .and_then(|registry| registry.record(participant));
            if let Some(session) = session {
                self.events.record_activity(
                    &session,
                    ubra_proto::ActivityKind::TaskTransition,
                    ubra_proto::ActivitySource {
                        producer: ubra_proto::ActivityProducer::Task,
                        id: record.task_id.clone(),
                        revision: record.revision,
                        state: state.clone(),
                    },
                    ubra_proto::DateMillis(record.updated_at_ms.unwrap_or_else(now_ms) as f64),
                );
            }
        }
    }
    /// Best-effort, at-most-once notice to the assigned Agent. The task
    /// receipt is already durable; delivery is reported, never retried here.
    fn notify_task_agent(&self, record: &TaskRecord, kind: &str, text: &str) -> String {
        let message = DeliverMessageParams {
            session_id: SessionId::new(&record.session_id),
            sender_id: record.sender_id.clone(),
            message_id: format!("{}:{kind}:{}", record.task_id, record.revision),
            submit: true,
            text: text.to_owned(),
        };
        self.session_deliver_message(Some(serde_json::to_value(message).unwrap()))
            .ok()
            .and_then(|receipt| receipt["delivery"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn submission() -> TaskSubmitParams {
        TaskSubmitParams {
            caller_id: "parent".into(),
            request_id: "work-1".into(),
            session_id: "child".into(),
            text: "private task".into(),
            result_schema: None,
            title: None,
        }
    }
    #[test]
    fn completion_requires_the_exact_task_acknowledgement_and_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (task, fresh) = reserve(&path, &submission()).unwrap();
        assert!(fresh);
        let mut p = TaskReportParams {
            caller_id: "child".into(),
            task_id: task.task_id.clone(),
            status: TaskStatus::Completed,
            result: Some("tested".into()),
        };
        assert_eq!(report(&path, &p).unwrap_err().code, "task_not_acknowledged");
        p.status = TaskStatus::Acknowledged;
        p.result = None;
        report(&path, &p).unwrap();
        p.status = TaskStatus::Completed;
        p.result = Some("tested".into());
        let finished = report(&path, &p).unwrap();
        assert_eq!(report(&path, &p).unwrap(), finished);
        assert_eq!(reserve(&path, &submission()).unwrap(), (finished, false));
        p.result = Some("different".into());
        assert!(report(&path, &p).is_err());
        p.caller_id = "stranger".into();
        assert_eq!(report(&path, &p).unwrap_err().code, "forbidden");
        assert!(
            get(
                &path,
                &TaskGetParams {
                    caller_id: "stranger".into(),
                    task_id: Some(task.task_id),
                    request_id: None,
                }
            )
            .is_err()
        );
        assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains("private task"));
    }

    #[test]
    fn senders_answer_blockers_and_cancel_while_agents_keep_the_result() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (task, _) = reserve(&path, &submission()).unwrap();
        let mut p = TaskReportParams {
            caller_id: "child".into(),
            task_id: task.task_id.clone(),
            status: TaskStatus::Acknowledged,
            result: None,
        };
        report(&path, &p).unwrap();
        p.status = TaskStatus::Blocked;
        p.result = Some("which database?".into());
        report(&path, &p).unwrap();
        let answer_params = |caller: &str| TaskAnswerParams {
            caller_id: caller.into(),
            task_id: task.task_id.clone(),
            text: "sqlite".into(),
        };
        assert_eq!(
            answer(&path, &answer_params("child")).unwrap_err().code,
            "forbidden"
        );
        let answered = answer(&path, &answer_params("parent")).unwrap();
        assert_eq!(answered.status, TaskStatus::Acknowledged);
        assert_eq!(answered.updates.last().unwrap().kind, "answer");

        let listed = list(
            &path,
            &TaskListParams {
                caller_id: "parent".into(),
                role: Some("sent".into()),
                include_terminal: false,
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(listed.len(), 1);

        let cancelled = cancel(
            &path,
            &TaskCancelParams {
                caller_id: "parent".into(),
                task_id: task.task_id.clone(),
                reason: Some("superseded".into()),
            },
        )
        .unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        p.status = TaskStatus::Completed;
        p.result = Some("done anyway".into());
        assert_eq!(report(&path, &p).unwrap_err().code, "task_terminal");
        assert!(
            list(
                &path,
                &TaskListParams {
                    caller_id: "child".into(),
                    role: Some("assigned".into()),
                    include_terminal: false,
                    limit: None,
                },
            )
            .unwrap()
            .is_empty()
        );
    }

    fn snapshot(path: &Path, session: &str) -> SessionTasksResult {
        session_tasks(
            path,
            &SessionTasksParams {
                session_id: session.into(),
                limit: None,
                cursor: None,
            },
        )
        .unwrap()
    }

    fn acknowledge(path: &Path, task: &TaskRecord) -> TaskRecord {
        report(
            path,
            &TaskReportParams {
                caller_id: task.session_id.clone(),
                task_id: task.task_id.clone(),
                status: TaskStatus::Acknowledged,
                result: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn task_session_queries_filter_before_paging_and_do_not_choose_current() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (first, _) = reserve(&path, &submission()).unwrap();
        let mut p = submission();
        p.request_id = "work-2".into();
        let (second, _) = reserve(&path, &p).unwrap();
        // New unrelated tasks must not consume this session's query budget.
        p.caller_id = "other-parent".into();
        p.session_id = "other-child".into();
        for n in 0..6 {
            p.request_id = format!("unrelated-{n}");
            reserve(&path, &p).unwrap();
        }
        let page = session_tasks(
            &path,
            &SessionTasksParams {
                session_id: "child".into(),
                limit: Some(1),
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(page.tasks, vec![second.clone()]);
        assert_eq!(page.current_task_id, None);
        let older = session_tasks(
            &path,
            &SessionTasksParams {
                session_id: "child".into(),
                limit: Some(1),
                cursor: page.next_cursor,
            },
        )
        .unwrap();
        assert_eq!(older.tasks, vec![first]);
        assert_eq!(older.next_cursor, None);
        assert_eq!(snapshot(&path, "parent").tasks.len(), 2);
        assert!(snapshot(&path, "stranger").tasks.is_empty());
        assert!(snapshot(&path, "stranger").current_task_id.is_none());
        assert!(
            session_tasks(
                &path,
                &SessionTasksParams {
                    session_id: "child".into(),
                    limit: None,
                    cursor: Some(0),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn task_current_selection_is_explicit_durable_assignee_validated_and_terminal_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (task, _) = reserve(&path, &submission()).unwrap();
        let mut selection = CurrentTaskParams {
            session_id: "child".into(),
            task_id: Some(task.task_id.clone()),
        };
        assert_eq!(
            set_current_task(&path, &selection).unwrap_err().code,
            "task_not_active"
        );
        let acknowledged = acknowledge(&path, &task);
        assert_eq!(snapshot(&path, "child").current_task_id, None);
        selection.session_id = "parent".into();
        assert_eq!(
            set_current_task(&path, &selection).unwrap_err().code,
            "forbidden"
        );
        selection.session_id = "child".into();
        set_current_task(&path, &selection).unwrap();
        assert_eq!(snapshot(&path, "child").current_task, Some(acknowledged));
        selection.task_id = None;
        set_current_task(&path, &selection).unwrap();
        assert!(snapshot(&path, "child").current_task.is_none());
        selection.task_id = Some(task.task_id.clone());
        set_current_task(&path, &selection).unwrap();
        let blocked = report(
            &path,
            &TaskReportParams {
                caller_id: "child".into(),
                task_id: task.task_id.clone(),
                status: TaskStatus::Blocked,
                result: Some("which port?".into()),
            },
        )
        .unwrap();
        // A newly opened connection observes the explicit choice and blocker.
        assert_eq!(snapshot(&path, "child").current_task, Some(blocked));
        let completed = report(
            &path,
            &TaskReportParams {
                caller_id: "child".into(),
                task_id: task.task_id.clone(),
                status: TaskStatus::Completed,
                result: Some("tested port 8080".into()),
            },
        )
        .unwrap();
        assert!(completed.completed_at_ms.is_some());
        assert!(snapshot(&path, "child").current_task_id.is_none());
        assert_eq!(snapshot(&path, "child").tasks[0], completed);
        assert_eq!(
            set_current_task(&path, &selection).unwrap_err().code,
            "task_not_active"
        );
    }

    #[test]
    fn task_participant_events_are_session_filtered_and_snapshots_recover_missed_updates() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (task, _) = reserve(&path, &submission()).unwrap();
        let events = crate::events::EventBus::default();
        let subscribe = |session: &str| {
            events.subscribe(
                None,
                crate::events::Filter::new(
                    Some(vec![session.into()]),
                    Some(vec!["task.updated".into()]),
                ),
            )
        };
        let sender = subscribe("parent");
        let assignee = subscribe("child");
        let stranger = subscribe("stranger");
        let acknowledged = acknowledge(&path, &task);
        publish_task_update(&events, &acknowledged);
        for stream in [sender, assignee] {
            let event = stream.recv(std::time::Duration::from_millis(10)).unwrap();
            let payload: TaskUpdatedEvent = serde_json::from_value(event.params()).unwrap();
            assert_eq!(payload.sender_id, "parent");
            assert_eq!(payload.session_id, "child");
            assert_eq!(payload.revision, acknowledged.revision);
        }
        assert!(stranger.recv(std::time::Duration::from_millis(1)).is_none());
        // No consumer was listening for the next receipt: authoritative read
        // still recovers its blocker and exact evidence.
        let blocked = report(
            &path,
            &TaskReportParams {
                caller_id: "child".into(),
                task_id: task.task_id.clone(),
                status: TaskStatus::Blocked,
                result: Some("database?".into()),
            },
        )
        .unwrap();
        assert_eq!(snapshot(&path, "parent").tasks, vec![blocked]);
    }

    #[test]
    fn task_display_metadata_is_explicit_bounded_and_private_brief_is_not_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let mut p = submission();
        p.title = Some("Public label".into());
        let (task, _) = reserve(&path, &p).unwrap();
        assert_eq!(task.title.as_deref(), Some("Public label"));
        assert!(task.created_at_ms.is_some());
        assert_eq!(reserve(&path, &p).unwrap(), (task.clone(), false));
        p.title = Some("Different".into());
        assert_eq!(reserve(&path, &p).unwrap_err().code, "task_id_conflict");
        for title in [
            " ".to_owned(),
            "x".repeat(257),
            "new\nline".into(),
            "界".repeat(86),
        ] {
            p.title = Some(title);
            assert_eq!(reserve(&path, &p).unwrap_err().code, "bad_request");
        }
        let mut legacy = serde_json::to_value(task).unwrap();
        for field in ["title", "created_at_ms", "updated_at_ms", "completed_at_ms"] {
            legacy.as_object_mut().unwrap().remove(field);
        }
        let legacy: TaskRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.title, None);
        assert_eq!(legacy.created_at_ms, None);
        assert!(!String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains("private task"));
        let stored: String = database(&path)
            .unwrap()
            .query_row("SELECT record FROM tasks_v1", [], |row| row.get(0))
            .unwrap();
        assert!(!stored.contains("private task"));
        assert!(stored.contains("Public label"));
    }

    #[test]
    fn task_completed_schema_is_enforced_in_engine_and_terminal_receipt_remains_immutable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let mut p = submission();
        p.result_schema = Some(json!({"type":"object","required":["answer"],
            "properties":{"answer":{"type":"integer","minimum":1}},"additionalProperties":false}));
        let (task, _) = reserve(&path, &p).unwrap();
        let acknowledged = acknowledge(&path, &task);
        let mut report_params = TaskReportParams {
            caller_id: "child".into(),
            task_id: task.task_id.clone(),
            status: TaskStatus::Completed,
            result: None,
        };
        for result in [
            "plain text",
            "{}",
            r#"{"answer":"x"}"#,
            r#"{"answer":0}"#,
            r#"{"answer":1,"extra":2}"#,
        ] {
            report_params.result = Some(result.into());
            assert!(report(&path, &report_params).is_err());
            assert_eq!(snapshot(&path, "child").tasks, vec![acknowledged.clone()]);
        }
        report_params.result = Some(r#"{"answer":42}"#.into());
        let completed = report(&path, &report_params).unwrap();
        assert_eq!(report(&path, &report_params).unwrap(), completed);
        assert_eq!(
            answer(
                &path,
                &TaskAnswerParams {
                    caller_id: "parent".into(),
                    task_id: task.task_id.clone(),
                    text: "change".into(),
                }
            )
            .unwrap_err()
            .code,
            "task_terminal"
        );
        assert_eq!(
            cancel(
                &path,
                &TaskCancelParams {
                    caller_id: "parent".into(),
                    task_id: task.task_id,
                    reason: None,
                }
            )
            .unwrap_err()
            .code,
            "task_terminal"
        );
    }

    #[test]
    fn task_self_assignment_emits_only_one_participant_event() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let mut p = submission();
        p.session_id = p.caller_id.clone();
        let (task, _) = reserve(&path, &p).unwrap();
        let events = crate::events::EventBus::default();
        let stream = events.subscribe(None, crate::events::Filter::all());
        publish_task_update(&events, &task);
        assert!(stream.recv(std::time::Duration::from_millis(10)).is_some());
        assert!(stream.recv(std::time::Duration::from_millis(1)).is_none());
    }

    #[test]
    fn task_session_pages_remain_bounded_by_control_line_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let mut p = submission();
        p.result_schema = Some(json!({"description": "x".repeat(600_000)}));
        for n in 0..8 {
            p.request_id = format!("bounded-{n}");
            reserve(&path, &p).unwrap();
        }
        let first = snapshot(&path, "child");
        assert!(first.next_cursor.is_some());
        assert!(
            serde_json::to_vec(&first).unwrap().len() < ubra_proto::control::MAX_CONTROL_LINE_BYTES
        );
        let second = session_tasks(
            &path,
            &SessionTasksParams {
                session_id: "child".into(),
                limit: None,
                cursor: first.next_cursor,
            },
        )
        .unwrap();
        assert_eq!(first.tasks.len() + second.tasks.len(), 8);
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn oversized_current_task_rejects_even_when_other_tasks_are_empty() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.sqlite");
        let (task, _) = reserve(&path, &submission()).unwrap();
        acknowledge(&path, &task);
        set_current_task(
            &path,
            &CurrentTaskParams {
                session_id: "child".into(),
                task_id: Some(task.task_id.clone()),
            },
        )
        .unwrap();
        let update = TaskAnswerParams {
            caller_id: "parent".into(),
            task_id: task.task_id,
            text: "\u{1}".repeat(65_536),
        };
        for _ in 0..12 {
            answer(&path, &update).unwrap();
        }
        let error = session_tasks(
            &path,
            &SessionTasksParams {
                session_id: "child".into(),
                limit: None,
                cursor: None,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "task_response_too_large");
    }
}
