use super::*;
use ubra_proto::tasks::{TaskRecord, TaskStatus};

const MAX_BATCH_TASKS: usize = 16;
const MAX_RESULT_BYTES: usize = 16_384;

impl Bridge {
    pub(super) fn submit_task(&self, args: &Value) -> Result<Value, String> {
        let target = required_string(args, "session_id")?;
        let snapshot = self.snapshot()?;
        McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )?
        .authorize(WriteAction::SendPrompt { target: &target })?;
        let request_id = optional_string(args, "request_id").unwrap_or_else(|| {
            format!(
                "auto:{}",
                Sha256::digest(json!([target, args["text"]]).to_string().as_bytes())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
        });
        let mut params = json!({
            "caller_id":self.require_caller()?,"request_id":request_id,
            "session_id":target,"text":required_string(args,"text")?
        });
        if let Some(schema) = args.get("result_schema") {
            params["result_schema"] = schema.clone();
        }
        if let Some(title) = args.get("title") {
            params["title"] = title.clone();
        }
        let since_ms = now_ms();
        let mut submitted = self.request(Method::TASK_SUBMIT,params,Duration::from_secs(30)).map_err(|error| format!("{error}. Task request identity: {request_id}. Retry only this identity and text; do not submit a new task to recover a lost reply."))?;
        submitted["since_ms"] = json!(since_ms);
        Ok(submitted)
    }
    pub(super) fn submit_tasks(&self, args: &Value) -> Result<Value, String> {
        let tasks = args["tasks"].as_array().cloned().unwrap_or_default();
        if tasks.is_empty() || tasks.len() > MAX_BATCH_TASKS {
            return Err(format!("tasks must contain 1–{MAX_BATCH_TASKS} entries"));
        }
        // Sequential on purpose: each submission is idempotent and quick, and
        // ordered delivery keeps retries of a partially applied batch simple.
        let results: Vec<Value> = tasks
            .iter()
            .map(|task| {
                self.submit_task(task)
                    .unwrap_or_else(|error| json!({"ok": false, "error": error}))
            })
            .collect();
        let task_ids: Vec<&Value> = results
            .iter()
            .filter_map(|result| result.pointer("/task/task_id"))
            .collect();
        Ok(json!({
            "ok": results.iter().all(|result| result["ok"] == true),
            "task_ids": task_ids,
            "results": results,
        }))
    }
    pub(super) fn answer_task(&self, args: &Value) -> Result<Value, String> {
        self.request(
            Method::TASK_ANSWER,
            json!({
                "caller_id":self.require_caller()?,"task_id":required_string(args,"task_id")?,
                "text":required_string(args,"text")?
            }),
            Duration::from_secs(30),
        )
    }
    pub(super) fn cancel_task(&self, args: &Value) -> Result<Value, String> {
        self.request(
            Method::TASK_CANCEL,
            json!({
                "caller_id":self.require_caller()?,"task_id":required_string(args,"task_id")?,
                "reason":optional_string(args,"reason")
            }),
            Duration::from_secs(30),
        )
    }
    pub(super) fn list_tasks(&self, args: &Value) -> Result<Value, String> {
        self.request(
            Method::TASK_LIST,
            json!({
                "caller_id":self.require_caller()?,
                "role":optional_string(args,"role"),
                "include_terminal":optional_bool(args,"include_terminal").unwrap_or(false),
                "limit":optional_number(args,"limit").map(|limit| limit as u32),
            }),
            DEFAULT_TIMEOUT,
        )
    }
    fn task_record(&self, task_id: &str) -> Result<TaskRecord, String> {
        self.request_typed(
            Method::TASK_GET,
            json!({"caller_id":self.require_caller()?,"task_id":task_id}),
            DEFAULT_TIMEOUT,
        )
    }
    /// The newest open task this session was assigned by `parent`, if any.
    pub(super) fn open_task_from(&self, parent: &str) -> Result<Option<String>, String> {
        let listed = self.request(
            Method::TASK_LIST,
            json!({"caller_id":self.require_caller()?,"role":"assigned","include_terminal":false}),
            DEFAULT_TIMEOUT,
        )?;
        Ok(listed["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|task| task["sender_id"] == parent)
            .and_then(|task| task["task_id"].as_str())
            .map(str::to_owned))
    }
    /// Maps a report_to_parent status onto the task state machine, first
    /// acknowledging a task the Agent never acknowledged explicitly.
    pub(super) fn record_report_on_task(
        &self,
        task_id: &str,
        status: &str,
        text: &str,
    ) -> Result<Value, String> {
        let task = self.task_record(task_id)?;
        let target = match status {
            "done" => TaskStatus::Completed,
            "failed" => TaskStatus::Failed,
            "blocked" => TaskStatus::Blocked,
            _ => TaskStatus::Acknowledged,
        };
        if target == TaskStatus::Completed && task.result_schema.is_some() {
            return Err(format!(
                "task {task_id} requires a JSON result matching its result_schema; report it with report_task"
            ));
        }
        if task.status == TaskStatus::AwaitingAcknowledgement && target != TaskStatus::Acknowledged
        {
            self.report_task(&json!({"task_id":task_id,"status":"acknowledged"}))?;
        }
        let status = serde_json::to_value(&target).map_err(|error| error.to_string())?;
        self.report_task(&json!({
            "task_id": task_id,
            "status": status,
            "result": clip(text, MAX_RESULT_BYTES),
        }))
    }
    pub(super) fn get_task(&self, args: &Value) -> Result<Value, String> {
        self.request(
            Method::TASK_GET,
            json!({"caller_id":self.require_caller()?,"task_id":args.get("task_id"),"request_id":args.get("request_id")}),
            DEFAULT_TIMEOUT,
        )
    }
    pub(super) fn report_task(&self, args: &Value) -> Result<Value, String> {
        self.request(
            Method::TASK_REPORT,
            json!({
                "caller_id":self.require_caller()?,"task_id":required_string(args,"task_id")?,
                "status":required_string(args,"status")?,"result":optional_string(args,"result")
            }),
            DEFAULT_TIMEOUT,
        )
    }
    pub(super) fn wait_for_task(&self, args: &Value) -> Result<Value, String> {
        let id = required_string(args, "task_id")?;
        let caller = self.require_caller()?;
        let timeout = Duration::from_secs_f64(optional_number(args, "timeout_s").unwrap_or(600.0));
        let deadline = Instant::now()
            + if timeout.is_zero() {
                DEFAULT_TIMEOUT
            } else {
                timeout
            };
        let refresh = || -> Result<TaskRecord, ControlFailure> {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or(ControlFailure::Timeout)?;
            let mut client = self.connect(remaining.min(DEFAULT_TIMEOUT))?;
            let raw = client.request_until(
                Method::TASK_GET.into(),
                json!({"caller_id":caller,"task_id":id}),
                deadline,
            )?;
            serde_json::from_value(raw)
                .map_err(|_| ControlFailure::Protocol("invalid task receipt".into()))
        };
        let mut task = refresh().map_err(render_failure)?;
        if !task.status.is_terminal() && !timeout.is_zero() {
            let mut client = self
                .connect(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(DEFAULT_TIMEOUT),
                )
                .map_err(render_failure)?;
            let result =
                client.subscribe_observing(json!({"kinds":["task.updated"]}), deadline, |event| {
                    if event.is_none_or(|(name, _, value)| {
                        name != "task.updated" || value["task_id"] == id
                    }) {
                        task = refresh()?;
                    }
                    Ok(!task.status.is_terminal())
                });
            if !matches!(result, Ok(()) | Err(ControlFailure::Timeout)) {
                result.map_err(render_failure)?;
            }
        }
        Ok(
            json!({"completed":task.status==ubra_proto::tasks::TaskStatus::Completed,"timed_out":!task.status.is_terminal(),"task":task}),
        )
    }
}

/// Truncates on a character boundary, marking the cut.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let notice = "\n[… clipped by ubra]";
    let mut end = limit - notice.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{notice}", &text[..end])
}
