use super::*;
use std::rc::Rc;

#[derive(Default)]
pub(super) enum Snapshot<T> {
    #[default]
    Empty,
    Loading,
    Ready(T),
    Failed(String),
}

pub(super) enum ResultPage {
    Runs(Result<ubra_proto::runs::RunListResult, String>),
    Tasks(Box<Result<ubra_proto::tasks::SessionTasksResult, String>>),
}

pub(super) fn update_snapshot<T>(
    snapshot: &mut Snapshot<T>,
    result: Result<T, String>,
) -> Option<String> {
    match result {
        Ok(value) => {
            *snapshot = Snapshot::Ready(value);
            None
        }
        Err(error) => {
            if !matches!(snapshot, Snapshot::Ready(_)) {
                *snapshot = Snapshot::Failed(error.clone());
            }
            Some(error)
        }
    }
}

/// A resource request records the intended page, independently of sidebar refreshes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PageRequest<C> {
    generation: u64,
    cursor: Option<C>,
}

pub(super) struct PageLoad<C> {
    pub cursor: Option<C>,
    pub loading: bool,
    generation: u64,
}

impl<C> Default for PageLoad<C> {
    fn default() -> Self {
        Self {
            cursor: None,
            loading: false,
            generation: 0,
        }
    }
}

impl<C: Clone + PartialEq> PageLoad<C> {
    pub fn begin(&mut self, cursor: Option<C>) -> PageRequest<C> {
        self.generation = self.generation.wrapping_add(1);
        self.cursor = cursor.clone();
        self.loading = true;
        PageRequest {
            generation: self.generation,
            cursor,
        }
    }

    /// Refresh the desired page without superseding pending navigation.
    pub fn refresh(&mut self) -> Option<PageRequest<C>> {
        if self.loading {
            None
        } else {
            Some(self.begin(self.cursor.clone()))
        }
    }

    pub fn settle(&mut self, request: &PageRequest<C>) -> bool {
        if !self.loading || self.generation != request.generation || self.cursor != request.cursor {
            return false;
        }
        self.loading = false;
        true
    }

    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.loading = false;
    }

    pub fn latest(&mut self) {
        self.cancel();
        self.cursor = None;
    }
}

pub(super) fn event_applies(
    event: &ubra_client::EventEnvelope,
    id: &SessionId,
    surface: Option<WorkspaceSurface>,
) -> bool {
    let scoped = ["session_id", "sessionID", "sessionId", "sender_id"]
        .iter()
        .any(|key| event.params.get(*key).and_then(|v| v.as_str()) == Some(id.0.as_str()));
    scoped
        && match event.name.as_str() {
            "run.updated" => surface == Some(WorkspaceSurface::Runs),
            "task.updated" | "task.current_changed" => surface == Some(WorkspaceSurface::Tasks),
            _ => false,
        }
}

pub(super) struct SidebarState {
    pub target: InspectorTarget,
    pub active: Option<SessionId>,
    pub runs_focus: FocusHandle,
    pub runs_refresh_focus: FocusHandle,
    pub tasks_focus: FocusHandle,
    pub context_focus: FocusHandle,
    pub context_clear_focus: FocusHandle,
    pub generation: u64,
    pub load: Option<Task<()>>,
    pub runs: HashMap<SessionId, runs::RunsState>,
    pub tasks: HashMap<SessionId, tasks::TasksState>,
    pub instructions: HashMap<SessionId, QueryEditor>,
    pub feedback: HashMap<SessionId, String>,
    pub preview_offsets: HashMap<(SessionId, String), usize>,
}
impl SidebarState {
    pub fn new(cx: &mut Context<WorkbenchInspector>) -> Self {
        Self {
            target: InspectorTarget::FollowActive,
            active: None,
            runs_focus: cx.focus_handle().tab_stop(true),
            runs_refresh_focus: cx.focus_handle().tab_stop(true),
            tasks_focus: cx.focus_handle().tab_stop(true),
            context_focus: cx.focus_handle().tab_stop(true),
            context_clear_focus: cx.focus_handle().tab_stop(true),
            generation: 0,
            load: None,
            runs: HashMap::new(),
            tasks: HashMap::new(),
            instructions: HashMap::new(),
            feedback: HashMap::new(),
            preview_offsets: HashMap::new(),
        }
    }

    pub(super) fn cancel_page_requests(&mut self, id: &SessionId) {
        if let Some(state) = self.runs.get_mut(id) {
            state.page_load.cancel();
        }
        if let Some(state) = self.tasks.get_mut(id) {
            state.page_load.cancel();
        }
    }
}

/// All sidebar commands share pointer, Tab, Enter/Space and visible-focus behavior.
pub(super) fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    colors: SemanticColors,
    window: &mut Window,
    cx: &mut Context<WorkbenchInspector>,
    command: impl Fn(&mut WorkbenchInspector, &mut Window, &mut Context<WorkbenchInspector>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let label = label.into();
    let focus = window
        .use_keyed_state(
            (gpui::ElementId::from("sidebar-command-focus"), id.clone()),
            cx,
            |_, cx| cx.focus_handle().tab_stop(true),
        )
        .read(cx)
        .clone();
    button_with_focus(id, label, colors, focus, cx, command)
}

pub(super) fn button_with_focus(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    colors: SemanticColors,
    focus: FocusHandle,
    cx: &mut Context<WorkbenchInspector>,
    command: impl Fn(&mut WorkbenchInspector, &mut Window, &mut Context<WorkbenchInspector>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let label = label.into();
    let command = Rc::new(command);
    let key_command = command.clone();
    div()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .track_focus(&focus)
        .px_2()
        .py_1()
        .rounded(px(Radius::CHIP))
        .text_sm()
        .border_1()
        .border_color(colors.primary.alpha(0.10))
        .text_color(colors.primary)
        .hover(move |style| style.bg(colors.primary.alpha(0.07)))
        .focus_visible(move |style| style.border_color(colors.primary))
        .child(label)
        .on_click(cx.listener(move |this, _, window, cx| {
            command(this, window, cx);
            cx.stop_propagation();
        }))
        .on_key_down(cx.listener(move |this, key: &KeyDownEvent, window, cx| {
            if matches!(key.keystroke.key.as_str(), "enter" | "space") {
                key_command(this, window, cx);
                cx.stop_propagation();
            }
        }))
}

// Eight independent view inputs; bundling them would obscure each call site.
#[allow(clippy::too_many_arguments)]
pub(super) fn field(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    text: String,
    retained_focus: Option<FocusHandle>,
    colors: SemanticColors,
    window: &mut Window,
    cx: &mut Context<WorkbenchInspector>,
    edit: impl Fn(&mut WorkbenchInspector, &KeyDownEvent, &mut Context<WorkbenchInspector>) + 'static,
) -> AnyElement {
    let id = id.into();
    let label = label.into();
    let focus = retained_focus.unwrap_or_else(|| {
        window
            .use_keyed_state(
                (gpui::ElementId::from("sidebar-field-focus"), id.clone()),
                cx,
                |_, cx| cx.focus_handle().tab_stop(true),
            )
            .read(cx)
            .clone()
    });
    let click_focus = focus.clone();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .text_sm()
                .text_color(colors.secondary)
                .child(label.clone()),
        )
        .child(
            div()
                .id(id.clone())
                .debug_selector(move || id.to_string())
                .aria_label(label)
                .track_focus(&focus)
                .min_w_0()
                .px_2()
                .py_2()
                .rounded(px(Radius::CHIP))
                .border_1()
                .border_color(colors.primary.alpha(0.15))
                .focus_visible(move |s| s.border_color(colors.primary))
                .text_sm()
                .font_family(crate::fonts::mono_family())
                .overflow_hidden()
                .child(if text.is_empty() {
                    " ".to_owned()
                } else {
                    text
                })
                .on_click(move |_, window, cx| window.focus(&click_focus, cx))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    if query_editor::edit_for(&event.keystroke).is_some() {
                        edit(this, event, cx);
                        cx.stop_propagation();
                    }
                })),
        )
        .into_any_element()
}

pub(super) fn edit_query(
    query: &mut QueryEditor,
    key: &KeyDownEvent,
    cx: &mut Context<WorkbenchInspector>,
) {
    match query_editor::edit_for(&key.keystroke) {
        Some(Edit::Local(edit)) => {
            query.apply(edit);
        }
        Some(Edit::Clipboard(ClipboardEdit::Copy)) => query_editor::copy_selection(query, cx),
        Some(Edit::Clipboard(ClipboardEdit::Cut)) => {
            query_editor::cut_selection(query, cx);
        }
        Some(Edit::Clipboard(ClipboardEdit::Paste)) => {
            if let Some(text) = cx.read_from_clipboard().and_then(|v| v.text()) {
                query.insert(&text);
            }
        }
        None => {}
    }
    cx.notify();
}

pub(super) fn panel(id: &'static str, colors: SemanticColors) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size_full()
        .min_h_0()
        .min_w_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_3()
        .p_3()
        .text_sm()
        .text_color(colors.primary)
}
pub(super) fn message(text: impl Into<SharedString>, colors: SemanticColors) -> AnyElement {
    div()
        .text_sm()
        .text_color(colors.secondary)
        .child(text.into())
        .into_any_element()
}

impl WorkbenchInspector {
    pub(crate) fn set_inspection_target(
        &mut self,
        target: InspectorTarget,
        active: Option<SessionId>,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self.notes.note_pane() {
            let store = self.runtime.store.read().expect("store");
            let recipient = target.session_id(active.clone()).and_then(|id| {
                store
                    .sessions()
                    .get(&id)
                    .filter(|s| !s.is_archived() && !s.is_note())
                    .map(|s| (s.id.clone(), s.title.clone()))
            });
            drop(store);
            pane.update(cx, |pane, cx| pane.set_attachment_recipient(recipient, cx));
        }
        if self.sidebar.target != target || self.sidebar.active != active {
            self.sidebar.target = target;
            self.sidebar.active = active;
            cx.notify();
        }
    }
    /// Badges use observed actionable state, never inferred terminal prose or cost.
    pub(crate) fn actionable_count(&self, surface: WorkspaceSurface) -> usize {
        let Some(id) = self.selected_context().map(|context| context.id) else {
            return 0;
        };
        match surface {
            WorkspaceSurface::Runs => match self.sidebar.runs.get(&id).map(|s| &s.snapshot) {
                Some(Snapshot::Ready(page)) => [
                    ubra_proto::runs::RunKind::Build,
                    ubra_proto::runs::RunKind::Test,
                    ubra_proto::runs::RunKind::Lint,
                    ubra_proto::runs::RunKind::Command,
                ]
                .iter()
                .filter(|kind| {
                    page.runs
                        .iter()
                        .find(|r| r.kind == **kind)
                        .is_some_and(|r| r.status == ubra_proto::runs::RunStatus::Failed)
                })
                .count(),
                _ => 0,
            },
            WorkspaceSurface::Tasks => match self.sidebar.tasks.get(&id).map(|s| &s.snapshot) {
                Some(Snapshot::Ready(page)) => page
                    .tasks
                    .iter()
                    .chain(page.current_task.iter())
                    .filter(|t| t.status == ubra_proto::tasks::TaskStatus::Blocked)
                    .count(),
                _ => 0,
            },
            _ => 0,
        }
    }

    pub(super) fn accepts_sidebar_response(&self, id: &SessionId, generation: u64) -> bool {
        self.sidebar.generation == generation
            && self.selected_session().as_ref().map(|s| &s.id) == Some(id)
    }

    pub(super) fn refresh_sidebar_for(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        if self.selected_session().as_ref().map(|s| &s.id) == Some(id) {
            self.refresh_sidebar(cx);
        }
    }

    pub(super) fn refresh_sidebar(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        let Some(session) = self.selected_session() else {
            return;
        };
        self.sidebar.generation = self.sidebar.generation.wrapping_add(1);
        let id = session.id;
        let surface = self.workspace_selected;
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        let mut run_request = None;
        let mut task_request = None;
        let mut run_cursor = None;
        let mut task_cursor = None;
        match surface {
            Some(WorkspaceSurface::Runs) => {
                let state = self.sidebar.runs.entry(id.clone()).or_default();
                let Some(request) = state.page_load.refresh() else {
                    self.refresh_expanded_run_output(cx);
                    return;
                };
                run_cursor = request.cursor.clone();
                run_request = Some(request);
                if !matches!(state.snapshot, Snapshot::Ready(_)) {
                    state.snapshot = Snapshot::Loading;
                }
            }
            Some(WorkspaceSurface::Tasks) => {
                self.refresh_linked_checklists(cx);
                let state = self.sidebar.tasks.entry(id.clone()).or_default();
                if state.busy {
                    return;
                }
                let Some(request) = state.page_load.refresh() else {
                    return;
                };
                task_cursor = request.cursor;
                task_request = Some(request);
                if !matches!(state.snapshot, Snapshot::Ready(_)) {
                    state.snapshot = Snapshot::Loading;
                }
            }
            // Account-level usage renders from the shared watch feed; no RPC fetch.
            Some(WorkspaceSurface::Usage) => return,
            Some(WorkspaceSurface::Context) => {
                self.refresh_context_preview(cx);
                return;
            }
            _ => return,
        }
        let load = cx.spawn(async move |this, cx| {
            let request_id = id.clone();
            let result = tokio
                .spawn(async move {
                    match surface.unwrap() {
                        WorkspaceSurface::Runs => ResultPage::Runs(
                            client
                                .run_list(ubra_proto::runs::RunListParams {
                                    session_id: request_id,
                                    limit: Some(200),
                                    cursor: run_cursor,
                                })
                                .await
                                .map_err(|e| e.to_string()),
                        ),
                        WorkspaceSurface::Tasks => ResultPage::Tasks(Box::new(
                            client
                                .session_tasks_page(ubra_proto::tasks::SessionTasksParams {
                                    session_id: request_id.0,
                                    limit: Some(50),
                                    cursor: task_cursor,
                                })
                                .await
                                .map_err(|e| e.to_string()),
                        )),
                        _ => unreachable!(),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let page = match result {
                    Ok(page) => page,
                    Err(error) => match surface.unwrap() {
                        WorkspaceSurface::Runs => ResultPage::Runs(Err(error.to_string())),
                        WorkspaceSurface::Tasks => {
                            ResultPage::Tasks(Box::new(Err(error.to_string())))
                        }
                        _ => unreachable!(),
                    },
                };
                let accepted = match page {
                    ResultPage::Runs(result) => {
                        this.selected_session().as_ref().map(|s| &s.id) == Some(&id)
                            && this
                                .sidebar
                                .runs
                                .entry(id.clone())
                                .or_default()
                                .settle_page(
                                    &id,
                                    run_request.as_ref().expect("run request"),
                                    result,
                                )
                    }
                    ResultPage::Tasks(result) => {
                        this.selected_session().as_ref().map(|s| &s.id) == Some(&id)
                            && this
                                .sidebar
                                .tasks
                                .entry(id.clone())
                                .or_default()
                                .settle_page(
                                    &id,
                                    task_request.as_ref().expect("task request"),
                                    *result,
                                )
                    }
                };
                if accepted {
                    this.refresh_expanded_run_output(cx);
                    cx.notify();
                }
            });
        });
        if matches!(
            surface,
            Some(WorkspaceSurface::Runs | WorkspaceSurface::Tasks)
        ) {
            load.detach();
        } else {
            self.sidebar.load = Some(load);
        }
        cx.notify();
    }
}
