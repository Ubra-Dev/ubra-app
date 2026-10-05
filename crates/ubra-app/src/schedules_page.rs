//! Settings > Schedules: agent runs the Engine starts at a set time.
//!
//! The Engine owns every schedule; this page only lists them, creates simple
//! ones (once, daily, weekdays, hourly at a time), and toggles, runs, or
//! deletes them. Agents create richer cron schedules through MCP.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, KeyDownEvent, PathPromptOptions, Render,
    SharedString, Task, Window, div, prelude::*, px,
};
use tokio::runtime::Runtime;
use ubra_client::DaemonClient;
use ubra_proto::schedules::{
    DEFAULT_CATCH_UP_WINDOW_MS, LateReason, ScheduleOutcome, ScheduleRecord, ScheduleRun,
    ScheduleSpec, ScheduleUpdateParams, ScheduleWhen,
};
use ubra_proto::{AgentKind, DateMillis, EventName, SessionId, SessionSpawnParams};
use ubra_ui::{Ink, Radius, SemanticColors, Typo};

use crate::agent_catalog::AgentOption;
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::store::{SessionStore, StoreRuntime};

const ROW_MIN_HEIGHT: f32 = 50.0;
/// Page size for the schedule list (Worktrees `PAGE_ROWS` pattern). Rows vary
/// with wrapped status text, so the list paginates instead of forcing a fixed
/// virtualized height.
const SCHEDULE_PAGE_ROWS: usize = 40;
pub(crate) const NIGHT: gpui::Rgba = gpui::Rgba {
    r: 0.49,
    g: 0.51,
    b: 0.97,
    a: 1.0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repeat {
    Once,
    Daily,
    Weekdays,
    Hourly,
}

impl Repeat {
    const ALL: [Self; 4] = [Self::Once, Self::Daily, Self::Weekdays, Self::Hourly];

    fn label(self) -> &'static str {
        match self {
            Self::Once => "Once",
            Self::Daily => "Every day",
            Self::Weekdays => "Weekdays",
            Self::Hourly => "Every hour",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Prompt,
    Time,
}

struct Draft {
    prompt: QueryEditor,
    time: QueryEditor,
    repeat: Repeat,
    agent: Option<AgentKind>,
    folder: Option<PathBuf>,
    catch_up: bool,
    keep_awake: bool,
    wake_mac: bool,
    field: Field,
    error: Option<String>,
    saving: bool,
}

impl Draft {
    fn new(agent: Option<AgentKind>, folder: Option<PathBuf>) -> Self {
        let mut time = QueryEditor::default();
        time.insert("09:00");
        Self {
            prompt: QueryEditor::default(),
            time,
            repeat: Repeat::Weekdays,
            agent,
            folder,
            catch_up: true,
            keep_awake: false,
            wake_mac: false,
            field: Field::Prompt,
            error: None,
            saving: false,
        }
    }

    fn editor(&mut self) -> &mut QueryEditor {
        match self.field {
            Field::Prompt => &mut self.prompt,
            Field::Time => &mut self.time,
        }
    }
}

pub struct SchedulesPage {
    store: Arc<RwLock<SessionStore>>,
    store_runtime: Arc<StoreRuntime>,
    runtime: Arc<Runtime>,
    focus: FocusHandle,
    schedules: Vec<ScheduleRecord>,
    loaded: bool,
    error: Option<String>,
    draft: Option<Draft>,
    /// Pager for the schedule list (Worktrees `PAGE_ROWS` pattern). Rows
    /// vary with wrapped status text, so the list paginates instead of
    /// forcing a fixed virtualized height. Draft form stays fixed chrome.
    page: usize,
    /// Schedule ids with a request in flight, so their buttons dim.
    busy: Vec<String>,
    login: LoginState,
    wake_helper: LoginState,
    /// The Engine's last failure reaching the wake helper.
    wake_helper_error: Option<String>,
    refresh_task: Option<Task<()>>,
    _events: Task<()>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoginState {
    status: login::Status,
    error: Option<String>,
}

impl SchedulesPage {
    pub fn new(
        store_runtime: Arc<StoreRuntime>,
        runtime: Arc<Runtime>,
        cx: &mut Context<Self>,
    ) -> Self {
        // `DaemonClient::events` fire-and-forgets a Tokio spawn when the
        // client is connected, and this constructor runs on GPUI's thread
        // with no ambient Tokio runtime. Enter ours explicitly: the spawn
        // binds to it and returns immediately, nothing blocks the UI.
        let mut events = {
            let _guard = runtime.enter();
            store_runtime.client().events()
        };
        let events_task = cx.spawn(async move |this, cx| {
            loop {
                match events.recv().await {
                    Ok(event) if event.name == EventName::SCHEDULE_UPDATED => {
                        if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                            return;
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Self {
            store: Arc::clone(&store_runtime.store),
            store_runtime,
            runtime,
            focus: cx.focus_handle(),
            schedules: Vec::new(),
            loaded: false,
            error: None,
            draft: None,
            page: 0,
            busy: Vec::new(),
            login: LoginState {
                status: login::status(),
                error: None,
            },
            wake_helper: LoginState {
                status: login::status_of(login::Service::WakeHelper),
                error: None,
            },
            wake_helper_error: None,
            refresh_task: None,
            _events: events_task,
        }
    }

    #[cfg(test)]
    pub(crate) fn seed_preview(&mut self, now_ms: f64, drafting: bool) {
        self.schedules = preview::records(now_ms);
        self.loaded = true;
        if drafting {
            let mut draft = Draft::new(
                Some(AgentKind::CLAUDE_CODE),
                Some(PathBuf::from("/Users/example/code/app")),
            );
            draft
                .prompt
                .insert("Triage new GitHub issues and label them");
            draft.wake_mac = true;
            self.draft = Some(draft);
        }
    }

    /// Called when the tab is shown.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.login.status = login::status();
        self.wake_helper.status = login::status_of(login::Service::WakeHelper);
        self.refresh(cx);
    }

    fn client(&self) -> Arc<DaemonClient> {
        Arc::clone(self.store_runtime.client())
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let client = self.client();
        let runtime = Arc::clone(&self.runtime);
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    client.schedules().await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(list)) => {
                        this.schedules = list.schedules;
                        this.wake_helper_error = list.wake_helper_error;
                        this.error = None;
                    }
                    Ok(Err(error)) => this.error = Some(error.to_string()),
                    Err(error) => this.error = Some(error.to_string()),
                }
                this.loaded = true;
                cx.notify();
            });
        }));
    }

    /// Runs one schedule request, dimming its row until the Engine answers.
    fn mutate<F, Fut>(&mut self, id: String, request: F, cx: &mut Context<Self>)
    where
        F: FnOnce(Arc<DaemonClient>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), ubra_client::ClientError>> + Send + 'static,
    {
        let client = self.client();
        let runtime = Arc::clone(&self.runtime);
        self.busy.push(id.clone());
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    request(client).await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy.retain(|busy| *busy != id);
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => this.error = Some(error.to_string()),
                    Err(error) => this.error = Some(error.to_string()),
                }
                this.refresh(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn set_enabled(&mut self, record: &ScheduleRecord, enabled: bool, cx: &mut Context<Self>) {
        let mut spec = record.spec.clone();
        spec.enabled = enabled;
        if let ScheduleWhen::Once { at } = spec.when
            && enabled
            && at.0 < now_ms()
        {
            self.error = Some("That one-time run has passed. Create a new schedule.".into());
            cx.notify();
            return;
        }
        let params = ScheduleUpdateParams {
            id: record.id.clone(),
            spec,
        };
        self.mutate(
            record.id.clone(),
            move |client| async move { client.update_schedule(&params).await.map(|_| ()) },
            cx,
        );
    }

    fn run_now(&mut self, id: String, cx: &mut Context<Self>) {
        let request_id = id.clone();
        self.mutate(
            id,
            move |client| async move { client.run_schedule_now(&request_id).await.map(|_| ()) },
            cx,
        );
    }

    fn delete(&mut self, id: String, cx: &mut Context<Self>) {
        let request_id = id.clone();
        self.mutate(
            id,
            move |client| async move { client.delete_schedule(&request_id).await },
            cx,
        );
    }

    fn open_session(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        self.store
            .write()
            .expect("session store lock poisoned")
            .select(id.clone());
        cx.notify();
    }

    fn agent_options(&self) -> Vec<AgentOption> {
        let store = self.store.read().expect("session store lock poisoned");
        let mru = store.preferences().recent_agents.clone();
        crate::agent_catalog::installed_agent_options(store.agent_catalog(None), &mru)
    }

    fn start_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (agent, folder) = {
            let store = self.store.read().expect("session store lock poisoned");
            let agent = store.agent_catalog(None).map(|catalog| {
                crate::agent_catalog::resolved_default_agent(
                    &store.preferences().default_agent,
                    catalog,
                )
            });
            let folder = store
                .selected_session()
                .filter(|session| session.host.is_none())
                .map(|session| PathBuf::from(&session.cwd));
            (agent, folder)
        };
        self.draft = Some(Draft::new(agent, folder));
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if let (Some(draft), Some(path)) = (this.draft.as_mut(), paths.into_iter().next()) {
                    draft.folder = Some(path);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        if draft.saving {
            return;
        }
        let spec = match plan::spec(
            draft.prompt.text(),
            draft.time.text(),
            draft.repeat,
            draft.agent.clone(),
            draft.folder.as_ref(),
            draft.catch_up,
            draft.keep_awake,
            draft.wake_mac,
            now_ms(),
        ) {
            Ok(spec) => spec,
            Err(error) => {
                draft.error = Some(error);
                cx.notify();
                return;
            }
        };
        draft.saving = true;
        draft.error = None;
        let client = self.client();
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    client.create_schedule(&spec).await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(_)) => this.draft = None,
                    Ok(Err(error)) => {
                        if let Some(draft) = this.draft.as_mut() {
                            draft.saving = false;
                            draft.error = Some(error.to_string());
                        }
                    }
                    Err(error) => {
                        if let Some(draft) = this.draft.as_mut() {
                            draft.saving = false;
                            draft.error = Some(error.to_string());
                        }
                    }
                }
                this.refresh(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Registers the wake helper. macOS then asks an administrator to allow
    /// it in System Settings > Login Items, which this opens.
    fn toggle_wake_helper(&mut self, cx: &mut Context<Self>) {
        let enable = self.wake_helper.status != login::Status::Enabled;
        match login::set_enabled_of(login::Service::WakeHelper, enable) {
            Ok(status) => {
                self.wake_helper = LoginState {
                    status,
                    error: None,
                };
                if status == login::Status::RequiresApproval {
                    login::open_settings();
                }
            }
            Err(error) => self.wake_helper.error = Some(error),
        }
        cx.notify();
    }

    fn toggle_login(&mut self, cx: &mut Context<Self>) {
        let enable = self.login.status != login::Status::Enabled;
        match login::set_enabled(enable) {
            Ok(status) => {
                self.login = LoginState {
                    status,
                    error: None,
                };
                if status == login::Status::RequiresApproval {
                    login::open_settings();
                }
            }
            Err(error) => self.login.error = Some(error),
        }
        cx.notify();
    }

    pub(crate) fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(draft) = self.draft.as_mut() else {
            return false;
        };
        let key = &event.keystroke;
        let handled = match key.key.as_str() {
            "escape" => {
                self.draft = None;
                true
            }
            "enter" => {
                self.submit(cx);
                true
            }
            "tab" => {
                draft.field = match draft.field {
                    Field::Prompt => Field::Time,
                    Field::Time => Field::Prompt,
                };
                true
            }
            _ => match query_editor::edit_for(key) {
                Some(Edit::Local(local)) => {
                    draft.editor().apply(local);
                    draft.error = None;
                    true
                }
                Some(Edit::Clipboard(ClipboardEdit::Copy)) => {
                    query_editor::copy_selection(draft.editor(), cx);
                    true
                }
                Some(Edit::Clipboard(ClipboardEdit::Cut)) => {
                    query_editor::cut_selection(draft.editor(), cx);
                    true
                }
                Some(Edit::Clipboard(ClipboardEdit::Paste)) => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        let text = text.replace(['\r', '\n'], " ");
                        draft.editor().insert(&text);
                    }
                    true
                }
                None => false,
            },
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
        handled
    }

    fn colors(&self) -> SemanticColors {
        crate::app_theme::colors_in(&self.store.read().expect("session store lock poisoned"))
    }
}

/// Hover text for the mark on a session a schedule opened.
pub(crate) fn scheduled_run_summary(run: &ubra_proto::schedules::ScheduledRunInfo) -> String {
    let due = local::describe(run.due_at.0, Some(now_ms()));
    let wake = if run.woke_mac {
        ". ubra woke the Mac for this run and lets it sleep again when the agent finishes"
    } else if run.wake_mac {
        ". The Mac was already awake"
    } else {
        ""
    };
    format!("Started by the schedule “{}”, due {due}{wake}.", run.title)
}

/// `~/code/app` for paths under the home folder.
fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => path
            .strip_prefix(&home)
            .map_or_else(|| path.to_owned(), |rest| format!("~{rest}")),
        _ => path.to_owned(),
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or_default()
}

/// Turning the form into a schedule, and schedules back into words.
pub(crate) mod plan {
    use super::*;

    /// `9`, `9:30`, `09:30`, `21:05` → (hour, minute).
    pub(crate) fn parse_time(text: &str) -> Option<(u32, u32)> {
        let text = text.trim();
        let (hour, minute) = match text.split_once(':') {
            Some((hour, minute)) if minute.len() == 2 => (hour, minute),
            Some(_) => return None,
            None => (text, "0"),
        };
        let hour: u32 = hour.parse().ok()?;
        let minute: u32 = minute.parse().ok()?;
        (hour < 24 && minute < 60).then_some((hour, minute))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spec(
        prompt: &str,
        time: &str,
        repeat: Repeat,
        agent: Option<AgentKind>,
        folder: Option<&PathBuf>,
        catch_up: bool,
        keep_awake: bool,
        wake_mac: bool,
        now_ms: f64,
    ) -> Result<ScheduleSpec, String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("Write what the agent should do.".into());
        }
        let folder = folder.ok_or("Choose a folder for the agent to work in.")?;
        let agent = agent.ok_or("No agent is installed yet.")?;
        let (hour, minute) = parse_time(time).ok_or("Enter a time like 9:00 or 17:30.")?;
        let when = match repeat {
            Repeat::Once => ScheduleWhen::Once {
                at: DateMillis(
                    local::next_at(hour, minute, now_ms).ok_or("That time isn't valid here.")?,
                ),
            },
            Repeat::Daily => ScheduleWhen::Cron {
                expr: format!("{minute} {hour} * * *"),
            },
            Repeat::Weekdays => ScheduleWhen::Cron {
                expr: format!("{minute} {hour} * * 1-5"),
            },
            Repeat::Hourly => ScheduleWhen::Cron {
                expr: format!("{minute} * * * *"),
            },
        };
        let title: String = prompt.chars().take(60).collect();
        Ok(ScheduleSpec {
            title,
            when,
            spawn: SessionSpawnParams {
                appearance: None,
                kind: agent,
                cwd: folder.to_string_lossy().into_owned(),
                new_worktree: None,
                worktree_branch: None,
                worktree_base: None,
                title: None,
                initial_prompt: Some(prompt.to_owned()),
                parent: None,
                initial_cols: None,
                initial_rows: None,
                host: None,
                same_repo_as: None,
                start_directory: None,
                note_id: None,
                note_workspace: None,
            },
            catch_up_window_ms: if catch_up {
                DEFAULT_CATCH_UP_WINDOW_MS
            } else {
                0
            },
            keep_awake,
            wake_mac,
            enabled: true,
        })
    }

    fn clock(hour: &str, minute: &str) -> Option<String> {
        let hour: u32 = hour.parse().ok()?;
        let minute: u32 = minute.parse().ok()?;
        (hour < 24 && minute < 60).then(|| format!("{hour:02}:{minute:02}"))
    }

    /// "Weekdays at 09:00", "Every hour at :15", or the cron itself.
    pub(crate) fn describe_when(when: &ScheduleWhen) -> String {
        match when {
            ScheduleWhen::Once { at } => format!("Once, {}", local::describe(at.0, None)),
            ScheduleWhen::Cron { expr } => {
                let fields: Vec<&str> = expr.split_whitespace().collect();
                match fields.as_slice() {
                    [minute, hour, "*", "*", "*"] if clock(hour, minute).is_some() => {
                        format!("Every day at {}", clock(hour, minute).unwrap())
                    }
                    [minute, hour, "*", "*", "1-5" | "mon-fri"]
                        if clock(hour, minute).is_some() =>
                    {
                        format!("Weekdays at {}", clock(hour, minute).unwrap())
                    }
                    [minute, hour, "*", "*", day]
                        if clock(hour, minute).is_some()
                            && day.parse::<usize>().is_ok_and(|day| day <= 7) =>
                    {
                        const DAYS: [&str; 8] = [
                            "Sundays",
                            "Mondays",
                            "Tuesdays",
                            "Wednesdays",
                            "Thursdays",
                            "Fridays",
                            "Saturdays",
                            "Sundays",
                        ];
                        let day: usize = day.parse().unwrap();
                        format!("{} at {}", DAYS[day], clock(hour, minute).unwrap())
                    }
                    [minute, "*", "*", "*", "*"] if minute.parse::<u32>().is_ok_and(|m| m < 60) => {
                        format!("Every hour at :{:02}", minute.parse::<u32>().unwrap())
                    }
                    _ => format!("Cron {expr}"),
                }
            }
        }
    }

    /// One line about the newest run; `None` before the first.
    pub(crate) fn describe_run(run: &ScheduleRun, now_ms: f64) -> (String, RunTone) {
        let due = local::describe(run.due_at.0, Some(now_ms));
        let fired = local::describe(run.fired_at.0, Some(now_ms));
        let reason = match run.late_reason {
            Some(LateReason::Asleep) => " because the Mac was asleep",
            Some(LateReason::NotRunning) => " because ubra wasn't running",
            None => "",
        };
        let folded = match run.collapsed {
            0 => String::new(),
            1 => ", covering 1 earlier missed run".into(),
            n => format!(", covering {n} earlier missed runs"),
        };
        match run.outcome {
            ScheduleOutcome::OnTime => (format!("Last ran {fired}{folded}"), RunTone::Quiet),
            ScheduleOutcome::Manual => {
                (format!("Last run started by hand {fired}"), RunTone::Quiet)
            }
            ScheduleOutcome::Late => (
                format!("Ran late {fired} (due {due}){reason}{folded}"),
                RunTone::Attention,
            ),
            ScheduleOutcome::Missed => (
                format!("Skipped the run due {due}{reason}: too late to catch up{folded}"),
                RunTone::Danger,
            ),
            ScheduleOutcome::Failed => (
                format!(
                    "Couldn't start the run due {due}: {}",
                    run.error.as_deref().unwrap_or("unknown error")
                ),
                RunTone::Danger,
            ),
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum RunTone {
        Quiet,
        Attention,
        Danger,
    }
}

/// Local wall-clock helpers over libc, the same zone the Engine's cron uses.
mod local {
    struct Tm {
        year: i32,
        month: i32,
        day: i32,
        hour: i32,
        minute: i32,
        wday: i32,
    }

    fn break_down(ms: f64) -> Option<Tm> {
        if !ms.is_finite() {
            return None;
        }
        let time = (ms / 1000.0).floor() as libc::time_t;
        // SAFETY: `localtime_r` writes only to the provided, zeroed `tm`.
        let tm = unsafe {
            let mut tm = std::mem::zeroed::<libc::tm>();
            if libc::localtime_r(&time, &mut tm).is_null() {
                return None;
            }
            tm
        };
        Some(Tm {
            year: tm.tm_year + 1900,
            month: tm.tm_mon + 1,
            day: tm.tm_mday,
            hour: tm.tm_hour,
            minute: tm.tm_min,
            wday: tm.tm_wday,
        })
    }

    fn compose(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> Option<f64> {
        // SAFETY: a zeroed `tm` is valid; mktime only normalizes it in place.
        let secs = unsafe {
            let mut tm = std::mem::zeroed::<libc::tm>();
            tm.tm_year = year - 1900;
            tm.tm_mon = month - 1;
            tm.tm_mday = day;
            tm.tm_hour = hour;
            tm.tm_min = minute;
            tm.tm_isdst = -1;
            libc::mktime(&mut tm)
        };
        (secs != -1).then_some(secs as f64 * 1000.0)
    }

    /// The next local `hour:minute` after `now_ms` (today, else tomorrow).
    #[cfg(test)]
    pub(super) fn today_at(hour: i32, minute: i32, now_ms: f64) -> f64 {
        let now = break_down(now_ms).expect("local time");
        compose(now.year, now.month, now.day, hour, minute).expect("local time")
    }

    pub(super) fn next_at(hour: u32, minute: u32, now_ms: f64) -> Option<f64> {
        let now = break_down(now_ms)?;
        let today = compose(now.year, now.month, now.day, hour as i32, minute as i32)?;
        if today > now_ms + 30_000.0 {
            return Some(today);
        }
        compose(now.year, now.month, now.day + 1, hour as i32, minute as i32)
    }

    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    /// "today at 09:00", "tomorrow at 09:00", "Mon at 09:00" within a week,
    /// else "Oct 5 at 09:00". With no `now`, always the date form.
    pub(crate) fn describe(ms: f64, now_ms: Option<f64>) -> String {
        let Some(tm) = break_down(ms) else {
            return "at an unknown time".into();
        };
        let clock = format!("{:02}:{:02}", tm.hour, tm.minute);
        let date = format!(
            "{} {}",
            MONTHS[(tm.month - 1).clamp(0, 11) as usize],
            tm.day
        );
        let Some(now) = now_ms.and_then(break_down) else {
            return format!("{date} at {clock}");
        };
        let day_of = |tm: &Tm| compose(tm.year, tm.month, tm.day, 12, 0).unwrap_or(0.0);
        let days = ((day_of(&tm) - day_of(&now)) / 86_400_000.0).round() as i64;
        match days {
            0 => format!("today at {clock}"),
            1 => format!("tomorrow at {clock}"),
            -1 => format!("yesterday at {clock}"),
            2..=6 => format!("{} at {clock}", DAYS[tm.wday.clamp(0, 6) as usize]),
            _ => format!("{date} at {clock}"),
        }
    }
}

mod login {
    #[cfg(target_os = "macos")]
    pub(super) use crate::macos::login_item::{
        LoginItemStatus as Status, Service, open_settings, set_enabled, set_enabled_of, status,
        status_of,
    };

    #[cfg(not(target_os = "macos"))]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[allow(
        dead_code,
        reason = "mirrors the macOS login-item states; only Unavailable occurs elsewhere"
    )]
    pub(super) enum Status {
        Enabled,
        Disabled,
        RequiresApproval,
        Unavailable,
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn status() -> Status {
        Status::Unavailable
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn set_enabled(_: bool) -> Result<Status, String> {
        Err("Login items aren't available on this platform.".into())
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn open_settings() {}

    #[cfg(not(target_os = "macos"))]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Service {
        WakeHelper,
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn status_of(_: Service) -> Status {
        Status::Unavailable
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn set_enabled_of(_: Service, _: bool) -> Result<Status, String> {
        Err("Waking the Mac is only available on macOS.".into())
    }
}

#[cfg(test)]
mod preview {
    use super::*;

    fn spawn(prompt: &str, kind: AgentKind) -> SessionSpawnParams {
        plan::spec(
            prompt,
            "9:00",
            Repeat::Daily,
            Some(kind),
            Some(&PathBuf::from("/Users/example/code/app")),
            true,
            false,
            false,
            0.0,
        )
        .unwrap()
        .spawn
    }

    /// Pinned to 11:00 today so every line reads as it would in real use.
    pub(super) fn records(real_now: f64) -> Vec<ScheduleRecord> {
        let hour = 3_600_000.0;
        let now = super::local::today_at(11, 0, real_now);
        let nine = super::local::today_at(9, 0, now);
        let half_eight = super::local::today_at(8, 30, now);
        let run = |due: f64, fired: f64, outcome, late_reason, collapsed| ScheduleRun {
            due_at: DateMillis(due),
            fired_at: DateMillis(fired),
            outcome,
            late_reason,
            collapsed,
            session_id: None,
            error: None,
        };
        vec![
            ScheduleRecord {
                id: "sched_1".into(),
                spec: ScheduleSpec {
                    title: "Triage new GitHub issues and label them".into(),
                    when: ScheduleWhen::Cron {
                        expr: "0 9 * * 1-5".into(),
                    },
                    spawn: spawn("Triage new GitHub issues", AgentKind::CLAUDE_CODE),
                    catch_up_window_ms: DEFAULT_CATCH_UP_WINDOW_MS,
                    keep_awake: false,
                    wake_mac: true,
                    enabled: true,
                },
                created_at: DateMillis(now - 72.0 * hour),
                revision: 4,
                next_due: Some(DateMillis(nine + 24.0 * hour)),
                runs: vec![run(nine, nine + 20_000.0, ScheduleOutcome::OnTime, None, 0)],
            },
            ScheduleRecord {
                id: "sched_2".into(),
                spec: ScheduleSpec {
                    title: "Summarize yesterday's merged PRs".into(),
                    when: ScheduleWhen::Cron {
                        expr: "30 8 * * *".into(),
                    },
                    spawn: spawn("Summarize yesterday's merged PRs", AgentKind::CODEX),
                    catch_up_window_ms: DEFAULT_CATCH_UP_WINDOW_MS,
                    keep_awake: true,
                    wake_mac: false,
                    enabled: true,
                },
                created_at: DateMillis(now - 48.0 * hour),
                revision: 2,
                next_due: Some(DateMillis(half_eight + 24.0 * hour)),
                runs: vec![run(
                    half_eight,
                    half_eight + 600_000.0,
                    ScheduleOutcome::Late,
                    Some(LateReason::Asleep),
                    0,
                )],
            },
            ScheduleRecord {
                id: "sched_3".into(),
                spec: ScheduleSpec {
                    title: "Bump dependencies and open a PR".into(),
                    when: ScheduleWhen::Cron {
                        expr: "0 7 * * 1".into(),
                    },
                    spawn: spawn("Bump dependencies", AgentKind::CLAUDE_CODE),
                    catch_up_window_ms: 0,
                    keep_awake: false,
                    wake_mac: false,
                    enabled: false,
                },
                created_at: DateMillis(now - 240.0 * hour),
                revision: 1,
                next_due: None,
                runs: vec![run(
                    super::local::today_at(7, 0, now - 72.0 * hour),
                    super::local::today_at(20, 0, now - 72.0 * hour),
                    ScheduleOutcome::Missed,
                    Some(LateReason::NotRunning),
                    0,
                )],
            },
        ]
    }
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .flex_none()
        .px(px(10.0))
        .h(px(26.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(colors.primary.alpha(0.1))
        .text_size(px(12.0))
        .text_color(colors.primary)
        .cursor_pointer()
        .hover(move |style| style.bg(colors.primary.alpha(0.06)))
        .active(move |style| style.bg(colors.primary.alpha(0.1)))
        .child(label)
}

fn switch(enabled: bool, colors: SemanticColors) -> gpui::Div {
    div()
        .flex_none()
        .w(px(30.0))
        .h(px(18.0))
        .p(px(2.0))
        .rounded(px(9.0))
        .bg(if enabled {
            Ink::FRESH.alpha(0.72)
        } else {
            colors.primary.alpha(0.14)
        })
        .flex()
        .justify_end()
        .when(!enabled, |toggle| toggle.justify_start())
        .child(div().size(px(14.0)).rounded(px(7.0)).bg(colors.primary))
}

fn section(title: &'static str, content: impl IntoElement, colors: SemanticColors) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .px(px(1.0))
                .text_size(px(Typo::SECTION_HEADER.size))
                .font_weight(Typo::SECTION_HEADER.weight)
                .text_color(colors.tertiary)
                .child(title),
        )
        .child(
            div()
                .rounded(px(Radius::ROW))
                .border_1()
                .border_color(colors.primary.alpha(0.065))
                .bg(colors.primary.alpha(0.02))
                .overflow_hidden()
                .flex()
                .flex_col()
                .child(content),
        )
}

/// Trails the title of a schedule that wakes the Mac: the same indigo clock
/// the session it opens carries in the sidebar. Not a moon: that already
/// means Sleeping there.
fn wake_mark() -> AnyElement {
    crate::icons::sf_symbol("clock.fill", 11.0, NIGHT)
}

fn divider(colors: SemanticColors) -> gpui::Div {
    div().mx(px(12.0)).h(px(1.0)).bg(colors.primary.alpha(0.06))
}

fn text_stack(
    label: impl IntoElement,
    detail: impl Into<SharedString>,
    colors: SemanticColors,
) -> gpui::Div {
    div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .whitespace_normal()
                .text_size(px(Typo::ROW_EMPHASIZED.size))
                .font_weight(Typo::ROW_EMPHASIZED.weight)
                .text_color(colors.primary)
                .child(label),
        )
        .child(
            div()
                .whitespace_normal()
                .text_size(px(Typo::META.size))
                .line_height(px(14.0))
                .text_color(colors.tertiary)
                .child(detail.into()),
        )
}

impl SchedulesPage {
    fn schedule_row(
        &self,
        index: usize,
        record: &ScheduleRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        let now = now_ms();
        let busy = self.busy.contains(&record.id);
        let agent = {
            let store = self.store.read().expect("session store lock poisoned");
            store
                .agent_catalog(None)
                .map(|catalog| crate::agent_catalog::display_name(&record.spec.spawn.kind, catalog))
                .unwrap_or_else(|| crate::agent_catalog::title_case_id(record.spec.spawn.kind.id()))
        };
        let folder = home_relative(&record.spec.spawn.cwd);
        let mut detail = format!(
            "{} · {agent} · {folder}",
            plan::describe_when(&record.spec.when)
        );
        if record.spec.wake_mac {
            detail.push_str(" · wakes the Mac");
        } else if record.spec.keep_awake {
            detail.push_str(" · keeps Mac awake");
        }
        let next = match (&record.next_due, record.spec.enabled) {
            (Some(next), true) => Some(format!("Next {}", local::describe(next.0, Some(now)))),
            (_, false) => Some("Paused".to_owned()),
            (None, true) => None,
        };
        let last = record.runs.last().map(|run| plan::describe_run(run, now));
        let session = record
            .runs
            .last()
            .and_then(|run| run.session_id.clone())
            .filter(|id| {
                self.store
                    .read()
                    .expect("session store lock poisoned")
                    .sessions()
                    .contains_key(id)
            });
        let id = record.id.clone();
        let enabled = record.spec.enabled;
        let toggle_record = record.clone();
        let mut status = div()
            .flex()
            .flex_wrap()
            .gap_x(px(8.0))
            .text_size(px(Typo::META.size));
        if let Some(next) = next {
            status = status.child(div().text_color(colors.secondary).child(next));
        }
        if let Some((text, tone)) = last {
            status = status.child(
                div()
                    .text_color(match tone {
                        plan::RunTone::Quiet => colors.tertiary,
                        plan::RunTone::Attention => Ink::ATTENTION,
                        plan::RunTone::Danger => Ink::DANGER,
                    })
                    .child(text),
            );
        }
        let mut actions = div().flex().items_center().gap(px(6.0));
        if let Some(session) = session {
            actions = actions.child(
                button(("schedule-open", record.revision as usize), "Open", colors)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_session(&session, cx))),
            );
        }
        let run_id = id.clone();
        let delete_id = id.clone();
        actions = actions
            .child(
                button(("schedule-run", index), "Run now", colors)
                    .on_click(cx.listener(move |this, _, _, cx| this.run_now(run_id.clone(), cx))),
            )
            .child(
                button(("schedule-delete", index), "Delete", colors).on_click(
                    cx.listener(move |this, _, _, cx| this.delete(delete_id.clone(), cx)),
                ),
            )
            .child(
                div()
                    .id(("schedule-toggle", index))
                    .role(gpui::Role::Switch)
                    .aria_label(if enabled {
                        "Pause schedule"
                    } else {
                        "Resume schedule"
                    })
                    .cursor_pointer()
                    .child(switch(enabled, colors))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_enabled(&toggle_record, !enabled, cx)
                    })),
            );
        div()
            .min_h(px(ROW_MIN_HEIGHT))
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .when(busy || !enabled, |row| {
                row.opacity(if busy { 0.5 } else { 0.72 })
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(text_stack(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(record.spec.title.clone())
                            .when(record.spec.wake_mac, |title| title.child(wake_mark())),
                        detail,
                        colors,
                    ))
                    .child(status),
            )
            .child(actions)
            .into_any_element()
    }

    fn field(
        &self,
        id: &'static str,
        field: Field,
        editor: &QueryEditor,
        placeholder: &'static str,
        width: Option<f32>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = self.colors();
        let focused = self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.field == field);
        let label = if editor.is_empty() && !focused {
            div()
                .text_color(colors.tertiary)
                .child(placeholder)
                .into_any_element()
        } else if focused {
            crate::navigation::query_label(editor)
        } else {
            div().child(editor.text().to_owned()).into_any_element()
        };
        div()
            .id(id)
            .role(gpui::Role::TextInput)
            .aria_label(placeholder)
            .when_some(width, |field, width| field.w(px(width)).flex_none())
            .when(width.is_none(), |field| field.flex_1().min_w(px(0.0)))
            .h(px(30.0))
            .px(px(9.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(px(7.0))
            .border_1()
            .border_color(colors.primary.alpha(if focused { 0.28 } else { 0.1 }))
            .bg(colors.primary.alpha(0.025))
            .text_size(px(13.0))
            .text_color(colors.primary)
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(draft) = this.draft.as_mut() {
                    draft.field = field;
                }
                this.focus.focus(window, cx);
                cx.notify();
            }))
    }

    fn chip(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        selected: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = self.colors();
        button(id, label, colors).when(selected, |chip| {
            chip.bg(colors.primary.alpha(0.1))
                .border_color(colors.primary.alpha(0.22))
        })
    }

    fn draft_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors();
        let Some(draft) = self.draft.as_ref() else {
            return div().into_any_element();
        };
        let label = |text: &'static str| {
            div()
                .w(px(64.0))
                .flex_none()
                .text_size(px(12.0))
                .text_color(colors.secondary)
                .child(text)
        };
        let row = || {
            div()
                .px(px(12.0))
                .py(px(8.0))
                .flex()
                .items_center()
                .gap(px(10.0))
        };

        let mut agents = div().flex().flex_wrap().gap(px(5.0));
        let options = self.agent_options();
        if options.is_empty() {
            agents = agents.child(
                div()
                    .text_size(px(12.0))
                    .text_color(colors.tertiary)
                    .child("Looking for installed agents…"),
            );
        }
        for (index, option) in options.into_iter().enumerate() {
            let selected = draft.agent.as_ref() == Some(&option.kind);
            let kind = option.kind.clone();
            agents = agents.child(
                self.chip(("schedule-agent", index), option.display_name, selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = this.draft.as_mut() {
                            draft.agent = Some(kind.clone());
                        }
                        cx.notify();
                    })),
            );
        }

        let mut repeats = div().flex().flex_wrap().gap(px(5.0));
        for (index, repeat) in Repeat::ALL.into_iter().enumerate() {
            repeats = repeats.child(
                self.chip(
                    ("schedule-repeat", index),
                    repeat.label(),
                    draft.repeat == repeat,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(draft) = this.draft.as_mut() {
                        draft.repeat = repeat;
                    }
                    cx.notify();
                })),
            );
        }

        let folder = draft
            .folder
            .as_ref()
            .map(|folder| home_relative(&folder.to_string_lossy()))
            .unwrap_or_else(|| "No folder chosen".into());
        let catch_up = draft.catch_up;
        let keep_awake = draft.keep_awake;
        let wake_mac = draft.wake_mac;
        let toggle = |id: &'static str,
                      label: &'static str,
                      detail: &'static str,
                      on: bool,
                      cx: &mut Context<Self>,
                      flip: fn(&mut Draft)| {
            div()
                .id(id)
                .role(gpui::Role::Switch)
                .aria_label(label)
                .min_h(px(ROW_MIN_HEIGHT))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(12.0))
                .cursor_pointer()
                .hover(move |style| style.bg(colors.primary.alpha(0.025)))
                .child(text_stack(label, detail, colors))
                .child(switch(on, colors))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(draft) = this.draft.as_mut() {
                        flip(draft);
                    }
                    cx.notify();
                }))
        };

        let mut form = div()
            .flex()
            .flex_col()
            .child(
                row().child(label("Prompt")).child(self.field(
                    "schedule-prompt",
                    Field::Prompt,
                    &draft.prompt,
                    "What should the agent do?",
                    None,
                    cx,
                )),
            )
            .child(divider(colors))
            .child(row().child(label("Agent")).child(agents))
            .child(divider(colors))
            .child(
                row()
                    .child(label("Folder"))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(13.0))
                            .text_color(if draft.folder.is_some() {
                                colors.primary
                            } else {
                                colors.tertiary
                            })
                            .child(folder),
                    )
                    .child(
                        button("schedule-folder", "Choose…", colors)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                    ),
            )
            .child(divider(colors))
            .child(
                row()
                    .child(label("When"))
                    .child(repeats)
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.secondary)
                            .child(if draft.repeat == Repeat::Hourly { "minute" } else { "at" }),
                    )
                    .child(self.field(
                        "schedule-time",
                        Field::Time,
                        &draft.time,
                        "09:00",
                        Some(72.0),
                        cx,
                    )),
            )
            .child(divider(colors))
            .child(toggle(
                "schedule-catch-up",
                "Catch up if missed",
                "If the Mac is asleep or ubra isn't open at that time, run once when it's back, up to 12 hours late.",
                catch_up,
                cx,
                |draft| draft.catch_up = !draft.catch_up,
            ))
            .child(divider(colors))
            .child(toggle(
                "schedule-keep-awake",
                "Keep the Mac awake",
                "Prevent idle sleep from 10 minutes before each run until it finishes. Closing the lid still sleeps.",
                keep_awake,
                cx,
                |draft| draft.keep_awake = !draft.keep_awake,
            ))
            .child(divider(colors))
            .child(toggle(
                "schedule-wake-mac",
                "Wake the Mac",
                "Wake a sleeping Mac 2 minutes before, keep it awake while the agent works, then let it sleep again. The lid must be open.",
                wake_mac,
                cx,
                |draft| draft.wake_mac = !draft.wake_mac,
            ));
        if wake_mac && self.wake_helper.status != login::Status::Enabled {
            form = form.child(
                div()
                    .px(px(12.0))
                    .pb(px(8.0))
                    .text_size(px(12.0))
                    .text_color(Ink::ATTENTION)
                    .child("Turn on “Allow ubra to wake the Mac” below first, or this schedule can't wake it."),
            );
        }
        if let Some(error) = &draft.error {
            form = form.child(
                div()
                    .px(px(12.0))
                    .pb(px(6.0))
                    .text_size(px(12.0))
                    .text_color(Ink::DANGER)
                    .child(error.clone()),
            );
        }
        form = form.child(divider(colors)).child(
            div()
                .px(px(12.0))
                .py(px(10.0))
                .flex()
                .justify_end()
                .gap(px(8.0))
                .child(
                    button("schedule-cancel", "Cancel", colors).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.draft = None;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button(
                        "schedule-create",
                        if draft.saving {
                            "Creating…"
                        } else {
                            "Create schedule"
                        },
                        colors,
                    )
                    .bg(colors.primary.alpha(0.1))
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                ),
        );
        section("New schedule", form, colors).into_any_element()
    }

    fn login_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors();
        let status = self.login.status;
        let detail = match (status, &self.login.error) {
            (_, Some(error)) => format!("Couldn't change this: {error}"),
            (login::Status::RequiresApproval, None) => {
                "Allow ubra in System Settings > General > Login Items.".to_owned()
            }
            (login::Status::Unavailable, None) => {
                "Only available when ubra is installed in Applications.".to_owned()
            }
            _ => "Schedules run only while ubra is open. Opening at login lets a run missed during a restart catch up.".to_owned(),
        };
        let row = div()
            .id("schedule-open-at-login")
            .role(gpui::Role::Switch)
            .aria_label("Open ubra at login")
            .min_h(px(ROW_MIN_HEIGHT))
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .cursor_pointer()
            .hover(move |style| style.bg(colors.primary.alpha(0.025)))
            .child(text_stack("Open ubra at login", detail, colors))
            .child(switch(status == login::Status::Enabled, colors))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_login(cx)));
        let helper = self.wake_helper.status;
        let wanted = self
            .schedules
            .iter()
            .any(|record| record.spec.wake_mac && record.spec.enabled);
        let helper_detail = match (helper, &self.wake_helper.error, &self.wake_helper_error) {
            (_, Some(error), _) => format!("Couldn't change this: {error}"),
            (login::Status::RequiresApproval, None, _) => {
                "Waiting for approval: switch on ubra in System Settings > General > Login Items. macOS asks for an administrator password once.".to_owned()
            }
            (login::Status::Unavailable, None, _) => {
                "Only available when ubra is installed in Applications.".to_owned()
            }
            (login::Status::Enabled, None, Some(error)) if wanted => {
                format!("Approved, but ubra couldn't reach it: {error}")
            }
            _ => "A small helper that can only schedule wakes for your runs and put the Mac back to sleep after them. Needs a one-time administrator approval.".to_owned(),
        };
        let helper_row = div()
            .id("schedule-wake-helper")
            .role(gpui::Role::Switch)
            .aria_label("Allow ubra to wake the Mac")
            .min_h(px(ROW_MIN_HEIGHT))
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .cursor_pointer()
            .hover(move |style| style.bg(colors.primary.alpha(0.025)))
            .child(text_stack(
                "Allow ubra to wake the Mac",
                helper_detail,
                colors,
            ))
            .child(switch(helper == login::Status::Enabled, colors))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_wake_helper(cx)));
        section(
            "When the Mac is asleep or restarts",
            div()
                .flex()
                .flex_col()
                .child(helper_row)
                .child(divider(colors))
                .child(row),
            colors,
        )
        .into_any_element()
    }
}

impl Render for SchedulesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors();
        let mut page = div()
            .id("schedules-page")
            .flex_none()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, _, cx| {
                this.handle_key(event, cx);
            }))
            .px(px(24.0))
            .pt(px(18.0))
            .pb(px(24.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .text_color(colors.primary)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(px(20.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Schedules"),
                    )
                    .when(self.draft.is_none(), |header| {
                        header.child(
                            button("schedule-new", "New schedule", colors).on_click(
                                cx.listener(|this, _, window, cx| this.start_draft(window, cx)),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(colors.secondary)
                    .child("Start an agent at a set time. Later runs continue in the same tab until you close it. You can also ask any agent in ubra, for example “every weekday at 9, triage new issues”."),
            );
        if self.draft.is_some() {
            page = page.child(self.draft_form(cx));
        }
        if let Some(error) = &self.error {
            page = page.child(
                div()
                    .text_size(px(12.0))
                    .text_color(Ink::DANGER)
                    .child(error.clone()),
            );
        }
        let mut list = div().flex().flex_col();
        if self.schedules.is_empty() {
            list = list.child(
                div()
                    .min_h(px(ROW_MIN_HEIGHT))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .text_color(colors.tertiary)
                    .child(if self.loaded {
                        "No schedules yet."
                    } else {
                        "Loading schedules…"
                    }),
            );
        }
        let records = self.schedules.clone();
        let total = records.len();
        let list_page = self.page.min(total.saturating_sub(1) / SCHEDULE_PAGE_ROWS);
        self.page = list_page;
        let start = list_page * SCHEDULE_PAGE_ROWS;
        let end = (start + SCHEDULE_PAGE_ROWS).min(total);
        for (index, record) in records.iter().enumerate().skip(start).take(end - start) {
            if index > start {
                list = list.child(divider(colors));
            }
            list = list.child(self.schedule_row(index, record, cx));
        }
        if total > SCHEDULE_PAGE_ROWS {
            let total_pages = total.div_ceil(SCHEDULE_PAGE_ROWS);
            list = list.child(divider(colors));
            let mut pager = div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(8.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.tertiary)
                        .child(format!("{}–{} of {total} schedules", start + 1, end)),
                );
            if list_page > 0 {
                pager = pager.child(button("schedules-previous", "Previous", colors).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.page = this.page.saturating_sub(1);
                        cx.notify();
                    }),
                ));
            }
            if end < total {
                pager = pager.child(button("schedules-next", "Next", colors).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.page = (this.page + 1).min(total_pages - 1);
                        cx.notify();
                    }),
                ));
            }
            list = list.child(pager);
        }
        page.child(section("Scheduled", list, colors))
            .child(self.login_section(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::plan::*;
    use super::*;

    #[test]
    fn times_parse_leniently_but_strictly_ranged() {
        assert_eq!(parse_time("9"), Some((9, 0)));
        assert_eq!(parse_time(" 09:05 "), Some((9, 5)));
        assert_eq!(parse_time("21:30"), Some((21, 30)));
        for bad in ["", "24:00", "9:60", "9:5", "nine", "9:00pm"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn form_builds_the_cron_each_repeat_means() {
        let folder = PathBuf::from("/tmp");
        let build = |repeat| {
            spec(
                "  triage  ",
                "9:30",
                repeat,
                Some(AgentKind::CLAUDE_CODE),
                Some(&folder),
                true,
                false,
                false,
                0.0,
            )
            .unwrap()
        };
        let weekdays = build(Repeat::Weekdays);
        assert_eq!(
            weekdays.when,
            ScheduleWhen::Cron {
                expr: "30 9 * * 1-5".into()
            }
        );
        assert_eq!(weekdays.title, "triage");
        assert_eq!(weekdays.spawn.initial_prompt.as_deref(), Some("triage"));
        assert_eq!(weekdays.catch_up_window_ms, DEFAULT_CATCH_UP_WINDOW_MS);
        assert_eq!(
            build(Repeat::Daily).when,
            ScheduleWhen::Cron {
                expr: "30 9 * * *".into()
            }
        );
        assert_eq!(
            build(Repeat::Hourly).when,
            ScheduleWhen::Cron {
                expr: "30 * * * *".into()
            }
        );
        let ScheduleWhen::Once { at } = build(Repeat::Once).when else {
            panic!("once");
        };
        assert!(at.0 > 0.0);
    }

    #[test]
    fn form_explains_what_is_missing() {
        let folder = PathBuf::from("/tmp");
        let agent = Some(AgentKind::CLAUDE_CODE);
        assert!(
            spec(
                " ",
                "9",
                Repeat::Daily,
                agent.clone(),
                Some(&folder),
                true,
                false,
                false,
                0.0
            )
            .is_err()
        );
        assert!(
            spec(
                "x",
                "9",
                Repeat::Daily,
                agent.clone(),
                None,
                true,
                false,
                false,
                0.0
            )
            .is_err()
        );
        assert!(
            spec(
                "x",
                "9",
                Repeat::Daily,
                None,
                Some(&folder),
                true,
                false,
                false,
                0.0
            )
            .is_err()
        );
        assert!(
            spec(
                "x",
                "25",
                Repeat::Daily,
                agent,
                Some(&folder),
                true,
                false,
                false,
                0.0
            )
            .is_err()
        );
    }

    #[test]
    fn once_at_a_passed_time_means_tomorrow() {
        let now = super::local::next_at(10, 0, 0.0).unwrap() + 60_000.0;
        let next = super::local::next_at(10, 0, now).unwrap();
        let hours = (next - now) / 3_600_000.0;
        assert!((23.0..=25.0).contains(&hours), "{hours}");
    }

    #[test]
    fn schedules_read_as_words() {
        let cron = |expr: &str| ScheduleWhen::Cron { expr: expr.into() };
        assert_eq!(describe_when(&cron("0 9 * * 1-5")), "Weekdays at 09:00");
        assert_eq!(describe_when(&cron("30 8 * * *")), "Every day at 08:30");
        assert_eq!(describe_when(&cron("15 * * * *")), "Every hour at :15");
        assert_eq!(describe_when(&cron("0 7 * * 1")), "Mondays at 07:00");
        assert_eq!(describe_when(&cron("0 9 1 * *")), "Cron 0 9 1 * *");
    }

    #[test]
    fn late_runs_say_why() {
        let now = super::local::next_at(9, 0, 0.0).unwrap() + 3_600_000.0;
        let run = ScheduleRun {
            due_at: DateMillis(now - 3_600_000.0),
            fired_at: DateMillis(now - 3_000_000.0),
            outcome: ScheduleOutcome::Late,
            late_reason: Some(LateReason::Asleep),
            collapsed: 2,
            session_id: None,
            error: None,
        };
        let (text, tone) = describe_run(&run, now);
        assert_eq!(tone, RunTone::Attention);
        assert!(
            text.starts_with("Ran late today at 09:10 (due today at 09:00)"),
            "{text}"
        );
        assert!(text.contains("Mac was asleep"), "{text}");
        assert!(text.contains("2 earlier missed runs"), "{text}");
    }

    /// Opening Settings builds this page on GPUI's thread with no ambient
    /// Tokio runtime while the client is connected; subscribing to daemon
    /// events must not panic there (it fire-and-forgets a Tokio spawn).
    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn new_subscribes_without_an_ambient_tokio_runtime(cx: &mut gpui::TestAppContext) {
        use std::io::{BufRead as _, Write as _};

        // Precondition, not paranoia: without it this test would pass
        // vacuously if GPUI ever gained an ambient Tokio runtime.
        assert!(tokio::runtime::Handle::try_current().is_err());

        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let home = tempfile::tempdir().expect("socket home");
        let socket = home.path().join("daemon.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind fake Engine");
        std::thread::spawn(move || {
            use ubra_proto::{ControlError, ControlMessage, Method};

            for stream in listener.incoming().flatten() {
                let mut writer = stream.try_clone().expect("clone fake Engine stream");
                let mut reader = std::io::BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                    let request = serde_json::from_str::<ControlMessage>(&line);
                    line.clear();
                    let Ok(ControlMessage::Request { id, method, .. }) = request else {
                        continue;
                    };
                    let result = if method == Method::HELLO {
                        Ok(serde_json::to_value(ubra_proto::HelloResult {
                            proto: ubra_proto::WIRE_VERSION,
                            build: "test-engine".to_owned(),
                            pid: std::process::id() as i32,
                            engine_instance_id: None,
                            engine_kind: Some(ubra_proto::RUST_ENGINE_KIND.to_owned()),
                            executable_hash: None,
                        })
                        .expect("hello"))
                    } else if method == Method::EVENTS_SUBSCRIBE {
                        Ok(serde_json::json!({ "subscribed": true }))
                    } else {
                        Err(ControlError::bad_request("unsupported by the fake Engine"))
                    };
                    let mut bytes = serde_json::to_vec(&ControlMessage::Response { id, result })
                        .expect("encode response");
                    bytes.push(b'\n');
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
            }
        });

        let client = Arc::new(DaemonClient::with_socket_path(&socket));
        runtime.block_on(async {
            client.connect();
            client
                .wait_until_connected(Duration::from_secs(5))
                .await
                .expect("fake Engine connects");
        });
        // Still no ambient runtime once block_on returns.
        assert!(tokio::runtime::Handle::try_current().is_err());

        let store_runtime = Arc::new(StoreRuntime::inert_with_client(client));
        // This panicked before the Tokio context fix (client connected, no
        // ambient runtime on GPUI's thread). No run_until_parked: driving the
        // test scheduler while the Tokio runtime is live trips GPUI's
        // determinism guard, and construction is the whole regression.
        let _page =
            cx.new(|cx| SchedulesPage::new(Arc::clone(&store_runtime), Arc::clone(&runtime), cx));
        // Quiesce before GPUI teardown: join the client tasks, then kill the
        // runtime while no GPUI task can be woken from its workers.
        runtime.block_on(store_runtime.client().shutdown());
        drop(runtime);
    }
}
