//! Native trailing workbench inspector.
//!
//! Root owns FollowActive/Pinned identity; this view owns bounded, captured-session
//! evidence projections, native command controls, Git review, and diff virtualization.
//! Notes are hosted only here: their detail editor never replaces the
//! main-pane session, and Back retains the list's scope, filter, selection,
//! and scroll independently of the main workspace.

mod context;
mod runs;
mod sidebar;
#[cfg(test)]
mod target_tests;
mod tasks;
mod usage;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyDownEvent, ListHorizontalSizingBehavior, MouseButton,
    Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Task,
    UniformListScrollHandle, Window, div, ease_out_quint, point, prelude::*, px, rgba,
    uniform_list,
};
use ubra_proto::{
    AgentKind as ProtoAgentKind, ArtifactKind, PrCheck, PrDiscussionItem, ProjectId,
    PullRequestStatus, SessionArtifact, SessionDiffBase, SessionId, SessionRecord, SessionStatus,
};
use ubra_ui::{
    AgentKind, Appearance, Fill, FloatingSurface, GlassMenuRow, Ink, Radius, SemanticColors, Typo,
};

use crate::code_viewer::CodeViewer;
use crate::diff::{
    DiffFile, DiffHunk, DiffLayer, DiffRow, DiffRowKind, DiffSelection, DiffSnapshot,
    load_local_diff, snapshot_from_read_diff,
};
use crate::git_review::{GitRepository, GitReviewError, PatchMutation, ReviewStatus};
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::markdown::MarkdownDocument;
use crate::markdown_view::render_markdown;
use crate::notes::panel::{NoteDetail, NotesPanel, PendingNote};
use crate::notes::{NotePane, NotePaneEvent};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::quote::{Quote, QuoteSource};
use crate::review_prompt::{ReviewEvidence, ReviewLayer, ReviewPrompt};
use crate::store::{InspectorTab, StoreRuntime};

use crate::inspector_target::InspectorTarget;
use crate::transcript::{
    ContextUsage, TranscriptDocument, TranscriptVersion, load as load_transcript,
};

const DIFF_ROW_HEIGHT: f32 = 20.0;
const GUTTER_WIDTH: f32 = 68.0;
/// Omitted file names sit under the notice text, past its disclosure chevron.
const OMITTED_PATH_INDENT: f32 = 15.0;
const OMITTED_PATH_INDENT_COLUMNS: usize = 3;
const OMITTED_PATH_ROW_GROUP: &str = "omitted-untracked-row";
const REFRESH_INTERVAL: Duration = Duration::from_millis(1400);
const TRANSCRIPT_REFRESH_DEBOUNCE: Duration = Duration::from_millis(120);
const SCROLLBAR_INSET: f32 = 4.0;
const SCROLLBAR_MIN_THUMB: f32 = 34.0;

#[derive(Clone, Copy)]
struct DraggedDiffScrollbar;

impl Render for DraggedDiffScrollbar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ScrollbarInteraction {
    dragging: bool,
    grab_offset: f32,
}

#[derive(Clone, Copy, Debug)]
struct ScrollbarMetrics {
    track_top: f32,
    track_height: f32,
    thumb_height: f32,
    thumb_top: f32,
}

/// Cached account-usage aggregation for the inspector Usage surface. Render
/// re-runs often, so the history merge, report pair, and 90-day chart buckets
/// are computed once per input change instead of once per frame. Mirrors the
/// Settings usage page over the same `UsageSnapshot` feed.
struct AccountUsageCache {
    updated_at: i64,
    days: usize,
    host: Option<String>,
    tokens: bool,
    remote: Vec<(String, i64)>,
    compare: crate::usage::dashboard::UsageCompare,
    providers: [Vec<crate::surface_shell::usage_chart::ChartSample>; 3],
}

#[derive(Clone, Debug)]
pub enum InspectorEvent {
    ComposePrompt(SessionId),
    SessionChanged,
    OpenChecklist {
        source: crate::prompt_draft::NoteSource,
        block: usize,
    },
    ContextUsageChanged,
    WorkspaceChanged(WorkspaceSurface),
    /// Restore a conversation's inspector without moving keyboard focus into it.
    WorkspaceRestored(WorkspaceSurface),
    WorkspaceClosed {
        surface: WorkspaceSurface,
        id: u64,
    },
    Browser(BrowserAction),
    /// Open a note from the Notes surface in the detail page: resolve its
    /// Session, unarchiving or adopting it when needed. Never selects.
    OpenNote {
        note_id: String,
        workspace: Option<ProjectId>,
    },
    /// A mention chip in the open note asked to show another Session: a note
    /// opens in the detail page, anything else in the main pane.
    RevealSession {
        session: SessionId,
    },
    /// Start a note in one scope (`None` is global).
    NewNote {
        workspace: Option<ProjectId>,
    },
    /// Pin or unpin a note's Session.
    PinNote {
        session: SessionId,
    },
    /// Archive a note's Session.
    ArchiveNote {
        session: SessionId,
    },
    /// Bring an archived note's Session back.
    ReviveNote {
        session: SessionId,
    },
    /// Move a note's file to the trash, removing its Session when it has one.
    TrashNote {
        note_id: String,
        workspace: Option<ProjectId>,
        session: Option<SessionId>,
    },
}

#[derive(Clone, Debug)]
pub enum BrowserAction {
    Navigate(String),
    Back,
    Forward,
    Reload,
    OpenExternal(String),
}

/// The native WebKit view owns navigation. This compact projection lets the
/// GPUI chrome accurately reflect redirects, in-page links, and history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserState {
    pub url: Option<String>,
    pub title: Option<String>,
    pub favicon: Option<Arc<gpui::Image>>,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub is_loading: bool,
    pub error: Option<String>,
}

impl InspectorTab {
    const fn index(self) -> i8 {
        match self {
            Self::Info => 0,
            Self::Changes => 1,
            Self::Code => 2,
            Self::Artifacts => 3,
        }
    }
}

/// A workspace surface is deliberately separate from `InspectorTab`: the
/// latter is persisted user state for the existing agent details views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceSurface {
    Runs,
    Tasks,
    Context,
    Usage,
    Details,
    Browser,
    Files,
    Review,
    Notes,
}

impl WorkspaceSurface {
    /// The visible right-rail destinations, in rail order. Runs, Tasks and
    /// Context remain reachable through the command palette, not as rail
    /// tabs.
    pub(crate) const CATALOG: [Self; 6] = [
        Self::Browser,
        Self::Files,
        Self::Review,
        Self::Details,
        Self::Usage,
        Self::Notes,
    ];

    /// Every surface is a singleton: the activity strip switches surfaces and
    /// each switch replaces the panel content, so reopening a surface focuses
    /// the existing one instead of adding a tab.
    const fn is_singleton(self) -> bool {
        true
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Runs => "Runs",
            Self::Tasks => "Tasks",
            Self::Context => "Context",
            Self::Usage => "Usage",
            Self::Details => "Details",
            Self::Browser => "Browser",
            Self::Files => "Files",
            Self::Review => "Review",
            Self::Notes => "Notes",
        }
    }

    pub(crate) const fn icon(self) -> &'static str {
        // Every name must resolve in ubra-ui IconName::from_system_name;
        // unmapped names render blank.
        match self {
            Self::Runs => "terminal",
            Self::Tasks => "checklist",
            Self::Context => "link",
            Self::Usage => "chart.bar",
            Self::Details => "info.circle",
            Self::Browser => "network",
            Self::Files => "folder",
            Self::Review => "arrow.branch",
            Self::Notes => "note.text",
        }
    }
}

/// Identity belongs to the tab instance, never to its surface kind.
struct WorkspaceTab {
    id: u64,
    surface: WorkspaceSurface,
    viewer: Option<Entity<CodeViewer>>,
    review_tab: InspectorTab,
    scroll: UniformListScrollHandle,
    diff_layer: DiffLayer,
    comparison: SessionDiffBase,
    browser_query: QueryEditor,
    browser_state: BrowserState,
}

impl WorkspaceTab {
    fn new(id: u64, surface: WorkspaceSurface) -> Self {
        Self {
            id,
            surface,
            viewer: None,
            review_tab: InspectorTab::Changes,
            scroll: UniformListScrollHandle::new(),
            diff_layer: DiffLayer::Branch,
            comparison: SessionDiffBase::DefaultBranch,
            browser_query: QueryEditor::default(),
            browser_state: BrowserState::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffContext {
    id: SessionId,
    cwd: PathBuf,
    remote: bool,
    agent_session_id: Option<String>,
    transcript_path: Option<PathBuf>,
    kind: ProtoAgentKind,
}

impl DiffContext {
    fn matches_record(&self, record: &SessionRecord) -> bool {
        self.id == record.id
            && self.cwd == Path::new(&record.cwd)
            && self.remote == record.host.is_some()
            && self.agent_session_id == record.agent_session_id
            && self.transcript_path.as_deref() == record.transcript_path.as_deref().map(Path::new)
            && &self.kind == record.effective_kind()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LoadState {
    NoSession,
    Loading,
    Ready(Arc<DiffSnapshot>),
    Error(String),
}
#[derive(Clone, Debug)]
enum TranscriptLoadState {
    Unavailable,
    Loading,
    Ready(Arc<TranscriptDocument>),
    Error,
}

#[derive(Clone, Debug)]
enum ReviewLoadState {
    NoSession,
    Remote,
    Loading,
    Ready(Arc<ReviewStatus>),
    Error(String),
}

#[derive(Clone, Debug)]
enum ReviewAction {
    Stage(Vec<PathBuf>),
    Unstage(Vec<PathBuf>),
    Discard(Vec<PathBuf>),
    Patch {
        patch: Vec<u8>,
        mutation: PatchMutation,
    },
    Commit(String),
}

#[derive(Clone, Debug)]
struct AskDraft {
    evidence: Vec<ReviewEvidence>,
    label: String,
}

#[derive(Clone, Debug)]
struct SelectedTurn {
    key: String,
    quote: Quote,
}

struct SessionWorkspace {
    tabs: Vec<WorkspaceTab>,
    active: Option<u64>,
}

/// The review file navigator as a panel target (see `crate::floating::Target`).
const INSPECTOR_FILES_MENU: crate::floating::Target<WorkbenchInspector> = crate::floating::Target {
    key: "inspector-files",
    radius: crate::floating::MENU_RADIUS,
    content: WorkbenchInspector::files_panel_content,
    dismiss: |this, _, cx| {
        this.files_open = false;
        cx.notify();
    },
};

/// The comparison base menu as a panel target.
const INSPECTOR_COMPARISON_MENU: crate::floating::Target<WorkbenchInspector> =
    crate::floating::Target {
        key: "inspector-comparison",
        radius: crate::floating::MENU_RADIUS,
        content: WorkbenchInspector::comparison_panel_content,
        dismiss: |this, _, cx| {
            this.comparison_menu_open = false;
            cx.notify();
        },
    };

pub struct WorkbenchInspector {
    runtime: Arc<StoreRuntime>,
    _tokio_owner: Arc<tokio::runtime::Runtime>,
    tokio: tokio::runtime::Handle,
    code_viewer: Entity<CodeViewer>,
    markdown_cache: HashMap<String, Arc<MarkdownDocument>>,
    focus: FocusHandle,
    sidebar: sidebar::SidebarState,
    context_preview_frame: Option<context::ContextPreviewFrame>,
    context_preview_pending: Option<context::ContextPreviewFrame>,
    context_preview_epoch: u64,
    visible: bool,
    selected_tab: InspectorTab,
    review_tab: InspectorTab,
    /// Project (`ProjectId.0`) whose tabs are live. Tab layout is per-project:
    /// switching panes within one project leaves tabs untouched.
    workspace_project: Option<String>,
    // None follows legacy selection; Some(None) is an empty saved workspace.
    session_context: Option<Option<SessionId>>,
    project_workspaces: HashMap<String, SessionWorkspace>,
    workspace_tabs: Vec<WorkspaceTab>,
    workspace_active: Option<u64>,
    next_workspace_id: u64,
    workspace_selected: Option<WorkspaceSurface>,
    tab_direction: f32,
    tab_transition_generation: u64,
    browser_query: QueryEditor,
    browser_address_focused: bool,
    browser_state: BrowserState,
    notes: NotesPanel,
    #[cfg(target_os = "macos")]
    native_browser: Option<std::rc::Rc<std::cell::RefCell<crate::macos::browser::NativeBrowser>>>,
    context: Option<DiffContext>,
    state: LoadState,
    review_state: ReviewLoadState,
    review_generation: u64,
    review_task: Option<Task<()>>,
    transcript_state: TranscriptLoadState,
    transcript_context: Option<DiffContext>,
    transcript_version: Option<TranscriptVersion>,
    transcript_generation: u64,
    transcript_task: Option<Task<()>>,
    transcript_home: PathBuf,
    review_action_task: Option<Task<()>>,
    review_action_busy: bool,
    review_action_generation: u64,
    review_feedback: Option<(bool, String)>,
    ask_draft: Option<AskDraft>,
    ask_generation: u64,
    ask_query: QueryEditor,
    ask_task: Option<Task<()>>,
    ask_busy: bool,
    ask_feedback: Option<(bool, String)>,
    commit_open: bool,
    commit_query: QueryEditor,
    discard_armed: bool,
    armed_hunk: Option<u64>,
    /// Whether the "not shown" notice is expanded into the omitted file names.
    omitted_untracked_open: bool,
    diff_selection: DiffSelection,
    selected_turn: Option<SelectedTurn>,
    diff_layer: DiffLayer,
    files_open: bool,
    comparison: SessionDiffBase,
    comparison_menu_open: bool,
    loading: bool,
    generation: u64,
    scroll: UniformListScrollHandle,
    scrollbar_interaction: ScrollbarInteraction,
    scrollbar_layout_primed: bool,
    refresh_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _store_changes: Task<()>,
    _sidebar_events: Task<()>,
    /// Account-level usage feed shared with Settings > Usage: local Claude
    /// Code / Codex transcripts plus billed Cursor usage. The Usage surface
    /// renders the same report as the settings page, not per-session Rpc
    /// accounting.
    account_usage: crate::usage::UsageSnapshot,
    /// Held while the Usage surface is shown so remote hosts keep polling.
    remote_usage_viewer: crate::usage::RemoteUsageViewer,
    account_usage_days: usize,
    account_usage_host: Option<String>,
    account_usage_tokens: bool,
    account_usage_numbers: crate::number_flow::Bank,
    account_usage_cache: RefCell<Option<AccountUsageCache>>,
    account_usage_chart_split: Option<[bool; 3]>,
}

impl EventEmitter<InspectorEvent> for WorkbenchInspector {}

impl Focusable for WorkbenchInspector {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WorkbenchInspector {
    pub fn new(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokio = tokio_owner.handle().clone();
        let (selected_tab, code_colors, workspace_project) = {
            let store = runtime.store.read().expect("session store lock poisoned");
            let project = store
                .selected_session_id()
                .and_then(|id| store.sessions().get(id))
                .map(|record| record.project_id.0.clone());
            let prefs = store.preferences();
            (
                project
                    .as_deref()
                    .map(|project| prefs.inspector_state_for(project))
                    .map(|state| state.tab)
                    .unwrap_or(prefs.inspector_tab),
                crate::app_theme::sidebar_colors_in(&store),
                project,
            )
        };
        let code_viewer = cx.new(|cx| CodeViewer::new(code_colors, cx));
        cx.observe(&code_viewer, |_, _, cx| cx.notify()).detach();
        let focus = cx.focus_handle();
        let mut changes = runtime.changes();
        let store_changes = cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this
                            .update(cx, |this, cx| this.refresh_if_context_changed(cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        let mut events = {
            let _guard = tokio.enter();
            runtime.client().events()
        };
        let mut states = runtime.client().connection_state();
        let sidebar_events = cx.spawn(async move |this, cx| {
            loop {
                let (forced,event) = tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => (false,Some(event)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => (true,None),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                    state = states.changed() => {
                        if state.is_err() { return; }
                        (matches!(*states.borrow_and_update(), ubra_client::ConnectionState::Connected(_)),None)
                    }
                };
                if this.update(cx, |this, cx| {
                    let applies=forced || event.as_ref().is_some_and(|event|this.selected_session().is_some_and(|session|sidebar::event_applies(event,&session.id,this.workspace_selected)));
                    if applies { this.refresh_sidebar(cx); }
                }).is_err() { return; }
            }
        });
        let initial_surface = WorkspaceSurface::CATALOG[0];
        let workspace_tabs = vec![WorkspaceTab::new(0, initial_surface)];
        let workspace_active = workspace_tabs.last().map(|tab| tab.id);
        let notes = NotesPanel::new(&runtime, cx);
        Self {
            runtime,
            _tokio_owner: tokio_owner,
            tokio,
            code_viewer,
            markdown_cache: HashMap::new(),
            focus,
            sidebar: sidebar::SidebarState::new(cx),
            context_preview_frame: None,
            context_preview_pending: None,
            context_preview_epoch: 0,
            visible: false,
            notes,
            selected_tab,
            review_tab: match selected_tab {
                InspectorTab::Changes | InspectorTab::Artifacts => selected_tab,
                InspectorTab::Info | InspectorTab::Code => InspectorTab::Changes,
            },
            workspace_project,
            session_context: None,
            project_workspaces: HashMap::new(),
            workspace_tabs,
            workspace_active,
            next_workspace_id: 1,
            workspace_selected: Some(initial_surface),
            tab_direction: 1.0,
            tab_transition_generation: 0,
            browser_query: QueryEditor::default(),
            browser_address_focused: false,
            browser_state: BrowserState::default(),
            #[cfg(target_os = "macos")]
            native_browser: None,
            context: None,
            state: LoadState::NoSession,
            review_state: ReviewLoadState::NoSession,
            review_generation: 0,
            review_task: None,
            transcript_state: TranscriptLoadState::Unavailable,
            transcript_context: None,
            transcript_version: None,
            transcript_generation: 0,
            transcript_task: None,
            transcript_home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            review_action_task: None,
            review_action_busy: false,
            review_action_generation: 0,
            review_feedback: None,
            ask_draft: None,
            ask_generation: 0,
            ask_query: QueryEditor::default(),
            ask_task: None,
            ask_busy: false,
            ask_feedback: None,
            commit_open: false,
            commit_query: QueryEditor::default(),
            discard_armed: false,
            armed_hunk: None,
            omitted_untracked_open: false,
            diff_selection: DiffSelection::default(),
            selected_turn: None,
            diff_layer: DiffLayer::Branch,
            files_open: false,
            comparison: SessionDiffBase::DefaultBranch,
            comparison_menu_open: false,
            loading: false,
            generation: 0,
            scroll: UniformListScrollHandle::new(),
            scrollbar_interaction: ScrollbarInteraction::default(),
            scrollbar_layout_primed: false,
            refresh_task: None,
            poll_task: None,
            _store_changes: store_changes,
            _sidebar_events: sidebar_events,
            account_usage: crate::usage::UsageSnapshot::default(),
            remote_usage_viewer: crate::usage::RemoteUsageViewer::default(),
            account_usage_days: 30,
            account_usage_host: None,
            account_usage_tokens: false,
            account_usage_numbers: crate::number_flow::Bank::default(),
            account_usage_cache: RefCell::new(None),
            account_usage_chart_split: None,
        }
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.sync_workspace_session(cx);
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.context_preview_epoch = self.context_preview_epoch.wrapping_add(1);
        if visible {
            self.refresh(true, cx);
        } else {
            self.comparison_menu_open = false;
            self.files_open = false;
            self.ask_draft = None;
            self.ask_feedback = None;
            self.ask_query.clear();
            self.release_hidden_state();
            cx.emit(InspectorEvent::ContextUsageChanged);
        }
        self.reconcile_diff_polling(cx);
        cx.notify();
    }

    /// Hidden panels release large diff snapshots; reopening rereads them.
    fn release_hidden_state(&mut self) {
        self.refresh_task = None;
        self.review_task = None;
        self.loading = false;
        // Row indices describe the dropped diff snapshot.
        self.diff_selection.clear();
        self.selected_turn = None;
        self.state = LoadState::NoSession;
        self.review_state = ReviewLoadState::NoSession;
        self.transcript_state = TranscriptLoadState::Unavailable;
        self.transcript_context = None;
        self.markdown_cache = HashMap::new();
    }

    #[must_use]
    pub(crate) fn has_active_workspace(&self) -> bool {
        self.workspace_selected.is_some()
    }

    /// The surface in front, so the activity strip can highlight its icon.
    #[must_use]
    pub(crate) fn selected_workspace(&self) -> Option<WorkspaceSurface> {
        self.workspace_selected
    }

    #[must_use]
    pub(crate) fn selected_review_tab(&self) -> InspectorTab {
        self.review_tab
    }

    /// The persisted inspector tab, so per-project state can snapshot it.
    #[must_use]
    pub(crate) fn selected_tab(&self) -> InspectorTab {
        self.selected_tab
    }

    /// Land keyboard focus on the tab already selected. A blank browser gets
    /// its address field; command panels focus their retained entry control.
    pub(crate) fn focus_active_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_selected == Some(WorkspaceSurface::Browser)
            && self.browser_state.url.is_none()
        {
            self.focus_browser_address(window, cx);
            return;
        }
        if let Some(session) = self.selected_session() {
            let control = match self.workspace_selected {
                Some(WorkspaceSurface::Runs) if session.host.is_none() && !session.is_note() => {
                    Some(self.sidebar.runs_focus.clone())
                }
                Some(WorkspaceSurface::Runs) => Some(self.sidebar.runs_refresh_focus.clone()),
                Some(WorkspaceSurface::Tasks) => Some(self.sidebar.tasks_focus.clone()),
                Some(WorkspaceSurface::Context) => Some(self.sidebar.context_focus.clone()),
                _ => None,
            };
            if let Some(focus) = control {
                window.focus(&focus, cx);
                return;
            }
        }
        window.focus(&self.focus, cx);
    }

    #[must_use]
    #[cfg(target_os = "macos")]
    pub fn is_browser_tab(&self) -> bool {
        self.workspace_selected == Some(WorkspaceSurface::Browser)
    }

    #[cfg(target_os = "macos")]
    pub fn set_native_browser(
        &mut self,
        browser: std::rc::Rc<std::cell::RefCell<crate::macos::browser::NativeBrowser>>,
    ) {
        self.native_browser = Some(browser);
    }

    #[must_use]
    #[cfg(target_os = "macos")]
    pub fn blocks_native_browser(&self) -> bool {
        self.comparison_menu_open
            || self.files_open
            || (self.workspace_selected == Some(WorkspaceSurface::Review)
                && self.ask_draft.is_some())
            || self.commit_open
    }

    #[cfg(target_os = "macos")]
    pub fn set_browser_state(&mut self, state: BrowserState, cx: &mut Context<Self>) {
        let blurred = self.browser_address_focused
            && self
                .native_browser
                .as_ref()
                .is_some_and(|browser| browser.borrow().has_focus());
        if blurred {
            self.browser_address_focused = false;
        }
        if self.browser_state == state && !blurred {
            return;
        }
        let update_address = !self.browser_address_focused;
        self.browser_state = state;
        if update_address {
            self.browser_query.clear();
            if let Some(url) = self.browser_state.url.as_deref() {
                self.browser_query.insert(url);
            }
        }
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub fn set_browser_tab_state(&mut self, id: u64, state: BrowserState, cx: &mut Context<Self>) {
        if self.workspace_active == Some(id) {
            self.set_browser_state(state, cx);
            return;
        }
        for tab in self.workspace_tabs.iter_mut().chain(
            self.project_workspaces
                .values_mut()
                .flat_map(|workspace| workspace.tabs.iter_mut()),
        ) {
            if tab.id == id && tab.browser_state != state {
                tab.browser_state = state;
                cx.notify();
                return;
            }
        }
    }

    #[must_use]
    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Returns the selection owned by the inspector's active surface.
    /// Review exposes diff selections in Changes and Markdown turns in Artifacts.
    #[must_use]
    pub fn quote_selection(&self) -> Option<Quote> {
        match self.workspace_selected {
            Some(WorkspaceSurface::Review) if self.review_tab == InspectorTab::Artifacts => {
                self.selected_turn.as_ref().map(|turn| turn.quote.clone())
            }
            Some(WorkspaceSurface::Review) => {
                let LoadState::Ready(snapshot) = &self.state else {
                    return None;
                };
                let session_id = self.context.as_ref()?.id.clone();
                self.diff_selection.quote(snapshot, session_id)
            }
            Some(WorkspaceSurface::Details) => match self.selected_tab {
                InspectorTab::Info | InspectorTab::Artifacts => {
                    self.selected_turn.as_ref().map(|turn| turn.quote.clone())
                }
                InspectorTab::Changes => {
                    let LoadState::Ready(snapshot) = &self.state else {
                        return None;
                    };
                    let session_id = self.context.as_ref()?.id.clone();
                    self.diff_selection.quote(snapshot, session_id)
                }
                InspectorTab::Code => None,
            },
            _ => None,
        }
    }

    fn select_diff_row(
        &mut self,
        row: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let LoadState::Ready(snapshot) = &self.state else {
            return;
        };
        self.selected_turn = None;
        self.diff_selection.select(snapshot, row, extend);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn select_turn(
        &mut self,
        key: String,
        source: QuoteSource,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(quote) = Quote::new(source, content) else {
            return;
        };
        self.diff_selection.clear();
        self.selected_turn = Some(SelectedTurn { key, quote });
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn selected_turn_key(&self) -> Option<&str> {
        self.selected_turn
            .as_ref()
            .map(|selection| selection.key.as_str())
    }

    fn reconcile_diff_polling(&mut self, cx: &mut Context<Self>) {
        let should_poll = self.visible
            && self.workspace_selected == Some(WorkspaceSurface::Review)
            && self.selected_tab == InspectorTab::Changes;
        if !should_poll {
            // Only Changes & commit owns Git producers; every other surface is passive here.
            self.poll_task = None;
            self.refresh_task = None;
            self.review_task = None;
            self.loading = false;
            return;
        }
        if self.poll_task.is_some() {
            return;
        }
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        if this.visible
                            && this.workspace_selected == Some(WorkspaceSurface::Review)
                            && this.selected_tab == InspectorTab::Changes
                        {
                            this.refresh(false, cx);
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    /// Opens a terminal or diff-shaped file reference in the native code tab.
    /// The viewer owns resolution and loading; the inspector only preserves
    /// the workbench's spatial context and selects the destination tab.
    pub fn open_file_reference(
        &mut self,
        cwd: impl Into<PathBuf>,
        reference: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let cwd = cwd.into();
        let reference = reference.into();
        self.select_tab(InspectorTab::Code, cx);
        self.code_viewer.update(cx, |viewer, cx| {
            viewer.open_reference(cwd, reference, cx);
        });
    }

    #[cfg(test)]
    pub(crate) fn session_id_for_test(&self) -> Option<SessionId> {
        self.selected_session().map(|session| session.id)
    }

    fn selected_context(&self) -> Option<DiffContext> {
        let session = self.selected_session()?;
        Some(DiffContext {
            id: session.id.clone(),
            cwd: PathBuf::from(&session.cwd),
            remote: session.host.is_some(),
            agent_session_id: session.agent_session_id.clone(),
            transcript_path: session.transcript_path.as_deref().map(PathBuf::from),
            kind: session.effective_kind().clone(),
        })
    }

    /// Opportunistic last-request occupancy from a loaded, identity-matched
    /// local Codex transcript. Reading this never starts or retains a load.
    pub(crate) fn reported_context_usage(&self, record: &SessionRecord) -> Option<ContextUsage> {
        if !self.visible {
            return None;
        }
        let stamped = self.transcript_context.as_ref()?;
        if stamped.remote
            || stamped.kind.id() != ProtoAgentKind::CODEX_ID
            || self.context.as_ref() != Some(stamped)
            || !stamped.matches_record(record)
        {
            return None;
        }
        let TranscriptLoadState::Ready(document) = &self.transcript_state else {
            return None;
        };
        document
            .context_usage
            .filter(|usage| usage.tokens >= 0 && usage.window > 0)
    }
    fn refresh_if_context_changed(&mut self, cx: &mut Context<Self>) {
        self.sync_workspace_session(cx);
        let colors = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            crate::app_theme::sidebar_colors_in(&store)
        };
        self.code_viewer
            .update(cx, |viewer, cx| viewer.set_colors(colors, cx));
        for tab in &self.workspace_tabs {
            if let Some(viewer) = &tab.viewer {
                viewer.update(cx, |viewer, cx| viewer.set_colors(colors, cx));
            }
        }
        if !self.visible {
            return;
        }
        // Context changes refresh the captured session, including passive
        // metadata. `refresh` starts Git producers only for visible Review /
        // Changes; the other surfaces refresh their own scoped snapshots.
        if self.workspace_selected == Some(WorkspaceSurface::Notes) {
            // The notes list is a projection of pins, archives, and the
            // selection's project, so same-session store changes repaint it.
            cx.notify();
        }
        if self.selected_context() != self.context {
            self.refresh(true, cx);
        } else {
            self.refresh_sidebar(cx);
            cx.notify();
        }
    }

    fn save_active_workspace(&mut self) {
        if let Some(tab) = self
            .workspace_tabs
            .iter_mut()
            .find(|tab| Some(tab.id) == self.workspace_active)
        {
            tab.review_tab = self.review_tab;
            tab.scroll = self.scroll.clone();
            tab.diff_layer = self.diff_layer;
            tab.comparison = self.comparison;
            tab.browser_query = self.browser_query.clone();
            tab.browser_state = self.browser_state.clone();
        }
    }

    fn prune_project_workspaces(&mut self) {
        {
            let store = self.runtime.store.read().expect("store");
            self.project_workspaces.retain(|project, workspace| {
                // Archived records stay in the store, so membership alone
                // would keep their viewers, indexes and web pages for good.
                // A project keeps its tabs while any available session
                // remains; losing its last one starts it fresh next time.
                let keep = store.sessions().values().any(|session| {
                    session.project_id.0 == *project && InspectorTarget::is_available(session)
                });
                if !keep {
                    #[cfg(target_os = "macos")]
                    if let Some(browser) = &self.native_browser {
                        for tab in &workspace.tabs {
                            if tab.surface == WorkspaceSurface::Browser {
                                browser.borrow_mut().close_tab(tab.id);
                            }
                        }
                    }
                    #[cfg(not(target_os = "macos"))]
                    let _ = workspace;
                }
                keep
            });
        }
    }

    /// Project identity owns tabs, including hidden ones. Tab IDs stay unique
    /// across projects so native WebKit pages cannot alias one another.
    /// Session-change cleanup (run output, page requests) runs on every
    /// session switch; only the tab save/swap is gated on project equality,
    /// so switching panes within one project leaves tabs untouched.
    pub(crate) fn sync_workspace_session(&mut self, cx: &mut Context<Self>) {
        if let Some(NoteDetail::Open { session, note_id }) = self.notes.detail().cloned() {
            let invalid = {
                let store = self.runtime.store.read().expect("store");
                store.sessions().get(&session).is_none_or(|record| {
                    !record.is_note()
                        || record.is_archived()
                        || record.note_id.as_deref() != Some(&note_id)
                })
            };
            if invalid {
                self.notes.set_detail(None);
                if let Some(pane) = self.notes.note_pane() {
                    pane.update(cx, |pane, cx| pane.discard_open(&session, &note_id, cx));
                }
                cx.notify();
            }
        }
        self.prune_project_workspaces();
        let record = self.selected_session();
        let session = record.as_ref().map(|record| record.id.clone());
        // Cleanup is session-scoped and unconditional: the previously
        // inspected session backgrounds even when its project's tabs stay.
        let previous_session = self.context.as_ref().map(|context| context.id.clone());
        if previous_session != session {
            if let Some(state) = previous_session
                .as_ref()
                .and_then(|id| self.sidebar.runs.get_mut(id))
            {
                state.reset_output();
            }
            if let Some(id) = previous_session.as_ref() {
                self.sidebar.cancel_page_requests(id);
            }
        }
        let project = record.map(|record| record.project_id.0);
        if self.workspace_project == project {
            return;
        }
        let Some(project) = project else {
            // No resolvable project (no selection, archived or note
            // filtered): leave the sidebar exactly as is rather than
            // borrowing another project's layout.
            return;
        };
        // Notes is session-global: moving the main-pane selection must not
        // yank the panel off the Notes tab (or its open detail page).
        let keep_notes = self.workspace_selected == Some(WorkspaceSurface::Notes);
        self.save_active_workspace();
        let previous = SessionWorkspace {
            tabs: std::mem::take(&mut self.workspace_tabs),
            active: self.workspace_active.take(),
        };
        if let Some(old) = self.workspace_project.take() {
            self.project_workspaces.insert(old, previous);
        } else {
            // First project resolution: the live tabs belong to it.
            self.project_workspaces.insert(project.clone(), previous);
        }
        self.prune_project_workspaces();
        self.workspace_project = Some(project.clone());
        self.sidebar.generation = self.sidebar.generation.wrapping_add(1);
        self.sidebar.load = None;
        let mut next = self.project_workspaces.remove(&project).unwrap_or_else(|| {
            let id = self.next_workspace_id;
            self.next_workspace_id += 1;
            SessionWorkspace {
                tabs: vec![WorkspaceTab::new(id, WorkspaceSurface::Runs)],
                active: Some(id),
            }
        });
        if keep_notes {
            if let Some(id) = next
                .tabs
                .iter()
                .find(|tab| tab.surface == WorkspaceSurface::Notes)
                .map(|tab| tab.id)
            {
                next.active = Some(id);
            } else {
                let id = self.next_workspace_id;
                self.next_workspace_id += 1;
                next.tabs
                    .push(WorkspaceTab::new(id, WorkspaceSurface::Notes));
                next.active = Some(id);
            }
        }
        self.workspace_tabs = next.tabs;
        // Panel visibility is global: switching sessions swaps tabs but never
        // opens or closes the sidebar on its own.
        self.workspace_selected = None;
        self.comparison_menu_open = false;
        self.files_open = false;
        self.commit_open = false;
        self.ask_draft = None;
        self.ask_feedback = None;
        self.ask_query.clear();
        self.commit_query.clear();
        self.discard_armed = false;
        self.armed_hunk = None;
        self.omitted_untracked_open = false;
        self.ask_generation = self.ask_generation.wrapping_add(1);
        self.review_action_generation = self.review_action_generation.wrapping_add(1);

        self.browser_address_focused = false;
        self.browser_query.clear();
        self.browser_state = BrowserState::default();
        self.context = None;
        self.refresh_task = None;
        self.review_task = None;
        self.review_action_task = None;
        self.ask_task = None;
        self.review_action_busy = false;
        self.ask_busy = false;
        self.review_feedback = None;
        self.loading = false;
        self.state = LoadState::NoSession;
        self.review_state = ReviewLoadState::NoSession;
        self.transcript_state = TranscriptLoadState::Unavailable;
        self.transcript_context = None;
        cx.emit(InspectorEvent::ContextUsageChanged);
        if let Some(id) = next.active
            && let Some(surface) = self.load_workspace(id, cx)
        {
            cx.emit(InspectorEvent::WorkspaceRestored(surface));
        }
        self.reconcile_diff_polling(cx);
        cx.emit(InspectorEvent::SessionChanged);
        cx.notify();
    }

    pub(crate) fn select_tab(&mut self, tab: InspectorTab, cx: &mut Context<Self>) {
        self.switch_tab(tab, cx, false);
    }

    /// Project-switch restore: applies the remembered tab like [`Self::select_tab`]
    /// but reports it as a background restore, so the panel never steals
    /// keyboard focus while hidden.
    pub(crate) fn restore_tab(&mut self, tab: InspectorTab, cx: &mut Context<Self>) {
        self.switch_tab(tab, cx, true);
    }

    fn switch_tab(&mut self, tab: InspectorTab, cx: &mut Context<Self>, background: bool) {
        let surface = match tab {
            InspectorTab::Changes | InspectorTab::Artifacts => WorkspaceSurface::Review,
            InspectorTab::Info => WorkspaceSurface::Details,
            InspectorTab::Code => WorkspaceSurface::Files,
        };
        // Apply an explicit Review destination before activation, so opening
        // Pull requests never starts the Changes producer on the way there.
        self.sync_workspace_session(cx);
        if matches!(tab, InspectorTab::Changes | InspectorTab::Artifacts) {
            self.review_tab = tab;
            if let Some(workspace) = self
                .workspace_tabs
                .iter_mut()
                .find(|workspace| workspace.surface == WorkspaceSurface::Review)
            {
                workspace.review_tab = tab;
            }
        }
        if background {
            self.restore_workspace(surface, cx);
        } else {
            self.select_workspace(surface, cx);
        }
        if self.selected_tab != tab {
            self.tab_direction = if tab.index() > self.selected_tab.index() {
                1.0
            } else {
                -1.0
            };
            self.selected_tab = tab;
            self.comparison_menu_open = false;
            self.diff_selection.clear();
            self.selected_turn = None;
            self.tab_transition_generation = self.tab_transition_generation.wrapping_add(1);
            let project = self.selected_session().map(|record| record.project_id.0);
            let visible = self.visible;
            if let Err(error) = self
                .runtime
                .store
                .write()
                .expect("store")
                .update_preferences(|prefs| match &project {
                    Some(project) => prefs.remember_inspector_project(project, visible, tab),
                    None => prefs.inspector_tab = tab,
                })
            {
                eprintln!("ubra: could not remember inspector tab: {error}");
            }
            if tab == InspectorTab::Changes {
                self.refresh(true, cx);
            }
        }
        self.reconcile_diff_polling(cx);
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn active_workspace_id(&self) -> Option<u64> {
        self.workspace_active
    }

    /// ⌘W while this inspector is focused leaves the current surface and falls
    /// back to Details. A focused browser page counts too: its web view is
    /// not this focus handle.
    pub(crate) fn close_focused_workspace(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.visible {
            return false;
        }
        #[cfg(target_os = "macos")]
        let browser_focused = self
            .native_browser
            .as_ref()
            .is_some_and(|browser| browser.borrow().has_focus());
        #[cfg(not(target_os = "macos"))]
        let browser_focused = false;
        if !self.is_focused(window) && !browser_focused {
            return false;
        }
        self.close_active_workspace(cx)
    }

    /// ⌘W on the active surface falls back to Details; surfaces are switched,
    /// never left empty.
    pub(crate) fn close_active_workspace(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.workspace_active else {
            return false;
        };
        self.close_workspace(id, cx);
        true
    }

    pub(crate) fn select_workspace(&mut self, surface: WorkspaceSurface, cx: &mut Context<Self>) {
        self.sync_workspace_session(cx);
        if self.workspace_selected == Some(surface) && self.workspace_active.is_some() {
            // Explicit reselect of the live surface still focuses it.
            cx.emit(InspectorEvent::WorkspaceChanged(surface));
            cx.notify();
            return;
        }
        if let Some(tab) = self
            .workspace_tabs
            .iter()
            .find(|tab| tab.surface == surface)
        {
            self.activate_workspace(tab.id, cx);
        } else {
            self.add_workspace(surface, cx);
        }
    }

    /// Background restore: switches like [`Self::select_workspace`] but reports
    /// [`InspectorEvent::WorkspaceRestored`], so the panel never steals focus.
    pub(crate) fn restore_workspace(&mut self, surface: WorkspaceSurface, cx: &mut Context<Self>) {
        self.sync_workspace_session(cx);
        if self.workspace_selected == Some(surface) && self.workspace_active.is_some() {
            cx.notify();
            return;
        }
        if let Some(tab) = self
            .workspace_tabs
            .iter()
            .find(|tab| tab.surface == surface)
        {
            self.activate_workspace_restored(tab.id, cx);
        } else {
            self.add_workspace_inner(surface, cx, true);
        }
    }

    fn add_workspace(&mut self, surface: WorkspaceSurface, cx: &mut Context<Self>) {
        self.add_workspace_inner(surface, cx, false);
    }

    fn add_workspace_inner(
        &mut self,
        surface: WorkspaceSurface,
        cx: &mut Context<Self>,
        background: bool,
    ) {
        // Single choke point for surface opens: every open path (shortcuts,
        // the activity strip) funnels through here, so a second open focuses
        // the existing surface instead of duplicating it.
        if surface.is_singleton()
            && let Some(id) = self
                .workspace_tabs
                .iter()
                .find(|tab| tab.surface == surface)
                .map(|tab| tab.id)
        {
            self.activate_workspace_inner(id, cx, background);
            return;
        }
        let id = self.next_workspace_id;
        self.next_workspace_id += 1;
        let mut tab = WorkspaceTab::new(id, surface);
        if surface == WorkspaceSurface::Files {
            let colors =
                crate::app_theme::sidebar_colors_in(&self.runtime.store.read().expect("store"));
            let viewer = cx.new(|cx| CodeViewer::new(colors, cx));
            cx.observe(&viewer, |_, _, cx| cx.notify()).detach();
            let cwd = self
                .selected_context()
                .filter(|context| !context.remote)
                .map(|context| context.cwd);
            viewer.update(cx, |viewer, cx| viewer.set_workspace(cwd, cx));
            tab.viewer = Some(viewer);
        }
        self.workspace_tabs.push(tab);
        self.activate_workspace_inner(id, cx, background);
    }

    fn activate_workspace(&mut self, id: u64, cx: &mut Context<Self>) {
        self.activate_workspace_inner(id, cx, false);
    }

    fn activate_workspace_restored(&mut self, id: u64, cx: &mut Context<Self>) {
        self.activate_workspace_inner(id, cx, true);
    }

    fn activate_workspace_inner(&mut self, id: u64, cx: &mut Context<Self>, background: bool) {
        let Some(surface) = self.load_workspace(id, cx) else {
            return;
        };
        if background {
            cx.emit(InspectorEvent::WorkspaceRestored(surface));
        } else {
            cx.emit(InspectorEvent::WorkspaceChanged(surface));
        }
    }

    fn load_workspace(&mut self, id: u64, cx: &mut Context<Self>) -> Option<WorkspaceSurface> {
        if self.workspace_active == Some(id) {
            cx.notify();
            return None;
        }
        let index = self.workspace_tabs.iter().position(|tab| tab.id == id)?;
        let previous_index = self
            .workspace_tabs
            .iter()
            .position(|tab| Some(tab.id) == self.workspace_active);
        self.save_active_workspace();
        self.tab_direction = if previous_index.is_none_or(|previous| index >= previous) {
            1.0
        } else {
            -1.0
        };
        let tab = &self.workspace_tabs[index];
        let surface = tab.surface;
        if let Some(viewer) = &tab.viewer {
            self.code_viewer = viewer.clone();
        }
        self.review_tab = tab.review_tab;
        self.scroll = tab.scroll.clone();
        self.diff_layer = tab.diff_layer;
        self.comparison = tab.comparison;
        self.browser_query = tab.browser_query.clone();
        self.browser_state = tab.browser_state.clone();
        self.workspace_active = Some(id);
        self.diff_selection.clear();
        self.selected_turn = None;
        self.tab_transition_generation = self.tab_transition_generation.wrapping_add(1);
        self.workspace_selected = Some(surface);
        self.comparison_menu_open = false;
        self.files_open = false;
        self.commit_open = false;
        self.browser_address_focused = false;
        let preference_tab = match surface {
            WorkspaceSurface::Files => Some(InspectorTab::Code),
            WorkspaceSurface::Review => Some(self.review_tab),
            WorkspaceSurface::Details => Some(InspectorTab::Info),
            _ => None,
        };
        if let Some(tab) = preference_tab {
            self.selected_tab = tab;
            let project = self.selected_session().map(|record| record.project_id.0);
            let visible = self.visible;
            let _ = self
                .runtime
                .store
                .write()
                .expect("session store lock poisoned")
                .update_preferences(|prefs| match &project {
                    Some(project) => prefs.remember_inspector_project(project, visible, tab),
                    None => prefs.inspector_tab = tab,
                });
        }
        if surface == WorkspaceSurface::Review {
            self.refresh(true, cx);
        }
        self.reconcile_diff_polling(cx);
        self.refresh_sidebar(cx);
        cx.notify();
        Some(surface)
    }

    fn close_workspace(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.workspace_tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let tab = self.workspace_tabs.remove(index);
        if self.workspace_active == Some(id) {
            self.workspace_active = None;
            self.workspace_selected = None;
            if let Some(next) = self
                .workspace_tabs
                .get(index.min(self.workspace_tabs.len().saturating_sub(1)))
            {
                self.activate_workspace(next.id, cx);
            } else {
                // The panel always shows a surface: closing the last one
                // falls back to Details instead of an empty panel.
                self.add_workspace(WorkspaceSurface::Details, cx);
            }
        }
        self.reconcile_diff_polling(cx);
        cx.emit(InspectorEvent::WorkspaceClosed {
            surface: tab.surface,
            id,
        });
        cx.notify();
    }

    fn select_comparison(&mut self, comparison: SessionDiffBase, cx: &mut Context<Self>) {
        self.comparison_menu_open = false;
        if self.comparison == comparison {
            cx.notify();
            return;
        }
        self.comparison = comparison;
        self.omitted_untracked_open = false;
        self.scroll = UniformListScrollHandle::new();
        self.scrollbar_interaction = ScrollbarInteraction::default();
        self.scrollbar_layout_primed = false;
        self.refresh(true, cx);
    }

    fn select_diff_layer(&mut self, layer: DiffLayer, cx: &mut Context<Self>) {
        self.files_open = false;
        self.armed_hunk = None;
        self.diff_selection.clear();
        self.selected_turn = None;
        self.discard_armed = false;
        self.commit_open = false;
        if self.diff_layer == layer {
            cx.notify();
            return;
        }
        self.diff_layer = layer;
        self.omitted_untracked_open = false;
        self.scroll = UniformListScrollHandle::new();
        self.scrollbar_interaction = ScrollbarInteraction::default();
        self.scrollbar_layout_primed = false;
        self.refresh(true, cx);
    }

    /// Expands or collapses the omitted-untracked notice. The open state
    /// survives a Git refresh so staging one listed file keeps the list open.
    fn toggle_omitted_untracked(&mut self, cx: &mut Context<Self>) {
        self.omitted_untracked_open = !self.omitted_untracked_open;
        self.scrollbar_layout_primed = false;
        cx.notify();
    }

    fn jump_to_diff_row(&mut self, row: usize, cx: &mut Context<Self>) {
        self.files_open = false;
        self.scroll.scroll_to_item(row, ScrollStrategy::Top);
        cx.notify();
    }

    fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.visible || (self.loading && !force) {
            return;
        }
        let Some(context) = self.selected_context() else {
            self.context = None;
            self.state = LoadState::NoSession;
            self.review_state = ReviewLoadState::NoSession;
            self.transcript_state = TranscriptLoadState::Unavailable;
            self.transcript_version = None;
            self.transcript_task = None;
            self.transcript_context = None;
            cx.emit(InspectorEvent::ContextUsageChanged);
            for tab in &self.workspace_tabs {
                if let Some(viewer) = &tab.viewer {
                    viewer.update(cx, |viewer, cx| viewer.set_workspace(None, cx));
                }
            }
            cx.notify();
            return;
        };
        let context_changed = self.context.as_ref() != Some(&context);
        if context_changed {
            self.scroll = UniformListScrollHandle::new();
            self.scrollbar_interaction = ScrollbarInteraction::default();
            self.scrollbar_layout_primed = false;
            self.files_open = false;
            self.armed_hunk = None;
            self.omitted_untracked_open = false;
            self.diff_selection.clear();
            self.selected_turn = None;
            self.ask_draft = None;
            self.ask_feedback = None;
            self.ask_query.clear();
            self.transcript_version = None;
            self.transcript_context = None;
            cx.emit(InspectorEvent::ContextUsageChanged);
            let workspace = (!context.remote).then(|| context.cwd.clone());
            for tab in &self.workspace_tabs {
                if let Some(viewer) = &tab.viewer {
                    viewer.update(cx, |viewer, cx| viewer.set_workspace(workspace.clone(), cx));
                }
            }
        }
        self.context = Some(context.clone());
        if context_changed || force {
            self.refresh_transcript(&context, false, cx);
        }
        if self.workspace_selected != Some(WorkspaceSurface::Review)
            || self.selected_tab != InspectorTab::Changes
        {
            self.loading = false;
            self.refresh_task = None;
            self.review_task = None;
            self.refresh_sidebar(cx);
            cx.notify();
            return;
        }
        self.refresh_review(&context, force, cx);
        if !force && !context_changed && matches!(self.state, LoadState::NoSession) {
            return;
        }

        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        if should_show_blocking_git_loading(context_changed, &self.state) {
            self.state = LoadState::Loading;
            cx.notify();
        }
        let cwd = context.cwd;
        let session_id = context.id;
        let remote = context.remote;
        let comparison = self.comparison;
        let layer = self.diff_layer;
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = if remote {
                tokio
                    .spawn(async move { client.read_diff(&session_id, comparison).await })
                    .await
                    .map_err(|error| format!("Diff request stopped: {error}"))
                    .and_then(|result| result.map_err(|error| error.to_string()))
                    .map(snapshot_from_read_diff)
                    .map(Arc::new)
            } else {
                cx.background_spawn(async move { load_local_diff(&cwd, layer) })
                    .await
                    .map_err(|error| error.to_string())
                    .map(Arc::new)
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                let next = match result {
                    Ok(snapshot) => LoadState::Ready(snapshot),
                    Err(error) => LoadState::Error(error),
                };
                if this.state != next {
                    this.state = next;
                    // Row indices are only meaningful for the snapshot they
                    // came from. Clearing avoids silently quoting a different
                    // hunk after a live Git refresh inserts or removes rows.
                    this.diff_selection.clear();
                    this.scrollbar_layout_primed = false;
                    cx.notify();
                }
            });
        }));
    }

    fn refresh_transcript(
        &mut self,
        context: &DiffContext,
        debounce: bool,
        cx: &mut Context<Self>,
    ) {
        self.transcript_task = None;
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        let generation = self.transcript_generation;
        let supported = matches!(
            context.kind.id(),
            ProtoAgentKind::CLAUDE_CODE_ID | ProtoAgentKind::CODEX_ID
        );
        let Some((path, agent_id)) = context
            .transcript_path
            .clone()
            .zip(context.agent_session_id.clone())
            .filter(|_| !context.remote && supported)
        else {
            self.transcript_state = TranscriptLoadState::Unavailable;
            self.transcript_version = None;
            self.transcript_context = None;
            cx.emit(InspectorEvent::ContextUsageChanged);
            return;
        };
        let kind = context.kind.clone();
        let cwd = context.cwd.to_string_lossy().into_owned();
        let home = self.transcript_home.clone();
        let previous = self.transcript_version;
        if previous.is_none() {
            self.transcript_state = TranscriptLoadState::Loading;
            self.transcript_context = None;
            cx.emit(InspectorEvent::ContextUsageChanged);
        }
        let loaded_context = context.clone();
        self.transcript_task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor()
                    .timer(TRANSCRIPT_REFRESH_DEBOUNCE)
                    .await;
            }
            let result = cx
                .background_spawn(async move {
                    load_transcript(&home, &path, &kind, &agent_id, &cwd, previous)
                })
                .await
                .map_err(|_| ());
            let _ = this.update(cx, |this, cx| {
                if this.transcript_generation != generation
                    || !this.visible
                    || this.context.as_ref() != Some(&loaded_context)
                {
                    return;
                }
                match result {
                    Ok(Some(snapshot)) => {
                        this.transcript_version = Some(snapshot.version);
                        this.transcript_state =
                            TranscriptLoadState::Ready(Arc::new(snapshot.document));
                        this.transcript_context = Some(loaded_context);
                    }
                    Ok(None) => {}
                    Err(()) => {
                        this.transcript_version = None;
                        this.transcript_state = TranscriptLoadState::Error;
                        this.transcript_context = None;
                    }
                }
                cx.emit(InspectorEvent::ContextUsageChanged);
                cx.notify();
            });
        }));
    }

    fn refresh_review(&mut self, context: &DiffContext, force: bool, cx: &mut Context<Self>) {
        if context.remote {
            self.review_state = ReviewLoadState::Remote;
            return;
        }
        if self.review_action_busy && !force {
            return;
        }
        self.review_generation = self.review_generation.wrapping_add(1);
        let generation = self.review_generation;
        let cwd = context.cwd.clone();
        if !matches!(self.review_state, ReviewLoadState::Ready(_)) {
            self.review_state = ReviewLoadState::Loading;
        }
        self.review_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let repository = GitRepository::discover(&cwd)?;
                    repository.status()
                })
                .await
                .map_err(|error: GitReviewError| error.to_string());
            let _ = this.update(cx, |this, cx| {
                if this.review_generation != generation {
                    return;
                }
                this.review_state = match result {
                    Ok(status) => ReviewLoadState::Ready(Arc::new(status)),
                    Err(error) => ReviewLoadState::Error(error),
                };
                cx.notify();
            });
        }));
    }

    fn run_review_action(&mut self, action: ReviewAction, cx: &mut Context<Self>) {
        if self.review_action_busy {
            return;
        }
        let Some(context) = self.context.clone().filter(|context| !context.remote) else {
            return;
        };
        self.review_action_busy = true;
        self.review_feedback = None;
        self.discard_armed = false;
        self.armed_hunk = None;
        let is_commit = matches!(action, ReviewAction::Commit(_));
        self.review_action_generation = self.review_action_generation.wrapping_add(1);
        let generation = self.review_action_generation;
        let target = context.id.clone();
        cx.notify();
        self.review_action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let repository = GitRepository::discover(&context.cwd)?;
                    match action {
                        ReviewAction::Stage(paths) => {
                            repository.stage_paths(&paths)?;
                            Ok("Changes staged".to_owned())
                        }
                        ReviewAction::Unstage(paths) => {
                            repository.unstage_paths(&paths)?;
                            Ok("Changes moved back to the working tree".to_owned())
                        }
                        ReviewAction::Discard(paths) => {
                            repository.discard_unstaged(&paths)?;
                            Ok("Unstaged edits discarded".to_owned())
                        }
                        ReviewAction::Patch { patch, mutation } => {
                            repository.apply_patch(&patch, mutation)?;
                            Ok(match mutation {
                                PatchMutation::Stage => "Hunk staged",
                                PatchMutation::Unstage => "Hunk moved back to the working tree",
                                PatchMutation::Discard => "Hunk discarded",
                            }
                            .to_owned())
                        }
                        ReviewAction::Commit(message) => {
                            let commit = repository.commit(&message)?;
                            Ok(format!("Committed {} · {}", commit.oid, commit.summary))
                        }
                    }
                })
                .await
                .map_err(|error: GitReviewError| error.to_string());
            let _ = this.update(cx, |this, cx| {
                if this.review_action_generation != generation
                    || this.selected_session().as_ref().map(|s| &s.id) != Some(&target)
                {
                    return;
                }
                this.review_action_busy = false;
                match result {
                    Ok(message) => {
                        this.review_feedback = Some((true, message));
                        // Staging, unstaging, and discarding share this path
                        // with the composer open; only a landed commit
                        // consumes the draft message.
                        if is_commit {
                            this.commit_open = false;
                            this.commit_query.clear();
                        }
                    }
                    Err(message) => this.review_feedback = Some((false, message)),
                }
                this.refresh(true, cx);
                cx.notify();
            });
        }));
    }

    fn submit_commit(&mut self, cx: &mut Context<Self>) {
        let message = self.commit_query.text().trim().to_owned();
        if message.is_empty() {
            self.review_feedback = Some((false, "Write a commit message first".to_owned()));
            cx.notify();
            return;
        }
        self.run_review_action(ReviewAction::Commit(message), cx);
    }

    fn open_ask(
        &mut self,
        evidence: Vec<ReviewEvidence>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = if evidence.len() == 1 {
            evidence[0].label()
        } else {
            format!("{} review contexts", evidence.len())
        };
        self.ask_generation = self.ask_generation.wrapping_add(1);
        self.ask_draft = Some(AskDraft { evidence, label });
        self.ask_feedback = None;
        self.ask_query.clear();
        self.ask_query
            .insert("Review this for correctness, regressions, and missing tests.");
        self.ask_query.select_all();
        self.commit_open = false;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn set_ask_question(&mut self, question: &str, cx: &mut Context<Self>) {
        self.ask_query.clear();
        self.ask_query.insert(question);
        self.ask_query.select_all();
        cx.notify();
    }

    fn submit_ask(&mut self, cx: &mut Context<Self>) {
        if self.ask_busy {
            return;
        }
        let Some(draft) = self.ask_draft.clone() else {
            return;
        };
        let question = self.ask_query.text().trim().to_owned();
        let prompt = match ReviewPrompt::compose(&draft.evidence, &question) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.ask_feedback = Some((false, error.to_string()));
                cx.notify();
                return;
            }
        };
        let Some(session) = self.selected_session() else {
            self.ask_feedback = Some((false, "Select an agent first".to_owned()));
            cx.notify();
            return;
        };

        self.ask_busy = true;
        self.ask_feedback = None;
        let subject = prompt.subject_label.clone();
        let session_id = session.id;
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        let drafts = self.runtime.prompt_drafts.clone();
        let remote = session.host.is_some();
        let target = session_id.clone();
        let generation = self.ask_generation;
        self.ask_task = Some(cx.spawn(async move |this, cx| {
            let result = tokio
                .spawn(async move {
                    let prepared = drafts.prepare(&session_id, remote, Some(&prompt.text))?;
                    drafts.deliver(&client, prepared).await
                })
                .await
                .map_err(|error| format!("Agent send stopped: {error}"))
                .and_then(|result| result);
            let _ = this.update(cx, |this, cx| {
                if this.ask_generation != generation
                    || this.selected_session().as_ref().map(|s| &s.id) != Some(&target)
                {
                    return;
                }
                this.ask_busy = false;
                match result {
                    Ok(()) => {
                        this.ask_feedback = Some((true, format!("Sent · {subject}")));
                        this.ask_query.clear();
                    }
                    Err(error) => this.ask_feedback = Some((false, error)),
                }
                cx.notify();
            });
        }));
    }

    /// A saved workspace supplies this window's focused pane; it never rewrites
    /// the shared session selection used by other windows.
    pub(crate) fn set_session_context(
        &mut self,
        context: Option<Option<SessionId>>,
        cx: &mut Context<Self>,
    ) {
        if self.session_context != context {
            let previous = self.selected_context();
            self.session_context = context;
            if self.selected_context() != previous {
                self.transcript_context = None;
            }
            cx.emit(InspectorEvent::ContextUsageChanged);
            self.refresh_if_context_changed(cx);
            cx.notify();
        }
    }

    fn selected_session(&self) -> Option<SessionRecord> {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        match &self.session_context {
            Some(id) => id
                .as_ref()
                .and_then(|id| store.sessions().get(id))
                .filter(|record| InspectorTarget::is_available(record))
                .map(AsRef::as_ref),
            None => store
                .selected_session()
                .filter(|record| InspectorTarget::is_available(record)),
        }
        .cloned()
    }

    fn markdown_document(&mut self, source: &str) -> Arc<MarkdownDocument> {
        if let Some(document) = self.markdown_cache.get(source) {
            return Arc::clone(document);
        }
        if self.markdown_cache.len() >= 24 {
            self.markdown_cache.clear();
        }
        let document = Arc::new(MarkdownDocument::parse(source));
        self.markdown_cache
            .insert(source.to_owned(), Arc::clone(&document));
        document
    }

    fn browser_url(&self) -> Option<String> {
        let typed = self.browser_query.text().trim();
        if typed.is_empty() {
            return None;
        }
        crate::agent_catalog::normal_web_url(typed).or_else(|| {
            let candidate = url::Url::parse(&format!("https://{typed}")).ok()?;
            let local = candidate.host_str().is_some_and(|host| {
                host == "localhost"
                    || host == "[::1]"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            crate::agent_catalog::normal_web_url(&format!(
                "{}://{typed}",
                if local { "http" } else { "https" }
            ))
        })
    }

    pub(crate) fn focus_browser_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.browser_address_focused = true;
        self.browser_query.select_all();
        window.focus(&self.focus, cx);
        #[cfg(target_os = "macos")]
        if let Some(browser) = &self.native_browser {
            browser.borrow().focus_chrome();
        }
        cx.notify();
    }

    pub(crate) fn browser_shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.visible || self.workspace_selected != Some(WorkspaceSurface::Browser) {
            return false;
        }
        let key = &event.keystroke;
        if !key.modifiers.platform || key.modifiers.control || key.modifiers.alt {
            return false;
        }
        if key.key.eq_ignore_ascii_case("l") {
            self.focus_browser_address(window, cx);
            return true;
        }
        let focused = self.focus.is_focused(window);
        #[cfg(target_os = "macos")]
        let focused = focused
            || self
                .native_browser
                .as_ref()
                .is_some_and(|browser| browser.borrow().has_focus());
        if !focused || key.modifiers.shift {
            return false;
        }
        match key.key.as_str() {
            "t" => {
                // The browser is a singleton: ⌘T reuses the surface and
                // focuses the address field so typing replaces the page.
                self.select_workspace(WorkspaceSurface::Browser, cx);
                // WorkspaceChanged resets root focus; defer until it has run.
                cx.defer_in(window, |this, window, cx| {
                    this.focus_browser_address(window, cx)
                });
            }
            "w" => {
                if let Some(id) = self.workspace_active {
                    self.close_workspace(id, cx);
                }
            }
            "r" => cx.emit(InspectorEvent::Browser(BrowserAction::Reload)),
            "[" => cx.emit(InspectorEvent::Browser(BrowserAction::Back)),
            "]" => cx.emit(InspectorEvent::Browser(BrowserAction::Forward)),
            _ => return false,
        }
        true
    }

    fn navigate_browser(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self.browser_url() else {
            return;
        };
        self.browser_query.clear();
        self.browser_query.insert(&url);
        self.browser_address_focused = false;
        cx.emit(InspectorEvent::Browser(BrowserAction::Navigate(url)));
        cx.notify();
    }

    fn apply_browser_edit(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        match event.keystroke.key.as_str() {
            "escape" => {
                self.browser_address_focused = false;
                self.browser_query.clear();
                if let Some(url) = &self.browser_state.url {
                    self.browser_query.insert(url);
                }
            }
            "enter" => self.navigate_browser(cx),
            _ => match query_editor::edit_for(&event.keystroke) {
                Some(Edit::Local(edit)) => {
                    self.browser_query.apply(edit);
                }
                Some(Edit::Clipboard(ClipboardEdit::Copy)) => {
                    query_editor::copy_selection(&self.browser_query, cx);
                }
                Some(Edit::Clipboard(ClipboardEdit::Cut)) => {
                    query_editor::cut_selection(&mut self.browser_query, cx);
                }
                Some(Edit::Clipboard(ClipboardEdit::Paste)) => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.browser_query.insert(&text);
                    }
                }
                None => return false,
            },
        }
        cx.notify();
        true
    }

    /// The Notes surface: the shared notes index, filtered to the
    /// inspector's session project or to the global notes — or the open
    /// note's detail page while one is open.
    fn render_notes(&mut self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        self.notes.sync_model(cx);
        let entries = self.notes.entries(cx);
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        if let Some(detail) = self.notes.detail().cloned() {
            return self.render_note_detail(detail, &entries, &store, colors, cx);
        }
        // Back restores the list's own highlight, not the main selection.
        let detail_session = self.notes.selected_session();
        let selected = self.selected_session().map(|record| record.id.clone());
        let project = selected
            .as_ref()
            .and_then(|id| store.sessions().get(id))
            .map(|record| record.project_id.clone());
        let project_name = project
            .as_ref()
            .and_then(|id| store.projects().get(id).map(|project| project.name.clone()));
        crate::notes::panel::render_panel(
            &mut self.notes,
            &store,
            &entries,
            project,
            project_name,
            detail_session.as_ref(),
            colors,
            &self.focus,
            cx,
        )
    }

    /// The open note's page: a back header over the editor, or the pending
    /// placeholder while the Engine has not answered yet.
    fn render_note_detail(
        &self,
        detail: NoteDetail,
        entries: &[crate::notes::search::NoteEntry],
        store: &crate::store::SessionStore,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (note_id, workspace, session, pending) = match &detail {
            NoteDetail::Open { session, note_id } => {
                let workspace = store
                    .sessions()
                    .get(session)
                    .and_then(|record| record.note_workspace.clone());
                (
                    Some(note_id.clone()),
                    workspace,
                    Some(session.clone()),
                    false,
                )
            }
            NoteDetail::Pending(pending) => (
                pending.note_id.clone(),
                pending.workspace.clone(),
                None,
                true,
            ),
        };
        let entry = note_id.as_ref().and_then(|id| {
            entries
                .iter()
                .find(|entry| &entry.id == id && entry.workspace == workspace)
        });
        let home = note_id
            .as_ref()
            .and_then(|id| crate::notes::panel::note_home(store, id, &workspace, session.as_ref()));
        let pinned = session.as_ref().is_some_and(|session| {
            store
                .preferences()
                .sidebar_pinned_sessions
                .contains(session)
        });
        let title = entry
            .map(|entry| entry.title.clone())
            .or_else(|| {
                session.as_ref().and_then(|session| {
                    store
                        .sessions()
                        .get(session)
                        .map(|record| record.title.clone())
                })
            })
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| {
                if pending {
                    "New note".to_owned()
                } else {
                    "Untitled".to_owned()
                }
            });
        let scope = match &workspace {
            Some(id) => store
                .projects()
                .get(id)
                .map(|project| project.name.clone())
                .unwrap_or_else(|| "Project".to_owned()),
            None => "Global".to_owned(),
        };
        let meta = match entry {
            Some(entry) => format!(
                "{scope} · {}",
                crate::navigation::relative_time(entry.modified_ms as f64)
            ),
            None => scope,
        };
        let pane = (!pending).then(|| self.notes.note_pane()).flatten();
        crate::notes::panel::render_detail(
            entry, home, pinned, title, meta, pending, pane, colors, cx,
        )
    }

    pub(crate) fn set_notes_scope(&mut self, scope: crate::notes::panel::NotesScope) {
        self.notes.set_scope(scope);
    }

    pub(crate) fn focus_notes_filter(&mut self) {
        self.notes.focus_filter();
    }

    /// Shows `note_id` for `session` in the detail page and focuses the
    /// editor. Switching notes saves the previous one; the list underneath
    /// keeps its scope, filter, and scroll.
    pub(crate) fn open_note_detail(
        &mut self,
        session: SessionId,
        note_id: String,
        block: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notes.blur_filter();
        self.notes.set_detail(Some(NoteDetail::Open {
            session: session.clone(),
            note_id: note_id.clone(),
        }));
        let pane = self.note_pane(window, cx);
        pane.update(cx, |pane, cx| {
            pane.request_focus();
            pane.show(&session, &note_id, window, cx);
            if let Some(block) = block {
                pane.reveal_block(block, window, cx);
            }
        });
        // Revealing the Notes surface resets focus during the ensuing render.
        // Refocus after that transition so the note editor retains ownership.
        let focus = pane.read(cx).focus_handle(cx);
        cx.defer_in(window, move |_, window, cx| {
            window.focus(&focus, cx);
        });
        cx.notify();
    }

    /// Parks a note the Engine has not answered for yet. The window's store
    /// sync resolves it into the detail page — or drops it, if the person
    /// backed out first.
    pub(crate) fn open_pending_note_detail(
        &mut self,
        pending: PendingNote,
        cx: &mut Context<Self>,
    ) {
        if let Some(NoteDetail::Open { session, note_id }) = self.notes.detail().cloned()
            && let Some(pane) = self.notes.note_pane()
        {
            pane.update(cx, |pane, cx| {
                pane.save(cx);
                pane.discard_open(&session, &note_id, cx);
            });
        }
        self.notes.blur_filter();
        self.notes.set_detail(Some(NoteDetail::Pending(pending)));
        cx.notify();
    }

    pub(crate) fn pending_note_detail(&self) -> Option<PendingNote> {
        match self.notes.detail() {
            Some(NoteDetail::Pending(pending)) => Some(pending.clone()),
            _ => None,
        }
    }

    /// The detail page's Session while one is resolved. Row clicks prefer it,
    /// so re-clicking the open note stays put.
    pub(crate) fn open_note_session(&self) -> Option<SessionId> {
        self.notes.detail_session()
    }

    /// Back to the list: saves the open note, drops the detail (resolved or
    /// still pending), and returns focus to the panel.
    pub(crate) fn close_note_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(NoteDetail::Open { session, note_id }) = self.notes.detail().cloned()
            && let Some(pane) = self.notes.note_pane()
        {
            pane.update(cx, |pane, cx| {
                pane.save(cx);
                pane.discard_open(&session, &note_id, cx);
            });
        }
        self.notes.set_detail(None);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Backs out of the detail page when it shows `note_id`: a trashed note
    /// must not linger open. A fresh pending has no id yet, so only an
    /// orphan pending or a resolved detail can match. The editor drops the
    /// trashed note without saving, so no later save resurrects its file.
    pub(crate) fn close_note_detail_if(
        &mut self,
        note_id: &str,
        workspace: &Option<ProjectId>,
        session: Option<&SessionId>,
        cx: &mut Context<Self>,
    ) {
        let discard = match self.notes.detail() {
            Some(NoteDetail::Open {
                session: open,
                note_id: id,
            }) if id == note_id
                && session.is_none_or(|known| known == open)
                && self
                    .runtime
                    .store
                    .read()
                    .expect("store")
                    .sessions()
                    .get(open)
                    .map_or(session == Some(open), |record| {
                        &record.note_workspace == workspace
                    }) =>
            {
                Some((open.clone(), id.clone()))
            }
            Some(NoteDetail::Pending(pending))
                if pending.note_id.as_deref() == Some(note_id)
                    && &pending.workspace == workspace =>
            {
                None
            }
            _ => return,
        };
        self.notes.set_detail(None);
        if let (Some((session, id)), Some(pane)) = (discard, self.notes.note_pane()) {
            pane.update(cx, |pane, cx| pane.discard_open(&session, &id, cx));
        }
        cx.notify();
    }

    /// Hosts a fixture note pane: the macOS window screenshots, and the
    /// cross-platform note-detail tests.
    #[cfg(test)]
    pub(crate) fn set_note_pane_for_test(&mut self, pane: Entity<NotePane>) {
        self.notes.set_note_pane(pane);
    }

    /// The surface's one note editor, created on first open. Escape with
    /// nothing left to dismiss backs out to the list; a mention chip routes
    /// through the window, which opens notes here and sessions in the pane.
    fn note_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<NotePane> {
        if let Some(pane) = self.notes.note_pane() {
            return pane;
        }
        let runtime = Arc::clone(&self.runtime);
        let pane = cx.new(|cx| NotePane::new(runtime, cx));
        let recipient = self
            .selected_session()
            .filter(|s| !s.is_note())
            .map(|s| (s.id, s.title));
        pane.update(cx, |pane, cx| pane.set_attachment_recipient(recipient, cx));
        cx.subscribe_in(&pane, window, |this, _, event, window, cx| match event {
            NotePaneEvent::Dismiss => this.close_note_detail(window, cx),
            NotePaneEvent::Reveal(id) => {
                cx.emit(InspectorEvent::RevealSession {
                    session: id.clone(),
                });
            }
            NotePaneEvent::Attach { recipient, source } => {
                if this.selected_session().as_ref().map(|s| &s.id) != Some(recipient) {
                    return;
                }
                let drafts = this.runtime.prompt_drafts.clone();
                let recipient = recipient.clone();
                let source = source.clone();
                cx.spawn(async move |this, cx| {
                    let target = recipient.clone();
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            drafts
                                .stage(&target, crate::prompt_draft::PromptAttachment::note(source))
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        match result {
                            Ok(_) => {
                                this.sidebar.feedback.remove(&recipient);
                            }
                            Err(error) => {
                                this.sidebar.feedback.insert(recipient, error);
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            NotePaneEvent::AttachSelection { recipient, quote } => {
                if this.selected_session().as_ref().map(|s| &s.id) != Some(recipient) {
                    return;
                }
                match this.runtime.prompt_drafts.stage(
                    recipient,
                    crate::prompt_draft::PromptAttachment::selection(quote.clone()),
                ) {
                    Ok(_) => {
                        this.sidebar.feedback.remove(recipient);
                    }
                    Err(error) => {
                        this.sidebar.feedback.insert(recipient.clone(), error);
                    }
                }
                cx.notify();
            }
        })
        .detach();
        self.notes.set_note_pane(pane.clone());
        pane
    }

    /// Keystrokes for the notes filter when it holds the keyboard.
    fn apply_notes_edit(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.workspace_selected != Some(WorkspaceSurface::Notes)
            || !self.notes.filter_focused()
            || self.notes.detail().is_some()
        {
            return false;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.notes.query_mut().clear();
                self.notes.blur_filter();
                cx.notify();
                true
            }
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return false;
                };
                match edit {
                    Edit::Local(local) => {
                        self.notes.query_mut().apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(self.notes.query_mut(), cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(self.notes.query_mut(), cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            self.notes.query_mut().insert(&text);
                        }
                    }
                }
                cx.notify();
                true
            }
        }
    }

    fn render_browser(
        &self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_url = self.browser_state.url.is_some() || self.browser_url().is_some();
        let mut detected = div()
            .id("browser-detected-links")
            .flex_none()
            .max_h(px(120.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .px_2();
        if let Some(session) = self.selected_session() {
            for (ix, (label, url)) in detected_browser_urls(&session).into_iter().enumerate() {
                detected = detected.child(sidebar::button(
                    format!("browser-detected-{ix}"),
                    label,
                    colors,
                    window,
                    cx,
                    move |_, _, cx| {
                        cx.emit(InspectorEvent::Browser(BrowserAction::Navigate(
                            url.clone(),
                        )))
                    },
                ));
            }
            if session.host.is_some()
                && session
                    .listening_ports
                    .as_ref()
                    .is_some_and(|p| !p.is_empty())
            {
                detected = detected.child(sidebar::message(
                    "Detected remote ports have no local preview transport.",
                    colors,
                ));
            }
        }
        let nav_button = |id: &'static str,
                          symbol: &'static str,
                          action: BrowserAction,
                          enabled: bool,
                          cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(26.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::BADGE))
                .text_color(if enabled {
                    colors.secondary
                } else {
                    colors.primary.alpha(0.24)
                })
                .when(enabled, |button| {
                    button
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                        .on_click(cx.listener(move |_this, _, _, cx| {
                            cx.emit(InspectorEvent::Browser(action.clone()));
                            cx.stop_propagation();
                        }))
                })
                .child(sf_symbol_weighted(
                    symbol,
                    10.5,
                    SymbolWeight::Semibold,
                    if enabled {
                        colors.secondary
                    } else {
                        colors.primary.alpha(0.24)
                    },
                ))
        };
        let url_label = if self.browser_query.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child("Enter a URL or local preview address")
                .into_any_element()
        } else {
            crate::navigation::query_label(&self.browser_query)
        };
        div()
            .id("workspace-browser")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(42.0))
                    .flex_none()
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(3.0))
                    .border_b_1()
                    .border_color(colors.primary.alpha(0.065))
                    .child(nav_button("browser-back", "chevron.left", BrowserAction::Back, self.browser_state.can_go_back, cx))
                    .child(nav_button("browser-forward", "chevron.right", BrowserAction::Forward, self.browser_state.can_go_forward, cx))
                    .child(nav_button("browser-reload", "arrow.triangle.2.circlepath", BrowserAction::Reload, has_url, cx))
                    .child(
                        div()
                            .id("browser-address")
                            .debug_selector(|| "browser-address".into())
                            .min_w(px(0.0))
                            .flex_1()
                            .h(px(28.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::BADGE))
                            .bg(colors.primary.alpha(0.045))
                            .border_1()
                            .border_color(if self.browser_address_focused { rgba(0x4f83f1cc) } else { colors.primary.alpha(0.075) })
                            .text_size(px(10.5))
                            .text_color(colors.primary)
                            .cursor_text()
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                                this.focus_browser_address(window, cx);
                                cx.stop_propagation();
                            }))
                            .child(url_label),
                    )
                    .child(
                        div()
                            .id("browser-open-external")
                            .size(px(26.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::BADGE))
                            .text_color(if has_url { colors.secondary } else { colors.primary.alpha(0.24) })
                            .when(has_url, |button| button.cursor_pointer().hover(move |button| button.bg(colors.primary.alpha(0.07)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(url) = this.browser_state.url.clone().or_else(|| this.browser_url()) {
                                        cx.emit(InspectorEvent::Browser(BrowserAction::OpenExternal(url)));
                                    }
                                    cx.stop_propagation();
                                })))
                            .child(sf_symbol("link", 10.5, if has_url { colors.secondary } else { colors.primary.alpha(0.24) })),
                    ),
            )
            .child(detected)
            .child(
                div()
                    .id("workspace-browser-loading-space")
                    .relative()
                    .min_h(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .text_center()
                    .text_color(colors.tertiary)
                    .when(!has_url, |body| body
                        .child(sf_symbol("network", 26.0, colors.tertiary))
                        .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).text_color(colors.secondary).child("Open a page"))
                        .child(div().max_w(px(230.0)).text_size(px(11.0)).line_height(px(17.0)).child("Browse a local preview or any secure web address without leaving the workspace.")))
                    .when_some(self.browser_state.error.clone(), |body, error| body.child(div().max_w(px(260.0)).text_size(px(12.0)).child(error)))
                    .when(self.browser_state.is_loading, |body| body.child(div().text_size(px(10.0)).child("Loading…")))
                    .map(|body| {
                        #[cfg(target_os = "macos")]
                        let body = body.when_some(self.native_browser.clone(), |body, browser| {
                            body.child(crate::macos::browser::NativeBrowser::surface(browser))
                        });
                        body
                    }),
            )
            .into_any_element()
    }

    fn render_info(
        &mut self,
        session: Option<&SessionRecord>,
        colors: SemanticColors,
    ) -> AnyElement {
        let Some(session) = session else {
            return sidebar::panel("inspector-info-scroll", colors)
                .child(sidebar::message("No inspected session", colors))
                .into_any_element();
        };
        let store = self.runtime.store.read().expect("store");
        let project = store
            .projects()
            .get(&session.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| folder_name(&session.cwd));
        let kind = ui_agent_kind(session.effective_kind());
        let agent = if kind == AgentKind::Generic {
            generic_agent_label(
                store.agent_descriptor(session.effective_kind()),
                session.effective_kind().id(),
            )
        } else {
            kind.label().to_owned()
        };
        let host = session
            .host
            .as_deref()
            .map(|h| store.host_display_name(h))
            .unwrap_or_else(|| "This Mac".into());
        drop(store);
        let mut content = sidebar::panel("inspector-info-scroll", colors)
            .child(section_label("Session metadata", colors))
            .child(detail_row("Session", session.id.0.clone(), true, colors))
            .child(detail_row("Agent", agent, false, colors))
            .child(detail_row("Project", project, false, colors))
            .child(detail_row("Directory", session.cwd.clone(), true, colors))
            .child(detail_row("Host", host, false, colors))
            .child(detail_row(
                "Status",
                session_status(session, colors).0.into(),
                false,
                colors,
            ))
            .child(detail_row(
                "Updated",
                relative_time(session.updated_at.0),
                false,
                colors,
            ));
        if let Some(branch) = &session.git_branch {
            content = content.child(detail_row("Branch", branch.clone(), true, colors));
        }
        if let Some(bytes) = session.memory_bytes {
            content = content.child(detail_row("Memory", format_bytes(bytes), true, colors));
        }
        content.into_any_element()
    }

    fn render_artifacts(
        &mut self,
        session: Option<&SessionRecord>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(session) = session else {
            return self
                .render_message(
                    colors,
                    "sidebar.left",
                    "Select a session",
                    "Artifacts follow the active agent.",
                )
                .into_any_element();
        };
        if artifact_count(session) == 0 {
            return self
                .render_message(
                    colors,
                    "shippingbox",
                    "No artifacts yet",
                    "Pull requests, previews, Linear issues, and links appear here as they’re discovered. Detected ports live in Browser.",
                )
                .into_any_element();
        }

        let mut content = div()
            .id("inspector-artifacts-scroll")
            .size_full()
            .min_h(px(0.0))
            .px(px(12.0))
            .pt(px(8.0))
            .pb(px(18.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .overflow_y_scroll();

        if let Some(pull_requests) = session.pull_requests.as_deref() {
            let inspector = cx.entity();
            for pull_request in pull_requests {
                let body = pull_request
                    .body
                    .as_deref()
                    .filter(|body| !body.trim().is_empty())
                    .map(|body| self.markdown_document(body));
                content = content.child(render_pull_request(
                    pull_request,
                    session.id.clone(),
                    colors,
                    inspector.clone(),
                    body,
                    self.selected_turn_key().map(str::to_owned),
                ));
            }
        }
        if let Some(artifacts) = session.artifacts.as_deref() {
            for artifact in artifacts {
                let represented_by_status = artifact.kind == ArtifactKind::PullRequest
                    && session.pull_requests.as_deref().is_some_and(|statuses| {
                        statuses.iter().any(|status| status.url == artifact.url)
                    });
                if !represented_by_status {
                    content = content.child(render_artifact_row(artifact, colors));
                }
            }
        }
        content.into_any_element()
    }

    fn scrollbar_metrics(&self) -> Option<ScrollbarMetrics> {
        let base = self.scroll.0.borrow().base_handle.clone();
        let bounds = base.bounds();
        let viewport_height = f32::from(bounds.size.height);
        let max_offset = f32::from(base.max_offset().y).max(0.0);
        if max_offset <= 0.0 || viewport_height <= SCROLLBAR_MIN_THUMB {
            return None;
        }

        let track_height = (viewport_height - SCROLLBAR_INSET * 2.0).max(0.0);
        let content_height = viewport_height + max_offset;
        let thumb_height = (track_height * viewport_height / content_height)
            .max(SCROLLBAR_MIN_THUMB)
            .min(track_height);
        let thumb_travel = (track_height - thumb_height).max(0.0);
        let progress = (-f32::from(base.offset().y) / max_offset).clamp(0.0, 1.0);

        Some(ScrollbarMetrics {
            track_top: f32::from(bounds.origin.y) + SCROLLBAR_INSET,
            track_height,
            thumb_height,
            thumb_top: thumb_travel * progress,
        })
    }

    fn set_scrollbar_offset(&mut self, pointer_y: f32, cx: &mut Context<Self>) {
        let Some(metrics) = self.scrollbar_metrics() else {
            return;
        };
        let thumb_travel = (metrics.track_height - metrics.thumb_height).max(0.0);
        if thumb_travel <= 0.0 {
            return;
        }

        let thumb_top = (pointer_y - metrics.track_top - self.scrollbar_interaction.grab_offset)
            .clamp(0.0, thumb_travel);
        let base = self.scroll.0.borrow().base_handle.clone();
        let max_offset = f32::from(base.max_offset().y).max(0.0);
        let current = base.offset();
        base.set_offset(point(
            current.x,
            px(-(max_offset * thumb_top / thumb_travel)),
        ));
        cx.notify();
    }

    fn finish_scrollbar_drag(&mut self, cx: &mut Context<Self>) {
        if self.scrollbar_interaction.dragging {
            self.scrollbar_interaction.dragging = false;
            cx.notify();
        }
    }

    fn render_scrollbar(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let metrics = self.scrollbar_metrics()?;
        let dragging = self.scrollbar_interaction.dragging;

        let thumb = div()
            .id("diff-scrollbar-thumb")
            .absolute()
            .top(px(metrics.thumb_top))
            .left(px(3.0))
            .right(px(3.0))
            .h(px(metrics.thumb_height))
            .rounded(px(3.0))
            .bg(colors.primary.alpha(if dragging { 0.46 } else { 0.24 }))
            .group_hover("diff-scrollbar", move |style| {
                style.bg(colors.primary.alpha(0.40))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.scrollbar_interaction.dragging = true;
                    this.scrollbar_interaction.grab_offset =
                        (f32::from(event.position.y) - metrics.track_top - metrics.thumb_top)
                            .clamp(0.0, metrics.thumb_height);
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .on_drag(DraggedDiffScrollbar, |value, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| *value)
            });

        Some(
            div()
                .id("diff-scrollbar-track")
                .group("diff-scrollbar")
                .absolute()
                .top(px(SCROLLBAR_INSET))
                .bottom(px(SCROLLBAR_INSET))
                .right(px(2.0))
                .w(px(12.0))
                .rounded(px(6.0))
                .occlude()
                .hover(move |style| style.bg(colors.primary.alpha(0.055)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        this.scrollbar_interaction.dragging = true;
                        this.scrollbar_interaction.grab_offset = metrics.thumb_height / 2.0;
                        this.set_scrollbar_offset(f32::from(event.position.y), cx);
                        cx.stop_propagation();
                    }),
                )
                .on_drag(DraggedDiffScrollbar, |value, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| *value)
                })
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.finish_scrollbar_drag(cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.finish_scrollbar_drag(cx)),
                )
                .child(thumb)
                .into_any_element(),
        )
    }

    fn render_diff(
        &mut self,
        snapshot: Arc<DiffSnapshot>,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // The omitted names are extra rows of the same virtualized list, so
        // thousands of them cost only the handful that are on screen.
        let omitted_notice = snapshot
            .omitted_untracked_notice_row()
            .map(|row| (row, self.omitted_untracked_open));
        let omitted_open = omitted_notice.is_some_and(|(_, open)| open);
        let row_count = snapshot.rows.len()
            + if omitted_open {
                snapshot.omitted_untracked_paths.len()
            } else {
                0
            };
        let text_columns = if omitted_open {
            snapshot
                .omitted_untracked_paths
                .iter()
                .map(|path| path.as_os_str().len() + OMITTED_PATH_INDENT_COLUMNS)
                .fold(snapshot.max_text_columns, usize::max)
        } else {
            snapshot.max_text_columns
        };
        let content_width = (GUTTER_WIDTH + 28.0 + text_columns as f32 * 7.1).clamp(320.0, 3700.0);
        let inspector = cx.entity();
        let armed_hunk = self.armed_hunk;
        let selection = self.diff_selection.clone();
        let list = uniform_list("inspector-diff", row_count, move |range, _, _| {
            render_rows(
                &snapshot,
                range,
                content_width,
                colors,
                inspector.clone(),
                armed_hunk,
                omitted_notice,
                &selection,
            )
        })
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&self.scroll)
        .size_full();
        let scrollbar = self.render_scrollbar(colors, cx);

        // The list's scroll bounds are available after its first layout pass.
        // Re-render once on the next frame so the fixed overlay can size itself.
        if !self.scrollbar_layout_primed {
            self.scrollbar_layout_primed = true;
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }

        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedDiffScrollbar>, _, cx| {
                    this.set_scrollbar_offset(f32::from(event.event.position.y), cx);
                },
            ))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_scrollbar_drag(cx)),
            )
            .child(list)
            .when_some(scrollbar, |body, scrollbar| body.child(scrollbar))
    }

    fn comparison_label(&self) -> String {
        if let LoadState::Ready(snapshot) = &self.state
            && let Some(base_ref) = snapshot.base_ref.as_deref()
        {
            return base_ref.to_owned();
        }
        match self.comparison {
            SessionDiffBase::DefaultBranch => "default branch".to_owned(),
            SessionDiffBase::Head => "HEAD".to_owned(),
        }
    }

    fn render_comparison_option(
        &self,
        comparison: SessionDiffBase,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, detail, selector) = match comparison {
            SessionDiffBase::DefaultBranch => (
                "Default branch",
                "Committed and working changes",
                "INSPECTOR_COMPARE_DEFAULT",
            ),
            SessionDiffBase::Head => ("HEAD", "Uncommitted changes only", "INSPECTOR_COMPARE_HEAD"),
        };
        let selected = self.comparison == comparison;
        div()
            .id(SharedString::from(format!("compare-option-{title}")))
            .debug_selector(move || selector.to_owned())
            .min_h(px(48.0))
            .px(px(10.0))
            .py(px(7.0))
            .flex()
            .items_center()
            .gap(px(9.0))
            .cursor_pointer()
            .bg(if selected {
                colors.primary.alpha(0.075)
            } else {
                colors.primary.alpha(0.0)
            })
            .hover(move |row| row.bg(colors.primary.alpha(0.09)))
            .child(
                div()
                    .w(px(14.0))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .when(selected, |slot| {
                        slot.child(sf_symbol("checkmark", 10.5, colors.primary))
                    }),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(px(Typo::ROW.size))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(detail),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_comparison(comparison, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_layer_option(
        &self,
        layer: DiffLayer,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, selector) = match layer {
            DiffLayer::Branch => ("Branch", "INSPECTOR_LAYER_BRANCH"),
            DiffLayer::Working => ("Working", "INSPECTOR_LAYER_WORKING"),
            DiffLayer::Staged => ("Staged", "INSPECTOR_LAYER_STAGED"),
        };
        let selected = self.diff_layer == layer;
        div()
            .id(SharedString::from(format!("review-layer-{label}")))
            .debug_selector(move || selector.to_owned())
            .h(px(25.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(Radius::CHIP))
            .bg(if selected {
                colors.primary.alpha(0.11)
            } else {
                colors.primary.alpha(0.0)
            })
            .cursor_pointer()
            .hover(move |button| button.bg(colors.primary.alpha(0.085)))
            .text_size(px(9.5))
            .font_weight(if selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::MEDIUM
            })
            .text_color(if selected {
                colors.primary
            } else {
                colors.tertiary
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_diff_layer(layer, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    /// The changed-file rows of the review navigator, without host chrome.
    fn file_navigator_list(
        &self,
        snapshot: Arc<DiffSnapshot>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let mut files = div()
            .id("review-file-navigator-list")
            .max_h(px(390.0))
            .py(px(4.0))
            .overflow_y_scroll();
        for (index, file) in snapshot.file_diffs.iter().enumerate() {
            let row = file.row_range.start;
            files = files.child(
                div()
                    .id(("review-file-navigator-row", index))
                    .debug_selector(move || format!("INSPECTOR_REVIEW_FILE_{index}"))
                    .min_h(px(38.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .mx(px(4.0))
                    .rounded(px(Radius::inner(crate::floating::MENU_RADIUS, 4.0)))
                    .glass_menu_row(colors, false)
                    .child(sf_symbol(
                        "chevron.left.forwardslash.chevron.right",
                        11.5,
                        colors.tertiary,
                    ))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(10.0))
                            .text_color(colors.secondary)
                            .child(file.path.to_string_lossy().into_owned()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(9.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Ink::FRESH)
                            .child(format!("+{}", file.additions)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(9.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Ink::DANGER)
                            .child(format!("−{}", file.deletions)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to_diff_row(row, cx);
                        cx.stop_propagation();
                    })),
            );
        }

        files
    }

    fn render_file_navigator(
        &self,
        snapshot: Arc<DiffSnapshot>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let files = self.file_navigator_list(snapshot, colors, cx);
        let scrim = div().absolute().inset_0().occlude().on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.files_open = false;
                cx.notify();
                cx.stop_propagation();
            }),
        );
        let panel = if crate::floating::uses_panels(false, colors, cx) {
            crate::floating::host_here(
                INSPECTOR_FILES_MENU,
                crate::floating::surface(colors, crate::floating::MENU_RADIUS, 330.0, files)
                    .into_any_element(),
                Some(330.0),
                gpui::Anchor::TopRight,
                8.0,
                cx,
            )
            .absolute()
            .top(px(40.0))
            .right(px(9.0))
            .w(px(0.0))
            .h(px(0.0))
            .into_any_element()
        } else {
            div()
                .id("review-file-navigator")
                .debug_selector(|| "INSPECTOR_FILE_NAVIGATOR".to_owned())
                .absolute()
                .top(px(40.0))
                .right(px(9.0))
                .w(px(330.0))
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.files_open = false;
                    cx.notify();
                }))
                .child(FloatingSurface::new(colors, files).radius(crate::floating::MENU_RADIUS))
                .into_any_element()
        };
        div()
            .absolute()
            .inset_0()
            .child(scrim)
            .child(panel)
            .into_any_element()
    }

    /// The comparison base rows, without host chrome.
    fn comparison_menu_items(&self, colors: SemanticColors, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .py(px(4.0))
            .overflow_hidden()
            .child(self.render_comparison_option(SessionDiffBase::DefaultBranch, colors, cx))
            .child(self.render_comparison_option(SessionDiffBase::Head, colors, cx))
    }

    fn remote_context(&self) -> bool {
        self.context.as_ref().is_some_and(|context| context.remote)
    }

    /// The sidebar palette the inspector paints with, for its panels.
    fn panel_colors(&self) -> SemanticColors {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        crate::app_theme::sidebar_colors_in(&store)
    }

    /// The file navigator's pixels for its floating panel.
    fn files_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.files_open || self.remote_context() {
            return None;
        }
        let LoadState::Ready(snapshot) = &self.state else {
            return None;
        };
        let snapshot = Arc::clone(snapshot);
        let colors = self.panel_colors();
        let files = self.file_navigator_list(snapshot, colors, cx);
        Some(
            crate::floating::surface(colors, crate::floating::MENU_RADIUS, 330.0, files)
                .into_any_element(),
        )
    }

    /// The comparison menu's pixels for its floating panel.
    fn comparison_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !(self.comparison_menu_open && self.remote_context()) {
            return None;
        }
        let colors = self.panel_colors();
        let items = self.comparison_menu_items(colors, cx);
        Some(
            crate::floating::surface(colors, crate::floating::MENU_RADIUS, 230.0, items)
                .into_any_element(),
        )
    }

    fn render_review_controls(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ReviewLoadState::Ready(status) = &self.review_state else {
            let (symbol, label) = match &self.review_state {
                ReviewLoadState::NoSession => ("minus.circle", "Select an agent to review"),
                ReviewLoadState::Remote => (
                    "network",
                    "Remote changes are view-only until Git actions move into the daemon",
                ),
                ReviewLoadState::Loading => ("ellipsis", "Reading index and working tree…"),
                ReviewLoadState::Error(error) => {
                    return div()
                        .px(px(10.0))
                        .py(px(7.0))
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .border_b_1()
                        .border_color(colors.primary.alpha(0.06))
                        .text_size(px(Typo::META.size))
                        .text_color(Ink::ATTENTION)
                        .child(sf_symbol("exclamationmark.triangle", 11.0, Ink::ATTENTION))
                        .child(error.clone())
                        .into_any_element();
                }
                ReviewLoadState::Ready(_) => unreachable!(),
            };
            return div()
                .h(px(36.0))
                .flex_none()
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(7.0))
                .border_b_1()
                .border_color(colors.primary.alpha(0.06))
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(sf_symbol(symbol, 11.0, colors.tertiary))
                .child(label)
                .into_any_element();
        };
        let status = Arc::clone(status);
        let staged_paths: Vec<_> = status
            .staged
            .iter()
            .map(|change| change.path.clone())
            .collect();
        // Conflicted paths are deliberately excluded. `git add` on a file that
        // still carries conflict markers both stages the markers and collapses
        // index stages 1/2/3, after which `git checkout --merge` can no longer
        // reconstruct the conflict. Resolving stays an explicit, per-file act.
        let mut stage_paths: Vec<_> = status
            .unstaged
            .iter()
            .chain(status.untracked.iter())
            .map(|change| change.path.clone())
            .collect();
        stage_paths.sort();
        stage_paths.dedup();
        let discard_paths: Vec<_> = status
            .unstaged
            .iter()
            .map(|change| change.path.clone())
            .collect();
        let staged_count = status.staged.len();
        let working_count = status.unstaged.len() + status.untracked.len();
        let conflicted_count = status.conflicted.len();
        let branch = status
            .branch
            .name
            .clone()
            .unwrap_or_else(|| "Detached HEAD".to_owned());
        let busy = self.review_action_busy;
        let commit_open = self.commit_open;
        let discard_armed = self.discard_armed;

        let mut actions = div().flex().items_center().gap(px(5.0));
        if self.diff_layer == DiffLayer::Working && !stage_paths.is_empty() {
            let paths = stage_paths;
            actions = actions.child(
                div()
                    .id("review-stage-all")
                    .h(px(25.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .bg(if staged_count == 0 {
                        rgba(0xd9775724)
                    } else {
                        colors.primary.alpha(0.055)
                    })
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if staged_count == 0 {
                        rgba(0xe89a7cff)
                    } else {
                        colors.secondary
                    })
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_review_action(ReviewAction::Stage(paths.clone()), cx);
                                cx.stop_propagation();
                            }))
                    })
                    .child(sf_symbol("plus", 9.0, colors.secondary))
                    .child("Stage all"),
            );
        }
        if self.diff_layer == DiffLayer::Staged && !staged_paths.is_empty() {
            let paths = staged_paths;
            actions = actions.child(
                div()
                    .id("review-unstage-all")
                    .h(px(25.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::BADGE))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.tertiary)
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_review_action(ReviewAction::Unstage(paths.clone()), cx);
                                cx.stop_propagation();
                            }))
                    })
                    .child("Unstage"),
            );
            actions = actions.child(
                div()
                    .id("review-open-commit")
                    .h(px(25.0))
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .bg(rgba(0xd9775730))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgba(0xf0aa8fff))
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .hover(|button| button.bg(rgba(0xd9775744)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit_open = !this.commit_open;
                                this.discard_armed = false;
                                this.ask_draft = None;
                                this.ask_feedback = None;
                                this.ask_query.clear();
                                if this.commit_open {
                                    window.focus(&this.focus, cx);
                                }
                                cx.notify();
                                cx.stop_propagation();
                            }))
                    })
                    .child(sf_symbol("checkmark", 9.5, rgba(0xf0aa8fff)))
                    .child(if commit_open { "Cancel" } else { "Commit" }),
            );
        }
        if self.diff_layer == DiffLayer::Working && !discard_paths.is_empty() {
            let paths = discard_paths;
            actions = actions.child(
                div()
                    .id("review-discard-all")
                    .h(px(25.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .rounded(px(Radius::BADGE))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if discard_armed {
                        Ink::DANGER
                    } else {
                        colors.tertiary
                    })
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |button| button.bg(Ink::DANGER.alpha(0.09)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.discard_armed {
                                    this.run_review_action(
                                        ReviewAction::Discard(paths.clone()),
                                        cx,
                                    );
                                } else {
                                    this.discard_armed = true;
                                    this.commit_open = false;
                                    cx.notify();
                                }
                                cx.stop_propagation();
                            }))
                    })
                    .child(sf_symbol(
                        "trash",
                        9.5,
                        if discard_armed {
                            Ink::DANGER
                        } else {
                            colors.tertiary
                        },
                    ))
                    .child(if discard_armed { "Discard?" } else { "Discard" }),
            );
        }

        let branch_detail = match (status.branch.ahead, status.branch.behind) {
            (0, 0) => None,
            (ahead, 0) => Some(format!("↑{ahead}")),
            (0, behind) => Some(format!("↓{behind}")),
            (ahead, behind) => Some(format!("↑{ahead} ↓{behind}")),
        };
        let counts = format!(
            "{staged_count} staged · {working_count} working{}",
            if conflicted_count > 0 {
                format!(" · {conflicted_count} conflicted")
            } else {
                String::new()
            }
        );
        let mut panel = div()
            .flex_none()
            .px(px(10.0))
            .py(px(7.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .border_b_1()
            .border_color(colors.primary.alpha(0.06))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(sf_symbol("arrow.branch", 11.0, colors.secondary))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(10.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.secondary)
                            .child(branch),
                    )
                    .when_some(branch_detail, |row, detail| {
                        row.child(
                            div()
                                .font_family(crate::fonts::mono_family())
                                .text_size(px(9.5))
                                .text_color(colors.tertiary)
                                .child(detail),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(if conflicted_count > 0 {
                                Ink::DANGER
                            } else {
                                colors.tertiary
                            })
                            .child(counts),
                    ),
            )
            .when(self.diff_layer != DiffLayer::Branch, |panel| {
                panel.child(actions)
            })
            .when(self.diff_layer == DiffLayer::Branch, |panel| {
                panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(9.5))
                        .text_color(colors.tertiary)
                        .child(sf_symbol("scope", 9.5, colors.tertiary))
                        .child("Overview only · choose Working or Staged to mutate hunks"),
                )
            });

        if self.commit_open {
            let empty = self.commit_query.is_empty();
            panel = panel.child(
                div()
                    .p(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .rounded(px(Radius::ROW))
                    .bg(colors.primary.alpha(0.035))
                    .border_1()
                    .border_color(colors.primary.alpha(0.08))
                    .child(
                        div()
                            .id("review-commit-message")
                            .min_w(px(0.0))
                            .h(px(27.0))
                            .flex_1()
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::CHIP))
                            .bg(colors.background)
                            .border_1()
                            .border_color(colors.primary.alpha(0.10))
                            .cursor_text()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(10.5))
                            .text_color(colors.primary)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    window.focus(&this.focus, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(if empty {
                                div()
                                    .text_color(colors.tertiary)
                                    .child("Commit message…")
                                    .into_any_element()
                            } else {
                                crate::navigation::query_label(&self.commit_query)
                            }),
                    )
                    .child(
                        div()
                            .id("review-submit-commit")
                            .h(px(27.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::CHIP))
                            .bg(if empty {
                                colors.primary.alpha(0.035)
                            } else {
                                rgba(0xd9775730)
                            })
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if empty {
                                colors.primary.alpha(0.25)
                            } else {
                                rgba(0xf0aa8fff)
                            })
                            .when(!empty && !busy, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(rgba(0xd9775744)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_commit(cx);
                                        cx.stop_propagation();
                                    }))
                            })
                            .child("Commit"),
                    ),
            );
        }
        if let Some((success, message)) = &self.review_feedback {
            let accent = if *success { Ink::FRESH } else { Ink::DANGER };
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(10.0))
                    .text_color(accent)
                    .child(sf_symbol(
                        if *success {
                            "checkmark.circle.fill"
                        } else {
                            "exclamationmark.circle.fill"
                        },
                        10.5,
                        accent,
                    ))
                    .child(message.clone()),
            );
        }
        let _ = window;
        panel.into_any_element()
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_selected == Some(WorkspaceSurface::Browser)
            && self.browser_address_focused
        {
            if self.apply_browser_edit(event, cx) {
                cx.stop_propagation();
            }
            return;
        }
        // The editor consumes Escape for its own menus and selection first.
        // Pending/missing details have no editor action handler, so the
        // inspector handles Escape only when its own handle has focus.
        if self.workspace_selected == Some(WorkspaceSurface::Notes)
            && self.notes.detail().is_some()
            && self.focus.is_focused(_window)
            && event.keystroke.key == "escape"
        {
            self.close_note_detail(_window, cx);
            cx.stop_propagation();
            return;
        }
        if self.apply_notes_edit(event, cx) {
            cx.stop_propagation();
            return;
        }
        if self.ask_draft.is_some() {
            match event.keystroke.key.as_str() {
                "escape" => {
                    self.ask_draft = None;
                    self.ask_feedback = None;
                    self.ask_query.clear();
                    cx.notify();
                }
                "enter" => self.submit_ask(cx),
                _ => {
                    let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                        return;
                    };
                    match edit {
                        Edit::Local(local) => {
                            self.ask_query.apply(local);
                        }
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(&self.ask_query, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
                            query_editor::cut_selection(&mut self.ask_query, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Paste) => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                self.ask_query.insert(&text);
                            }
                        }
                    }
                    cx.notify();
                }
            }
            cx.stop_propagation();
            return;
        }
        if !self.commit_open
            && (self.workspace_selected == Some(WorkspaceSurface::Review)
                || (self.workspace_selected == Some(WorkspaceSurface::Details)
                    && self.selected_tab == InspectorTab::Changes))
        {
            let moved = match event.keystroke.key.as_str() {
                "up" => match &self.state {
                    LoadState::Ready(snapshot) => {
                        self.diff_selection
                            .move_by(snapshot, -1, event.keystroke.modifiers.shift)
                    }
                    _ => false,
                },
                "down" => match &self.state {
                    LoadState::Ready(snapshot) => {
                        self.diff_selection
                            .move_by(snapshot, 1, event.keystroke.modifiers.shift)
                    }
                    _ => false,
                },
                "escape" if !self.diff_selection.is_empty() => {
                    self.diff_selection.clear();
                    true
                }
                _ => false,
            };
            if moved {
                self.selected_turn = None;
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        if !self.commit_open {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.commit_open = false;
                cx.notify();
            }
            "enter" => self.submit_commit(cx),
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return;
                };
                match edit {
                    Edit::Local(local) => {
                        self.commit_query.apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(&self.commit_query, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(&mut self.commit_query, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            self.commit_query.insert(&text);
                        }
                    }
                }
                cx.notify();
            }
        }
        cx.stop_propagation();
    }

    fn render_changes(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = self.comparison_label();
        let remote = self.context.as_ref().is_some_and(|context| context.remote);
        let empty_detail = match self.diff_layer {
            DiffLayer::Branch => format!("This branch matches {label}."),
            DiffLayer::Working => "The working tree matches the index.".to_owned(),
            DiffLayer::Staged => "The index matches HEAD.".to_owned(),
        };
        let body = match self.state.clone() {
            LoadState::Ready(snapshot) if snapshot.rows.is_empty() => self
                .render_message(colors, "checkmark.circle", "No changes", empty_detail)
                .into_any_element(),
            LoadState::Ready(snapshot) => self
                .render_diff(snapshot, colors, window, cx)
                .into_any_element(),
            LoadState::Loading => self
                .render_message(
                    colors,
                    "ellipsis",
                    "Loading changes",
                    "Reading the working tree…",
                )
                .into_any_element(),
            LoadState::NoSession => self
                .render_message(
                    colors,
                    "sidebar.left",
                    "Select a session",
                    "Changes follow the active agent.",
                )
                .into_any_element(),
            LoadState::Error(error) if git_is_not_a_repository(&error) => self
                .render_message(
                    colors,
                    "folder",
                    "Not a Git repository",
                    "This folder has no Git working tree.",
                )
                .into_any_element(),
            LoadState::Error(error) if git_is_not_installed(&error) => self
                .render_message(
                    colors,
                    "terminal",
                    "Git unavailable",
                    "Git is not installed on this host.",
                )
                .into_any_element(),
            LoadState::Error(error) => self
                .render_message(
                    colors,
                    "exclamationmark.triangle",
                    "Couldn't load changes",
                    error,
                )
                .into_any_element(),
        };
        let comparison_open = remote && self.comparison_menu_open;
        let snapshot = match &self.state {
            LoadState::Ready(snapshot) => Some(Arc::clone(snapshot)),
            _ => None,
        };
        let file_count = snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.file_diffs.len());
        let menu = if !remote && self.files_open {
            snapshot.map(|snapshot| self.render_file_navigator(snapshot, colors, cx))
        } else if comparison_open {
            Some(
                div()
                    .absolute()
                    .inset_0()
                    .child(div().absolute().inset_0().occlude().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.comparison_menu_open = false;
                            cx.notify();
                            cx.stop_propagation();
                        }),
                    ))
                    .child(if crate::floating::uses_panels(false, colors, cx) {
                        crate::floating::host_here(
                            INSPECTOR_COMPARISON_MENU,
                            crate::floating::surface(
                                colors,
                                crate::floating::MENU_RADIUS,
                                230.0,
                                self.comparison_menu_items(colors, cx),
                            )
                            .into_any_element(),
                            Some(230.0),
                            gpui::Anchor::TopRight,
                            8.0,
                            cx,
                        )
                        .absolute()
                        .top(px(40.0))
                        .right(px(10.0))
                        .w(px(0.0))
                        .h(px(0.0))
                        .into_any_element()
                    } else {
                        div()
                            .id("inspector-comparison-menu")
                            .absolute()
                            .top(px(40.0))
                            .right(px(10.0))
                            .w(px(230.0))
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.comparison_menu_open = false;
                                cx.notify();
                            }))
                            .child(
                                FloatingSurface::new(
                                    colors,
                                    self.comparison_menu_items(colors, cx),
                                )
                                .radius(crate::floating::MENU_RADIUS),
                            )
                            .into_any_element()
                    })
                    .into_any_element(),
            )
        } else {
            None
        };

        let toolbar = if remote {
            div()
                .h(px(38.0))
                .flex_none()
                .px(px(10.0))
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(colors.primary.alpha(0.06))
                .child(
                    div()
                        .text_size(px(Typo::META.size))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.tertiary)
                        .child("Remote branch"),
                )
                .child(
                    div()
                        .id("inspector-comparison-button")
                        .debug_selector(|| "INSPECTOR_COMPARE_BUTTON".to_owned())
                        .max_w(px(184.0))
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .rounded(px(Radius::BADGE))
                        .bg(colors
                            .primary
                            .alpha(if comparison_open { 0.10 } else { 0.055 }))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                        .child(sf_symbol("arrow.branch", 11.0, colors.secondary))
                        .child(
                            div()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(Typo::META.size))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(colors.secondary)
                                .child(format!("vs {label}")),
                        )
                        .child(sf_symbol("chevron.down", 9.0, colors.tertiary))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.comparison_menu_open = !this.comparison_menu_open;
                            cx.notify();
                            cx.stop_propagation();
                        })),
                )
                .into_any_element()
        } else {
            div()
                .h(px(38.0))
                .flex_none()
                .px(px(9.0))
                .flex()
                .items_center()
                .gap(px(7.0))
                .border_b_1()
                .border_color(colors.primary.alpha(0.06))
                .child(
                    div()
                        .h(px(29.0))
                        .p(px(2.0))
                        .flex()
                        .items_center()
                        .gap(px(1.0))
                        .rounded(px(Radius::BADGE))
                        .bg(colors.primary.alpha(0.035))
                        .border_1()
                        .border_color(colors.primary.alpha(0.055))
                        .child(self.render_layer_option(DiffLayer::Branch, colors, cx))
                        .child(self.render_layer_option(DiffLayer::Working, colors, cx))
                        .child(self.render_layer_option(DiffLayer::Staged, colors, cx)),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("review-files-button")
                        .debug_selector(|| "INSPECTOR_REVIEW_FILES".to_owned())
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .rounded(px(Radius::BADGE))
                        .bg(colors
                            .primary
                            .alpha(if self.files_open { 0.10 } else { 0.045 }))
                        .text_size(px(9.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if file_count > 0 {
                            colors.secondary
                        } else {
                            colors.primary.alpha(0.28)
                        })
                        .when(file_count > 0, |button| {
                            button
                                .cursor_pointer()
                                .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.files_open = !this.files_open;
                                    cx.notify();
                                    cx.stop_propagation();
                                }))
                        })
                        .child(sf_symbol("list.bullet", 10.5, colors.tertiary))
                        .child(format!("{file_count} files"))
                        .child(sf_symbol("chevron.down", 8.5, colors.tertiary)),
                )
                .into_any_element()
        };

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(self.render_review_controls(colors, window, cx))
            .child(div().min_h(px(0.0)).flex_1().overflow_hidden().child(body))
            .when_some(menu, |panel, menu| panel.child(menu))
            .into_any_element()
    }

    fn render_ask_preset(
        &self,
        id: &'static str,
        label: &'static str,
        question: &'static str,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .h(px(21.0))
            .px(px(7.0))
            .flex()
            .items_center()
            .rounded_full()
            .bg(colors.primary.alpha(0.045))
            .border_1()
            .border_color(colors.primary.alpha(0.065))
            .cursor_pointer()
            .hover(move |button| button.bg(colors.primary.alpha(0.085)))
            .text_size(px(9.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(colors.secondary)
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_ask_question(question, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_ask_composer(
        &self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let draft = self.ask_draft.as_ref()?;
        let empty = self.ask_query.text().trim().is_empty();
        let busy = self.ask_busy;
        let label = draft.label.clone();
        let pending = self
            .selected_session()
            .map(|s| {
                let draft = self.runtime.prompt_drafts.shared_snapshot(&s.id);
                format!(
                    "{} pending attachments · recipient {} · draft revision {}",
                    draft.attachments.len(),
                    s.id.0,
                    draft.revision
                )
            })
            .unwrap_or_else(|| "No captured recipient".into());

        let mut composer = div()
            .id("inspector-ask-composer")
            .debug_selector(|| "INSPECTOR_ASK_COMPOSER".to_owned())
            .flex_none()
            .px(px(11.0))
            .py(px(9.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .border_t_1()
            .border_color(colors.primary.alpha(0.09))
            .bg(rgba(0x17191ef8))
            .child(sidebar::message(pending, colors))
            .child(sidebar::button(
                "review-context",
                "Review pending Context",
                colors,
                window,
                cx,
                |this, window, cx| {
                    this.select_workspace(WorkspaceSurface::Context, cx);
                    this.focus_active_surface(window, cx);
                },
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(sf_symbol_weighted(
                        "sparkles",
                        11.5,
                        SymbolWeight::Semibold,
                        rgba(0xe9a381ff),
                    ))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.primary)
                            .child(
                                self.selected_session()
                                    .map(|s| format!("Ask {}", s.title))
                                    .unwrap_or_else(|| "No recipient".into()),
                            ),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .text_size(px(9.5))
                            .text_color(colors.tertiary)
                            .child(label),
                    )
                    .child(
                        div()
                            .id("inspector-ask-close")
                            .size(px(20.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::CHIP))
                            .cursor_pointer()
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .child(sf_symbol("xmark", 9.5, colors.tertiary))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ask_draft = None;
                                this.ask_feedback = None;
                                this.ask_query.clear();
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(self.render_ask_preset(
                        "ask-preset-review",
                        "Review",
                        "Review this for correctness, regressions, and missing tests.",
                        colors,
                        cx,
                    ))
                    .child(self.render_ask_preset(
                        "ask-preset-risks",
                        "Find risks",
                        "Find the highest-risk behavior changes and explain why they matter.",
                        colors,
                        cx,
                    ))
                    .child(self.render_ask_preset(
                        "ask-preset-tests",
                        "Suggest tests",
                        "Identify missing tests and propose concrete cases for this context.",
                        colors,
                        cx,
                    )),
            )
            .child(
                div()
                    .h(px(34.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .id("inspector-ask-input")
                            .min_w(px(0.0))
                            .h_full()
                            .flex_1()
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::BADGE))
                            .bg(colors.primary.alpha(0.045))
                            .border_1()
                            .border_color(colors.primary.alpha(0.075))
                            .text_size(px(10.5))
                            .text_color(colors.primary)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    window.focus(&this.focus, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(if empty {
                                div()
                                    .text_color(colors.tertiary)
                                    .child("Ask a follow-up…")
                                    .into_any_element()
                            } else {
                                crate::navigation::query_label(&self.ask_query)
                            }),
                    )
                    .child(
                        div()
                            .id("inspector-ask-send")
                            .debug_selector(|| "INSPECTOR_ASK_SEND".to_owned())
                            .h_full()
                            .px(px(11.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .rounded(px(Radius::BADGE))
                            .bg(if empty || busy {
                                colors.primary.alpha(0.04)
                            } else {
                                rgba(0xd97757d9)
                            })
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if empty || busy {
                                colors.primary.alpha(0.28)
                            } else {
                                rgba(0xffffffff)
                            })
                            .when(!empty && !busy, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(rgba(0xe38563ff)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_ask(cx);
                                        cx.stop_propagation();
                                    }))
                            })
                            .child(if busy { "Sending…" } else { "Send" })
                            .child(sf_symbol(
                                "arrow.up",
                                9.0,
                                if empty || busy {
                                    colors.primary.alpha(0.28)
                                } else {
                                    rgba(0xffffffff)
                                },
                            )),
                    ),
            );
        if let Some((success, message)) = &self.ask_feedback {
            let accent = if *success { Ink::FRESH } else { Ink::DANGER };
            composer = composer.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(9.5))
                    .text_color(accent)
                    .child(sf_symbol(
                        if *success {
                            "checkmark.circle.fill"
                        } else {
                            "exclamationmark.circle.fill"
                        },
                        10.0,
                        accent,
                    ))
                    .child(message.clone()),
            );
        }
        Some(composer.into_any_element())
    }

    fn render_message(
        &self,
        colors: SemanticColors,
        symbol: &'static str,
        title: &'static str,
        body: impl Into<SharedString>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .px(px(28.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .text_center()
            .child(sf_symbol(symbol, 28.0, colors.tertiary))
            .child(
                div()
                    .text_size(px(Typo::ROW_EMPHASIZED.size))
                    .font_weight(Typo::ROW_EMPHASIZED.weight)
                    .text_color(colors.primary.alpha(0.86))
                    .child(title),
            )
            .child(
                div()
                    .max_w(px(280.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(body.into()),
            )
    }
}

fn detected_browser_urls(session: &SessionRecord) -> Vec<(String, String)> {
    let mut links = Vec::new();
    for artifact in session
        .artifacts
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|a| matches!(a.kind, ArtifactKind::Preview | ArtifactKind::Link))
    {
        if artifact.url.starts_with("http://") || artifact.url.starts_with("https://") {
            links.push((
                format!("Detected {:?}: {}", artifact.kind, artifact.url),
                artifact.url.clone(),
            ));
        }
    }
    if session.host.is_none() {
        for port in session.listening_ports.as_deref().unwrap_or_default() {
            let url = format!("http://localhost:{}", port.port);
            links.push((format!("Detected listening port · {}", url), url));
        }
    }
    links
}

fn git_is_not_a_repository(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("not a git repository")
        || error.contains("session cwd is not inside a git repository")
}

fn git_is_not_installed(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("git is not installed")
        || error.contains("git: command not found")
        || error.contains("git: not found")
}

fn should_show_blocking_git_loading(context_changed: bool, state: &LoadState) -> bool {
    context_changed || matches!(state, LoadState::NoSession)
}

impl Render for WorkbenchInspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            crate::app_theme::sidebar_colors_in(&store)
        };
        let session = self.selected_session();
        let body = match self.workspace_selected {
            Some(WorkspaceSurface::Details) => self.render_info(session.as_ref(), colors),
            Some(WorkspaceSurface::Browser) => self.render_browser(colors, window, cx),
            Some(WorkspaceSurface::Files) => {
                let mut view = div().size_full().flex().flex_col();
                if let Some(session) = session.as_ref() {
                    let recipient = session.id.clone();
                    let label = format!("Attach displayed file to {}", session.title);
                    view = view.child(sidebar::button(
                        "files-attach",
                        label,
                        colors,
                        window,
                        cx,
                        move |this, _, cx| this.attach_displayed_file(recipient.clone(), cx),
                    ));
                } else {
                    view = view.child(sidebar::message(
                        "Select or pin a recipient to attach a file.",
                        colors,
                    ));
                }
                view.when_some(
                    session
                        .as_ref()
                        .and_then(|s| self.sidebar.feedback.get(&s.id)),
                    |view, feedback| view.child(sidebar::message(feedback.clone(), colors)),
                )
                .child(div().min_h_0().flex_1().child(self.code_viewer.clone()))
                .into_any_element()
            }
            Some(WorkspaceSurface::Review) => {
                let detail = if self.review_tab == InspectorTab::Artifacts {
                    self.render_artifacts(session.as_ref(), colors, cx)
                } else {
                    self.render_changes(colors, window, cx)
                };
                div()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .child(detail)
                    .into_any_element()
            }
            Some(WorkspaceSurface::Notes) => self.render_notes(colors, cx),
            Some(WorkspaceSurface::Runs) => self.render_runs(colors, window, cx),
            Some(WorkspaceSurface::Tasks) => self.render_tasks(colors, window, cx),
            Some(WorkspaceSurface::Context) => self.render_context(colors, window, cx),
            Some(WorkspaceSurface::Usage) => self.render_usage(colors, window, cx),
            None => div().size_full().into_any_element(),
        };
        let transition_id = SharedString::from(format!(
            "inspector-tab-transition-{}",
            self.tab_transition_generation
        ));
        let direction = self.tab_direction;
        let ask_composer = matches!(self.workspace_selected, Some(WorkspaceSurface::Review))
            .then(|| self.render_ask_composer(colors, window, cx))
            .flatten();
        let body = div().relative().size_full().child(body);
        // A native child cannot share GPUI's opacity or clipping animation.
        // Keep its measured viewport stable when entering the Browser tab.
        let body =
            if cx.reduce_motion() || self.workspace_selected == Some(WorkspaceSurface::Browser) {
                body.into_any_element()
            } else {
                body.with_animation(
                    transition_id,
                    Animation::new(Duration::from_millis(190)).with_easing(ease_out_quint()),
                    move |body, delta| {
                        body.left(px(direction * (1.0 - delta) * 8.0))
                            .opacity(0.70 + 0.30 * delta)
                    },
                )
                .into_any_element()
            };
        div()
            .id("workbench-inspector")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key_down))
            .bg(colors.sidebar_surface())
            .text_color(colors.primary)
            .child(div().min_h(px(0.0)).flex_1().overflow_hidden().child(body))
            .when_some(ask_composer, |panel, composer| panel.child(composer))
    }
}

fn section_label(label: &'static str, colors: SemanticColors) -> AnyElement {
    div()
        .px(px(2.0))
        .text_size(px(Typo::SECTION_HEADER.size))
        .font_weight(Typo::SECTION_HEADER.weight)
        .text_color(colors.tertiary)
        .child(label)
        .into_any_element()
}

fn detail_row(
    label: &'static str,
    value: String,
    monospaced: bool,
    colors: SemanticColors,
) -> AnyElement {
    div()
        .min_h(px(38.0))
        .px(px(11.0))
        .flex()
        .items_center()
        .gap(px(12.0))
        .border_b_1()
        .border_color(colors.primary.alpha(0.05))
        .child(
            div()
                .w(px(64.0))
                .flex_none()
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(label),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .truncate()
                .when(monospaced, |value| {
                    value.font_family(crate::fonts::mono_family())
                })
                .text_size(px(if monospaced {
                    Typo::META_MONO.size
                } else {
                    Typo::META.size
                }))
                .text_color(colors.secondary)
                .child(value),
        )
        .into_any_element()
}

fn render_pull_request(
    pull_request: &PullRequestStatus,
    session_id: SessionId,
    colors: SemanticColors,
    inspector: Entity<WorkbenchInspector>,
    body: Option<Arc<MarkdownDocument>>,
    selected_turn: Option<String>,
) -> AnyElement {
    let number = if pull_request.number > 0 {
        format!("PR #{}", pull_request.number)
    } else {
        "Pull request".to_owned()
    };
    let title = pull_request.title.clone().unwrap_or_else(|| number.clone());
    let author = pull_request.author.as_deref().unwrap_or("contributor");
    let (state_label, state_color) = pull_request_state(pull_request, colors);
    let discussion_total = pull_request.comment_count + pull_request.review_count;
    let can_merge = pull_request_can_merge(pull_request);
    let view_url = pull_request.url.clone();
    let merge_url = pull_request.url.clone();
    let discussion = pull_request.discussion.as_deref().unwrap_or_default();
    let ask_evidence = ReviewEvidence::PullRequest {
        url: pull_request.url.clone(),
        title: title.clone(),
        body: body.as_ref().map(|document| document.plain_text()),
        base: pull_request.base_ref_name.clone(),
        head: pull_request.head_ref_name.clone(),
    };
    let ask_inspector = inspector.clone();

    let mut surface = div()
        .id(SharedString::from(format!(
            "inspector-pr-{}",
            pull_request.url
        )))
        .flex()
        .flex_col()
        .gap(px(14.0))
        .rounded(px(Radius::CARD))
        .bg(colors.primary.alpha(0.022))
        .border_1()
        .border_color(colors.primary.alpha(0.075))
        .overflow_hidden()
        .child(
            div()
                .p(px(13.0))
                .pb(px(12.0))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(9.0))
                        .child(
                            div()
                                .size(px(30.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(state_color.alpha(0.12))
                                .child(sf_symbol_weighted(
                                    "arrow.triangle.pull",
                                    13.0,
                                    SymbolWeight::Semibold,
                                    state_color,
                                )),
                        )
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(
                                    div()
                                        .line_height(px(17.0))
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(colors.primary)
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .text_size(px(Typo::META.size))
                                        .text_color(colors.tertiary)
                                        .child(format!("{author} opened {number}")),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "inspector-pr-ask-{}",
                                    pull_request.number
                                )))
                                .debug_selector(|| "INSPECTOR_PR_ASK".to_owned())
                                .h(px(24.0))
                                .px(px(8.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .rounded(px(Radius::CHIP))
                                .bg(rgba(0xd9775717))
                                .cursor_pointer()
                                .hover(|button| button.bg(rgba(0xd9775728)))
                                .text_size(px(9.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgba(0xe9a381ff))
                                .child(sf_symbol("sparkles", 9.5, rgba(0xe9a381ff)))
                                .child("Ask")
                                .on_click(move |_, window, cx| {
                                    ask_inspector.update(cx, |inspector, cx| {
                                        inspector.open_ask(vec![ask_evidence.clone()], window, cx);
                                    });
                                    cx.stop_propagation();
                                }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .px(px(7.0))
                                .h(px(21.0))
                                .flex()
                                .items_center()
                                .rounded_full()
                                .bg(state_color.alpha(0.12))
                                .text_size(px(10.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(state_color)
                                .child(state_label),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "inspector-pr-open-{}",
                                    pull_request.number
                                )))
                                .debug_selector(|| "INSPECTOR_PR_OPEN".to_owned())
                                .size(px(24.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(Radius::CHIP))
                                .cursor_pointer()
                                .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                                .child(sf_symbol("arrow.up.right", 10.5, colors.tertiary))
                                .on_click(move |_, _, cx| cx.open_url(&view_url)),
                        ),
                )
                .when(
                    pull_request.head_ref_name.is_some() || pull_request.base_ref_name.is_some(),
                    |header| {
                        let head = pull_request
                            .head_ref_name
                            .clone()
                            .unwrap_or_else(|| "head".to_owned());
                        let base = pull_request
                            .base_ref_name
                            .clone()
                            .unwrap_or_else(|| "base".to_owned());
                        header.child(
                            div()
                                .h(px(24.0))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(branch_badge(base, colors))
                                .child(sf_symbol("arrow.left", 9.5, colors.tertiary))
                                .child(branch_badge(head, colors)),
                        )
                    },
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(diff_stat(
                            format!("+{}", pull_request.additions),
                            Ink::FRESH,
                        ))
                        .child(diff_stat(
                            format!("−{}", pull_request.deletions),
                            Ink::DANGER,
                        ))
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.tertiary)
                                .child(format!(
                                    "{} changed {}",
                                    pull_request.changed_files,
                                    if pull_request.changed_files == 1 {
                                        "file"
                                    } else {
                                        "files"
                                    }
                                )),
                        )
                        .when_some(pull_request.total_threads, |stats, total| {
                            stats.child(
                                div()
                                    .ml_auto()
                                    .text_size(px(10.5))
                                    .text_color(colors.tertiary)
                                    .child(format!(
                                        "{}/{} resolved",
                                        pull_request.resolved_threads.unwrap_or(0),
                                        total
                                    )),
                            )
                        }),
                )
                .when_some(body, |header, body| {
                    let key = format!("pr:{}:body", pull_request.url);
                    let selected = selected_turn.as_deref() == Some(key.as_str());
                    let content = body.plain_text();
                    let source = QuoteSource::Markdown {
                        session_id: session_id.clone(),
                        document: number.clone(),
                        turn: 0,
                    };
                    let selection_inspector = inspector.clone();
                    header.child(
                        div()
                            .id(SharedString::from(format!(
                                "inspector-pr-{}-body",
                                pull_request.number
                            )))
                            .debug_selector(|| "INSPECTOR_PR_BODY".to_owned())
                            .mt(px(1.0))
                            .p(px(11.0))
                            .rounded(px(Radius::BADGE))
                            .bg(if selected {
                                rgba(0x5b8fd12f)
                            } else {
                                colors.primary.alpha(0.035)
                            })
                            .border_1()
                            .border_color(if selected {
                                rgba(0x8bb9e8aa)
                            } else {
                                colors.primary.alpha(0.055)
                            })
                            .cursor_pointer()
                            .hover(move |turn| turn.bg(rgba(0x5b8fd122)))
                            .on_click(move |_, window, cx| {
                                selection_inspector.update(cx, |inspector, cx| {
                                    inspector.select_turn(
                                        key.clone(),
                                        source.clone(),
                                        content.clone(),
                                        window,
                                        cx,
                                    );
                                });
                                cx.stop_propagation();
                            })
                            .child(render_markdown(&body, colors)),
                    )
                }),
        );

    if discussion_total > 0 {
        let mut conversation = div().px(px(13.0)).flex().flex_col().gap(px(8.0)).child(
            div()
                .flex()
                .items_center()
                .child(section_label("Conversation", colors))
                .child(
                    div()
                        .ml_auto()
                        .text_size(px(10.5))
                        .text_color(colors.tertiary)
                        .child(format!("{discussion_total} items")),
                ),
        );
        if discussion.is_empty() {
            conversation = conversation.child(render_discussion_fallback(pull_request, colors));
        } else {
            for (index, item) in discussion.iter().enumerate() {
                conversation = conversation.child(render_discussion_item(
                    item,
                    index,
                    discussion.len(),
                    session_id.clone(),
                    colors,
                    inspector.clone(),
                    selected_turn.as_deref(),
                ));
            }
        }
        surface = surface.child(conversation);
    }

    if pull_request.state == "OPEN" {
        let (merge_detail, merge_color) = if can_merge {
            ("Ready to merge", Ink::FRESH)
        } else {
            (merge_blocker_label(pull_request), Ink::ATTENTION)
        };
        surface = surface.child(
            div()
                .mt(px(1.0))
                .p(px(13.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .border_t_1()
                .border_color(colors.primary.alpha(0.07))
                .bg(merge_color.alpha(0.045))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.primary)
                                .child(merge_detail),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(colors.tertiary)
                                .child("Review and confirm on GitHub"),
                        ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "inspector-pr-merge-{}",
                            pull_request.number
                        )))
                        .debug_selector(|| "INSPECTOR_PR_MERGE".to_owned())
                        .h(px(30.0))
                        .px(px(10.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(Radius::BADGE))
                        .cursor_pointer()
                        .bg(merge_color.alpha(if can_merge { 0.86 } else { 0.13 }))
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if can_merge {
                            rgba(0xffffffff)
                        } else {
                            merge_color
                        })
                        .hover(move |button| {
                            button.bg(merge_color.alpha(if can_merge { 1.0 } else { 0.19 }))
                        })
                        .child("Merge pull request")
                        .child(sf_symbol(
                            "arrow.up.right",
                            9.0,
                            if can_merge {
                                rgba(0xffffffff)
                            } else {
                                merge_color
                            },
                        ))
                        .on_click(move |_, _, cx| cx.open_url(&merge_url)),
                ),
        );
    }

    surface.pb(px(13.0)).into_any_element()
}

fn branch_badge(branch: String, colors: SemanticColors) -> AnyElement {
    div()
        .min_w(px(0.0))
        .max_w(px(158.0))
        .h(px(22.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .rounded(px(Radius::CHIP))
        .bg(colors.primary.alpha(0.045))
        .font_family(crate::fonts::mono_family())
        .text_size(px(10.0))
        .text_color(colors.secondary)
        .truncate()
        .child(branch)
        .into_any_element()
}

fn diff_stat(label: String, color: gpui::Rgba) -> AnyElement {
    div()
        .px(px(7.0))
        .h(px(22.0))
        .flex()
        .items_center()
        .rounded(px(Radius::CHIP))
        .bg(color.alpha(0.09))
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(label)
        .into_any_element()
}

fn render_discussion_item(
    item: &PrDiscussionItem,
    index: usize,
    total: usize,
    session_id: SessionId,
    colors: SemanticColors,
    inspector: Entity<WorkbenchInspector>,
    selected_turn: Option<&str>,
) -> AnyElement {
    let author = item.author.clone();
    let initial = author
        .chars()
        .next()
        .map(|character| character.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".to_owned());
    let is_review = item.kind == "review";
    let (review_label, review_color) = discussion_state(item, colors);
    let body = MarkdownDocument::parse(&item.body);
    let body_fallback = if item.body.trim().is_empty() {
        review_label
            .clone()
            .unwrap_or_else(|| "Commented".to_owned())
    } else {
        String::new()
    };
    let time = item.created_at.as_ref().map(|date| relative_time(date.0));
    let url = item.url.clone();
    let key = format!("discussion:{index}:{}", url.as_deref().unwrap_or("local"));
    let selected = selected_turn == Some(key.as_str());
    let selection_content = if item.body.trim().is_empty() {
        body_fallback.clone()
    } else {
        body.plain_text()
    };
    let source = QuoteSource::Markdown {
        session_id,
        document: format!("pull request discussion by {author}"),
        turn: index,
    };
    let selection_inspector = inspector;

    div()
        .id(SharedString::from(format!("inspector-pr-comment-{index}")))
        .debug_selector(move || format!("INSPECTOR_PR_COMMENT_{index}"))
        .flex()
        .items_stretch()
        .gap(px(8.0))
        .child(
            div()
                .w(px(26.0))
                .flex_none()
                .flex()
                .flex_col()
                .items_center()
                .child(
                    div()
                        .size(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(if is_review {
                            review_color.alpha(0.13)
                        } else {
                            colors.primary.alpha(0.075)
                        })
                        .text_size(px(9.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if is_review {
                            review_color
                        } else {
                            colors.secondary
                        })
                        .child(initial),
                )
                .when(index + 1 < total, |rail| {
                    rail.child(
                        div()
                            .mt(px(4.0))
                            .w(px(1.0))
                            .flex_1()
                            .min_h(px(10.0))
                            .bg(colors.primary.alpha(0.08)),
                    )
                }),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "inspector-pr-comment-card-{index}"
                )))
                .min_w(px(0.0))
                .flex_1()
                .mb(px(if index + 1 < total { 2.0 } else { 0.0 }))
                .rounded(px(Radius::BADGE))
                .border_1()
                .border_color(if selected {
                    rgba(0x8bb9e8aa)
                } else {
                    colors.primary.alpha(0.07)
                })
                .bg(if selected {
                    rgba(0x5b8fd12f)
                } else {
                    colors.primary.alpha(0.025)
                })
                .cursor_pointer()
                .hover(move |card| card.bg(rgba(0x5b8fd122)))
                .child(
                    div()
                        .min_h(px(29.0))
                        .px(px(9.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .border_b_1()
                        .border_color(colors.primary.alpha(0.055))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.primary)
                                .child(author),
                        )
                        .when_some(review_label, |header, label| {
                            header.child(
                                div()
                                    .px(px(5.0))
                                    .h(px(17.0))
                                    .flex()
                                    .items_center()
                                    .rounded_full()
                                    .bg(review_color.alpha(0.11))
                                    .text_size(px(9.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(review_color)
                                    .child(label),
                            )
                        })
                        .when_some(time, |header, time| {
                            header.child(
                                div()
                                    .ml_auto()
                                    .text_size(px(9.5))
                                    .text_color(colors.tertiary)
                                    .child(time),
                            )
                        })
                        .when_some(url, |header, url| {
                            header.child(
                                div()
                                    .id(("open-discussion-item", index))
                                    .size(px(19.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(Radius::CHIP))
                                    .cursor_pointer()
                                    .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                                    .child(sf_symbol("arrow.up.right", 8.5, colors.tertiary))
                                    .on_click(move |_, _, cx| {
                                        cx.open_url(&url);
                                        cx.stop_propagation();
                                    }),
                            )
                        }),
                )
                .child(
                    div()
                        .px(px(9.0))
                        .py(px(8.0))
                        .child(if body_fallback.is_empty() {
                            render_markdown(&body, colors)
                        } else {
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.secondary)
                                .child(body_fallback)
                                .into_any_element()
                        }),
                )
                .on_click(move |_, window, cx| {
                    selection_inspector.update(cx, |inspector, cx| {
                        inspector.select_turn(
                            key.clone(),
                            source.clone(),
                            selection_content.clone(),
                            window,
                            cx,
                        );
                    });
                    cx.stop_propagation();
                }),
        )
        .into_any_element()
}

fn render_discussion_fallback(
    pull_request: &PullRequestStatus,
    colors: SemanticColors,
) -> AnyElement {
    let discussion = pull_request_discussion(pull_request)
        .unwrap_or_else(|| "Open the conversation on GitHub".to_owned());
    let url = pull_request.url.clone();
    div()
        .id(SharedString::from(format!(
            "inspector-pr-discussion-{}",
            pull_request.number
        )))
        .min_h(px(38.0))
        .px(px(9.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(colors.primary.alpha(0.07))
        .bg(colors.primary.alpha(0.025))
        .cursor_pointer()
        .hover(move |row| row.bg(colors.primary.alpha(0.05)))
        .child(sf_symbol(
            "bubble.left.and.bubble.right",
            12.0,
            colors.secondary,
        ))
        .child(
            div()
                .flex_1()
                .text_size(px(Typo::META.size))
                .text_color(colors.secondary)
                .child(discussion),
        )
        .child(sf_symbol("arrow.up.right", 9.0, colors.tertiary))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .into_any_element()
}

fn sorted_pr_checks(pull_request: &PullRequestStatus) -> Vec<PrCheck> {
    let mut checks = pull_request.checks.clone().unwrap_or_default();
    checks.sort_by_key(|check| match check.result.as_str() {
        "fail" => 0,
        "pending" => 1,
        "pass" => 2,
        _ => 3,
    });
    checks
}

fn checks_rollup(pull_request: &PullRequestStatus) -> (String, gpui::Rgba) {
    if pull_request.checks_failed > 0 {
        return (
            format!("{} failed", pull_request.checks_failed),
            Ink::DANGER,
        );
    }
    if pull_request.checks_pending > 0 {
        return (
            format!("{} running", pull_request.checks_pending),
            Ink::ATTENTION,
        );
    }
    ("All passed".to_owned(), Ink::FRESH)
}

fn discussion_state(
    item: &PrDiscussionItem,
    colors: SemanticColors,
) -> (Option<String>, gpui::Rgba) {
    match item.state.as_deref() {
        Some("APPROVED") => (Some("Approved".to_owned()), Ink::FRESH),
        Some("CHANGES_REQUESTED") => (Some("Requested changes".to_owned()), Ink::DANGER),
        Some("COMMENTED") => (Some("Reviewed".to_owned()), colors.secondary),
        Some(state) => (Some(humanize_github_state(state)), colors.secondary),
        None => (None, colors.secondary),
    }
}

fn pull_request_can_merge(pull_request: &PullRequestStatus) -> bool {
    pull_request.state == "OPEN"
        && !pull_request.is_draft
        && pull_request.mergeable.as_deref() != Some("CONFLICTING")
        && pull_request.checks_failed == 0
        && pull_request.checks_pending == 0
        && !matches!(
            pull_request.review_decision.as_deref(),
            Some("CHANGES_REQUESTED") | Some("REVIEW_REQUIRED")
        )
        && !matches!(
            pull_request.merge_state_status.as_deref(),
            Some("BLOCKED") | Some("DIRTY") | Some("DRAFT")
        )
}

fn merge_blocker_label(pull_request: &PullRequestStatus) -> &'static str {
    if pull_request.checks_failed > 0 {
        "Checks are failing"
    } else if pull_request.checks_pending > 0 {
        "Checks are still running"
    } else if pull_request.mergeable.as_deref() == Some("CONFLICTING") {
        "Resolve merge conflicts"
    } else if pull_request.review_decision.as_deref() == Some("CHANGES_REQUESTED") {
        "Changes were requested"
    } else if pull_request.review_decision.as_deref() == Some("REVIEW_REQUIRED") {
        "Review is required"
    } else {
        "GitHub is blocking the merge"
    }
}

fn humanize_github_state(value: &str) -> String {
    let lower = value.replace('_', " ").to_ascii_lowercase();
    let mut chars = lower.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn render_artifact_row(artifact: &SessionArtifact, colors: SemanticColors) -> AnyElement {
    let (symbol, kind_label) = match artifact.kind {
        ArtifactKind::PullRequest => ("arrow.triangle.pull", "Pull request"),
        ArtifactKind::LinearIssue => ("checklist", "Linear issue"),
        ArtifactKind::Preview => ("network", "Preview"),
        ArtifactKind::Link | ArtifactKind::Unknown => ("link", "Link"),
    };
    let title = artifact_title(artifact);
    let url = artifact.url.clone();
    div()
        .id(SharedString::from(format!(
            "inspector-artifact-{}",
            artifact.url
        )))
        .min_h(px(54.0))
        .px(px(11.0))
        .py(px(9.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .rounded(px(Radius::ROW))
        .bg(colors.primary.alpha(0.035))
        .border_1()
        .border_color(colors.primary.alpha(0.06))
        .cursor_pointer()
        .hover(move |row| row.bg(colors.primary.alpha(0.065)))
        .child(artifact_icon(symbol, colors))
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(Typo::ROW_EMPHASIZED.size))
                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                        .text_color(colors.primary)
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(kind_label),
                ),
        )
        .child(sf_symbol("arrow.up.right", 11.0, colors.tertiary))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .into_any_element()
}

fn artifact_icon(symbol: &'static str, colors: SemanticColors) -> AnyElement {
    div()
        .size(px(30.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::BADGE))
        .bg(Fill::subtle(colors))
        .child(sf_symbol(symbol, 13.0, colors.secondary))
        .into_any_element()
}

fn artifact_count(session: &SessionRecord) -> usize {
    let artifacts = session.artifacts.as_deref().unwrap_or_default();
    let status_only_pull_requests = session
        .pull_requests
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|status| {
            !artifacts.iter().any(|artifact| {
                artifact.kind == ArtifactKind::PullRequest && artifact.url == status.url
            })
        })
        .count();
    artifacts.len() + status_only_pull_requests
}

fn ui_agent_kind(kind: &ProtoAgentKind) -> AgentKind {
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => AgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => AgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => AgentKind::Cursor,
        "omp" => AgentKind::Omp,
        ProtoAgentKind::SHELL_ID => AgentKind::Shell,
        _ => AgentKind::Generic,
    }
}

/// Human-facing name for an agent the client has no brand treatment for:
/// the manifest's own display name, else its id in title case.
fn generic_agent_label(descriptor: Option<&ubra_proto::AgentDescriptor>, id: &str) -> String {
    descriptor
        .map(|descriptor| descriptor.display_name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| crate::agent_catalog::title_case_id(id))
}

fn session_status(session: &SessionRecord, colors: SemanticColors) -> (&'static str, gpui::Rgba) {
    if session.hibernation.is_some() {
        return ("Sleeping", colors.secondary);
    }
    match session.status {
        SessionStatus::Starting => (
            "Starting",
            Ink::working(ui_agent_kind(session.effective_kind()), colors),
        ),
        SessionStatus::Working => (
            "Working",
            Ink::working(ui_agent_kind(session.effective_kind()), colors),
        ),
        SessionStatus::NeedsInput(_) => {
            let destructive = session
                .needs_input
                .as_ref()
                .is_some_and(|detail| detail.risk_hint == ubra_proto::RiskHint::Destructive);
            (
                "Needs input",
                if destructive {
                    Ink::DANGER
                } else {
                    Ink::ATTENTION
                },
            )
        }
        SessionStatus::Idle if session.attention() == ubra_proto::AttentionLevel::DoneUnseen => {
            ("Finished", Ink::FRESH)
        }
        SessionStatus::Idle => ("Idle", colors.secondary),
        SessionStatus::Exited(_) => ("Ended", colors.tertiary),
        SessionStatus::Unknown => ("Unknown", colors.tertiary),
    }
}

fn pull_request_state(
    pull_request: &PullRequestStatus,
    colors: SemanticColors,
) -> (&'static str, gpui::Rgba) {
    if pull_request.state == "MERGED" {
        return ("Merged", rgba(0xaf7cf7ff));
    }
    if pull_request.state == "CLOSED" {
        return ("Closed", Ink::DANGER);
    }
    if pull_request.is_draft {
        return ("Draft", colors.secondary);
    }
    if pull_request.mergeable.as_deref() == Some("CONFLICTING") {
        return ("Conflicts", Ink::DANGER);
    }
    match pull_request.review_decision.as_deref() {
        Some("APPROVED") => ("Approved", Ink::FRESH),
        Some("CHANGES_REQUESTED") => ("Needs work", Ink::DANGER),
        Some("REVIEW_REQUIRED") => ("Review needed", Ink::ATTENTION),
        _ => ("Open", colors.secondary),
    }
}

fn pull_request_discussion(pull_request: &PullRequestStatus) -> Option<String> {
    let mut parts = Vec::new();
    if pull_request.comment_count > 0 {
        parts.push(format!(
            "{} {}",
            pull_request.comment_count,
            if pull_request.comment_count == 1 {
                "comment"
            } else {
                "comments"
            }
        ));
    }
    if pull_request.review_count > 0 {
        parts.push(format!(
            "{} {}",
            pull_request.review_count,
            if pull_request.review_count == 1 {
                "review"
            } else {
                "reviews"
            }
        ));
    }
    if let Some(total) = pull_request.total_threads.filter(|total| *total > 0) {
        parts.push(format!(
            "{} of {total} threads resolved",
            pull_request.resolved_threads.unwrap_or(0)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn artifact_title(artifact: &SessionArtifact) -> String {
    match artifact.kind {
        ArtifactKind::PullRequest => pr_number(&artifact.url)
            .map(|number| format!("PR #{number}"))
            .unwrap_or_else(|| "Pull request".to_owned()),
        ArtifactKind::LinearIssue => {
            linear_key(&artifact.url).unwrap_or_else(|| "Linear issue".to_owned())
        }
        ArtifactKind::Preview => url_authority(&artifact.url),
        ArtifactKind::Link | ArtifactKind::Unknown => url_authority(&artifact.url),
    }
}

fn pr_number(url: &str) -> Option<String> {
    let parts = url
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if let Some(index) = parts.iter().position(|part| *part == "pull") {
        return parts
            .get(index + 1)
            .map(|part| part.chars().take_while(char::is_ascii_digit).collect())
            .filter(|part: &String| !part.is_empty());
    }
    parts
        .last()
        .filter(|part| part.chars().all(|character| character.is_ascii_digit()))
        .map(|part| (*part).to_owned())
}

fn linear_key(url: &str) -> Option<String> {
    let parts = url.split('/').collect::<Vec<_>>();
    let index = parts.iter().position(|part| *part == "issue")?;
    parts.get(index + 1).map(|part| (*part).to_owned())
}

fn url_authority(url: &str) -> String {
    url.split_once("://")
        .map_or(url, |(_, remainder)| remainder)
        .split('/')
        .next()
        .filter(|authority| !authority.is_empty())
        .unwrap_or(url)
        .to_owned()
}

fn folder_name(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1_073_741_824.0;
    const MIB: f64 = 1_048_576.0;
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MB", bytes as f64 / MIB)
    }
}

fn relative_time(milliseconds: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64() * 1000.0);
    let seconds = ((now - milliseconds).max(0.0) / 1000.0) as u64;
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[derive(Clone)]
struct DiffRowRenderContext {
    content_width: f32,
    colors: SemanticColors,
    inspector: Entity<WorkbenchInspector>,
    repo_root: PathBuf,
    layer: DiffLayer,
    armed_hunk: Option<u64>,
    /// The omitted-untracked notice row and whether it is expanded.
    omitted_notice: Option<(usize, bool)>,
}

#[allow(clippy::too_many_arguments)]
fn render_rows(
    snapshot: &DiffSnapshot,
    range: Range<usize>,
    content_width: f32,
    colors: SemanticColors,
    inspector: Entity<WorkbenchInspector>,
    armed_hunk: Option<u64>,
    omitted_notice: Option<(usize, bool)>,
    selection: &DiffSelection,
) -> Vec<AnyElement> {
    let context = DiffRowRenderContext {
        content_width,
        colors,
        inspector,
        repo_root: snapshot.repo_root.clone(),
        layer: snapshot.layer,
        armed_hunk,
        omitted_notice,
    };
    range
        .map(|index| {
            // Rows past the snapshot are the expanded notice's file names.
            if let Some(ordinal) = index.checked_sub(snapshot.rows.len()) {
                return render_omitted_path_row(
                    index,
                    ordinal,
                    &snapshot.omitted_untracked_paths[ordinal],
                    &context,
                );
            }
            let owning_file = snapshot
                .file_diffs
                .iter()
                .find(|file| file.row_range.contains(&index));
            let file = (snapshot.rows[index].kind == DiffRowKind::File)
                .then(|| owning_file.cloned())
                .flatten();
            let hunk = (snapshot.rows[index].kind == DiffRowKind::Hunk)
                .then(|| {
                    owning_file.and_then(|file| {
                        file.hunks
                            .iter()
                            .find(|hunk| hunk.row_range.start == index)
                            .cloned()
                            .map(|hunk| (file.path.clone(), hunk))
                    })
                })
                .flatten();
            render_row(
                index,
                &snapshot.rows[index],
                &context,
                file,
                hunk,
                selection.contains(snapshot, index),
            )
        })
        .collect()
}

fn prompt_layer(layer: DiffLayer) -> ReviewLayer {
    match layer {
        DiffLayer::Branch => ReviewLayer::Branch,
        DiffLayer::Staged => ReviewLayer::Staged,
        DiffLayer::Working => ReviewLayer::Working,
    }
}

fn patch_creates_file(patch: &[u8]) -> bool {
    patch
        .windows(b"--- /dev/null".len())
        .any(|window| window == b"--- /dev/null")
}

fn render_row(
    index: usize,
    row: &DiffRow,
    context: &DiffRowRenderContext,
    file: Option<DiffFile>,
    hunk: Option<(PathBuf, DiffHunk)>,
    selected: bool,
) -> AnyElement {
    let content_width = context.content_width;
    let colors = context.colors;
    let inspector = context.inspector.clone();
    let repo_root = &context.repo_root;
    let layer = context.layer;
    let armed_hunk = context.armed_hunk;
    let DiffRowStyle {
        background,
        foreground,
        marker,
    } = diff_row_style(row.kind, colors);
    let line_number = |line: Option<u32>| line.map_or_else(String::new, |line| line.to_string());
    let text = if row.kind == DiffRowKind::File {
        SharedString::from(row.text.clone())
    } else {
        SharedString::from(format!("{marker}{}", row.text))
    };

    let reference = row.text.clone();
    let cwd = repo_root.to_path_buf();
    let open_inspector = inspector.clone();
    let select_inspector = inspector.clone();
    let selectable = matches!(
        row.kind,
        DiffRowKind::Hunk | DiffRowKind::Context | DiffRowKind::Addition | DiffRowKind::Deletion
    );
    let mut actions = diff_row_actions(colors);
    let disclosure = context
        .omitted_notice
        .and_then(|(row, open)| (row == index).then_some(open));
    let toggle_inspector = inspector.clone();

    if let Some(file) = file.as_ref() {
        let ask_inspector = inspector.clone();
        let evidence = ReviewEvidence::File {
            path: file.path.clone(),
            layer: prompt_layer(layer),
            patch: file
                .hunks
                .iter()
                .map(|hunk| String::from_utf8_lossy(&hunk.patch))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        actions = actions.child(
            div()
                .id(("ask-diff-file", index))
                .h_full()
                .px(px(5.0))
                .flex()
                .items_center()
                .gap(px(3.0))
                .rounded(px(Radius::CHIP))
                .cursor_pointer()
                .hover(move |button| button.bg(rgba(0xd9775722)))
                .text_size(px(8.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgba(0xe9a381ff))
                .child(sf_symbol("sparkles", 8.0, rgba(0xe9a381ff)))
                .child("Ask")
                .on_click(move |_, window, cx| {
                    ask_inspector.update(cx, |inspector, cx| {
                        inspector.open_ask(vec![evidence.clone()], window, cx);
                    });
                    cx.stop_propagation();
                }),
        );
        match layer {
            DiffLayer::Working => {
                actions = actions.child(stage_file_action(
                    index,
                    file.path.clone(),
                    inspector.clone(),
                    colors,
                ));
            }
            DiffLayer::Staged => {
                let unstage_inspector = inspector.clone();
                let path = file.path.clone();
                actions = actions.child(
                    div()
                        .id(("unstage-diff-file", index))
                        .h_full()
                        .px(px(5.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                        .text_size(px(8.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.tertiary)
                        .child("Unstage")
                        .on_click(move |_, _, cx| {
                            unstage_inspector.update(cx, |inspector, cx| {
                                inspector.run_review_action(
                                    ReviewAction::Unstage(vec![path.clone()]),
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        }),
                );
            }
            DiffLayer::Branch => {}
        }
    }

    if let Some((path, hunk)) = hunk.as_ref() {
        let ask_inspector = inspector.clone();
        let evidence = ReviewEvidence::Hunk {
            path: path.clone(),
            layer: prompt_layer(layer),
            header: hunk.header.clone(),
            patch: String::from_utf8_lossy(&hunk.patch).into_owned(),
        };
        actions = actions.child(
            div()
                .id(("ask-diff-hunk", index))
                .h_full()
                .px(px(5.0))
                .flex()
                .items_center()
                .gap(px(3.0))
                .rounded(px(Radius::CHIP))
                .cursor_pointer()
                .hover(move |button| button.bg(rgba(0xd9775722)))
                .text_size(px(8.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgba(0xe9a381ff))
                .child(sf_symbol("sparkles", 8.0, rgba(0xe9a381ff)))
                .child("Ask")
                .on_click(move |_, window, cx| {
                    ask_inspector.update(cx, |inspector, cx| {
                        inspector.open_ask(vec![evidence.clone()], window, cx);
                    });
                    cx.stop_propagation();
                }),
        );
        let patch = hunk.patch.clone();
        match layer {
            DiffLayer::Working => {
                let stage_inspector = inspector.clone();
                let stage_patch = patch.clone();
                actions = actions.child(
                    div()
                        .id(("stage-diff-hunk", index))
                        .h_full()
                        .px(px(5.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                        .text_size(px(8.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.secondary)
                        .child("Stage")
                        .on_click(move |_, _, cx| {
                            stage_inspector.update(cx, |inspector, cx| {
                                inspector.run_review_action(
                                    ReviewAction::Patch {
                                        patch: stage_patch.clone(),
                                        mutation: PatchMutation::Stage,
                                    },
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        }),
                );
                if !patch_creates_file(&patch) {
                    let discard_inspector = inspector.clone();
                    let discard_patch = patch;
                    let fingerprint = hunk.fingerprint;
                    let armed = armed_hunk == Some(fingerprint);
                    actions = actions.child(
                        div()
                            .id(("discard-diff-hunk", index))
                            .h_full()
                            .px(px(5.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::CHIP))
                            .cursor_pointer()
                            .bg(if armed {
                                Ink::DANGER.alpha(0.12)
                            } else {
                                colors.primary.alpha(0.0)
                            })
                            .hover(move |button| button.bg(Ink::DANGER.alpha(0.13)))
                            .text_size(px(8.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Ink::DANGER)
                            .child(if armed { "Confirm" } else { "Discard" })
                            .on_click(move |_, _, cx| {
                                discard_inspector.update(cx, |inspector, cx| {
                                    if inspector.armed_hunk == Some(fingerprint) {
                                        inspector.run_review_action(
                                            ReviewAction::Patch {
                                                patch: discard_patch.clone(),
                                                mutation: PatchMutation::Discard,
                                            },
                                            cx,
                                        );
                                    } else {
                                        inspector.armed_hunk = Some(fingerprint);
                                        inspector.review_feedback = Some((
                                            false,
                                            "Click Confirm to discard this hunk".to_owned(),
                                        ));
                                        cx.notify();
                                    }
                                });
                                cx.stop_propagation();
                            }),
                    );
                }
            }
            DiffLayer::Staged => {
                let unstage_inspector = inspector.clone();
                actions = actions.child(
                    div()
                        .id(("unstage-diff-hunk", index))
                        .h_full()
                        .px(px(5.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                        .text_size(px(8.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.tertiary)
                        .child("Unstage")
                        .on_click(move |_, _, cx| {
                            unstage_inspector.update(cx, |inspector, cx| {
                                inspector.run_review_action(
                                    ReviewAction::Patch {
                                        patch: patch.clone(),
                                        mutation: PatchMutation::Unstage,
                                    },
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        }),
                );
            }
            DiffLayer::Branch => {}
        }
    }

    let has_actions = file.is_some() || hunk.is_some();
    div()
        .id(index)
        .relative()
        .h(px(DIFF_ROW_HEIGHT))
        .min_w(px(content_width))
        .w_full()
        .flex()
        .items_center()
        .bg(if selected {
            rgba(0x5b8fd13d)
        } else {
            background
        })
        .when(selected, |line| {
            line.border_l_2().border_color(rgba(0x8bb9e8dd))
        })
        .when(selectable, |line| {
            line.cursor_pointer()
                .hover(move |line| line.bg(rgba(0x5b8fd129)))
                .on_click(move |event: &gpui::ClickEvent, window, cx| {
                    select_inspector.update(cx, |inspector, cx| {
                        inspector.select_diff_row(index, event.modifiers().shift, window, cx);
                    });
                    cx.stop_propagation();
                })
        })
        .when(row.kind == DiffRowKind::File, |line| {
            line.border_t_1()
                .border_color(colors.primary.alpha(0.08))
                .cursor_pointer()
                .hover(move |line| line.bg(colors.primary.alpha(0.07)))
                .on_click(move |_, _, cx| {
                    open_inspector.update(cx, |inspector, cx| {
                        inspector.open_file_reference(cwd.clone(), reference.clone(), cx);
                    });
                    cx.stop_propagation();
                })
        })
        .when(disclosure.is_some(), |line| {
            line.debug_selector(|| "INSPECTOR_OMITTED_UNTRACKED_NOTICE".to_owned())
                .cursor_pointer()
                .hover(move |line| line.bg(colors.primary.alpha(0.07)))
                .on_click(move |_, _, cx| {
                    toggle_inspector.update(cx, |inspector, cx| {
                        inspector.toggle_omitted_untracked(cx);
                    });
                    cx.stop_propagation();
                })
        })
        .child(
            div()
                .w(px(GUTTER_WIDTH))
                .h_full()
                .flex_none()
                .pr(px(7.0))
                .flex()
                .items_center()
                .justify_end()
                .gap(px(7.0))
                .border_r_1()
                .border_color(colors.primary.alpha(0.055))
                .font_family(crate::fonts::mono_family())
                .text_size(px(10.5))
                .text_color(colors.primary.alpha(0.25))
                .child(line_number(row.old_line))
                .child(line_number(row.new_line)),
        )
        .child(
            div()
                .h_full()
                .flex()
                .items_center()
                .pl(px(if row.kind == DiffRowKind::File {
                    10.0
                } else {
                    8.0
                }))
                .gap(px(6.0))
                .font_family(crate::fonts::mono_family())
                .text_size(px(11.5))
                .font_weight(if row.kind == DiffRowKind::File {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .text_color(foreground)
                .when(row.kind == DiffRowKind::File, |content| {
                    content.child(sf_symbol(
                        "chevron.left.forwardslash.chevron.right",
                        13.0,
                        colors.secondary,
                    ))
                })
                .when_some(disclosure, |content, open| {
                    content.child(sf_symbol(
                        if open {
                            "chevron.down"
                        } else {
                            "chevron.right"
                        },
                        9.0,
                        foreground,
                    ))
                })
                .child(text),
        )
        .when(has_actions, |line| line.child(actions))
        .into_any_element()
}

/// The floating action strip at the right edge of a file, hunk, or omitted
/// file-name row.
fn diff_row_actions(colors: SemanticColors) -> gpui::Div {
    div()
        .absolute()
        .right(px(6.0))
        .top(px(2.0))
        .h(px(16.0))
        .flex()
        .items_center()
        .gap(px(2.0))
        .rounded(px(Radius::CHIP))
        .bg(colors.background.alpha(0.96))
        .border_1()
        .border_color(colors.primary.alpha(0.10))
}

fn stage_file_action(
    index: usize,
    path: PathBuf,
    inspector: Entity<WorkbenchInspector>,
    colors: SemanticColors,
) -> impl IntoElement {
    div()
        .id(("stage-diff-file", index))
        .h_full()
        .px(px(5.0))
        .flex()
        .items_center()
        .rounded(px(Radius::CHIP))
        .cursor_pointer()
        .hover(move |button| button.bg(colors.primary.alpha(0.09)))
        .text_size(px(8.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(colors.secondary)
        .child("Stage")
        .on_click(move |_, _, cx| {
            inspector.update(cx, |inspector, cx| {
                inspector.run_review_action(ReviewAction::Stage(vec![path.clone()]), cx);
            });
            cx.stop_propagation();
        })
}

/// One file name from the expanded omitted-untracked notice. It carries no
/// diff body: clicking opens the file, and the Working lane can stage it.
fn render_omitted_path_row(
    index: usize,
    ordinal: usize,
    path: &Path,
    context: &DiffRowRenderContext,
) -> AnyElement {
    let colors = context.colors;
    let foreground = diff_row_style(DiffRowKind::Context, colors).foreground;
    let reference = path.to_string_lossy().into_owned();
    let cwd = context.repo_root.clone();
    let open_inspector = context.inspector.clone();
    let stageable = context.layer == DiffLayer::Working;
    div()
        .id(index)
        .debug_selector(move || format!("INSPECTOR_OMITTED_UNTRACKED_PATH_{ordinal}"))
        .group(OMITTED_PATH_ROW_GROUP)
        .relative()
        .h(px(DIFF_ROW_HEIGHT))
        .min_w(px(context.content_width))
        .w_full()
        .flex()
        .items_center()
        .cursor_pointer()
        .hover(move |line| line.bg(colors.primary.alpha(0.07)))
        .on_click({
            let reference = reference.clone();
            move |_, _, cx| {
                open_inspector.update(cx, |inspector, cx| {
                    inspector.open_file_reference(cwd.clone(), reference.clone(), cx);
                });
                cx.stop_propagation();
            }
        })
        .child(
            div()
                .w(px(GUTTER_WIDTH))
                .h_full()
                .flex_none()
                .border_r_1()
                .border_color(colors.primary.alpha(0.055)),
        )
        .child(
            div()
                .h_full()
                .flex()
                .items_center()
                .pl(px(8.0 + OMITTED_PATH_INDENT))
                .font_family(crate::fonts::mono_family())
                .text_size(px(11.5))
                .text_color(foreground)
                .child(SharedString::from(reference)),
        )
        // A long run of names stays a plain list; Stage appears on the row
        // under the pointer.
        .when(stageable, |line| {
            line.child(
                diff_row_actions(colors)
                    .invisible()
                    .group_hover(OMITTED_PATH_ROW_GROUP, |actions| actions.visible())
                    .child(stage_file_action(
                        index,
                        path.to_path_buf(),
                        context.inspector.clone(),
                        colors,
                    )),
            )
        })
        .into_any_element()
}

#[derive(Clone, Copy)]
struct DiffRowStyle {
    background: gpui::Rgba,
    foreground: gpui::Rgba,
    marker: &'static str,
}

fn diff_row_style(kind: DiffRowKind, colors: SemanticColors) -> DiffRowStyle {
    let (background, foreground, marker) = match (colors.appearance, kind) {
        (Appearance::Dark, DiffRowKind::Addition) => (rgba(0x2f7d4a24), rgba(0xc7ebd2ff), "+"),
        (Appearance::Dark, DiffRowKind::Deletion) => (rgba(0x9f3a4424), rgba(0xf0c4c8ff), "−"),
        (Appearance::Dark, DiffRowKind::Hunk) => (rgba(0x4675a31c), rgba(0x9bbde0ff), ""),
        (Appearance::Dark, DiffRowKind::File) => (rgba(0xffffff09), colors.primary, ""),
        (Appearance::Dark, DiffRowKind::Context) => (rgba(0x00000000), rgba(0xffffffb8), ""),
        (Appearance::Dark, DiffRowKind::Meta) => (rgba(0x00000000), rgba(0xffffff66), ""),
        (Appearance::Light, DiffRowKind::Addition) => (rgba(0x2f7d4a18), rgba(0x24522eff), "+"),
        (Appearance::Light, DiffRowKind::Deletion) => (rgba(0x9f3a4418), rgba(0x812c32ff), "−"),
        (Appearance::Light, DiffRowKind::Hunk) => (rgba(0x4675a316), rgba(0x285b85ff), ""),
        (Appearance::Light, DiffRowKind::File) => (rgba(0x00000008), rgba(0x34312dff), ""),
        (Appearance::Light, DiffRowKind::Context) => (rgba(0x00000000), rgba(0x34312dff), ""),
        (Appearance::Light, DiffRowKind::Meta) => (rgba(0x00000000), rgba(0x5a5650ff), ""),
    };
    DiffRowStyle {
        background,
        foreground,
        marker,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use gpui::{Entity, Modifiers, TestAppContext};
    use ubra_proto::DateMillis;

    struct InspectorHarness {
        inspector: Entity<WorkbenchInspector>,
    }

    #[test]
    fn generic_agents_are_named_by_their_manifest() {
        use ubra_proto::AgentDescriptor;
        let descriptor = |display_name: &str| AgentDescriptor {
            id: "pi".to_owned(),
            display_name: display_name.to_owned(),
            ..AgentDescriptor::default()
        };
        assert_eq!(generic_agent_label(Some(&descriptor("Pi")), "pi"), "Pi");
        assert_eq!(
            generic_agent_label(Some(&descriptor("  ")), "opencode"),
            "Opencode"
        );
        assert_eq!(generic_agent_label(None, "claude-code"), "Claude Code");
        assert_eq!(generic_agent_label(None, "note"), "Note");
    }

    fn composite(foreground: gpui::Rgba, background: gpui::Rgba) -> gpui::Rgba {
        let alpha = foreground.a + background.a * (1.0 - foreground.a);
        if alpha == 0.0 {
            return rgba(0x00000000);
        }
        gpui::Rgba {
            r: (foreground.r * foreground.a + background.r * background.a * (1.0 - foreground.a))
                / alpha,
            g: (foreground.g * foreground.a + background.g * background.a * (1.0 - foreground.a))
                / alpha,
            b: (foreground.b * foreground.a + background.b * background.a * (1.0 - foreground.a))
                / alpha,
            a: alpha,
        }
    }

    fn relative_luminance(color: gpui::Rgba) -> f32 {
        fn linear(channel: f32) -> f32 {
            if channel <= 0.03928 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    fn contrast(left: gpui::Rgba, right: gpui::Rgba) -> f32 {
        let left = relative_luminance(left);
        let right = relative_luminance(right);
        (left.max(right) + 0.05) / (left.min(right) + 0.05)
    }

    impl Render for InspectorHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(300.0))
                .h_full()
                .overflow_hidden()
                .child(self.inspector.clone())
        }
    }

    /// A Working-lane snapshot whose file-count notice has names to expand.
    fn omitted_untracked_preview() -> DiffSnapshot {
        const OMITTED: usize = 5_000;
        let mut snapshot = crate::diff::parse_unified_diff(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n",
        );
        snapshot.layer = DiffLayer::Working;
        snapshot.truncated = true;
        snapshot.omitted_untracked = OMITTED;
        snapshot.omitted_untracked_paths = (0..OMITTED)
            .map(|index| PathBuf::from(format!("generated/file-{index:04}.txt")))
            .collect();
        snapshot.rows.push(DiffRow {
            kind: DiffRowKind::Meta,
            old_line: None,
            new_line: None,
            text: "5000 more untracked files not shown (limit 200); Stage all still includes them"
                .to_owned(),
        });
        snapshot
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the isolated workspace-panel screenshot"]
    fn render_workspace_preview_screenshot() {
        let output = std::env::var_os("UBRA_VISUAL_OUTPUT")
            .map(PathBuf::from)
            .expect("output path");
        let platform = gpui_platform::current_platform(true);
        let mut cx = gpui::HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(ubra_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let window = cx
            .open_window(
                gpui::size(
                    px(std::env::var("UBRA_VISUAL_WIDTH")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(700.0)),
                    px(700.0),
                ),
                |_, cx| {
                    let runtime = Arc::new(StoreRuntime::inert());
                    if std::env::var_os("UBRA_VISUAL_LIGHT").is_some() {
                        runtime
                            .store
                            .write()
                            .unwrap()
                            .update_preferences(|prefs| prefs.terminal_theme = "github-light".into())
                            .unwrap();
                    }
                    let tokio = Arc::new(
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap(),
                    );
                    cx.new(|cx| {
                        let mut inspector = WorkbenchInspector::new(runtime, tokio, cx);
                        inspector.workspace_tabs.clear();
                        inspector.workspace_active = None;
                        inspector.workspace_selected = None;
                        if std::env::var_os("UBRA_VISUAL_FILES").is_some() {
                            inspector.add_workspace(WorkspaceSurface::Files, cx);
                            inspector
                                .code_viewer
                                .update(cx, |viewer, cx| viewer.seed_explorer_preview(cx));
                        }
                        if std::env::var_os("UBRA_VISUAL_OMITTED").is_some() {
                            inspector.select_workspace(WorkspaceSurface::Review, cx);
                            inspector.state =
                                LoadState::Ready(Arc::new(omitted_untracked_preview()));
                            inspector.omitted_untracked_open = true;
                        }
                        if std::env::var_os("UBRA_VISUAL_BROWSER").is_some() {
                            inspector.select_workspace(WorkspaceSurface::Review, cx);
                            inspector.select_workspace(WorkspaceSurface::Browser, cx);
                            if std::env::var_os("UBRA_VISUAL_BROWSER_TABS").is_some() {
                                inspector.browser_state.title = Some("Local preview".into());
                                inspector.add_workspace(WorkspaceSurface::Browser, cx);
                                inspector.browser_state = BrowserState {
                                    url: Some("https://ubra.app/docs".into()),
                                    title: Some("Ubra documentation and guides".into()),
                                    favicon: Some(Arc::new(gpui::Image::from_bytes(
                                        gpui::ImageFormat::Png,
                                        include_bytes!("../../../assets/icon.png").to_vec(),
                                    ))),
                                    ..BrowserState::default()
                                };
                                inspector.browser_query.insert("https://ubra.app/docs");
                            }
                        }
                        inspector
                    })
                },
            )
            .expect("headless window");
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .expect("screenshot")
            .save(output)
            .expect("save");
    }

    #[gpui::test]
    fn closing_a_surface_preserves_remaining_tabs_and_last_close_restores_metadata(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));

        inspector.update(cx, |inspector, cx| {
            inspector.select_workspace(WorkspaceSurface::Review, cx);
            assert!(inspector.close_active_workspace(cx));
            assert_eq!(
                inspector.workspace_selected,
                Some(WorkspaceSurface::Browser),
                "closing Review preserves the existing Browser surface"
            );
            inspector.close_workspace(0, cx);
            assert_eq!(
                inspector.workspace_selected,
                Some(WorkspaceSurface::Details),
                "closing the last surface must recreate Details"
            );
            inspector.select_workspace(WorkspaceSurface::Browser, cx);
            assert!(inspector.close_active_workspace(cx));
            assert_eq!(
                inspector.workspace_selected,
                Some(WorkspaceSurface::Details)
            );
        });

        inspector.read_with(cx, |inspector, _| {
            assert_eq!(inspector.workspace_tabs.len(), 1);
            assert_eq!(
                inspector.workspace_tabs[0].surface,
                WorkspaceSurface::Details
            );
            assert_eq!(
                inspector.workspace_selected,
                Some(WorkspaceSurface::Details)
            );
        });
        assert_eq!(
            runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .preferences()
                .inspector_tab,
            InspectorTab::Info
        );
    }

    #[gpui::test]
    fn saved_pane_context_is_window_local_and_empty_does_not_fall_back(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let ids: Vec<_> = fixture
            .list
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let first = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio.clone(), cx));
        let second = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio.clone(), cx));
        first.update(cx, |inspector, cx| {
            inspector.set_session_context(Some(Some(ids[1].clone())), cx);
            assert_eq!(inspector.selected_context().unwrap().id, ids[1]);
            assert_eq!(inspector.selected_session().unwrap().id, ids[1]);
            inspector.add_workspace(WorkspaceSurface::Browser, cx);
        });
        second.read_with(cx, |inspector, _| {
            assert_eq!(inspector.selected_session().unwrap().id, ids[0])
        });
        assert_eq!(
            runtime.store.read().unwrap().selected_session_id(),
            Some(&ids[0])
        );
        first.update(cx, |inspector, cx| {
            inspector.set_session_context(Some(None), cx);
            assert!(inspector.selected_context().is_none());
            assert!(inspector.selected_session().is_none());
            inspector.set_session_context(Some(Some(ids[1].clone())), cx);
            assert_eq!(
                inspector.workspace_selected,
                Some(WorkspaceSurface::Browser)
            );
            inspector.set_session_context(None, cx);
            assert_eq!(inspector.selected_session().unwrap().id, ids[0]);
        });
    }

    #[gpui::test]
    fn workspace_tabs_stay_within_project_and_swap_across_projects(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        fixture.list.sessions[0].cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root")
            .to_string_lossy()
            .into_owned();
        let ids: Vec<_> = fixture.list.sessions.iter().map(|s| s.id.clone()).collect();
        // ids[0] and ids[1] share a project; ids[6] lives in another one.
        let home = fixture.list.sessions[0].project_id.0.clone();
        let away = fixture.list.sessions[6].project_id.0.clone();
        assert_ne!(home, away);
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        let first = inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            i.add_workspace(WorkspaceSurface::Files, cx);
            i.add_workspace(WorkspaceSurface::Files, cx);
            i.add_workspace(WorkspaceSurface::Browser, cx);
            i.browser_query.insert("https://example.com/session-a");
            i.workspace_active.unwrap()
        });
        let files = inspector.read_with(cx, |i, _| {
            i.workspace_tabs
                .iter()
                .filter_map(|t| t.viewer.clone())
                .collect::<Vec<_>>()
        });
        files[0].update(cx, |viewer, cx| viewer.seed_explorer_preview(cx));
        // Same-project pane switch: tabs untouched, only content follows.
        runtime.store.write().unwrap().select(ids[1].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(i.workspace_active, Some(first));
            assert_eq!(
                i.workspace_selected,
                Some(WorkspaceSurface::Browser),
                "same-project switch must not touch tabs"
            );
            assert_eq!(i.browser_query.text(), "https://example.com/session-a");
            assert_eq!(
                i.selected_context().map(|context| context.id),
                Some(ids[1].clone()),
                "inspected content still follows the focused pane"
            );
            assert!(
                !i.project_workspaces.contains_key(&home),
                "no stash on a same-project switch"
            );
        });
        // Cross-project switch: the home tabs are stashed, away starts fresh.
        runtime.store.write().unwrap().select(ids[6].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(
                i.workspace_selected,
                Some(WorkspaceSurface::Runs),
                "project B must start with its own sidebar"
            );
            assert!(i.workspace_tabs.iter().all(|tab| tab.id != first));
            i.add_workspace(WorkspaceSurface::Browser, cx);
            assert!(i.browser_query.is_empty());
            assert_ne!(i.workspace_active, Some(first));
            i.set_visible(true, cx);
        });
        runtime.store.write().unwrap().select(ids[0].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(i.workspace_active, Some(first));
            assert_eq!(i.browser_query.text(), "https://example.com/session-a");
            assert!(
                i.visible,
                "panel visibility is global: selecting A must not close it"
            );
            assert_eq!(
                files[0].read(cx).tab_label().as_deref(),
                Some("code_intelligence.rs"),
                "B must not clear A's open file"
            );
            assert_eq!(
                i.workspace_tabs
                    .iter()
                    .filter_map(|t| t.viewer.clone())
                    .collect::<Vec<_>>(),
                files
            );
            i.set_visible(false, cx);
        });
        runtime.store.write().unwrap().select(ids[6].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert!(
                !i.visible,
                "panel visibility is global: selecting B must not reopen it"
            );
        });
        // Closing a project's last sessions releases its hidden tabs; the
        // other project's stash survives.
        let away_ids: Vec<_> = {
            let store = runtime.store.read().unwrap();
            store
                .sessions()
                .values()
                .filter(|session| session.project_id.0 == away)
                .map(|session| session.id.clone())
                .collect()
        };
        {
            let mut store = runtime.store.write().unwrap();
            for id in &away_ids {
                store.remove_session_record(id);
            }
        }
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert!(
                !i.project_workspaces.contains_key(&away),
                "closing a project's last sessions releases its hidden tabs"
            );
            // Focus fell back to the live project, so its tabs are live (not
            // stashed) and restored exactly.
            assert_eq!(i.workspace_project, Some(home.clone()));
            assert_eq!(i.workspace_active, Some(first));
        });
    }

    #[gpui::test]
    fn session_switch_keeps_the_notes_surface_selected(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let ids: Vec<_> = fixture.list.sessions.iter().map(|s| s.id.clone()).collect();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            i.add_workspace(WorkspaceSurface::Notes, cx);
        });
        // Creating or opening a note selects a new session; the panel must
        // stay on the session-global Notes surface instead of jumping to
        // the new session's Details tab.
        runtime.store.write().unwrap().select(ids[1].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(
                i.workspace_selected,
                Some(WorkspaceSurface::Notes),
                "selecting a session must not leave the Notes tab"
            );
            assert_eq!(
                i.workspace_tabs
                    .iter()
                    .filter(|tab| tab.surface == WorkspaceSurface::Notes)
                    .count(),
                1,
                "the new session gets one Notes tab, not a duplicate"
            );
        });
        // Switching back restores the first session's Notes tab as-is.
        runtime.store.write().unwrap().select(ids[0].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(
                i.workspace_selected,
                Some(WorkspaceSurface::Notes),
                "returning to a session must restore its Notes tab"
            );
            assert_eq!(
                i.workspace_tabs
                    .iter()
                    .filter(|tab| tab.surface == WorkspaceSurface::Notes)
                    .count(),
                1
            );
        });
    }

    #[gpui::test]
    fn archiving_a_projects_last_session_releases_its_hidden_workspace(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let sessions = fixture.list.sessions.clone();
        let ids: Vec<_> = sessions.iter().map(|s| s.id.clone()).collect();
        let home = sessions[0].project_id.0.clone();
        let home_ids: Vec<_> = sessions
            .iter()
            .filter(|session| session.project_id.0 == home)
            .map(|session| session.id.clone())
            .collect();
        // ids[6] lives in another project so switching stashes home's tabs.
        assert_ne!(sessions[6].project_id.0, home);
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            i.add_workspace(WorkspaceSurface::Browser, cx);
        });
        runtime.store.write().unwrap().select(ids[6].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert!(i.project_workspaces.contains_key(&home));
        });
        // Archiving one session keeps the project entry while siblings remain.
        let mut archived = sessions[0].clone();
        archived.archived_at = Some(DateMillis(1.0));
        runtime.store.write().unwrap().upsert_session(archived);
        inspector.update(cx, |i, cx| {
            i.sync_workspace_session(cx);
            assert!(
                i.project_workspaces.contains_key(&home),
                "live siblings keep the project's hidden tabs"
            );
        });
        // Archiving the project's last session releases its hidden tabs.
        {
            let mut store = runtime.store.write().unwrap();
            for id in &home_ids {
                let mut record = store
                    .sessions()
                    .get(id)
                    .map(AsRef::as_ref)
                    .expect("sibling")
                    .clone();
                record.archived_at = Some(DateMillis(1.0));
                store.upsert_session(record);
            }
        }
        inspector.update(cx, |i, cx| {
            i.sync_workspace_session(cx);
            assert!(
                !i.project_workspaces.contains_key(&home),
                "a project without live sessions keeps no hidden tabs"
            );
        });
        // Unarchived and reselected, it starts from the default workspace.
        let mut restored = sessions[0].clone();
        restored.archived_at = None;
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(restored);
            store.select(ids[0].clone());
        }
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(i.workspace_project, Some(home.clone()));
            assert_eq!(i.workspace_tabs.len(), 1);
            assert_eq!(i.workspace_tabs[0].surface, WorkspaceSurface::Runs);
        });
    }

    #[gpui::test]
    fn browser_address_shortcuts_edit_navigate_and_reuse_the_singleton(cx: &mut TestAppContext) {
        struct BrowserHarness {
            inspector: Entity<WorkbenchInspector>,
            navigations: Vec<String>,
        }
        impl Render for BrowserHarness {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if this
                            .inspector
                            .update(cx, |i, cx| i.browser_shortcut(event, window, cx))
                        {
                            cx.stop_propagation();
                        }
                    }))
                    .child(self.inspector.clone())
            }
        }
        let (harness, cx) = cx.add_window_view(|window, cx| {
            let runtime = Arc::new(StoreRuntime::inert());
            let tokio = Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            );
            let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
            cx.subscribe(&inspector, |this: &mut BrowserHarness, _, event, _| {
                if let InspectorEvent::Browser(BrowserAction::Navigate(url)) = event {
                    this.navigations.push(url.clone());
                }
            })
            .detach();
            inspector.update(cx, |i, cx| {
                i.set_visible(true, cx);
                i.add_workspace(WorkspaceSurface::Browser, cx);
                i.browser_state.url = Some("https://example.com/original".into());
                i.browser_query.insert("https://example.com/original");
                window.focus(&i.focus, cx);
            });
            BrowserHarness {
                inspector,
                navigations: Vec::new(),
            }
        });
        cx.simulate_resize(gpui::size(px(600.0), px(500.0)));
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-l");
        cx.simulate_keystrokes("x enter");
        cx.run_until_parked();
        harness.read_with(cx, |h, _| assert_eq!(h.navigations, ["https://x"]));
        let inspector = harness.read_with(cx, |h, _| h.inspector.clone());
        cx.simulate_keystrokes("cmd-l");
        cx.simulate_keystrokes("z escape");
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.browser_query.text(), "https://example.com/original")
        });
        let original = inspector.read_with(cx, |i, _| i.workspace_active.unwrap());
        cx.simulate_keystrokes("cmd-t");
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            // The browser is a singleton: ⌘T reuses the surface and focuses
            // the address field so typing replaces the page.
            assert_eq!(i.workspace_active, Some(original));
            assert_eq!(i.workspace_selected, Some(WorkspaceSurface::Browser));
            assert!(i.browser_address_focused);
        });
        // Open a second surface so ⌘W has a remaining tab to preserve. The
        // browser shortcut only fires while Browser is selected.
        inspector.update(cx, |i, cx| {
            i.select_workspace(WorkspaceSurface::Review, cx);
            i.select_workspace(WorkspaceSurface::Browser, cx);
        });
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            assert_eq!(
                i.workspace_selected,
                Some(WorkspaceSurface::Review),
                "⌘W preserves the remaining Review surface"
            );
        });
        // Reopening the browser restores a fresh singleton.
        inspector.update(cx, |i, cx| {
            i.select_workspace(WorkspaceSurface::Browser, cx)
        });
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.workspace_selected, Some(WorkspaceSurface::Browser));
            assert_eq!(
                i.workspace_tabs
                    .iter()
                    .filter(|tab| tab.surface == WorkspaceSurface::Browser)
                    .count(),
                1
            );
        });
    }

    #[gpui::test]
    fn notes_surface_lists_scopes_filters_and_switches(cx: &mut TestAppContext) {
        use crate::notes::panel::NotesScope;
        use crate::notes::todos::TodosModel;

        struct NotesHarness {
            inspector: Entity<WorkbenchInspector>,
        }
        impl Render for NotesHarness {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        this.inspector
                            .update(cx, |i, cx| i.handle_key_down(event, window, cx));
                    }))
                    .child(self.inspector.clone())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let notes = Arc::new(ubra_notes::store::NoteStore::open(dir.path().join("notes")).unwrap());
        let (_, global_doc) = ubra_notes::markdown::parse("# Global plan\n\nShip it.\n");
        let (global_id, _) = notes.create(global_doc, None).unwrap();
        let scoped =
            ubra_notes::store::NoteStore::open_workspace(dir.path().join("notes"), "p_launch")
                .unwrap();
        let (_, scoped_doc) = ubra_notes::markdown::parse("# Workspace plan\n\nBuild it.\n");
        let (scoped_id, _) = scoped.create(scoped_doc, None).unwrap();

        let (harness, cx) = cx.add_window_view(|_, cx| {
            let runtime = Arc::new(StoreRuntime::inert());
            // A selected terminal names the workspace project; the global
            // note already has its Session, the workspace note is an orphan.
            let terminal = crate::notes::work_item_tests::record("s_term", ProtoAgentKind::SHELL);
            let mut note = crate::notes::work_item_tests::record("s_note", ProtoAgentKind::NOTE);
            note.note_id = Some(global_id.clone());
            {
                let mut store = runtime.store.write().expect("store");
                store.upsert_session(terminal);
                store.upsert_session(note);
                store.select(ubra_proto::SessionId::new("s_term"));
            }
            let model = cx.new(|cx| {
                TodosModel::with_store(Arc::clone(&runtime), Some(Arc::clone(&notes)), false, cx)
            });
            TodosModel::install(model, cx);
            let tokio = Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            );
            let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
            inspector.update(cx, |i, cx| {
                i.set_visible(true, cx);
                i.add_workspace(WorkspaceSurface::Notes, cx);
            });
            NotesHarness { inspector }
        });
        cx.simulate_resize(gpui::size(px(600.0), px(500.0)));
        cx.run_until_parked();

        let inspector = harness.read_with(cx, |h, _| h.inspector.clone());
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.workspace_selected, Some(WorkspaceSurface::Notes));
            assert_eq!(
                i.selected_session().map(|record| record.project_id),
                Some(ubra_proto::ProjectId::new("p_launch")),
                "the inspector session names the workspace project"
            );
            assert_eq!(i.notes.scope(), None, "the scope starts unset");
        });
        // Both scopes reached the shared index the panel reads.
        inspector.update(cx, |i, cx| {
            i.notes.sync_model(cx);
            let entries = i.notes.entries(cx);
            let mut ids: Vec<(String, Option<String>)> = entries
                .iter()
                .map(|entry| {
                    (
                        entry.id.clone(),
                        entry.workspace.as_ref().map(|id| id.0.clone()),
                    )
                })
                .collect();
            ids.sort();
            let mut expected = vec![
                (global_id.clone(), None),
                (scoped_id.clone(), Some("p_launch".to_owned())),
            ];
            expected.sort();
            assert_eq!(ids, expected);
        });

        // Typing filters; escape clears and releases the keyboard.
        inspector.update_in(cx, |i, window, cx| {
            window.focus(&i.focus, cx);
            i.focus_notes_filter();
        });
        cx.simulate_keystrokes("workspace");
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.notes.query_text(), "workspace");
            assert!(i.notes.filter_focused());
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.notes.query_text(), "");
            assert!(!i.notes.filter_focused());
        });

        // Picking a segment sticks, whatever the selection names.
        inspector.update(cx, |i, _| i.set_notes_scope(NotesScope::Global));
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.notes.scope(), Some(NotesScope::Global));
            assert_eq!(i.workspace_selected, Some(WorkspaceSurface::Notes));
        });
    }

    /// Trashing the open note backs the detail out to the list and drops the
    /// editor's state without saving, so no later save resurrects its file.
    /// A trash for any other note leaves the detail alone.
    #[gpui::test]
    fn trashing_the_open_note_closes_the_detail_without_saving(cx: &mut TestAppContext) {
        use crate::notes::todos::TodosModel;

        let dir = tempfile::tempdir().unwrap();
        let notes = Arc::new(ubra_notes::store::NoteStore::open(dir.path().join("notes")).unwrap());
        let (_, first_doc) = ubra_notes::markdown::parse("# First\n\nAlpha.\n");
        let (first_id, _) = notes.create(first_doc, None).unwrap();
        let (_, second_doc) = ubra_notes::markdown::parse("# Second\n\nBeta.\n");
        let (second_id, _) = notes.create(second_doc, None).unwrap();
        for index in 0..30 {
            let (_, doc) =
                ubra_notes::markdown::parse(&format!("# First extra {index}\n\nText.\n"));
            notes.create(doc, None).expect("scrollable note list");
        }

        let runtime = Arc::new(StoreRuntime::inert());
        let mut first = crate::notes::work_item_tests::record("s_first", ProtoAgentKind::NOTE);
        first.note_id = Some(first_id.clone());
        let mut second = crate::notes::work_item_tests::record("s_second", ProtoAgentKind::NOTE);
        second.note_id = Some(second_id.clone());
        {
            let mut store = runtime.store.write().expect("store");
            store.upsert_session(first);
            store.upsert_session(second);
            store.upsert_session(crate::notes::work_item_tests::record(
                "s_term",
                ProtoAgentKind::SHELL,
            ));
            store.select(SessionId::new("s_term"));
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        cx.update(|cx| cx.bind_keys(crate::notes::key_bindings()));
        let (inspector, cx) = cx.add_window_view({
            let runtime = Arc::clone(&runtime);
            let notes = Arc::clone(&notes);
            move |_, cx| {
                let model = cx
                    .new(|cx| TodosModel::with_store(Arc::clone(&runtime), Some(notes), false, cx));
                TodosModel::install(model, cx);
                WorkbenchInspector::new(runtime, tokio, cx)
            }
        });
        inspector.update_in(cx, |i, window, cx| {
            let pane = cx.new(|cx| {
                crate::notes::NotePane::with_store(
                    Arc::clone(&runtime),
                    Some(Arc::clone(&notes)),
                    false,
                    cx,
                )
            });
            cx.subscribe_in(&pane, window, |this, _, event, window, cx| {
                if matches!(event, NotePaneEvent::Dismiss) {
                    this.close_note_detail(window, cx);
                }
            })
            .detach();
            i.set_note_pane_for_test(pane);
            i.set_visible(true, cx);
            i.add_workspace(WorkspaceSurface::Notes, cx);
        });
        cx.simulate_resize(gpui::size(px(280.0), px(300.0)));
        inspector.update(cx, |i, cx| {
            i.set_notes_scope(crate::notes::panel::NotesScope::Global);
            i.notes.query_mut().insert("First");
            cx.notify();
        });
        cx.run_until_parked();
        inspector.update(cx, |i, _| {
            i.notes.scroll().set_offset(point(px(0.0), px(-120.0)));
        });
        let scroll_offset = inspector.read_with(cx, |i, _| i.notes.scroll().offset());
        inspector.update_in(cx, |i, window, cx| {
            i.open_note_detail(
                SessionId::new("s_first"),
                first_id.clone(),
                None,
                window,
                cx,
            )
        });
        inspector.read_with(cx, |i, _| {
            assert_eq!(
                i.open_note_session(),
                Some(SessionId::new("s_first")),
                "precondition: the first note is open"
            );
        });
        inspector.update_in(cx, |i, window, cx| {
            let pane = i.notes.note_pane().expect("pane");
            assert!(pane.read(cx).focus_handle(cx).is_focused(window));
        });
        // Scope changes do not replace the detail; back restores the list's
        // query and retained scroll owner without taking the main selection.
        inspector.update(cx, |i, _| {
            i.set_notes_scope(crate::notes::panel::NotesScope::Workspace)
        });
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.open_note_session(), Some(SessionId::new("s_first")))
        });
        inspector.update(cx, |i, _| {
            i.set_notes_scope(crate::notes::panel::NotesScope::Global)
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            assert!(i.notes.detail().is_none());
            assert_eq!(i.notes.query_text(), "First");
            assert_eq!(i.notes.selected_session(), Some(SessionId::new("s_first")));
            assert!(
                i.open_note_session().is_none(),
                "list selection is not an open detail"
            );
            assert_eq!(i.notes.scroll().offset(), scroll_offset);
            assert_eq!(
                runtime.store.read().expect("store").selected_session_id(),
                Some(&SessionId::new("s_term"))
            );
        });
        inspector.update_in(cx, |i, window, cx| {
            assert!(i.focus.is_focused(window));
            i.open_note_detail(
                SessionId::new("s_first"),
                first_id.clone(),
                None,
                window,
                cx,
            );
        });

        // Any other note's trash is not ours to close.
        inspector.update(cx, |i, cx| {
            i.close_note_detail_if(&second_id, &None, None, cx)
        });
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.open_note_session(), Some(SessionId::new("s_first")));
        });

        // Trash the open note's file, as the row button does, then close.
        notes.trash(&first_id).expect("trash the open file");
        inspector.update(cx, |i, cx| {
            i.close_note_detail_if(&first_id, &None, Some(&SessionId::new("s_first")), cx)
        });
        inspector.read_with(cx, |i, cx| {
            assert_eq!(i.open_note_session(), None, "the detail backs out");
            let pane = i.notes.note_pane().expect("fixture pane");
            assert!(
                pane.read(cx).editor_for_test().is_none(),
                "the editor drops the trashed note without saving it"
            );
        });
        assert!(
            notes.path_for(&first_id).is_ok_and(|path| !path.exists()),
            "the trashed file stays trashed"
        );

        // Opening the next note saves the previous one first — which is now
        // nothing, so the trashed file is not resurrected on the way.
        inspector.update_in(cx, |i, window, cx| {
            i.open_note_detail(
                SessionId::new("s_second"),
                second_id.clone(),
                None,
                window,
                cx,
            )
        });
        assert!(
            notes.path_for(&first_id).is_ok_and(|path| !path.exists()),
            "opening on does not resurrect the trashed file"
        );
        inspector.read_with(cx, |i, _| {
            assert_eq!(
                i.open_note_session(),
                Some(SessionId::new("s_second")),
                "the next note opens normally"
            );
        });
        // Archiving elsewhere in the app is still authoritative, including
        // while Notes is hidden: the detail must release its editor.
        let mut archived = runtime
            .store
            .read()
            .expect("store")
            .sessions()
            .get(&SessionId::new("s_second"))
            .expect("second note")
            .as_ref()
            .clone();
        archived.archived_at = Some(ubra_proto::DateMillis(1.0));
        runtime
            .store
            .write()
            .expect("store")
            .upsert_session(archived.clone());
        inspector.update(cx, |i, cx| i.sync_workspace_session(cx));
        inspector.read_with(cx, |i, cx| {
            assert!(i.notes.detail().is_none());
            assert!(
                i.notes
                    .note_pane()
                    .expect("pane")
                    .read(cx)
                    .editor_for_test()
                    .is_none()
            );
        });
        archived.archived_at = None;
        runtime
            .store
            .write()
            .expect("store")
            .upsert_session(archived);
        inspector.update_in(cx, |i, window, cx| {
            i.open_note_detail(
                SessionId::new("s_second"),
                second_id.clone(),
                None,
                window,
                cx,
            );
        });
        std::fs::remove_file(notes.path_for(&second_id).expect("path")).expect("external deletion");
        inspector.update(cx, |i, cx| {
            i.notes
                .note_pane()
                .expect("pane")
                .update(cx, |pane, cx| pane.reconcile(cx));
        });
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            assert!(
                i.notes.detail().is_none(),
                "external deletion returns to the list"
            );
            assert_eq!(
                runtime.store.read().expect("store").selected_session_id(),
                Some(&SessionId::new("s_term"))
            );
        });
        assert!(!notes.path_for(&second_id).expect("path").exists());
    }

    #[gpui::test]
    fn reopening_a_surface_focuses_it_without_duplicating(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (inspector, cx) =
            cx.add_window_view(move |_, cx| WorkbenchInspector::new(runtime, tokio, cx));
        cx.simulate_resize(gpui::size(px(600.0), px(500.0)));
        // Every surface is a singleton: reopening focuses instead of adding.
        for surface in WorkspaceSurface::CATALOG {
            inspector.update(cx, |inspector, cx| {
                inspector.select_workspace(surface, cx);
            });
            cx.run_until_parked();
            inspector.update(cx, |inspector, cx| {
                let id = inspector.workspace_active.unwrap();
                inspector.select_workspace(surface, cx);
                assert_eq!(
                    inspector.workspace_active,
                    Some(id),
                    "{surface:?} re-open must focus instead of duplicating"
                );
                assert_eq!(
                    inspector
                        .workspace_tabs
                        .iter()
                        .filter(|tab| tab.surface == surface)
                        .count(),
                    1,
                    "{surface:?} allows at most one surface"
                );
            });
            cx.run_until_parked();
        }
        inspector.update(cx, |inspector, _| {
            assert_eq!(
                inspector.workspace_tabs.len(),
                WorkspaceSurface::CATALOG.len(),
                "one surface per catalog entry, nothing more"
            );
        });
        inspector.update(cx, |inspector, cx| {
            // Reopening Review preserves its selected PR surface, not a Details sub-tab.
            inspector.select_workspace(WorkspaceSurface::Files, cx);
            inspector.select_tab(InspectorTab::Artifacts, cx);
            let review = inspector.workspace_active.unwrap();
            inspector.add_workspace(WorkspaceSurface::Review, cx);
            assert_eq!(inspector.workspace_active, Some(review));
            assert_eq!(inspector.selected_tab, InspectorTab::Artifacts);
            inspector.select_workspace(WorkspaceSurface::Files, cx);
            inspector.select_workspace(WorkspaceSurface::Review, cx);
            assert_eq!(inspector.workspace_active, Some(review));
            assert_eq!(inspector.selected_tab, InspectorTab::Artifacts);
        });
    }

    #[gpui::test]
    fn light_theme_reaches_the_code_tab(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|preferences| {
                preferences.terminal_theme = "github-light".to_owned();
                preferences.inspector_tab = InspectorTab::Code;
            })
            .expect("inert preferences update");
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector_runtime = Arc::clone(&runtime);
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(inspector_runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let code_viewer = harness.read_with(cx, |harness, cx| {
            harness
                .inspector
                .read_with(cx, |inspector, _| inspector.code_viewer.clone())
        });

        assert_eq!(
            code_viewer.read_with(cx, |viewer, _| viewer.appearance()),
            ubra_ui::Appearance::Light
        );
    }

    #[gpui::test]
    fn code_tab_tracks_live_light_theme_changes(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector_runtime = Arc::clone(&runtime);
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(inspector_runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        let code_viewer = inspector.read_with(cx, |inspector, _| inspector.code_viewer.clone());
        assert_eq!(
            code_viewer.read_with(cx, |viewer, _| viewer.appearance()),
            ubra_ui::Appearance::Dark
        );

        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|preferences| {
                preferences.terminal_theme = "github-light".to_owned();
            })
            .expect("inert preferences update");
        runtime.publish_local_change();
        cx.run_until_parked();

        assert_eq!(
            code_viewer.read_with(cx, |viewer, _| viewer.appearance()),
            ubra_ui::Appearance::Light
        );
    }

    #[test]
    fn light_review_rows_keep_readable_contrast() {
        for theme in ["github-light", "solarized-light", "github-light"] {
            let colors = crate::app_theme::sidebar_colors(theme);
            let inspector_surface = composite(colors.sidebar_surface(), colors.background);

            for kind in [
                DiffRowKind::Addition,
                DiffRowKind::Deletion,
                DiffRowKind::Hunk,
                DiffRowKind::File,
                DiffRowKind::Context,
                DiffRowKind::Meta,
            ] {
                let style = diff_row_style(kind, colors);
                let row_surface = composite(style.background, inspector_surface);
                let text = composite(style.foreground, row_surface);
                assert!(
                    contrast(text, row_surface) >= 4.5,
                    "{kind:?} contrast must remain readable with {theme}"
                );
            }
        }
    }

    #[test]
    fn background_git_refresh_keeps_the_last_settled_surface() {
        assert!(!should_show_blocking_git_loading(
            false,
            &LoadState::Error("not a git repository".to_owned())
        ));
        assert!(!should_show_blocking_git_loading(
            false,
            &LoadState::Ready(Arc::new(DiffSnapshot::default()))
        ));
        assert!(should_show_blocking_git_loading(
            true,
            &LoadState::Error("old project".to_owned())
        ));
    }

    #[gpui::test]
    fn passive_details_follow_context_without_starting_git_producers(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let ids: Vec<_> = fixture
            .list
            .sessions
            .iter()
            .filter(|s| !s.is_note())
            .map(|s| s.id.clone())
            .take(2)
            .collect();
        {
            let mut store = runtime.store.write().expect("store");
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        inspector.update(cx, |i, cx| {
            i.select_tab(InspectorTab::Info, cx);
            i.set_visible(true, cx);
            assert_eq!(i.context.as_ref().map(|c| &c.id), Some(&ids[0]));
            assert!(i.refresh_task.is_none() && i.review_task.is_none() && i.poll_task.is_none());
        });
        runtime.store.write().expect("store").select(ids[1].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            i.select_tab(InspectorTab::Info, cx);
            assert_eq!(i.workspace_selected, Some(WorkspaceSurface::Details));
            assert_eq!(i.context.as_ref().map(|c| &c.id), Some(&ids[1]));
            assert!(i.refresh_task.is_none() && i.review_task.is_none() && i.poll_task.is_none());
            i.select_tab(InspectorTab::Artifacts, cx);
            assert_eq!(i.workspace_selected, Some(WorkspaceSurface::Review));
            assert_eq!(i.selected_tab, InspectorTab::Artifacts);
            assert!(i.refresh_task.is_none() && i.review_task.is_none() && i.poll_task.is_none());
            i.select_tab(InspectorTab::Changes, cx);
            assert!(i.poll_task.is_some());
            i.select_tab(InspectorTab::Info, cx);
            assert!(i.refresh_task.is_none() && i.review_task.is_none() && i.poll_task.is_none());
        });
    }

    /// The commit composer shares `run_review_action` with staging, unstaging,
    /// and discarding. Only a commit that actually landed may consume the
    /// draft message; every other outcome leaves it for the user.
    #[gpui::test]
    fn commit_draft_is_cleared_only_by_a_successful_commit(cx: &mut TestAppContext) {
        fn git(root: &std::path::Path, arguments: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(arguments)
                .output()
                .expect("git command");
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let repository = tempfile::tempdir().expect("temporary repository");
        let root = repository.path();
        git(root, &["init", "--quiet"]);
        git(root, &["config", "user.name", "ubra tests"]);
        git(root, &["config", "user.email", "ubra@example.invalid"]);
        std::fs::write(root.join("one.txt"), "one\n").unwrap();
        std::fs::write(root.join("two.txt"), "two\n").unwrap();
        git(root, &["add", "one.txt", "two.txt"]);

        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        fixture.list.sessions[0].cwd = root.to_string_lossy().into_owned();
        fixture.list.sessions[0].host = None;
        let id = fixture.list.sessions[0].id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(id);
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        inspector.update(cx, |inspector, cx| inspector.set_visible(true, cx));
        cx.run_until_parked();

        inspector.update(cx, |inspector, cx| {
            inspector.commit_open = true;
            inspector.commit_query.insert("Add the first file");
            inspector.run_review_action(ReviewAction::Unstage(vec![PathBuf::from("two.txt")]), cx);
        });
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(
                inspector.review_feedback.as_ref().map(|(ok, _)| *ok),
                Some(true)
            );
            assert!(
                inspector.commit_open,
                "unstaging must not close the composer"
            );
            assert_eq!(inspector.commit_query.text(), "Add the first file");
        });

        inspector.update(cx, |inspector, cx| inspector.submit_commit(cx));
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(
                inspector.review_feedback.as_ref().map(|(ok, _)| *ok),
                Some(true)
            );
            assert!(!inspector.commit_open);
            assert!(inspector.commit_query.is_empty());
        });

        // Nothing is staged any more, so this commit fails and keeps its draft.
        inspector.update(cx, |inspector, cx| {
            inspector.commit_open = true;
            inspector.commit_query.insert("Add the second file");
            inspector.submit_commit(cx);
        });
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(
                inspector.review_feedback.as_ref().map(|(ok, _)| *ok),
                Some(false)
            );
            assert!(inspector.commit_open);
            assert_eq!(inspector.commit_query.text(), "Add the second file");
        });

        // Cancel any leftover refresh/review tasks on this thread before the
        // TestAppContext tears the entity down.
        inspector.update(cx, |inspector, _| {
            inspector.refresh_task = None;
            inspector.review_task = None;
            inspector.transcript_task = None;
            inspector.poll_task = None;
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn status_bar_reported_context_requires_loaded_conversation_identity(cx: &mut TestAppContext) {
        let home = tempfile::tempdir().expect("transcript home");
        let agent_id = "22222222-2222-4222-8222-222222222222";
        let path = home
            .path()
            .join(".codex/sessions/2026/08/13")
            .join(format!("rollout-2026-08-13T12-00-00-{agent_id}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).expect("transcript directory");
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.selected_session_id.clone().unwrap();
        let record = fixture
            .list
            .sessions
            .iter_mut()
            .find(|record| record.id == selected)
            .unwrap();
        record.kind = ProtoAgentKind::CODEX;
        record.foreground_agent = None;
        record.host = None;
        record.agent_session_id = Some(agent_id.into());
        record.transcript_path = Some(path.to_string_lossy().into_owned());
        let record = record.clone();
        std::fs::write(&path, format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{agent_id}\",\"cwd\":{}}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"total_tokens\":72000}},\"model_context_window\":100000}}}}}}\n",
            serde_json::to_string(&record.cwd).unwrap()
        )).expect("transcript");
        let runtime = Arc::new(StoreRuntime::inert());
        {
            let mut store = runtime.store.write().expect("store");
            store.hydrate(fixture.list);
            store.select(selected);
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
        inspector.update(cx, |inspector, cx| {
            inspector.transcript_home = home.path().to_path_buf();
            inspector.visible = true;
            let context = inspector.selected_context().unwrap();
            inspector.context = Some(context.clone());
            assert_eq!(inspector.reported_context_usage(&record), None);
            inspector.refresh_transcript(&context, false, cx);
            assert_eq!(
                inspector.reported_context_usage(&record),
                None,
                "loading is unknown"
            );
        });
        cx.run_until_parked();
        inspector.update(cx, |inspector, cx| {
            assert_eq!(
                inspector.reported_context_usage(&record),
                Some(ContextUsage {
                    tokens: 72000,
                    window: 100000
                })
            );
            for mutate in [
                |r: &mut SessionRecord| r.agent_session_id = Some("other".into()),
                |r: &mut SessionRecord| r.transcript_path = Some("/other".into()),
                |r: &mut SessionRecord| r.cwd = "/other".into(),
                |r: &mut SessionRecord| r.host = Some("remote".into()),
                |r: &mut SessionRecord| r.kind = ProtoAgentKind::CLAUDE_CODE,
            ] {
                let mut other = record.clone();
                mutate(&mut other);
                assert_eq!(inspector.reported_context_usage(&other), None);
            }
            let context = inspector.context.clone();
            inspector.context.as_mut().unwrap().agent_session_id = Some("new conversation".into());
            assert_eq!(
                inspector.reported_context_usage(&record),
                None,
                "retained Ready snapshot is not a new conversation"
            );
            inspector.context = context;
            inspector.set_visible(false, cx);
            assert_eq!(inspector.reported_context_usage(&record), None);
            assert!(inspector.transcript_context.is_none());
        });
    }

    #[test]
    fn artifact_titles_extract_the_useful_destination() {
        let pull_request = SessionArtifact {
            kind: ArtifactKind::PullRequest,
            url: "https://github.com/acme/ubra/pull/42".to_owned(),
            first_seen_at: DateMillis(0.0),
        };
        let issue = SessionArtifact {
            kind: ArtifactKind::LinearIssue,
            url: "https://linear.app/acme/issue/DIR-19/polish-inspector".to_owned(),
            first_seen_at: DateMillis(0.0),
        };
        let preview = SessionArtifact {
            kind: ArtifactKind::Preview,
            url: "https://feature-ubra.vercel.app/build".to_owned(),
            first_seen_at: DateMillis(0.0),
        };

        assert_eq!(artifact_title(&pull_request), "PR #42");
        assert_eq!(artifact_title(&issue), "DIR-19");
        assert_eq!(artifact_title(&preview), "feature-ubra.vercel.app");
    }

    #[test]
    fn merge_gate_waits_for_checks_and_review_blockers() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Artifacts);
        let pull_request = fixture.list.sessions[0].pull_requests.as_ref().unwrap()[0].clone();
        assert!(!pull_request_can_merge(&pull_request));
        assert_eq!(
            merge_blocker_label(&pull_request),
            "Checks are still running"
        );

        let mut ready = pull_request;
        ready.checks_pending = 0;
        ready.checks_passed = 3;
        for check in ready.checks.as_mut().unwrap() {
            check.result = "pass".to_owned();
        }
        assert!(pull_request_can_merge(&ready));
    }

    #[gpui::test]
    fn tabs_fit_and_switch_at_the_minimum_inspector_width(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Artifacts);
        let selected = fixture
            .selected_session_id
            .clone()
            .expect("selected fixture session");
        let session = fixture
            .list
            .sessions
            .iter_mut()
            .find(|session| session.id == selected)
            .expect("selected session exists");
        let pull_request = session
            .pull_requests
            .as_ref()
            .and_then(|pull_requests| pull_requests.first())
            .expect("fixture pull request")
            .clone();
        let expected_body =
            MarkdownDocument::parse(pull_request.body.as_deref().expect("fixture Markdown"))
                .plain_text();
        let expected_quote = Quote {
            source: QuoteSource::Markdown {
                session_id: selected.clone(),
                document: format!("PR #{}", pull_request.number),
                turn: 0,
            },
            content: expected_body.clone(),
        };
        let expected_evidence = ReviewEvidence::PullRequest {
            url: pull_request.url.clone(),
            title: pull_request.title.clone().expect("fixture PR title"),
            body: Some(expected_body),
            base: pull_request.base_ref_name.clone(),
            head: pull_request.head_ref_name.clone(),
        };
        session.artifacts = Some(vec![SessionArtifact {
            kind: ArtifactKind::Preview,
            url: "https://preview.example.com".to_owned(),
            first_seen_at: DateMillis(0.0),
        }]);
        session.listening_ports = Some(vec![ubra_proto::PortInfo {
            port: 3000,
            process_name: "node".to_owned(),
        }]);
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            store.select(selected);
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector_runtime = Arc::clone(&runtime);
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| {
                let mut inspector = WorkbenchInspector::new(inspector_runtime, tokio, cx);
                inspector.state = LoadState::Ready(Arc::new(DiffSnapshot {
                    files: 88,
                    additions: 556,
                    deletions: 19,
                    ..DiffSnapshot::default()
                }));
                inspector
            });
            InspectorHarness { inspector }
        });
        cx.simulate_resize(gpui::size(px(300.0), px(900.0)));
        cx.run_until_parked();

        // Sub-tab buttons are removed by design; the rail drives review_tab.
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Changes, cx)
        });
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.workspace_selected),
            Some(WorkspaceSurface::Review)
        );
        cx.run_until_parked();

        let working = cx
            .debug_bounds("INSPECTOR_LAYER_WORKING")
            .expect("working-tree layer");
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.diff_layer),
            DiffLayer::Branch
        );
        cx.simulate_click(working.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.diff_layer),
            DiffLayer::Working
        );

        // The rail is the only Artifacts entry point: select it the way the
        // Artifacts icon tab does, through the same select_tab path.
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Artifacts, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.selected_tab),
            InspectorTab::Artifacts
        );
        assert_eq!(
            runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .preferences()
                .inspector_tab,
            InspectorTab::Artifacts
        );

        cx.run_until_parked();
        assert!(cx.debug_bounds("INSPECTOR_PR_MERGE").is_some());
        assert!(cx.debug_bounds("INSPECTOR_PR_CHECK_0").is_none());
        assert!(cx.debug_bounds("INSPECTOR_PR_COMMENT_0").is_some());
        let markdown = cx
            .debug_bounds("INSPECTOR_PR_BODY")
            .expect("pull request Markdown body");
        // The full Markdown card can extend below the scroll viewport. Click
        // its visible top padding, not its center or a nested outbound link.
        cx.simulate_click(
            point(markdown.left() + px(16.0), markdown.top() + px(8.0)),
            Modifiers::none(),
        );
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(
                inspector.selected_turn_key(),
                Some(format!("pr:{}:body", pull_request.url).as_str()),
                "the Markdown card must own the selection"
            );
            assert_eq!(
                inspector.quote_selection(),
                Some(expected_quote),
                "Review's Artifacts tab must expose the complete PR body with its provenance"
            );
        });
        let ask = cx.debug_bounds("INSPECTOR_PR_ASK").expect("PR ask action");
        cx.simulate_click(ask.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("INSPECTOR_ASK_COMPOSER").is_some());
        assert!(cx.debug_bounds("INSPECTOR_ASK_SEND").is_some());
        inspector.read_with(cx, |inspector, _| {
            let draft = inspector.ask_draft.as_ref().expect("PR review draft");
            assert_eq!(
                draft.evidence,
                vec![expected_evidence],
                "Ask must carry the clicked PR's body, URL, title, and comparison refs"
            );
        });
    }

    /// The file-count notice is the only way to see what the bounded preview
    /// left out, so it opens in place into the omitted names — as rows of the
    /// same virtualized list, never one element per omitted file.
    #[gpui::test]
    fn omitted_untracked_notice_expands_into_a_virtualized_name_list(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.list.sessions[0].id.clone();
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            store.select(selected);
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Changes, cx)
        });
        cx.run_until_parked();
        // The fixture session has no checkout; stand in for a loaded snapshot.
        inspector.update(cx, |inspector, cx| {
            inspector.state = LoadState::Ready(Arc::new(omitted_untracked_preview()));
            cx.notify();
        });
        cx.run_until_parked();

        let notice = cx
            .debug_bounds("INSPECTOR_OMITTED_UNTRACKED_NOTICE")
            .expect("omission notice");
        assert!(
            cx.debug_bounds("INSPECTOR_OMITTED_UNTRACKED_PATH_0")
                .is_none()
        );
        cx.simulate_click(notice.center(), Modifiers::none());
        cx.run_until_parked();

        assert!(inspector.read_with(cx, |inspector, _| inspector.omitted_untracked_open));
        let first = cx
            .debug_bounds("INSPECTOR_OMITTED_UNTRACKED_PATH_0")
            .expect("first omitted name");
        assert!(first.top() >= notice.bottom());
        assert!(
            cx.debug_bounds("INSPECTOR_OMITTED_UNTRACKED_PATH_4999")
                .is_none(),
            "names outside the viewport must not be built"
        );

        let notice = cx
            .debug_bounds("INSPECTOR_OMITTED_UNTRACKED_NOTICE")
            .expect("omission notice");
        cx.simulate_click(notice.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("INSPECTOR_OMITTED_UNTRACKED_PATH_0")
                .is_none()
        );
    }

    #[test]
    fn ordinary_remote_git_absence_is_rendered_as_compatibility_state() {
        assert!(git_is_not_a_repository(
            "internal: fatal: not a git repository (or any parent)"
        ));
        assert!(git_is_not_installed(
            "internal: git is not installed on this host"
        ));
        assert!(!git_is_not_a_repository("ssh connection timed out"));
    }
}
