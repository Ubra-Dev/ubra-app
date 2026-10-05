//! The inspector's Notes surface: one project workspace's notes or the
//! global notes, listed, filtered, and managed from the narrow right panel.
//!
//! The panel is presentational: it reads the shared [`TodosModel`] index and
//! the session store, and every action travels to the window as an
//! [`InspectorEvent`] so spawns and session state stay with the window that
//! owns them. The panel never writes a note file itself.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, Entity, FocusHandle, FontWeight, ScrollHandle, SharedString, div,
    prelude::*, px,
};
use ubra_proto::{AgentKind, ProjectId, SessionId};
use ubra_ui::{AgentLogo, GlassMenuRow, Radius, SemanticColors, Typo};

use crate::icons::sf_symbol;
use crate::inspector::{InspectorEvent, WorkbenchInspector};
use crate::notes::NotePane;
use crate::notes::search::{NoteEntry, NoteHit, NotesSearch};
use crate::notes::todos::TodosModel;
use crate::query_editor::QueryEditor;
use crate::store::{SessionStore, StoreRuntime};

/// Which notes the panel lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotesScope {
    Workspace,
    Global,
}

/// The Notes surface's open note, if any. While set, the surface shows the
/// detail page (a back header over the editor) instead of the list; the
/// scope, filter, and list state underneath are untouched, so backing out
/// restores them exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoteDetail {
    /// Waiting on the Engine: a fresh spawn, whose file id is unknown until
    /// the Engine answers, or an orphan adoption, matched by file id. The
    /// window's store sync resolves it into [`NoteDetail::Open`].
    Pending(PendingNote),
    Open {
        session: SessionId,
        note_id: String,
    },
}

/// A note the Engine has not handed a Session yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingNote {
    /// The file id, when opening an existing file; `None` for a fresh note.
    pub note_id: Option<String>,
    /// The scope asked for. The resolved record is authoritative: a fresh
    /// note lands wherever the Engine puts it.
    pub workspace: Option<ProjectId>,
    /// The exact spawn request receipt; `None` for adoption of an existing
    /// file, which resolves by scoped file id instead.
    pub receipt: Option<u64>,
    /// A caret reveal waiting for the note to open (palette/To-dos jumps).
    pub block: Option<usize>,
}

/// Panel state, owned by the inspector. The scope starts unset and follows
/// the selection (the workspace when a session is selected, else global)
/// until the person picks a segment.
pub(crate) struct NotesPanel {
    scope: Option<NotesScope>,
    query: QueryEditor,
    search: NotesSearch,
    filter_focused: bool,
    model: Entity<TodosModel>,
    detail: Option<NoteDetail>,
    /// The list's last opened row, independent of the current detail and
    /// main-pane selection; Back restores its highlight.
    selected: Option<SessionId>,
    note_pane: Option<Entity<NotePane>>,
    scroll: ScrollHandle,
}

impl NotesPanel {
    pub(crate) fn new(runtime: &Arc<StoreRuntime>, cx: &mut Context<WorkbenchInspector>) -> Self {
        let model = TodosModel::global(runtime, cx);
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            scope: None,
            query: QueryEditor::default(),
            search: NotesSearch::default(),
            filter_focused: false,
            model,
            detail: None,
            selected: None,
            note_pane: None,
            scroll: ScrollHandle::new(),
        }
    }

    pub(crate) fn set_scope(&mut self, scope: NotesScope) {
        self.scope = Some(scope);
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> Option<NotesScope> {
        self.scope
    }

    #[cfg(test)]
    pub(crate) fn scroll(&self) -> &ScrollHandle {
        &self.scroll
    }

    #[cfg(test)]
    pub(crate) fn query_text(&self) -> &str {
        self.query.text()
    }

    pub(crate) fn focus_filter(&mut self) {
        self.filter_focused = true;
    }

    pub(crate) fn blur_filter(&mut self) {
        self.filter_focused = false;
    }

    pub(crate) fn filter_focused(&self) -> bool {
        self.filter_focused
    }

    pub(crate) fn query_mut(&mut self) -> &mut QueryEditor {
        &mut self.query
    }

    pub(crate) fn sync_model(&self, cx: &mut Context<WorkbenchInspector>) {
        self.model.update(cx, |model, cx| model.sync(cx));
    }

    pub(crate) fn entries(&self, cx: &Context<WorkbenchInspector>) -> Arc<Vec<NoteEntry>> {
        self.model.read(cx).notes()
    }

    pub(crate) fn detail(&self) -> Option<&NoteDetail> {
        self.detail.as_ref()
    }

    pub(crate) fn set_detail(&mut self, detail: Option<NoteDetail>) {
        if let Some(NoteDetail::Open { session, .. }) = &detail {
            self.selected = Some(session.clone());
        }
        self.detail = detail;
    }

    /// The open detail's Session, if the detail is resolved. The list
    /// highlights and prefers it, so re-clicking the open note stays put.
    pub(crate) fn detail_session(&self) -> Option<SessionId> {
        match &self.detail {
            Some(NoteDetail::Open { session, .. }) => Some(session.clone()),
            _ => None,
        }
    }

    pub(crate) fn selected_session(&self) -> Option<SessionId> {
        self.selected.clone()
    }

    pub(crate) fn note_pane(&self) -> Option<Entity<NotePane>> {
        self.note_pane.clone()
    }

    pub(crate) fn set_note_pane(&mut self, pane: Entity<NotePane>) {
        self.note_pane = Some(pane);
    }
}

/// The scope in effect: the person's choice, else the workspace when a
/// session (and so a project) is selected, else global.
pub(crate) fn effective_scope(
    scope: Option<NotesScope>,
    project: Option<&ProjectId>,
) -> NotesScope {
    scope.unwrap_or(if project.is_some() {
        NotesScope::Workspace
    } else {
        NotesScope::Global
    })
}

/// One note file's owning Session: every Session whose (workspace, file id)
/// matches claims it, but a file can be double-claimed (an orphan adopted
/// while its first Session was still around, or an archived record beside
/// its live twin). The row's highlight, its buttons, and the click that
/// opens it must all agree on one home, so they share this ordering
/// instead of re-scanning the map (whose iteration order is arbitrary):
///
/// 1. the currently selected Session, when it claims the file and is live
///    (re-clicking the open note is a no-op, never a jump to its twin);
/// 2. a live Session over an archived one;
/// 3. the smallest SessionId, so the winner never depends on map order.
pub(crate) fn note_home(
    store: &SessionStore,
    note_id: &str,
    workspace: &Option<ProjectId>,
    prefer: Option<&SessionId>,
) -> Option<(SessionId, bool)> {
    let mut best: Option<(SessionId, bool)> = None;
    for record in store.sessions().values() {
        if !record.is_note()
            || record.note_workspace != *workspace
            || record.note_id.as_deref() != Some(note_id)
        {
            continue;
        }
        let archived = record.is_archived();
        if home_is_better(&record.id, archived, &best, prefer) {
            best = Some((record.id.clone(), archived));
        }
    }
    best
}

/// Orders two claimants of one note file: a live selected Session first,
/// then live over archived, then the smallest id. Map iteration order
/// never participates.
fn home_is_better(
    id: &SessionId,
    archived: bool,
    best: &Option<(SessionId, bool)>,
    prefer: Option<&SessionId>,
) -> bool {
    let Some((best_id, best_archived)) = best else {
        return true;
    };
    // A live preferred claimant beats everything; otherwise live beats
    // archived; otherwise the smallest id wins.
    match (
        prefer == Some(id) && !archived,
        prefer == Some(best_id) && !best_archived,
    ) {
        (true, false) => true,
        (false, true) => false,
        _ => match (archived, best_archived) {
            (false, true) => true,
            (true, false) => false,
            _ => id.0 < best_id.0,
        },
    }
}

/// (Scope, note id) → the Session the row opens, highlights, and manages.
/// Each key's winner follows the [`note_home`] ordering (the `prefer`
/// Session is the window's selection), so the highlight and its buttons
/// always name the Session a click would open.
pub(crate) fn homes(
    store: &SessionStore,
    prefer: Option<&SessionId>,
) -> HashMap<(Option<ProjectId>, String), (SessionId, bool)> {
    let mut map: HashMap<(Option<ProjectId>, String), (SessionId, bool)> = HashMap::new();
    for record in store.sessions().values() {
        if !record.is_note() {
            continue;
        }
        let Some(note_id) = record.note_id.clone() else {
            continue;
        };
        let archived = record.is_archived();
        let candidate = (record.id.clone(), archived);
        match map.entry((record.note_workspace.clone(), note_id)) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if home_is_better(&candidate.0, candidate.1, &Some(slot.get().clone()), prefer) {
                    slot.insert(candidate);
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(candidate);
            }
        }
    }
    map
}

/// The index hits in one scope, ranked for `query` (an empty query lists
/// every note by last edit).
pub(crate) fn visible(
    entries: &[NoteEntry],
    scope: NotesScope,
    project: Option<&ProjectId>,
    query: &str,
    search: &mut NotesSearch,
) -> Vec<NoteHit> {
    search
        .rank(entries, query, usize::MAX)
        .into_iter()
        .filter(|hit| {
            let entry = &entries[hit.entry];
            match scope {
                NotesScope::Global => entry.workspace.is_none(),
                NotesScope::Workspace => {
                    project.is_some_and(|id| entry.workspace.as_ref() == Some(id))
                }
            }
        })
        .collect()
}

/// The panel body: header, scope segments, filter, and the note list. The
/// inspector syncs the model and reads the session store; this only draws.
/// `selected` is the list's retained last-opened Session, independent of
/// the detail and never the main-pane selection.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_panel(
    panel: &mut NotesPanel,
    store: &SessionStore,
    entries: &[NoteEntry],
    project: Option<ProjectId>,
    project_name: Option<String>,
    selected: Option<&SessionId>,
    colors: SemanticColors,
    focus: &FocusHandle,
    cx: &mut Context<WorkbenchInspector>,
) -> AnyElement {
    let scope = effective_scope(panel.scope, project.as_ref());
    let hubs = homes(store, selected);
    let pinned = &store.preferences().sidebar_pinned_sessions;
    let hits = visible(
        entries,
        scope,
        project.as_ref(),
        panel.query.text(),
        &mut panel.search,
    );
    let context = match scope {
        NotesScope::Workspace => project_name.clone().unwrap_or_else(|| {
            project
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "No project selected".to_owned())
        }),
        NotesScope::Global => "Every project".to_owned(),
    };
    let new_workspace = match scope {
        NotesScope::Workspace => project.clone(),
        NotesScope::Global => None,
    };
    let can_create = scope == NotesScope::Global || project.is_some();

    let mut list = div()
        .id("notes-list")
        .min_h(px(0.0))
        .flex_1()
        .overflow_y_scroll()
        .track_scroll(&panel.scroll)
        .flex()
        .flex_col()
        .px(px(4.0))
        .pb(px(8.0));
    if hits.is_empty() {
        list = list.child(empty_state(
            scope,
            project.is_some(),
            panel.query.text(),
            colors,
        ));
    } else {
        for hit in &hits {
            let entry = &entries[hit.entry];
            let home = hubs.get(&(entry.workspace.clone(), entry.id.clone()));
            let is_pinned = home.is_some_and(|(session, _)| pinned.contains(session));
            let is_selected = home.is_some_and(|(session, _)| Some(session) == selected);
            list = list.child(note_row(
                entry,
                hit,
                home.cloned(),
                is_pinned,
                is_selected,
                scope,
                colors,
                cx,
            ));
        }
    }

    div()
        .id("notes-panel")
        .debug_selector(|| "NOTES_PANEL".to_owned())
        .size_full()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(8.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(4.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(Typo::ROW.size))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.primary)
                                .child("Notes"),
                        )
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.tertiary)
                                .truncate()
                                .child(context),
                        ),
                )
                .child(new_note_button(new_workspace, can_create, colors, cx)),
        )
        .child(segments(scope, colors, cx))
        .child(filter_field(panel, colors, focus, cx))
        .child(list)
        .into_any_element()
}

/// The detail page: a back header (title, scope line, the row's
/// pin/archive/trash buttons) over the editor, or an "Opening…" placeholder
/// while the Engine has not answered yet. The list underneath keeps its
/// scope, filter, and scroll, so backing out restores it exactly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_detail(
    entry: Option<&NoteEntry>,
    home: Option<(SessionId, bool)>,
    pinned: bool,
    title: String,
    meta: String,
    pending: bool,
    pane: Option<Entity<NotePane>>,
    colors: SemanticColors,
    cx: &mut Context<WorkbenchInspector>,
) -> AnyElement {
    let body: AnyElement = match pane {
        Some(pane) => div()
            .min_h(px(0.0))
            .flex_1()
            .overflow_hidden()
            .rounded(px(Radius::ROW))
            .border_1()
            .border_color(colors.primary.alpha(0.075))
            .child(pane)
            .into_any_element(),
        None => div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .py(px(32.0))
            .child(sf_symbol("note.text", 22.0, colors.tertiary))
            .child(
                div()
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.secondary)
                    .child("Opening…"),
            )
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child("The Engine is preparing this note"),
            )
            .into_any_element(),
    };
    let actions = entry.map(|entry| row_actions(entry, home, pinned, colors, cx));
    div()
        .id("notes-detail")
        .debug_selector(|| "NOTES_DETAIL".to_owned())
        .size_full()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(8.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(4.0))
                .child(back_button(colors, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(Typo::ROW.size))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.primary)
                                .truncate()
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.tertiary)
                                .truncate()
                                .child(meta),
                        ),
                )
                .when_some(actions, |header, actions| {
                    header
                        .child(actions)
                        .when(pending, |header| header.opacity(0.4))
                }),
        )
        .child(body)
        .into_any_element()
}

fn back_button(colors: SemanticColors, cx: &mut Context<WorkbenchInspector>) -> AnyElement {
    div()
        .id("notes-back")
        .debug_selector(|| "NOTES_BACK".to_owned())
        .size(px(26.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::BADGE))
        .cursor_pointer()
        .hover(move |style| style.bg(colors.primary.alpha(0.07)))
        .child(sf_symbol("chevron.left", 13.0, colors.secondary))
        .on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.close_note_detail(window, cx);
        }))
        .into_any_element()
}

fn segments(
    scope: NotesScope,
    colors: SemanticColors,
    cx: &mut Context<WorkbenchInspector>,
) -> AnyElement {
    let cell = |candidate: NotesScope, label: &'static str| {
        let selected = scope == candidate;
        div()
            .id(SharedString::from(format!("notes-scope-{label}")))
            .debug_selector(move || format!("NOTES_SCOPE_{label}"))
            .h_full()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radius::ROW))
            .text_size(px(Typo::META.size + 1.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(if selected {
                colors.primary
            } else {
                colors.secondary
            })
            .bg(colors.primary.alpha(if selected { 0.12 } else { 0.0 }))
            .cursor_pointer()
            .when(!selected, |cell| {
                cell.hover(move |style| style.bg(colors.primary.alpha(0.05)))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.set_notes_scope(candidate);
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    };
    div()
        .id("notes-segments")
        .h(px(26.0))
        .p(px(2.0))
        .rounded(px(Radius::ROW))
        .bg(colors.primary.alpha(0.05))
        .flex()
        .gap(px(2.0))
        .child(cell(NotesScope::Workspace, "Project"))
        .child(cell(NotesScope::Global, "Global"))
        .into_any_element()
}

fn filter_field(
    panel: &NotesPanel,
    colors: SemanticColors,
    focus: &FocusHandle,
    cx: &mut Context<WorkbenchInspector>,
) -> AnyElement {
    let focus = focus.clone();
    let empty = panel.query.text().trim().is_empty();
    div()
        .id("notes-filter")
        .debug_selector(|| "NOTES_FILTER".to_owned())
        .h(px(30.0))
        .px(px(9.0))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(7.0))
        .rounded(px(Radius::BADGE))
        .bg(colors.primary.alpha(0.045))
        .border_1()
        .border_color(colors.primary.alpha(0.075))
        .text_size(px(Typo::META.size + 1.0))
        .text_color(colors.primary)
        .cursor_text()
        .on_click(cx.listener(move |this, _, window, cx| {
            window.focus(&focus, cx);
            this.focus_notes_filter();
            cx.stop_propagation();
        }))
        .child(sf_symbol("magnifyingglass", 11.0, colors.tertiary))
        .child(div().flex_1().min_w(px(0.0)).truncate().child(if empty {
            div()
                .text_color(colors.tertiary)
                .child("Filter notes…")
                .into_any_element()
        } else {
            crate::navigation::query_label(&panel.query)
        }))
        .into_any_element()
}

fn new_note_button(
    workspace: Option<ProjectId>,
    can_create: bool,
    colors: SemanticColors,
    cx: &mut Context<WorkbenchInspector>,
) -> AnyElement {
    let button = div()
        .id("notes-new")
        .debug_selector(|| "NOTES_NEW".to_owned())
        .size(px(26.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::BADGE));
    if !can_create {
        return button
            .opacity(0.35)
            .child(sf_symbol("plus", 13.0, colors.tertiary))
            .into_any_element();
    }
    button
        .cursor_pointer()
        .hover(move |style| style.bg(colors.primary.alpha(0.07)))
        .child(sf_symbol("plus", 13.0, colors.secondary))
        .on_click(cx.listener(move |_, _, _, cx| {
            cx.stop_propagation();
            cx.emit(InspectorEvent::NewNote {
                workspace: workspace.clone(),
            });
        }))
        .into_any_element()
}

fn empty_state(
    scope: NotesScope,
    has_project: bool,
    query: &str,
    colors: SemanticColors,
) -> AnyElement {
    let (title, hint) = if !query.trim().is_empty() {
        ("No matches", "Try a different filter")
    } else {
        match (scope, has_project) {
            (NotesScope::Workspace, false) => (
                "No project selected",
                "Select a session to see its project notes",
            ),
            (NotesScope::Workspace, true) => ("No notes yet", "Create one with + above"),
            (NotesScope::Global, _) => ("No notes yet", "Create one with + above"),
        }
    };
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .py(px(32.0))
        .child(sf_symbol("note.text", 22.0, colors.tertiary))
        .child(
            div()
                .text_size(px(Typo::ROW.size))
                .text_color(colors.secondary)
                .child(title),
        )
        .child(
            div()
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(hint),
        )
        .into_any_element()
}

/// A row's stable identity: the scope and the file id. The same file id
/// can live in two scopes, so the id alone is not the key.
fn row_key(entry: &NoteEntry) -> String {
    match &entry.workspace {
        Some(id) => format!("{}:{}", id.0, entry.id),
        None => format!("global:{}", entry.id),
    }
}

#[allow(clippy::too_many_arguments)]
fn note_row(
    entry: &NoteEntry,
    hit: &NoteHit,
    home: Option<(SessionId, bool)>,
    pinned: bool,
    selected: bool,
    scope: NotesScope,
    colors: SemanticColors,
    cx: &mut Context<WorkbenchInspector>,
) -> AnyElement {
    let archived = home
        .as_ref()
        .map(|(_, archived)| *archived)
        .unwrap_or(false);
    let mut meta: Vec<String> = Vec::new();
    if archived {
        meta.push("Archived".to_owned());
    }
    if pinned {
        meta.push("Pinned".to_owned());
    }
    if scope == NotesScope::Global
        && let Some(root) = entry.project.as_deref()
        && let Some(name) = std::path::Path::new(root).file_name()
    {
        meta.push(name.to_string_lossy().into_owned());
    }
    if entry.open_todos > 0 {
        meta.push(format!(
            "{} to-do{}",
            entry.open_todos,
            if entry.open_todos == 1 { "" } else { "s" }
        ));
    }
    meta.push(crate::navigation::relative_time(entry.modified_ms as f64));
    let (snippet, ranges) = match &hit.snippet {
        Some(snippet) => (snippet.text.clone(), snippet.ranges.clone()),
        None => (
            entry
                .lines
                .first()
                .map(|line| line.text.clone())
                .unwrap_or_else(|| "Empty note".to_owned()),
            Vec::new(),
        ),
    };
    let note_kind = crate::session_presentation::ui_agent_kind(&AgentKind::NOTE);
    let open = InspectorEvent::OpenNote {
        note_id: entry.id.clone(),
        workspace: entry.workspace.clone(),
    };
    let key = row_key(entry);

    div()
        .id(SharedString::from(format!("notes-row-{key}")))
        .debug_selector({
            let key = key.clone();
            move || format!("NOTES_ROW_{key}")
        })
        .h(px(54.0))
        .flex_none()
        .py(px(2.0))
        .cursor_pointer()
        .active(move |style| style.opacity(0.74))
        .on_click(cx.listener(move |_, _, _, cx| {
            cx.stop_propagation();
            cx.emit(open.clone());
        }))
        .child(
            div()
                .h_full()
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .glass_menu_row(colors, selected)
                .child(
                    div()
                        .flex_none()
                        .when(archived, |logo| logo.opacity(0.55))
                        .child(AgentLogo::new(note_kind, 24.0, colors).badged(false)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .min_w(px(0.0))
                                .child(
                                    div()
                                        .min_w(px(0.0))
                                        .flex_shrink(1.0)
                                        .text_size(px(Typo::ROW.size))
                                        .text_color(if archived {
                                            colors.secondary
                                        } else {
                                            colors.primary
                                        })
                                        .truncate()
                                        .child(crate::navigation::highlighted_label(
                                            entry.title.clone(),
                                            &hit.title_ranges,
                                        )),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(Typo::META.size))
                                        .text_color(colors.tertiary)
                                        .child(meta.join(" · ")),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(Typo::META.size + 1.0))
                                .text_color(colors.tertiary)
                                .when(archived, |line| line.opacity(0.7))
                                .truncate()
                                .child(crate::navigation::highlighted_label(snippet, &ranges)),
                        ),
                )
                .child(row_actions(entry, home, pinned, colors, cx)),
        )
        .into_any_element()
}

/// A row's management buttons: pin and archive need a Session, so orphans
/// (files no Session claims) only offer trash. Opening an orphan adopts it.
fn row_actions(
    entry: &NoteEntry,
    home: Option<(SessionId, bool)>,
    pinned: bool,
    colors: SemanticColors,
    cx: &mut Context<WorkbenchInspector>,
) -> AnyElement {
    let key = row_key(entry);
    let trash_session = home.as_ref().map(|(session, _)| session.clone());
    let mut actions = div().flex_none().flex().items_center().gap(px(2.0));
    match home {
        Some((session, false)) => {
            let pin = session.clone();
            actions = actions
                .child(row_button(
                    &format!("notes-pin-{key}"),
                    if pinned { "pin.fill" } else { "pin" },
                    colors.secondary,
                    InspectorEvent::PinNote { session: pin },
                    colors,
                    cx,
                ))
                .child(row_button(
                    &format!("notes-archive-{key}"),
                    "archivebox",
                    colors.secondary,
                    InspectorEvent::ArchiveNote { session },
                    colors,
                    cx,
                ));
        }
        Some((session, true)) => {
            actions = actions.child(row_button(
                &format!("notes-unarchive-{key}"),
                "tray.and.arrow.up",
                colors.secondary,
                InspectorEvent::ReviveNote { session },
                colors,
                cx,
            ));
        }
        None => {}
    }
    actions
        .child(row_button(
            &format!("notes-trash-{key}"),
            "trash",
            colors.tertiary,
            InspectorEvent::TrashNote {
                note_id: entry.id.clone(),
                workspace: entry.workspace.clone(),
                session: trash_session,
            },
            colors,
            cx,
        ))
        .into_any_element()
}

fn row_button(
    id: &str,
    symbol: &'static str,
    tint: gpui::Rgba,
    event: InspectorEvent,
    colors: SemanticColors,
    cx: &mut Context<WorkbenchInspector>,
) -> AnyElement {
    div()
        .id(SharedString::from(id))
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::CHIP))
        .cursor_pointer()
        .hover(move |style| style.bg(colors.primary.alpha(0.08)))
        .child(sf_symbol(symbol, 12.0, tint))
        .on_click(cx.listener(move |_, _, _, cx| {
            cx.stop_propagation();
            cx.emit(event.clone());
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::search::entry_from_markdown;

    fn scoped(id: &str, title: &str, workspace: Option<&str>) -> NoteEntry {
        let mut entry = entry_from_markdown(id, &format!("# {title}\n\nBody of {title}.\n"), 1);
        entry.workspace = workspace.map(ubra_proto::ProjectId::new);
        entry
    }

    #[test]
    fn scope_follows_the_selection_until_chosen() {
        let project = ubra_proto::ProjectId::new("p_ws");
        assert_eq!(effective_scope(None, Some(&project)), NotesScope::Workspace);
        assert_eq!(effective_scope(None, None), NotesScope::Global);
        assert_eq!(
            effective_scope(Some(NotesScope::Global), Some(&project)),
            NotesScope::Global
        );
        assert_eq!(
            effective_scope(Some(NotesScope::Workspace), None),
            NotesScope::Workspace
        );
    }

    #[test]
    fn visible_lists_one_scope_ranked_for_the_query() {
        let entries = vec![
            scoped("g1", "Global roadmap", None),
            scoped("w1", "Workspace roadmap", Some("p_ws")),
            scoped("w2", "Other project notes", Some("p_other")),
        ];
        let project = ubra_proto::ProjectId::new("p_ws");
        let mut search = NotesSearch::default();

        let ids = |hits: &[NoteHit]| {
            hits.iter()
                .map(|hit| entries[hit.entry].id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&visible(
                &entries,
                NotesScope::Global,
                None,
                "",
                &mut search
            )),
            ["g1"]
        );
        assert_eq!(
            ids(&visible(
                &entries,
                NotesScope::Workspace,
                Some(&project),
                "",
                &mut search
            )),
            ["w1"]
        );
        // The query ranks within the scope; other scopes never leak in.
        assert_eq!(
            ids(&visible(
                &entries,
                NotesScope::Global,
                None,
                "roadmap",
                &mut search
            )),
            ["g1"]
        );
        assert!(
            visible(&entries, NotesScope::Workspace, None, "", &mut search).is_empty(),
            "a workspace scope without a project lists nothing"
        );
    }

    #[test]
    fn homes_are_keyed_by_scope_and_note() {
        use crate::notes::work_item_tests::record;
        use crate::store::{Prefs, SessionStore};

        let (mut store, _) = SessionStore::headless(Prefs::default());
        let mut live = record("s_live", ubra_proto::AgentKind::NOTE);
        live.note_id = Some("n1".into());
        live.note_workspace = Some(ubra_proto::ProjectId::new("p_ws"));
        let mut twin = record("s_twin", ubra_proto::AgentKind::NOTE);
        twin.note_id = Some("n1".into());
        let mut archived = record("s_old", ubra_proto::AgentKind::NOTE);
        archived.note_id = Some("n2".into());
        archived.archived_at = Some(ubra_proto::DateMillis(1.0));
        store.upsert_session(live);
        store.upsert_session(twin);
        store.upsert_session(archived);

        let hubs = homes(&store, None);
        assert_eq!(hubs.len(), 3);
        assert_eq!(
            hubs.get(&(Some(ubra_proto::ProjectId::new("p_ws")), "n1".to_owned())),
            Some(&(SessionId::new("s_live"), false))
        );
        assert_eq!(
            hubs.get(&(None, "n1".to_owned())),
            Some(&(SessionId::new("s_twin"), false))
        );
        assert_eq!(
            hubs.get(&(None, "n2".to_owned())),
            Some(&(SessionId::new("s_old"), true))
        );
    }

    #[test]
    fn double_claimed_files_resolve_to_one_stable_home() {
        use crate::notes::work_item_tests::record;
        use crate::store::{Prefs, SessionStore};

        let (mut store, _) = SessionStore::headless(Prefs::default());
        // Inserted twin-first so map order favors it; the resolver must not.
        let mut twin = record("s_dup_b", ubra_proto::AgentKind::NOTE);
        twin.note_id = Some("n1".into());
        let mut live = record("s_dup_a", ubra_proto::AgentKind::NOTE);
        live.note_id = Some("n1".into());
        store.upsert_session(twin);
        store.upsert_session(live);

        // The click path (`note_home`) and the highlight/buttons path
        // (`homes`) must name the same Session: the smallest id wins.
        let winner = Some((SessionId::new("s_dup_a"), false));
        assert_eq!(note_home(&store, "n1", &None, None), winner);
        assert_eq!(
            homes(&store, None).get(&(None, "n1".to_owned())),
            winner.as_ref()
        );
        // While the twin is selected it stays home: re-clicking the open
        // note is a no-op instead of a jump to its double.
        let sticky = Some((SessionId::new("s_dup_b"), false));
        assert_eq!(
            note_home(&store, "n1", &None, Some(&SessionId::new("s_dup_b"))),
            sticky
        );
        assert_eq!(
            homes(&store, Some(&SessionId::new("s_dup_b"))).get(&(None, "n1".to_owned())),
            sticky.as_ref()
        );
    }

    #[test]
    fn live_home_beats_an_archived_twin() {
        use crate::notes::work_item_tests::record;
        use crate::store::{Prefs, SessionStore};

        let (mut store, _) = SessionStore::headless(Prefs::default());
        let mut old = record("s_old", ubra_proto::AgentKind::NOTE);
        old.note_id = Some("n1".into());
        old.archived_at = Some(ubra_proto::DateMillis(1.0));
        let mut live = record("s_new", ubra_proto::AgentKind::NOTE);
        live.note_id = Some("n1".into());
        store.upsert_session(old);
        store.upsert_session(live);

        let winner = Some((SessionId::new("s_new"), false));
        assert_eq!(note_home(&store, "n1", &None, None), winner);
        assert_eq!(
            homes(&store, None).get(&(None, "n1".to_owned())),
            winner.as_ref()
        );
        // Even preferred, an archived twin never wins while a live home
        // exists: the click revives nothing and opens the live Session.
        assert_eq!(
            note_home(&store, "n1", &None, Some(&SessionId::new("s_old"))),
            winner
        );
    }
}
