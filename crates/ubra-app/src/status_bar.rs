//! Bottom narrow status bar: one quiet strip under the workbench.
//!
//! The bar answers two questions at a glance — "what is my session doing"
//! and "what needs me" — with every segment opening an existing surface.
//! Active-session context reads left to right; app-wide actions stay on the right.
//!
//! Performance contract: the bar owns no timers, tasks, or store bindings.
//! RootView rebuilds [`StatusBarModel`] on the store publications it already
//! receives and pushes it here; [`StatusBarView::set_model`] repaints only
//! when the model differs, and `render` borrows the model without computing,
//! allocating display text, reading the store or a clock, or requesting
//! another frame.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Context, Div, EventEmitter, FocusHandle,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use ubra_proto::{
    AgentReadinessResult, AttentionLevel, PortInfo, Project, ProjectId, SessionId, SessionRecord,
    TerminalProgress, TerminalProgressState,
};
use ubra_ui::{AgentKind as UiAgentKind, Fill, Icon, IconName, Ink, Radius, SemanticColors, Typo};

use crate::agent_catalog;
use crate::session_presentation::{status_state, ui_agent_kind};
use crate::terminal_pane::{TerminalAccess, TerminalChromeState};
use crate::tooltip_warmth::WarmTooltip as _;
use crate::transcript::ContextUsage;
use crate::updates::{UpdateCommand, UpdatePhase, UpdateState};

/// Fixed bar height. `RootView::terminal_card` subtracts this from the card so
/// the strip never covers terminal rows.
pub(crate) const STATUS_BAR_HEIGHT: f32 = 26.0;
const SEGMENT_ICON: f32 = 12.0;

/// Optional presentation only; data collection and navigation stay unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StatusBarVisibility {
    pub context: bool,
    pub git: bool,
    pub worktree: bool,
    pub ports: bool,
}

impl Default for StatusBarVisibility {
    fn default() -> Self {
        Self {
            context: true,
            git: true,
            worktree: true,
            ports: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WidthTier {
    Full,
    Reduced,
    Compact,
    Minimal,
}

impl WidthTier {
    fn for_width(width: f32) -> Self {
        if width >= 1200.0 {
            Self::Full
        } else if width >= 1000.0 {
            Self::Reduced
        } else if width >= 900.0 {
            Self::Compact
        } else {
            Self::Minimal
        }
    }

    fn visibility(self, enabled: StatusBarVisibility) -> StatusBarVisibility {
        StatusBarVisibility {
            context: enabled.context && matches!(self, Self::Full | Self::Reduced),
            git: enabled.git && self != Self::Minimal,
            worktree: enabled.worktree && self == Self::Full,
            ports: enabled.ports && self != Self::Minimal,
        }
    }

    // At 1000px full safety labels plus enabled context/ports can consume the
    // identity lane. Compact the independent controls before hiding metadata.
    fn compact_feedback(self) -> bool {
        self != Self::Full
    }
}

/// Identity for one clickable segment: its focus handle slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Segment {
    Status,
    Location,
    Scrollback,
    Access,
    Git,
    Worktree,
    Message,
    Progress,
    Context,
    Ports,
    Attention,
    Bell,
    Update,
}

/// The update commands the bar can send, mirroring the sidebar pill exactly:
/// each noteworthy phase maps to at most one deliberate action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpdateAction {
    Download,
    Install,
    Dismiss,
}

impl UpdateAction {
    fn command(self) -> UpdateCommand {
        match self {
            Self::Download => UpdateCommand::Download,
            Self::Install => UpdateCommand::Install,
            Self::Dismiss => UpdateCommand::Dismiss,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UpdateSegment {
    /// Short bar label (`Update`, `42%`, `Restart`).
    pub label: SharedString,
    /// Full sentence for the accessible name.
    pub detail: SharedString,
    pub action: Option<UpdateAction>,
    pub danger: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SessionSegment {
    pub id: SessionId,
    /// `Claude Code · Add auth flow`, truncated by layout, never wrapped.
    pub display: SharedString,
    pub status: ubra_ui::StatusState,
    pub status_label: SharedString,
    pub status_aria: SharedString,
    pub location: SharedString,
    pub location_tail: Option<SharedString>,
    pub location_aria: SharedString,
    pub glyph: UiAgentKind,
    /// Branch name from the record. Dirty/ahead/behind counts need a git
    /// invocation, which the bar never performs; the Review surface owns them.
    pub branch: Option<SharedString>,
    pub git_aria: Option<SharedString>,
    pub project: SharedString,
    pub worktree_aria: SharedString,
    /// Whole-percent progress while a foreground job reports it.
    pub progress: Option<TerminalProgress>,
    pub progress_label: Option<SharedString>,
    pub progress_aria: Option<SharedString>,
    /// Compact label (`:3000`, `:3000 +2`).
    pub ports_label: Option<SharedString>,
    pub ports_aria: Option<SharedString>,
    pub scrolled_back: bool,
    pub access: Option<TerminalAccess>,
    pub context_label: Option<SharedString>,
    pub context_aria: Option<SharedString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusMessageTone {
    Progress,
    Success,
    Warning,
}

/// The one action the bar's initialization message can offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusMessageAction {
    RetryConnection,
}

/// Initialization feedback that takes the left slot: engine connection and
/// session-resume progress. `None` shows the normal session context.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StatusMessage {
    pub text: SharedString,
    pub detail: Option<SharedString>,
    pub tone: StatusMessageTone,
    pub action: Option<StatusMessageAction>,
    /// One atomic sentence for the `Role::Status` name while this shows.
    pub aria: SharedString,
}

impl StatusMessage {
    pub(crate) fn connecting() -> Self {
        Self {
            text: "Connecting…".into(),
            detail: None,
            tone: StatusMessageTone::Progress,
            action: None,
            aria: "Connecting to the Ubra engine. Sessions stay visible meanwhile.".into(),
        }
    }

    pub(crate) fn reconnecting() -> Self {
        Self {
            text: "Reconnecting…".into(),
            detail: None,
            tone: StatusMessageTone::Progress,
            action: Some(StatusMessageAction::RetryConnection),
            aria: "Reconnecting to the Ubra engine. Activate to retry now; sessions stay readable."
                .into(),
        }
    }

    pub(crate) fn resuming(finished: usize, total: usize) -> Self {
        Self {
            text: format!("Resuming {finished} of {total}…").into(),
            detail: None,
            tone: StatusMessageTone::Progress,
            action: None,
            aria: format!("Resuming sessions: {finished} of {total} resumed.").into(),
        }
    }

    /// Benign confirmation (copy, link) that takes the left slot briefly.
    /// Initialization state wins while it shows; see `RootView::status_message`.
    pub(crate) fn notice(text: impl Into<SharedString>) -> Self {
        let text = text.into();
        Self {
            aria: text.clone(),
            text,
            detail: None,
            tone: StatusMessageTone::Success,
            action: None,
        }
    }

    pub(crate) fn resume_summary(
        resumed: usize,
        total: usize,
        first_failure: Option<&str>,
    ) -> Self {
        let sessions = if resumed == 1 { "session" } else { "sessions" };
        let text: SharedString = if resumed == total {
            format!("Resumed {resumed} {sessions}").into()
        } else {
            format!("Resumed {resumed} of {total} {sessions}").into()
        };
        let detail = first_failure.map(|reason| format!("Some didn’t resume: {reason}").into());
        let aria = match &detail {
            Some(detail) => format!("{text}. {detail}").into(),
            None => text.clone(),
        };
        Self {
            text,
            detail,
            tone: if first_failure.is_some() {
                StatusMessageTone::Warning
            } else {
                StatusMessageTone::Success
            },
            action: None,
            aria,
        }
    }

    /// How long this message stays without new state to show. Progress is
    /// persistence, not a timeout: RootView only arms a timer for summaries.
    pub(crate) fn hold(&self) -> Duration {
        match self.tone {
            StatusMessageTone::Success => Duration::from_secs(4),
            StatusMessageTone::Warning => Duration::from_millis(8_500),
            StatusMessageTone::Progress => Duration::MAX,
        }
    }
}

/// Everything the bar paints, computed once per store publication by RootView.
/// `PartialEq` is the repaint gate: an unchanged model never invalidates.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StatusBarModel {
    pub session: Option<SessionSegment>,
    pub visibility: StatusBarVisibility,
    pub needs_input: usize,
    pub done_unseen: usize,
    pub attention_label: Option<SharedString>,
    pub attention_count: Option<SharedString>,
    pub attention_aria: Option<SharedString>,
    pub unread: usize,
    pub bell_label: Option<SharedString>,
    pub bell_aria: Option<SharedString>,
    pub update: Option<UpdateSegment>,
    /// Initialization feedback (connection, resume progress) that takes the
    /// left slot in place of the session context while it shows.
    pub message: Option<StatusMessage>,
    /// Resolved palette. Render borrows it instead of reading the store, so
    /// a theme, material, or transparency change invalidates the model and
    /// repaints; nothing else can leave the bar one theme behind.
    pub colors: SemanticColors,
    /// One atomic sentence for the `Role::Status` accessible name, rebuilt
    /// only when its inputs change so assistive tech announces real changes.
    pub announcement: SharedString,
}

pub(crate) struct ModelInputs<'a> {
    pub sessions: &'a HashMap<SessionId, Arc<SessionRecord>>,
    pub selected: Option<&'a SessionRecord>,
    pub hosts: &'a [ubra_proto::HostEntry],
    pub chrome: Option<TerminalChromeState>,
    pub context: Option<ContextUsage>,
    pub visibility: StatusBarVisibility,
    pub projects: &'a HashMap<ProjectId, Project>,
    pub migrating: &'a HashSet<SessionId>,
    pub catalog: Option<&'a AgentReadinessResult>,
    pub unread: usize,
    pub update: &'a UpdateState,
    pub message: Option<StatusMessage>,
    pub colors: SemanticColors,
}

impl Default for StatusBarModel {
    fn default() -> Self {
        Self {
            session: None,
            visibility: StatusBarVisibility::default(),
            needs_input: 0,
            done_unseen: 0,
            attention_label: None,
            attention_count: None,
            attention_aria: None,
            unread: 0,
            bell_label: None,
            bell_aria: None,
            update: None,
            message: None,
            colors: SemanticColors::light(),
            announcement: "All caught up".into(),
        }
    }
}

impl StatusBarModel {
    /// Pure reduction over already-loaded store state. No I/O, no clock.
    pub(crate) fn build(inputs: ModelInputs<'_>) -> Self {
        let mut needs_input = 0;
        let mut done_unseen = 0;
        for session in inputs.sessions.values() {
            if session.is_archived() {
                continue;
            }
            match session.attention() {
                AttentionLevel::NeedsInput => needs_input += 1,
                AttentionLevel::DoneUnseen => done_unseen += 1,
                _ => {}
            }
        }
        let session = inputs.selected.map(|record| {
            let agent = agent_label(record, inputs.catalog);
            let status = status_state(record, inputs.migrating.contains(&record.id));
            let status_label: SharedString = status.label().into();
            let title = record.title.as_str();
            let branch = record.git_branch.clone().map(SharedString::from);
            let project = project_label(record, inputs.projects);
            let (progress_label, progress_aria) = progress_texts(record.terminal_progress);
            let ports_copy = ports_copy(record);
            let (location, location_tail, location_aria) = execution_location(record, inputs.hosts);
            let chrome = inputs
                .chrome
                .as_ref()
                .filter(|chrome| chrome.id == record.id);
            let access = chrome.map(|chrome| chrome.access).filter(|access| {
                *access != TerminalAccess::Live
                    && !(*access == TerminalAccess::Unavailable
                        && matches!(record.status, ubra_proto::SessionStatus::Exited(_)))
            });
            let context = inputs
                .context
                .filter(|usage| usage.tokens >= 0 && usage.window > 0);
            SessionSegment {
                id: record.id.clone(),
                display: format!("{agent} · {title}").into(),
                status,
                status_label: status_label.clone(),
                status_aria: format!(
                    "Session {title}, {agent}, {status_label}. Activate to focus it."
                )
                .into(),
                location,
                location_tail,
                location_aria,
                scrolled_back: chrome.is_some_and(|chrome| chrome.scrolled_back),
                access,
                context_label: context.map(|usage| {
                    format!(
                        "Context {:.0}%",
                        (usage.tokens as f64 * 100.0 / usage.window as f64).round()
                    )
                    .into()
                }),
                context_aria: context.map(|usage| {
                    format!(
                        "Last reported request context: {} / {} tokens. Open Usage in Settings.",
                        usage.tokens, usage.window
                    )
                    .into()
                }),
                glyph: ui_agent_kind(record.effective_kind()),
                git_aria: branch
                    .as_ref()
                    .map(|name| format!("Git branch {name}. Activate to open review.").into()),
                branch,
                worktree_aria: format!(
                    "Project {project}. Activate to open Worktrees in Settings."
                )
                .into(),
                project,
                progress: record.terminal_progress,
                progress_label,
                progress_aria,
                ports_label: ports_label(record),
                ports_aria: ports_copy.as_ref().map(|list| {
                    format!("Listening on port {list}. Activate to open Browser.").into()
                }),
            }
        });
        let (attention_label, attention_aria) = attention_texts(needs_input, done_unseen);
        let attention_count = (needs_input.max(done_unseen) > 0).then(|| {
            (if needs_input > 0 {
                needs_input
            } else {
                done_unseen
            })
            .to_string()
            .into()
        });
        let (bell_label, bell_aria) = bell_texts(inputs.unread);
        let update = update_segment(inputs.update);
        let message = inputs.message;
        let announcement = match &message {
            Some(message) => message.aria.clone(),
            None => announcement(needs_input, done_unseen, inputs.unread, update.is_some()),
        };
        let mut colors = inputs.colors;
        colors.primary = colors.readable_foreground();
        Self {
            session,
            visibility: inputs.visibility,
            needs_input,
            done_unseen,
            attention_label,
            attention_count,
            attention_aria,
            unread: inputs.unread,
            bell_label,
            bell_aria,
            update,
            message,
            colors,
            announcement,
        }
    }

    /// Whole-percent progress acceptance: `TerminalProgress` already carries a
    /// `u8` percent, so model equality coalesces repeat reports for free and a
    /// chatty sender cannot invalidate faster than the value visibly changes.
    #[cfg(test)]
    pub(crate) fn progress_percent(&self) -> Option<u8> {
        self.session
            .as_ref()
            .and_then(|session| session.progress.map(|progress| progress.percent))
    }
}

fn execution_location(
    record: &SessionRecord,
    hosts: &[ubra_proto::HostEntry],
) -> (SharedString, Option<SharedString>, SharedString) {
    match record.host.as_deref() {
        None => (
            "Local".into(),
            None,
            "Local machine. Open session Details.".into(),
        ),
        Some(id) => {
            let host = hosts.iter().find(|host| host.id == id);
            let name = host.map_or(id, |host| host.display_name());
            let destination = host.map_or(id, |host| host.ssh.as_str());
            ("SSH ·".into(), Some(name.to_owned().into()),
             format!("SSH host {name}, identifier {id}, destination {destination}. Open session Details.").into())
        }
    }
}

fn agent_label(record: &SessionRecord, catalog: Option<&AgentReadinessResult>) -> SharedString {
    let kind = record.effective_kind();
    // Same terminal wording as every other launch surface.
    if kind.is_terminal() {
        return "Terminal".into();
    }
    match catalog {
        Some(catalog) => agent_catalog::display_name(kind, catalog).into(),
        None => kind.id().into(),
    }
}

fn project_label(record: &SessionRecord, projects: &HashMap<ProjectId, Project>) -> SharedString {
    if let Some(project) = projects.get(&record.project_id) {
        return project.name.clone().into();
    }
    record
        .worktree_path
        .as_deref()
        .or(Some(record.cwd.as_str()))
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("No project")
        .into()
}

/// Live foreground ports first, governor scan results as fallback.
fn session_ports(record: &SessionRecord) -> &[PortInfo] {
    record
        .foreground_ports
        .as_deref()
        .filter(|ports| !ports.is_empty())
        .or_else(|| {
            record
                .listening_ports
                .as_deref()
                .filter(|ports| !ports.is_empty())
        })
        .unwrap_or(&[])
}

fn ports_label(record: &SessionRecord) -> Option<SharedString> {
    let ports = session_ports(record);
    let first = ports.first()?;
    if ports.len() == 1 {
        Some(format!(":{}", first.port).into())
    } else {
        Some(format!(":{} +{}", first.port, ports.len() - 1).into())
    }
}

fn ports_copy(record: &SessionRecord) -> Option<SharedString> {
    let ports = session_ports(record);
    if ports.is_empty() {
        return None;
    }
    let list = ports
        .iter()
        .map(|port| port.port.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(list.into())
}

fn progress_texts(
    progress: Option<TerminalProgress>,
) -> (Option<SharedString>, Option<SharedString>) {
    let Some(progress) = progress else {
        return (None, None);
    };
    let percent = progress.percent;
    let (label, aria) = match progress.state {
        TerminalProgressState::Normal => (
            format!("{percent}%"),
            format!("Task progress {percent} percent. Activate to focus the terminal."),
        ),
        TerminalProgressState::Error => (
            format!("{percent}%"),
            format!("Task failed at {percent} percent. Activate to focus the terminal."),
        ),
        TerminalProgressState::Indeterminate => (
            "Busy".to_owned(),
            "Task is busy with no known end. Activate to focus the terminal.".to_owned(),
        ),
        TerminalProgressState::Paused => (
            format!("{percent}%"),
            format!("Task paused at {percent} percent. Activate to focus the terminal."),
        ),
        TerminalProgressState::Unknown => (
            format!("{percent}%"),
            format!("Task progress {percent} percent. Activate to focus the terminal."),
        ),
    };
    (Some(label.into()), Some(aria.into()))
}

/// Needs-input wins over done: the queue walker lands on blockers first.
fn attention_texts(
    needs_input: usize,
    done_unseen: usize,
) -> (Option<SharedString>, Option<SharedString>) {
    if needs_input > 0 {
        let label = if needs_input == 1 {
            "1 needs input".to_owned()
        } else {
            format!("{needs_input} need input")
        };
        let aria = format!(
            "{}. Activate to go to the next one.",
            plural(needs_input, "session needs input", "sessions need input")
        );
        (Some(label.into()), Some(aria.into()))
    } else if done_unseen > 0 {
        let label = if done_unseen == 1 {
            "1 done".to_owned()
        } else {
            format!("{done_unseen} done")
        };
        let aria = format!(
            "{}. Activate to go to the next one.",
            plural(done_unseen, "session finished", "sessions finished")
        );
        (Some(label.into()), Some(aria.into()))
    } else {
        (None, None)
    }
}

fn bell_texts(unread: usize) -> (Option<SharedString>, Option<SharedString>) {
    if unread == 0 {
        return (None, None);
    }
    (
        Some(unread.to_string().into()),
        Some(
            format!(
                "{}. Activate to open notifications.",
                plural(unread, "unread notification", "unread notifications")
            )
            .into(),
        ),
    )
}

/// Same phase table as the sidebar pill: same visibility gate, same labels,
/// same single deliberate action per phase.
fn update_segment(state: &UpdateState) -> Option<UpdateSegment> {
    if !state.is_noteworthy() {
        return None;
    }
    let detail = state.summary().into();
    let (label, action, danger) = match &state.phase {
        crate::updates::UpdatePhase::Available(_) => {
            ("Update".to_owned(), Some(UpdateAction::Download), false)
        }
        crate::updates::UpdatePhase::Downloading { progress, .. } => (
            format!("{}%", (progress * 100.0).round() as u32),
            None,
            false,
        ),
        crate::updates::UpdatePhase::Ready(_) => {
            ("Restart".to_owned(), Some(UpdateAction::Install), false)
        }
        crate::updates::UpdatePhase::Installing => ("Restarting".to_owned(), None, false),
        crate::updates::UpdatePhase::Failed(_) => (
            "Update failed".to_owned(),
            Some(UpdateAction::Dismiss),
            true,
        ),
        crate::updates::UpdatePhase::Checking => ("Checking…".to_owned(), None, false),
        crate::updates::UpdatePhase::UpToDate => {
            ("Up to date".to_owned(), Some(UpdateAction::Dismiss), false)
        }
        crate::updates::UpdatePhase::Idle | crate::updates::UpdatePhase::Unsupported(_) => {
            return None;
        }
    };
    Some(UpdateSegment {
        label: label.into(),
        detail,
        action,
        danger,
    })
}

/// Preview-only chrome for the `StatusBar` fixture scenario. Preview windows
/// mount no terminal pane, so without this the scrollback and terminal-access
/// segments could never be clicked through. `UBRA_STATUSBAR_ACCESS` picks the
/// access variant: `active-elsewhere` (default), `attaching`, `reconnecting`,
/// `unavailable`, or `live` (hides the access segment). `UBRA_STATUSBAR_SCROLLED=0`
/// hides the scrollback segment; it shows by default.
pub(crate) fn preview_chrome(id: &SessionId) -> TerminalChromeState {
    let access = match std::env::var("UBRA_STATUSBAR_ACCESS")
        .as_deref()
        .unwrap_or("active-elsewhere")
    {
        "attaching" => TerminalAccess::Attaching,
        "reconnecting" => TerminalAccess::Reconnecting,
        "unavailable" => TerminalAccess::Unavailable,
        "live" => TerminalAccess::Live,
        _ => TerminalAccess::ActiveElsewhere,
    };
    TerminalChromeState {
        id: id.clone(),
        scrolled_back: !std::env::var("UBRA_STATUSBAR_SCROLLED").is_ok_and(|value| value == "0"),
        access,
    }
}

/// Preview-only context for the `StatusBar` fixture scenario: a Codex
/// conversation at 67% of its window, as if already loaded and reported.
pub(crate) fn preview_context() -> ContextUsage {
    ContextUsage {
        tokens: 134_000,
        window: 200_000,
    }
}

/// Preview-only update state for the `StatusBar` fixture scenario.
/// `UBRA_STATUSBAR_UPDATE` picks the phase: `available` (default),
/// `downloading`, `ready`, `failed`, `checking`, `uptodate`, or `off`.
pub(crate) fn preview_update(current_version: &str) -> UpdateState {
    let release = || ubra_updater::Release {
        version: "0.5.0".to_owned(),
        ..ubra_updater::Release::default()
    };
    let phase = match std::env::var("UBRA_STATUSBAR_UPDATE")
        .as_deref()
        .unwrap_or("available")
    {
        "downloading" => UpdatePhase::Downloading {
            release: release(),
            progress: 0.43,
        },
        "ready" => UpdatePhase::Ready(release()),
        "failed" => UpdatePhase::Failed("connection reset".into()),
        "checking" => UpdatePhase::Checking,
        "uptodate" => UpdatePhase::UpToDate,
        "off" | "idle" => UpdatePhase::Idle,
        _ => UpdatePhase::Available(release()),
    };
    UpdateState {
        phase,
        current_version: current_version.into(),
        ..UpdateState::default()
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

fn announcement(
    needs_input: usize,
    done_unseen: usize,
    unread: usize,
    update: bool,
) -> SharedString {
    let mut parts = Vec::new();
    if needs_input > 0 {
        parts.push(plural(
            needs_input,
            "session needs input",
            "sessions need input",
        ));
    }
    if done_unseen > 0 {
        parts.push(plural(done_unseen, "session finished", "sessions finished"));
    }
    if unread > 0 {
        parts.push(plural(
            unread,
            "unread notification",
            "unread notifications",
        ));
    }
    if update {
        parts.push("update available".to_owned());
    }
    if parts.is_empty() {
        return "All caught up".into();
    }
    parts.join("; ").into()
}

/// One segment activated, by pointer or keyboard. RootView owns every
/// consequence; the bar never navigates, copies, or sends anything itself.
#[derive(Clone, Debug)]
pub(crate) enum StatusBarEvent {
    FocusSession(SessionId),
    OpenGitReview(SessionId),
    OpenWorktrees(SessionId),
    OpenBrowser(SessionId),
    OpenDetails(SessionId),
    OpenUsage(SessionId),
    JumpToLive(SessionId),
    ExplainTerminalAccess(SessionId),
    NextAttention,
    ToggleNotifications,
    Update(UpdateCommand),
    RetryConnection,
}

struct SegmentFocus {
    status: FocusHandle,
    location: FocusHandle,
    scrollback: FocusHandle,
    access: FocusHandle,
    git: FocusHandle,
    worktree: FocusHandle,
    message: FocusHandle,
    progress: FocusHandle,
    context: FocusHandle,
    ports: FocusHandle,
    attention: FocusHandle,
    bell: FocusHandle,
    update: FocusHandle,
}

impl SegmentFocus {
    fn new(cx: &mut App) -> Self {
        Self {
            status: cx.focus_handle().tab_stop(true),
            location: cx.focus_handle().tab_stop(true),
            scrollback: cx.focus_handle().tab_stop(true),
            access: cx.focus_handle().tab_stop(true),
            git: cx.focus_handle().tab_stop(true),
            worktree: cx.focus_handle().tab_stop(true),
            message: cx.focus_handle().tab_stop(true),
            progress: cx.focus_handle().tab_stop(true),
            context: cx.focus_handle().tab_stop(true),
            ports: cx.focus_handle().tab_stop(true),
            attention: cx.focus_handle().tab_stop(true),
            bell: cx.focus_handle().tab_stop(true),
            update: cx.focus_handle().tab_stop(true),
        }
    }

    fn handle(&self, segment: Segment) -> &FocusHandle {
        match segment {
            Segment::Status => &self.status,
            Segment::Location => &self.location,
            Segment::Scrollback => &self.scrollback,
            Segment::Access => &self.access,
            Segment::Git => &self.git,
            Segment::Worktree => &self.worktree,
            Segment::Message => &self.message,
            Segment::Progress => &self.progress,
            Segment::Context => &self.context,
            Segment::Ports => &self.ports,
            Segment::Attention => &self.attention,
            Segment::Bell => &self.bell,
            Segment::Update => &self.update,
        }
    }
}

/// Retained view over a pushed [`StatusBarModel`]. RootView mounts it with
/// [`Entity::cached`], so GPUI recycles its prepaint until a pushed model
/// differs: terminal output and sidebar motion cannot reach segment code.
pub(crate) struct StatusBarView {
    model: StatusBarModel,
    focus: SegmentFocus,
    #[cfg(test)]
    render_count: usize,
}

impl StatusBarView {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            model: StatusBarModel::default(),
            focus: SegmentFocus::new(cx),
            #[cfg(test)]
            render_count: 0,
        }
    }

    /// Push a rebuilt model. Repaints only when something the bar shows
    /// actually differs; identical publications are free.
    pub(crate) fn set_model(&mut self, model: StatusBarModel, cx: &mut Context<Self>) {
        if self.model != model {
            self.model = model;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn render_count(&self) -> usize {
        self.render_count
    }

    #[cfg(test)]
    pub(crate) fn model(&self) -> &StatusBarModel {
        &self.model
    }

    #[cfg(test)]
    pub(crate) fn focus_location_for_test(&self, window: &mut Window, cx: &mut App) {
        self.focus.location.focus(window, cx);
    }
}

impl Render for StatusBarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.render_count += 1;
        }
        let colors = self.model.colors;
        let tier = WidthTier::for_width(f32::from(window.viewport_size().width));
        let visibility = tier.visibility(self.model.visibility);
        let mut left = div()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0));
        if let Some(message) = self.message_segment(colors, cx) {
            left = left.child(message);
        } else {
            if let Some(session) = self.status_segment(tier, colors, cx) {
                left = left.child(session);
            }
            if let Some(location) = self.location_segment(colors, cx) {
                left = left.child(location);
            }
            if visibility.git
                && let Some(git) = self.git_segment(tier, colors, cx)
            {
                left = left.child(git);
            }
            if visibility.worktree
                && let Some(worktree) = self.worktree_segment(colors, cx)
            {
                left = left.child(worktree);
            }
            if self.model.session.is_none() {
                left = left.child(
                    div()
                        .px(px(6.0))
                        .text_size(px(Typo::META.size))
                        .text_color(colors.primary)
                        .child("No session"),
                );
            }
        }
        let mut center = div()
            .flex_none()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0));
        if let Some(progress) = self.progress_segment(colors, cx) {
            center = center.child(progress);
        }
        if visibility.context
            && let Some(context) = self.context_segment(colors, cx)
        {
            center = center.child(context);
        }
        if visibility.ports
            && let Some(ports) = self.ports_segment(tier, colors, cx)
        {
            center = center.child(ports);
        }
        if let Some(scrollback) = self.scrollback_segment(tier, colors, cx) {
            center = center.child(scrollback);
        }
        if let Some(access) = self.access_segment(tier, colors, cx) {
            center = center.child(access);
        }
        let mut right = div()
            .flex_none()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0));
        if let Some(attention) = self.attention_segment(tier, colors, cx) {
            right = right.child(attention);
        }
        if let Some(bell) = self.bell_segment(colors, cx) {
            right = right.child(bell);
        }
        if let Some(update) = self.update_segment(tier, colors, cx) {
            right = right.child(update);
        }
        div()
            .id("status-bar")
            .debug_selector(|| "STATUS_BAR".to_owned())
            .role(Role::Status)
            .aria_label(self.model.announcement.clone())
            .w_full()
            .h(px(STATUS_BAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .whitespace_nowrap()
            .text_size(px(Typo::META.size))
            .line_height(gpui::relative(1.2))
            .text_color(colors.primary)
            .px(px(4.0))
            .gap(px(8.0))
            .border_t_1()
            .border_color(colors.primary.alpha(0.08))
            .bg(colors.sidebar_surface())
            .child(left)
            .child(center)
            .child(right)
    }
}

impl EventEmitter<StatusBarEvent> for StatusBarView {}

// Segment builders. They borrow precomputed model strings and describe
// layout only: no formatting, no store reads, no follow-up frames.
impl StatusBarView {
    fn button(
        &self,
        segment: Segment,
        selector: &'static str,
        label: SharedString,
        event: StatusBarEvent,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let key_event = event.clone();
        div()
            .id(selector)
            .debug_selector(move || selector.to_owned())
            .role(Role::Button)
            .aria_label(label.clone())
            .track_focus(self.focus.handle(segment))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .whitespace_nowrap()
            .h_full()
            .px(px(6.0))
            .gap(px(4.0))
            .rounded(px(Radius::CHIP))
            .border_1()
            .border_color(colors.primary.alpha(0.0))
            .focus_visible(move |style| style.border_color(colors.primary))
            .hover(move |button| button.bg(Fill::hover(colors, true)))
            .active(move |button| button.bg(colors.primary.alpha(0.12)))
            .warm_tooltip(move |_, cx| {
                cx.new(|_| crate::palette_chrome::PaletteTooltip(label.to_string(), colors))
                    .into()
            })
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(event.clone());
            }))
            .on_key_down(cx.listener(move |_, key: &KeyDownEvent, _, cx| {
                if matches!(key.keystroke.key.as_str(), "enter" | "space") {
                    cx.emit(key_event.clone());
                    cx.stop_propagation();
                }
            }))
    }

    fn context_segment(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.model.session.as_ref()?;
        let label = session.context_label.clone()?;
        let detail = session.context_aria.clone()?;
        Some(
            self.button(
                Segment::Context,
                "status-bar-context",
                detail,
                StatusBarEvent::OpenUsage(session.id.clone()),
                colors,
                cx,
            )
            .child(
                div()
                    .max_w(px(100.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Typo::META.size))
                    .child(label),
            )
            .into_any_element(),
        )
    }

    fn scrollback_segment(
        &self,
        tier: WidthTier,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self
            .model
            .session
            .as_ref()
            .filter(|session| session.scrolled_back)?;
        Some(
            self.button(
                Segment::Scrollback,
                "status-bar-scrollback",
                "Scrolled back. Jump to live output.".into(),
                StatusBarEvent::JumpToLive(session.id.clone()),
                colors,
                cx,
            )
            .child(Icon::new(
                IconName::ArrowDown,
                SEGMENT_ICON,
                colors.secondary,
            ))
            .children(
                (!tier.compact_feedback())
                    .then(|| div().text_size(px(Typo::META.size)).child("Scrolled back")),
            )
            .into_any_element(),
        )
    }

    fn access_segment(
        &self,
        tier: WidthTier,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.model.session.as_ref()?;
        let (label, detail, icon) = match session.access? {
            TerminalAccess::Live => return None,
            TerminalAccess::ActiveElsewhere => (
                "Active elsewhere",
                "This terminal is active in another view. Focus it to type here.",
                IconName::Keyboard,
            ),
            TerminalAccess::Attaching => (
                "Attaching…",
                "Terminal attaching. Input is unavailable until attachment completes.",
                IconName::Refresh,
            ),
            TerminalAccess::Reconnecting => (
                "Reconnecting…",
                "Terminal reconnecting. Input is unavailable until the connection returns.",
                IconName::Refresh,
            ),
            TerminalAccess::Unavailable => (
                "Unavailable",
                "Terminal unavailable. The terminal attachment was refused.",
                IconName::Warning,
            ),
        };
        Some(
            self.button(
                Segment::Access,
                "status-bar-access",
                detail.into(),
                StatusBarEvent::ExplainTerminalAccess(session.id.clone()),
                colors,
                cx,
            )
            .child(Icon::new(icon, SEGMENT_ICON, colors.secondary))
            .children(
                (!tier.compact_feedback())
                    .then(|| div().text_size(px(Typo::META.size)).child(label)),
            )
            .into_any_element(),
        )
    }

    /// Initialization feedback in the left slot: connection and resume
    /// progress. A persistent tone paints plain chrome with no consequence.
    fn message_segment(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let message = self.model.message.as_ref()?;
        let (icon, tint) = match message.tone {
            StatusMessageTone::Progress => (IconName::Refresh, colors.secondary),
            StatusMessageTone::Success => (IconName::Check, Ink::on_surface(Ink::FRESH, colors)),
            StatusMessageTone::Warning => {
                (IconName::Warning, Ink::on_surface(Ink::ATTENTION, colors))
            }
        };
        if message.action.is_none() {
            return Some(
                div()
                    .id("status-bar-message")
                    .debug_selector(|| "status-bar-message".to_owned())
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .px(px(6.0))
                    .gap(px(4.0))
                    .aria_label(message.aria.clone())
                    .warm_tooltip({
                        let detail = message.aria.clone();
                        move |_, cx| {
                            cx.new(|_| {
                                crate::palette_chrome::PaletteTooltip(detail.to_string(), colors)
                            })
                            .into()
                        }
                    })
                    .child(Icon::new(icon, SEGMENT_ICON, tint))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(Typo::META.size))
                            .font_weight(Typo::META.weight)
                            .text_color(colors.primary)
                            .child(message.text.clone()),
                    )
                    .into_any_element(),
            );
        }
        let button = self
            .button(
                Segment::Message,
                "status-bar-message",
                message.aria.clone(),
                StatusBarEvent::RetryConnection,
                colors,
                cx,
            )
            .flex_1()
            .min_w(px(0.0))
            .child(Icon::new(icon, SEGMENT_ICON, tint))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(Typo::META.size))
                    .font_weight(Typo::META.weight)
                    .text_color(colors.primary)
                    .child(message.text.clone()),
            );
        Some(button.into_any_element())
    }

    fn status_segment(
        &self,
        tier: WidthTier,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.model.session.as_ref()?;
        let mark =
            ubra_ui::StatusGlyph::new(session.glyph, session.status, 14.0, colors).rendered_mark();
        let button = self
            .button(
                Segment::Status,
                "status-bar-session",
                session.status_aria.clone(),
                StatusBarEvent::FocusSession(session.id.clone()),
                colors,
                cx,
            )
            .flex_shrink(1.0)
            .min_w(px(0.0))
            .max_w(px(340.0))
            .child(mark)
            .child(
                div()
                    .flex_none()
                    .text_size(px(Typo::META.size))
                    .font_weight(Typo::META.weight)
                    .text_color(colors.primary)
                    .child(session.status_label.clone()),
            )
            .children((tier != WidthTier::Minimal).then(|| {
                div()
                    .debug_selector(|| "status-bar-session-title".to_owned())
                    .flex_shrink(1.0)
                    .min_w(px(0.0))
                    .max_w(px(180.0))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.primary)
                    .child(session.display.clone())
            }));
        Some(button.into_any_element())
    }

    fn location_segment(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.model.session.as_ref()?;
        let detail = session.location_aria.clone();
        let button = self
            .button(
                Segment::Location,
                "status-bar-location",
                detail,
                StatusBarEvent::OpenDetails(session.id.clone()),
                colors,
                cx,
            )
            .flex_shrink(1.0)
            .min_w(px(if session.location_tail.is_some() {
                92.0
            } else {
                48.0
            }))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Typo::META.size))
                    .child(session.location.clone()),
            )
            .children(session.location_tail.as_ref().map(|tail| {
                div()
                    .flex_shrink(1.0)
                    .w(px(110.0))
                    .min_w(px(40.0))
                    .max_w(px(110.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Typo::META.size))
                    .child(tail.clone())
            }));
        Some(button.into_any_element())
    }

    fn git_segment(
        &self,
        tier: WidthTier,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.model.session.as_ref()?;
        let branch = session.branch.as_ref()?;
        let button = self
            .button(
                Segment::Git,
                "status-bar-git",
                session.git_aria.clone().unwrap_or_else(|| "Git".into()),
                StatusBarEvent::OpenGitReview(session.id.clone()),
                colors,
                cx,
            )
            .flex_shrink(1.0)
            .min_w(px(30.0))
            .child(Icon::new(IconName::Branch, SEGMENT_ICON, colors.secondary))
            .child(
                div()
                    .flex_shrink(1.0)
                    .min_w(px(0.0))
                    .max_w(px(if tier == WidthTier::Compact {
                        72.0
                    } else {
                        140.0
                    }))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(Typo::META_MONO.size))
                    .text_color(colors.primary)
                    .child(branch.clone()),
            );
        Some(button.into_any_element())
    }

    fn worktree_segment(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.model.session.as_ref()?;
        let button = self
            .button(
                Segment::Worktree,
                "status-bar-worktree",
                session.worktree_aria.clone(),
                StatusBarEvent::OpenWorktrees(session.id.clone()),
                colors,
                cx,
            )
            .flex_shrink(1.0)
            .min_w(px(30.0))
            .child(Icon::new(
                IconName::Worktree,
                SEGMENT_ICON,
                colors.secondary,
            ))
            .child(
                div()
                    .flex_shrink(1.0)
                    .min_w(px(0.0))
                    .max_w(px(110.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Typo::META.size))
                    .font_weight(Typo::META.weight)
                    .text_color(colors.primary)
                    .child(session.project.clone()),
            );
        Some(button.into_any_element())
    }

    fn progress_segment(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.model.session.as_ref()?;
        let progress = session.progress?;
        let label = session.progress_label.clone()?;
        let tint = match progress.state {
            TerminalProgressState::Error => Ink::on_surface(Ink::DANGER, colors),
            TerminalProgressState::Paused => Ink::on_surface(Ink::ATTENTION, colors),
            _ => colors.primary,
        };
        let width = 56.0 * f32::from(progress.percent) / 100.0;
        let track = div()
            .flex_none()
            .w(px(56.0))
            .h(px(4.0))
            .bg(colors.primary.alpha(0.14))
            .child(div().w(px(width)).h_full().bg(tint));
        let button = self
            .button(
                Segment::Progress,
                "status-bar-progress",
                session
                    .progress_aria
                    .clone()
                    .unwrap_or_else(|| "Progress".into()),
                StatusBarEvent::FocusSession(session.id.clone()),
                colors,
                cx,
            )
            .child(track)
            .child(
                div()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(Typo::META_MONO.size))
                    .text_color(colors.primary)
                    .child(label),
            );
        Some(button.into_any_element())
    }

    fn ports_segment(
        &self,
        tier: WidthTier,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.model.session.as_ref()?;
        let label = session.ports_label.as_ref()?;
        let button = self
            .button(
                Segment::Ports,
                "status-bar-ports",
                session.ports_aria.clone().unwrap_or_else(|| "Ports".into()),
                StatusBarEvent::OpenBrowser(session.id.clone()),
                colors,
                cx,
            )
            .child(Icon::new(IconName::Server, SEGMENT_ICON, colors.secondary))
            .children((tier != WidthTier::Compact).then(|| {
                div()
                    .max_w(px(90.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(Typo::META_MONO.size))
                    .text_color(colors.primary)
                    .child(label.clone())
            }));
        Some(button.into_any_element())
    }

    fn attention_segment(
        &self,
        tier: WidthTier,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let label = if tier == WidthTier::Minimal {
            self.model.attention_count.as_ref()?
        } else {
            self.model.attention_label.as_ref()?
        };
        let aria = self.model.attention_aria.clone()?;
        let destructive = self.model.session.as_ref().is_some_and(|session| {
            session.status == ubra_ui::StatusState::NeedsInput { destructive: true }
        });
        let (icon, tint) = if self.model.needs_input > 0 {
            (
                IconName::Warning,
                Ink::on_surface(
                    if destructive {
                        Ink::DANGER
                    } else {
                        Ink::ATTENTION
                    },
                    colors,
                ),
            )
        } else {
            (IconName::Check, Ink::on_surface(Ink::FRESH, colors))
        };
        let button = self
            .button(
                Segment::Attention,
                "status-bar-attention",
                aria,
                StatusBarEvent::NextAttention,
                colors,
                cx,
            )
            .child(Icon::new(icon, SEGMENT_ICON, tint))
            .child(
                div()
                    .max_w(px(if tier == WidthTier::Minimal {
                        36.0
                    } else {
                        110.0
                    }))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Typo::META.size))
                    .font_weight(Typo::META.weight)
                    .text_color(colors.primary)
                    .child(label.clone()),
            );
        Some(button.into_any_element())
    }

    fn bell_segment(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let label = self.model.bell_label.as_ref()?;
        let aria = self.model.bell_aria.clone()?;
        let button = self
            .button(
                Segment::Bell,
                "status-bar-bell",
                aria,
                StatusBarEvent::ToggleNotifications,
                colors,
                cx,
            )
            .child(Icon::new(IconName::Bell, SEGMENT_ICON, colors.secondary))
            .child(
                div()
                    .max_w(px(36.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(Typo::META_MONO.size))
                    .text_color(colors.primary)
                    .child(label.clone()),
            );
        Some(button.into_any_element())
    }

    fn update_segment(
        &self,
        tier: WidthTier,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let update = self.model.update.as_ref()?;
        let (icon, tint) = if update.danger {
            (IconName::Warning, Ink::on_surface(Ink::DANGER, colors))
        } else if update.action.is_some() {
            (IconName::Download, Ink::on_surface(Ink::FRESH, colors))
        } else {
            (IconName::Refresh, colors.secondary)
        };
        let detail = update.detail.clone();
        // Phases without an action (downloading, installing, checking) are
        // plain status: focusable chrome with no consequence would lie.
        let Some(action) = update.action else {
            return Some(
                div()
                    .id("status-bar-update")
                    .debug_selector(|| "STATUS_BAR_UPDATE".to_owned())
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .px(px(6.0))
                    .gap(px(4.0))
                    .aria_label(update.detail.clone())
                    .warm_tooltip(move |_, cx| {
                        cx.new(|_| {
                            crate::palette_chrome::PaletteTooltip(detail.to_string(), colors)
                        })
                        .into()
                    })
                    .child(Icon::new(icon, SEGMENT_ICON, tint))
                    .children((tier != WidthTier::Minimal).then(|| {
                        div()
                            .max_w(px(90.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(Typo::META.size))
                            .font_weight(Typo::META.weight)
                            .text_color(colors.primary)
                            .child(update.label.clone())
                    }))
                    .into_any_element(),
            );
        };
        let button = self
            .button(
                Segment::Update,
                "status-bar-update",
                update.detail.clone(),
                StatusBarEvent::Update(action.command()),
                colors,
                cx,
            )
            .child(Icon::new(icon, SEGMENT_ICON, tint))
            .children((tier != WidthTier::Minimal).then(|| {
                div()
                    .max_w(px(90.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Typo::META.size))
                    .font_weight(Typo::META.weight)
                    .text_color(colors.primary)
                    .child(update.label.clone())
            }));
        Some(button.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use ubra_proto::{
        AgentKind, DateMillis, NeedsInputKind, ProjectId, Resumability, SessionStatus, TitleSource,
    };

    use crate::updates::UpdatePhase;

    use super::*;

    fn record(id: &str) -> SessionRecord {
        SessionRecord {
            attention_state: None,
            id: SessionId::new(id),
            kind: AgentKind::CLAUDE_CODE,
            cwd: format!("/work/{id}"),
            project_id: ProjectId::new("acme"),
            worktree_path: None,
            git_branch: None,
            title: id.to_owned(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Idle,
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

    fn inputs<'a>(
        sessions: &'a HashMap<SessionId, Arc<SessionRecord>>,
        selected: Option<&'a SessionRecord>,
        projects: &'a HashMap<ProjectId, Project>,
        migrating: &'a HashSet<SessionId>,
        unread: usize,
        update: &'a UpdateState,
    ) -> ModelInputs<'a> {
        ModelInputs {
            sessions,
            selected,
            projects,
            hosts: &[],
            chrome: None,
            context: None,
            visibility: StatusBarVisibility::default(),
            migrating,
            catalog: None,
            unread,
            update,
            message: None,
            colors: SemanticColors::light(),
        }
    }

    fn idle_update() -> UpdateState {
        UpdateState {
            current_version: "1.0".to_owned(),
            ..UpdateState::default()
        }
    }

    #[test]
    fn status_bar_width_tiers_preserve_enabled_metadata_in_priority_order() {
        let all = StatusBarVisibility::default();
        for (width, tier, visibility) in [
            (1200.0, WidthTier::Full, all),
            (
                1199.0,
                WidthTier::Reduced,
                StatusBarVisibility {
                    worktree: false,
                    ..all
                },
            ),
            (
                1000.0,
                WidthTier::Reduced,
                StatusBarVisibility {
                    worktree: false,
                    ..all
                },
            ),
            (
                999.0,
                WidthTier::Compact,
                StatusBarVisibility {
                    context: false,
                    worktree: false,
                    ..all
                },
            ),
            (
                900.0,
                WidthTier::Compact,
                StatusBarVisibility {
                    context: false,
                    worktree: false,
                    ..all
                },
            ),
            (
                899.0,
                WidthTier::Minimal,
                StatusBarVisibility {
                    context: false,
                    git: false,
                    worktree: false,
                    ports: false,
                },
            ),
            (
                700.0,
                WidthTier::Minimal,
                StatusBarVisibility {
                    context: false,
                    git: false,
                    worktree: false,
                    ports: false,
                },
            ),
        ] {
            assert_eq!(WidthTier::for_width(width), tier);
            assert_eq!(tier.visibility(all), visibility);
        }
        let none = StatusBarVisibility {
            context: false,
            git: false,
            worktree: false,
            ports: false,
        };
        for tier in [
            WidthTier::Full,
            WidthTier::Reduced,
            WidthTier::Compact,
            WidthTier::Minimal,
        ] {
            assert_eq!(
                tier.visibility(none),
                none,
                "width must never re-enable a hidden indicator"
            );
        }
    }

    #[test]
    fn status_bar_visibility_changes_presentation_without_discarding_session_facts() {
        let mut selected = record("selected");
        selected.git_branch = Some("feature/status".into());
        selected.foreground_ports = Some(vec![PortInfo {
            port: 3000,
            process_name: "node".into(),
        }]);
        let sessions = HashMap::new();
        let projects = HashMap::new();
        let migrating = HashSet::new();
        let update = idle_update();
        let build = |visibility| {
            let mut input = inputs(
                &sessions,
                Some(&selected),
                &projects,
                &migrating,
                2,
                &update,
            );
            input.visibility = visibility;
            input.context = Some(ContextUsage {
                tokens: 72,
                window: 100,
            });
            input.chrome = Some(TerminalChromeState {
                id: selected.id.clone(),
                scrolled_back: true,
                access: TerminalAccess::ActiveElsewhere,
            });
            StatusBarModel::build(input)
        };
        let shown = build(StatusBarVisibility::default());
        for visibility in [
            StatusBarVisibility {
                context: false,
                ..StatusBarVisibility::default()
            },
            StatusBarVisibility {
                git: false,
                ..StatusBarVisibility::default()
            },
            StatusBarVisibility {
                worktree: false,
                ..StatusBarVisibility::default()
            },
            StatusBarVisibility {
                ports: false,
                ..StatusBarVisibility::default()
            },
        ] {
            let hidden = build(visibility);
            assert_ne!(
                shown, hidden,
                "visibility must invalidate the equality-gated view"
            );
            assert_eq!(shown.session, hidden.session);
            assert_eq!(shown.bell_label, hidden.bell_label);
            assert_eq!(hidden.visibility, visibility);
        }
    }

    #[test]
    fn status_bar_context_rounds_reported_occupancy_without_capping_or_inventing_zero() {
        let selected = record("selected");
        let sessions = HashMap::new();
        let projects = HashMap::new();
        let migrating = HashSet::new();
        let update = idle_update();
        for (context, expected) in [
            (
                Some(ContextUsage {
                    tokens: 72000,
                    window: 100000,
                }),
                Some("Context 72%"),
            ),
            (
                Some(ContextUsage {
                    tokens: 0,
                    window: 100000,
                }),
                Some("Context 0%"),
            ),
            (
                Some(ContextUsage {
                    tokens: 1450,
                    window: 1000,
                }),
                Some("Context 145%"),
            ),
            (
                Some(ContextUsage {
                    tokens: 725,
                    window: 1000,
                }),
                Some("Context 73%"),
            ),
            (
                Some(ContextUsage {
                    tokens: -1,
                    window: 100000,
                }),
                None,
            ),
            (
                Some(ContextUsage {
                    tokens: 72000,
                    window: 0,
                }),
                None,
            ),
            (None, None),
        ] {
            let mut input = inputs(
                &sessions,
                Some(&selected),
                &projects,
                &migrating,
                0,
                &update,
            );
            input.context = context;
            let model = StatusBarModel::build(input);
            assert_eq!(model.session.unwrap().context_label.as_deref(), expected);
        }
    }

    #[test]
    fn status_bar_chrome_is_identity_scoped_and_conditions_are_independent() {
        let selected = record("selected");
        let sessions = HashMap::new();
        let projects = HashMap::new();
        let migrating = HashSet::new();
        let update = idle_update();
        let build = |id: SessionId, access, scrolled_back| {
            let mut input = inputs(
                &sessions,
                Some(&selected),
                &projects,
                &migrating,
                0,
                &update,
            );
            input.chrome = Some(TerminalChromeState {
                id,
                access,
                scrolled_back,
            });
            StatusBarModel::build(input).session.unwrap()
        };
        let both = build(selected.id.clone(), TerminalAccess::ActiveElsewhere, true);
        assert!(both.scrolled_back);
        assert_eq!(both.access, Some(TerminalAccess::ActiveElsewhere));
        let wrong = build(
            SessionId::new("other"),
            TerminalAccess::ActiveElsewhere,
            true,
        );
        assert!(!wrong.scrolled_back);
        assert_eq!(wrong.access, None);
        let live = build(selected.id.clone(), TerminalAccess::Live, true);
        assert!(live.scrolled_back);
        assert_eq!(live.access, None);
    }

    #[test]
    fn status_bar_location_uses_session_host_even_without_catalog() {
        let mut session = record("remote");
        assert_eq!(execution_location(&session, &[]).0.as_str(), "Local");
        session.host = Some("staging".into());
        let missing = execution_location(&session, &[]);
        assert_eq!(missing.0.as_str(), "SSH ·");
        assert_eq!(missing.1.unwrap().as_str(), "staging");
        let hosts = [ubra_proto::HostEntry {
            id: "staging".into(),
            name: Some("Staging".into()),
            ssh: "deploy@staging".into(),
            default_cwd: None,
            node: None,
        }];
        let known = execution_location(&session, &hosts);
        assert_eq!(known.0.as_str(), "SSH ·");
        assert_eq!(known.1.unwrap().as_str(), "Staging");
        assert!(known.2.contains("deploy@staging"));
    }

    #[test]
    fn aggregates_skip_archived_and_count_attention() {
        let mut blocked = record("blocked");
        blocked.status = SessionStatus::NeedsInput(NeedsInputKind::Question);
        let mut archived = record("archived");
        archived.status = SessionStatus::NeedsInput(NeedsInputKind::Question);
        archived.archived_at = Some(DateMillis(2.0));
        let mut finished = record("finished");
        finished.last_turn_completed_at = Some(DateMillis(3.0));
        finished.last_seen_at = Some(DateMillis(1.0));
        let working = SessionRecord {
            status: SessionStatus::Working,
            ..record("working")
        };
        let sessions: HashMap<_, _> = [&blocked, &archived, &finished, &working]
            .into_iter()
            .map(|session| (session.id.clone(), Arc::new(session.clone())))
            .collect();
        let projects = HashMap::new();
        let migrating = HashSet::new();
        let update = idle_update();
        let model = StatusBarModel::build(inputs(
            &sessions,
            Some(&blocked),
            &projects,
            &migrating,
            0,
            &update,
        ));
        assert_eq!(model.needs_input, 1);
        assert_eq!(model.done_unseen, 1);
        assert_eq!(
            model.attention_label.as_ref().map(SharedString::as_str),
            Some("1 needs input"),
            "blockers win over finished sessions"
        );
        assert!(model.attention_aria.as_ref().unwrap().contains("next one"));
        assert!(model.bell_label.is_none());
        assert!(model.update.is_none());
        let session = model.session.expect("selected session");
        assert_eq!(session.display.as_str(), "claude-code · blocked");
        assert_eq!(
            session.status,
            ubra_ui::StatusState::NeedsInput { destructive: false }
        );
    }

    #[test]
    fn initialization_messages_carry_their_copy_and_hold() {
        let sessions = HashMap::new();
        let projects = HashMap::new();
        let migrating = HashSet::new();
        let update = idle_update();
        let model = |message: Option<StatusMessage>| {
            let mut inputs = inputs(&sessions, None, &projects, &migrating, 0, &update);
            inputs.message = message;
            StatusBarModel::build(inputs)
        };

        assert_eq!(
            StatusMessage::resuming(2, 5).text.as_str(),
            "Resuming 2 of 5…"
        );

        let failed = StatusMessage::resume_summary(3, 5, Some("boom"));
        assert_eq!(failed.tone, StatusMessageTone::Warning);
        assert_eq!(
            failed.detail.as_ref().map(SharedString::as_str),
            Some("Some didn’t resume: boom")
        );
        assert_eq!(
            failed.aria.as_str(),
            "Resumed 3 of 5 sessions. Some didn’t resume: boom"
        );
        let complete = StatusMessage::resume_summary(2, 2, None);
        assert_eq!(complete.text.as_str(), "Resumed 2 sessions");
        assert_eq!(complete.tone, StatusMessageTone::Success);
        assert_eq!(StatusMessage::connecting().hold(), std::time::Duration::MAX);
        assert_eq!(
            StatusMessage::resume_summary(1, 1, None).hold(),
            std::time::Duration::from_secs(4)
        );

        let message = StatusMessage::reconnecting();
        let live = model(Some(message.clone()));
        assert!(live.message.is_some());
        assert_eq!(live.announcement, message.aria);
        assert_eq!(model(None).announcement.as_str(), "All caught up");
    }

    #[test]
    fn empty_model_hides_everything() {
        let model = StatusBarModel::default();
        assert!(model.session.is_none());
        assert!(model.attention_label.is_none());
        assert!(model.bell_label.is_none());
        assert!(model.update.is_none());
        assert_eq!(model.announcement.as_str(), "All caught up");
    }

    #[test]
    fn identical_publications_compare_equal() {
        let mut selected = record("one");
        selected.git_branch = Some("main".to_owned());
        selected.terminal_progress = Some(TerminalProgress {
            state: TerminalProgressState::Normal,
            percent: 42,
        });
        selected.foreground_ports = Some(vec![PortInfo {
            port: 3000,
            process_name: "node".to_owned(),
        }]);
        let sessions: HashMap<_, _> = [(&selected.id, Arc::new(selected.clone()))]
            .into_iter()
            .map(|(id, record)| (id.clone(), record))
            .collect();
        let projects = HashMap::new();
        let migrating = HashSet::new();
        let update = idle_update();
        let build = || {
            StatusBarModel::build(inputs(
                &sessions,
                Some(&selected),
                &projects,
                &migrating,
                3,
                &update,
            ))
        };
        assert_eq!(build(), build());
        let session = build().session.unwrap();
        assert_eq!(
            session.branch.as_ref().map(SharedString::as_str),
            Some("main")
        );
        assert_eq!(
            session.progress_label.as_ref().map(SharedString::as_str),
            Some("42%")
        );
        assert_eq!(
            session.ports_label.as_ref().map(SharedString::as_str),
            Some(":3000")
        );
        assert_eq!(build().progress_percent(), Some(42));
    }

    #[test]
    fn update_mapping_mirrors_the_pill() {
        let release = ubra_updater::Release {
            version: "9.9".to_owned(),
            ..ubra_updater::Release::default()
        };
        let state = |phase| UpdateState {
            phase,
            current_version: "1.0".to_owned(),
            user_initiated: true,
            ..UpdateState::default()
        };
        let available = update_segment(&state(UpdatePhase::Available(release.clone()))).unwrap();
        assert_eq!(available.label.as_str(), "Update");
        assert_eq!(available.action, Some(UpdateAction::Download));
        assert!(!available.danger);
        let downloading = update_segment(&state(UpdatePhase::Downloading {
            release: release.clone(),
            progress: 0.42,
        }))
        .unwrap();
        assert_eq!(downloading.label.as_str(), "42%");
        assert_eq!(downloading.action, None);
        let ready = update_segment(&state(UpdatePhase::Ready(release))).unwrap();
        assert_eq!(ready.action, Some(UpdateAction::Install));
        let failed = update_segment(&state(UpdatePhase::Failed("nope".to_owned()))).unwrap();
        assert_eq!(failed.action, Some(UpdateAction::Dismiss));
        assert!(failed.danger);
        assert!(update_segment(&state(UpdatePhase::Idle)).is_none());
        let quiet = UpdateState {
            user_initiated: false,
            ..state(UpdatePhase::UpToDate)
        };
        assert!(update_segment(&quiet).is_none());
    }

    #[test]
    fn ports_prefer_live_foreground_over_scans() {
        let mut selected = record("one");
        selected.listening_ports = Some(vec![PortInfo {
            port: 1111,
            process_name: "stale".to_owned(),
        }]);
        selected.foreground_ports = Some(vec![
            PortInfo {
                port: 3000,
                process_name: "node".to_owned(),
            },
            PortInfo {
                port: 5173,
                process_name: "vite".to_owned(),
            },
        ]);
        assert_eq!(
            ports_label(&selected).as_ref().map(SharedString::as_str),
            Some(":3000 +1")
        );
        assert_eq!(
            ports_copy(&selected).as_ref().map(SharedString::as_str),
            Some("3000, 5173")
        );
        selected.foreground_ports = Some(Vec::new());
        assert_eq!(
            ports_label(&selected).as_ref().map(SharedString::as_str),
            Some(":1111")
        );
        selected.listening_ports = None;
        assert!(ports_label(&selected).is_none());
        assert!(ports_copy(&selected).is_none());
    }

    #[test]
    fn project_falls_back_to_path_tail() {
        let projects = HashMap::new();
        let selected = record("one");
        assert_eq!(project_label(&selected, &projects).as_str(), "one");
        let mut worktree = record("two");
        worktree.worktree_path = Some("/work/acme-fix".to_owned());
        assert_eq!(project_label(&worktree, &projects).as_str(), "acme-fix");
    }

    #[test]
    fn status_bar_mockup_fixture_lights_every_segment() {
        // Guards the click-through mockup: every one of the twelve segments
        // must be present in the same model, or the scenario rots silently.
        let fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::StatusBar);
        let sessions: HashMap<_, _> = fixture
            .list
            .sessions
            .iter()
            .map(|session| (session.id.clone(), Arc::new(session.clone())))
            .collect();
        let projects: HashMap<_, _> = fixture
            .list
            .projects
            .iter()
            .map(|project| (project.id.clone(), project.clone()))
            .collect();
        let selected_id = fixture.selected_session_id.expect("mockup selection");
        let selected = sessions[&selected_id].as_ref();
        let migrating = HashSet::new();
        let update = preview_update("1.0");
        let model = StatusBarModel::build(ModelInputs {
            sessions: &sessions,
            selected: Some(selected),
            hosts: &[],
            chrome: Some(preview_chrome(&selected.id)),
            context: Some(preview_context()),
            visibility: StatusBarVisibility::default(),
            projects: &projects,
            migrating: &migrating,
            catalog: None,
            unread: 2,
            update: &update,
            message: None,
            colors: SemanticColors::light(),
        });
        let session = model.session.expect("mockup session");
        // 1. Session status and title.
        assert_eq!(session.status_label.as_str(), "Working");
        assert!(
            session
                .display
                .as_str()
                .contains("Rebuild the bottom status bar")
        );
        // 2. Execution location.
        assert_eq!(session.location.as_str(), "SSH ·");
        assert_eq!(
            session.location_tail.as_ref().map(SharedString::as_str),
            Some("preview-ssh")
        );
        // 3. Git branch.
        assert_eq!(
            session.branch.as_ref().map(SharedString::as_str),
            Some("feature/status-bar-mockup")
        );
        // 4. Project / worktree.
        assert_eq!(session.project.as_str(), "Ubra");
        // 5. Terminal progress.
        assert_eq!(
            session.progress_label.as_ref().map(SharedString::as_str),
            Some("42%")
        );
        // 6. Context consumption.
        assert_eq!(
            session.context_label.as_ref().map(SharedString::as_str),
            Some("Context 67%")
        );
        // 7. Ports.
        assert_eq!(
            session.ports_label.as_ref().map(SharedString::as_str),
            Some(":3000 +1")
        );
        // 8. Scrollback feedback.
        assert!(session.scrolled_back);
        // 9. Terminal access feedback.
        assert_eq!(session.access, Some(TerminalAccess::ActiveElsewhere));
        // 10. Global attention queue: two blockers and one unseen finish.
        assert_eq!(model.needs_input, 2);
        assert_eq!(model.done_unseen, 1);
        assert_eq!(
            model.attention_label.as_ref().map(SharedString::as_str),
            Some("2 need input")
        );
        // 11. Notifications.
        assert_eq!(
            model.bell_label.as_ref().map(SharedString::as_str),
            Some("2")
        );
        // 12. App update.
        assert_eq!(
            model.update.as_ref().map(|update| update.label.as_str()),
            Some("Update")
        );
    }
}
