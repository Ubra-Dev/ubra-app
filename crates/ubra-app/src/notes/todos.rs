//! To-dos: every open to-do across your notes, in one place.
//!
//! The sidebar's To-dos row and the To-dos page read one shared model. It
//! lists note Sessions (so archived notes drop out with their tab), reads each
//! note's file off the main thread from its own scope (the global notes or
//! one workspace's folder), and refreshes when a notes directory or the set
//! of note Sessions changes. Ticking a to-do here writes through
//! `NoteStore::update`, the same locked path agents use, so an open editor
//! picks it up like any other outside edit.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FontWeight, Global, MouseButton,
    MouseDownEvent, Render, SharedString, Task, Window, div, prelude::*, px,
};
use ubra_notes::handoff;
use ubra_notes::store::NoteStore;
use ubra_proto::{ProjectId, SessionId, SessionRecord};
use ubra_ui::{AgentLogo, Fill, SemanticColors};

use crate::icons::sf_symbol;
use crate::store::StoreRuntime;

const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TodoItem {
    /// Block index in the note file (not counting the title).
    pub index: usize,
    pub text: String,
    /// Sessions working on it, linked from the to-do.
    pub linked: Vec<SessionId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TodoGroup {
    pub note_session: SessionId,
    pub note_id: String,
    /// The workspace whose notes folder holds the file; `None` is global.
    pub workspace: Option<ProjectId>,
    pub title: String,
    pub project: Option<String>,
    pub items: Vec<TodoItem>,
    pub updated_at: i64,
}

/// Which note Sessions exist, cheaply compared to decide when to re-read.
fn note_signature(
    store: &crate::store::SessionStore,
) -> Vec<(SessionId, Option<ProjectId>, String)> {
    let mut notes: Vec<(SessionId, Option<ProjectId>, String)> = store
        .sessions()
        .values()
        .filter(|record| record.is_note() && !record.is_archived())
        .filter_map(|record| {
            Some((
                record.id.clone(),
                record.note_workspace.clone(),
                record.note_id.clone()?,
            ))
        })
        .collect();
    notes.sort_by(|a, b| a.0.0.cmp(&b.0.0));
    notes
}

pub(crate) struct TodosModel {
    runtime: Arc<StoreRuntime>,
    store: Option<Arc<NoteStore>>,
    /// Workspace notes stores, discovered from the global store's
    /// `workspaces/` folder. The global store stays `store`, above.
    scoped: Vec<(ProjectId, Arc<NoteStore>)>,
    groups: Vec<TodoGroup>,
    /// Every note in every scope, for "Search notes": built in the same
    /// off-thread pass as the to-dos, reusing unchanged files.
    notes: Arc<Vec<super::search::NoteEntry>>,
    /// The index has been built at least once.
    notes_ready: bool,
    signature: Vec<(SessionId, Option<ProjectId>, String)>,
    dirty: bool,
    refresh: Task<()>,
    _watch: Task<()>,
    _watcher: Option<notify::RecommendedWatcher>,
}

struct TodosGlobal(Entity<TodosModel>);

impl Global for TodosGlobal {}

impl TodosModel {
    /// The app-wide model, created on first use.
    pub(crate) fn global(runtime: &Arc<StoreRuntime>, cx: &mut App) -> Entity<Self> {
        if let Some(model) = cx.try_global::<TodosGlobal>() {
            return model.0.clone();
        }
        let runtime = Arc::clone(runtime);
        let model = cx.new(|cx| {
            // Tests never read the real notes directory or start a watcher
            // (its thread would trip the deterministic scheduler); they
            // install their own model with `install`.
            if cfg!(test) {
                return Self::with_store(runtime, None, false, cx);
            }
            let store = NoteStore::resolve_dir()
                .and_then(|dir| NoteStore::open(dir).ok())
                .map(Arc::new);
            Self::with_store(runtime, store, true, cx)
        });
        cx.set_global(TodosGlobal(model.clone()));
        model
    }

    /// Tests and the macOS window screenshots install a fixture model.
    #[cfg(test)]
    pub(crate) fn install(model: Entity<Self>, cx: &mut App) {
        cx.set_global(TodosGlobal(model));
    }

    pub(crate) fn with_store(
        runtime: Arc<StoreRuntime>,
        store: Option<Arc<NoteStore>>,
        watch: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (watcher, task) = match (&store, watch) {
            (Some(store), true) => watch_dir(store, cx),
            _ => (None, Task::ready(())),
        };
        Self {
            runtime,
            store,
            scoped: Vec::new(),
            groups: Vec::new(),
            notes: Arc::default(),
            notes_ready: false,
            signature: Vec::new(),
            dirty: true,
            refresh: Task::ready(()),
            _watch: task,
            _watcher: watcher,
        }
    }

    pub(crate) fn groups(&self) -> &[TodoGroup] {
        &self.groups
    }

    /// The notes index (live, archived and orphan files alike).
    pub(crate) fn notes(&self) -> Arc<Vec<super::search::NoteEntry>> {
        Arc::clone(&self.notes)
    }

    /// Whether the index has been read at least once, for loading states.
    pub(crate) fn notes_ready(&self) -> bool {
        self.notes_ready || self.store.is_none()
    }

    pub(crate) fn open_count(&self) -> usize {
        self.groups.iter().map(|g| g.items.len()).sum()
    }

    /// Called from renders: re-reads when the note Sessions changed or a
    /// notes directory reported a write. Reading happens off the main
    /// thread; only the cheap workspace discovery runs here.
    pub(crate) fn sync(&mut self, cx: &mut Context<Self>) {
        let signature = note_signature(&self.runtime.store.read().expect("store"));
        if signature != self.signature {
            self.signature = signature;
            self.dirty = true;
        }
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(store) = self.store.clone() else {
            return;
        };
        self.discover_scopes(&store);
        let scoped = self.scoped.clone();
        let wanted = self.signature.clone();
        let records: Vec<SessionRecord> = {
            let sessions = self.runtime.store.read().expect("store");
            wanted
                .iter()
                .filter_map(|(id, _, _)| sessions.sessions().get(id).map(|r| (**r).clone()))
                .collect()
        };
        let previous = Arc::clone(&self.notes);
        self.refresh = cx.spawn(async move |this, cx| {
            let (groups, notes) = cx
                .background_executor()
                .spawn(async move {
                    let groups = read_groups(&store, &scoped, &records);
                    let mut stores: Vec<(Option<ProjectId>, &NoteStore)> = vec![(None, &store)];
                    stores.extend(
                        scoped
                            .iter()
                            .map(|(id, store)| (Some(id.clone()), store.as_ref())),
                    );
                    let notes = super::search::build_index(&stores, &previous);
                    (groups, notes)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.notes = Arc::new(notes);
                this.notes_ready = true;
                this.groups = groups;
                cx.notify();
            });
        });
    }

    /// Opens workspace notes stores that appeared since the last sync and
    /// drops ones whose folder is gone. The watcher follows new folders;
    /// a removed folder simply stops reporting.
    fn discover_scopes(&mut self, global: &Arc<NoteStore>) {
        self.scoped.retain(|(_, store)| store.dir().is_dir());
        let Ok(ids) = global.list_workspaces() else {
            return;
        };
        for id in ids {
            if self.scoped.iter().any(|(known, _)| known.0 == id) {
                continue;
            }
            let Ok(store) = NoteStore::open_workspace(global.dir(), &id) else {
                continue;
            };
            self.watch_more(store.dir());
            self.scoped.push((ProjectId::new(id), Arc::new(store)));
        }
    }

    fn watch_more(&mut self, dir: &std::path::Path) {
        use notify::{RecursiveMode, Watcher};
        if let Some(watcher) = self._watcher.as_mut() {
            let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
        }
    }

    /// The next `sync` re-reads the folder (without starting it now).
    pub(crate) fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.sync(cx);
    }

    /// Ticks a to-do from the list, through the locked store path.
    pub(crate) fn tick(
        &mut self,
        workspace: Option<&ProjectId>,
        note_id: &str,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let store = match workspace {
            None => self.store.clone(),
            Some(id) => self
                .scoped
                .iter()
                .find(|(known, _)| known == id)
                .map(|(_, store)| store.clone()),
        };
        let Some(store) = store else {
            return;
        };
        // Optimistic: the row leaves at once; the watcher confirms.
        for group in &mut self.groups {
            if group.workspace.as_ref() == workspace && group.note_id == note_id {
                group.items.retain(|item| item.index != index);
            }
        }
        self.groups.retain(|group| !group.items.is_empty());
        cx.notify();
        let note_id = note_id.to_owned();
        cx.background_executor()
            .spawn(async move {
                let _ = store.update(&note_id, &ubra_notes::history::Author::User, |note| {
                    handoff::set_checked(note, index, true);
                    Ok(())
                });
            })
            .detach();
    }
}

fn read_groups(
    global: &NoteStore,
    scoped: &[(ProjectId, Arc<NoteStore>)],
    records: &[SessionRecord],
) -> Vec<TodoGroup> {
    let mut groups = Vec::new();
    for record in records {
        let Some(note_id) = &record.note_id else {
            continue;
        };
        let store = match &record.note_workspace {
            None => global,
            Some(id) => {
                let Some((_, store)) = scoped.iter().find(|(known, _)| known == id) else {
                    continue;
                };
                store.as_ref()
            }
        };
        let Ok(note) = store.load(note_id) else {
            continue;
        };
        let items: Vec<TodoItem> = handoff::todos(&note)
            .into_iter()
            .filter(|todo| !todo.checked && !todo.text.trim().is_empty())
            .map(|todo| TodoItem {
                index: todo.index,
                text: todo.text,
                linked: todo.sessions.into_iter().map(SessionId::new).collect(),
            })
            .collect();
        if items.is_empty() {
            continue;
        }
        let title = if note.doc.title.trim().is_empty() {
            "Untitled".to_owned()
        } else {
            note.doc.title.clone()
        };
        groups.push(TodoGroup {
            note_session: record.id.clone(),
            note_id: note_id.clone(),
            workspace: record.note_workspace.clone(),
            title,
            project: note.project().map(str::to_owned),
            items,
            updated_at: record.updated_at.0 as i64,
        });
    }
    // Most recently touched notes first, like the sidebar's recency order.
    groups.sort_by_key(|group| std::cmp::Reverse(group.updated_at));
    groups
}

fn watch_dir(
    store: &Arc<NoteStore>,
    cx: &mut Context<TodosModel>,
) -> (Option<notify::RecommendedWatcher>, Task<()>) {
    use notify::{EventKind, RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = &event
            && matches!(event.kind, EventKind::Access(_))
        {
            return;
        }
        let _ = tx.send(());
    })
    .ok()
    .and_then(|mut watcher| {
        watcher
            .watch(store.dir(), RecursiveMode::NonRecursive)
            .ok()
            .map(|()| watcher)
    });
    let task = cx.spawn(async move |this, cx| {
        while rx.recv().await.is_some() {
            cx.background_executor().timer(WATCH_DEBOUNCE).await;
            while rx.try_recv().is_ok() {}
            if this.update(cx, |this, cx| this.mark_dirty(cx)).is_err() {
                break;
            }
        }
    });
    (watcher, task)
}

// ---------------------------------------------------------------------------
// The To-dos page

pub(crate) enum TodosEvent {
    /// Show a note (and put the caret on one of its blocks).
    OpenNote { session: SessionId, block: usize },
    /// Show a session working on a to-do.
    OpenSession(SessionId),
}

pub(crate) struct TodosPage {
    runtime: Arc<StoreRuntime>,
    model: Entity<TodosModel>,
    /// Fixture palette; the live page follows the store's theme.
    pub(crate) colors_override: Option<SemanticColors>,
}

impl EventEmitter<TodosEvent> for TodosPage {}

impl TodosPage {
    pub(crate) fn new(
        runtime: Arc<StoreRuntime>,
        model: Entity<TodosModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            runtime,
            model,
            colors_override: None,
        }
    }

    fn colors(&self) -> SemanticColors {
        self.colors_override.unwrap_or_else(|| {
            crate::app_theme::colors_in(&self.runtime.store.read().expect("store"))
        })
    }

    fn project_name(&self, root: &str) -> String {
        let store = self.runtime.store.read().expect("store");
        store
            .projects()
            .values()
            .find(|p| p.root == root)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| {
                std::path::Path::new(root)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(root)
                    .to_owned()
            })
    }

    fn session(&self, id: &SessionId) -> Option<Arc<SessionRecord>> {
        self.runtime
            .store
            .read()
            .expect("store")
            .sessions()
            .get(id)
            .cloned()
    }

    fn session_chip(
        &self,
        record: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let kind = crate::session_presentation::ui_agent_kind(record.effective_kind());
        // The same live state the note shows under the to-do.
        let facts = ubra_notes::work::SessionFacts::from_record(record);
        let state = ubra_notes::work::state(false, Some(Some(&facts)));
        let tone = match &state {
            ubra_notes::work::WorkState::NeedsYou(_) => ubra_ui::Ink::ATTENTION,
            ubra_notes::work::WorkState::Review(_) => crate::notes::editor_view::accent(),
            ubra_notes::work::WorkState::Starting | ubra_notes::work::WorkState::Working => {
                colors.secondary
            }
            _ => colors.tertiary,
        };
        let label = state.label();
        let id = record.id.clone();
        div()
            .id(SharedString::from(format!("todo-session-{}", record.id.0)))
            .flex()
            .items_center()
            .gap(px(5.0))
            .h(px(20.0))
            .px(px(6.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|el| el.bg(Fill::hover(colors, true)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    cx.emit(TodosEvent::OpenSession(id.clone()));
                }),
            )
            .child(AgentLogo::new(kind, 14.0, colors).badged(false).inset(0.08))
            .child(div().text_size(px(11.5)).text_color(tone).child(label))
            .into_any_element()
    }
}

impl Render for TodosPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.model.update(cx, |model, cx| model.sync(cx));
        let colors = self.colors();
        let accent = crate::notes::editor_view::accent();
        let groups = self.model.read(cx).groups().to_vec();
        let total: usize = groups.iter().map(|g| g.items.len()).sum();

        let mut column = div()
            .w_full()
            .max_w(px(crate::notes::editor_view::MEASURE))
            .flex()
            .flex_col()
            .gap(px(28.0));
        column = column.child(
            div()
                .flex()
                .items_baseline()
                .gap(px(12.0))
                .child(
                    div()
                        .text_size(px(30.0))
                        .line_height(px(38.0))
                        .font_weight(FontWeight::BOLD)
                        .text_color(colors.primary)
                        .child("To-dos"),
                )
                .when(total > 0, |el| {
                    el.child(
                        div()
                            .text_size(px(15.0))
                            .text_color(colors.tertiary)
                            .child(format!("{total} open")),
                    )
                })
                .child(div().flex_1())
                .child(search_notes_button(colors)),
        );
        if groups.is_empty() {
            column = column.child(
                div()
                    .pt(px(80.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.0))
                    .child(sf_symbol("checkmark.circle", 28.0, accent))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .text_color(colors.secondary)
                            .child("Nothing left to do"),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(colors.tertiary)
                            .child("Type [] in any note to add a to-do"),
                    ),
            );
        }
        for group in groups {
            let note_session = group.note_session.clone();
            let header = div()
                .id(SharedString::from(format!("todos-note-{}", group.note_id)))
                .flex()
                .items_center()
                .gap(px(8.0))
                .pb(px(8.0))
                .mb(px(2.0))
                .border_b_1()
                .border_color(colors.primary.alpha(0.07))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                        cx.emit(TodosEvent::OpenNote {
                            session: note_session.clone(),
                            block: 0,
                        });
                    }),
                )
                .child(sf_symbol("doc.text", 13.0, colors.secondary))
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.primary)
                        .child(group.title.clone()),
                )
                .when_some(
                    group.project.as_deref().map(|root| self.project_name(root)),
                    |el, name| {
                        el.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(colors.tertiary)
                                .child(name),
                        )
                    },
                );
            let mut list = div().flex().flex_col().gap(px(1.0)).child(header);
            for item in &group.items {
                let note_id = group.note_id.clone();
                let workspace = group.workspace.clone();
                let index = item.index;
                let open_session = group.note_session.clone();
                // The newest attempt speaks for the to-do; earlier ones are
                // history in the note.
                let chips: Vec<gpui::AnyElement> = item
                    .linked
                    .last()
                    .and_then(|id| self.session(id))
                    .map(|record| self.session_chip(&record, colors, cx))
                    .into_iter()
                    .collect();
                list = list.child(
                    div()
                        .id(SharedString::from(format!(
                            "todo-{}-{index}",
                            group.note_id
                        )))
                        .flex()
                        .items_start()
                        .gap(px(10.0))
                        .px(px(6.0))
                        .py(px(5.0))
                        .rounded(px(7.0))
                        .hover(|el| el.bg(Fill::hover(colors, true)))
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "tick-{}-{index}",
                                    group.note_id
                                )))
                                .mt(px(3.0))
                                .size(px(16.0))
                                .flex_none()
                                .rounded(px(5.0))
                                .border(px(1.5))
                                .border_color(colors.primary.alpha(0.32))
                                .cursor_pointer()
                                .hover(|el| el.border_color(accent).bg(accent.alpha(0.1)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        let note_id = note_id.clone();
                                        let workspace = workspace.clone();
                                        this.model.update(cx, |model, cx| {
                                            model.tick(workspace.as_ref(), &note_id, index, cx)
                                        });
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .cursor_pointer()
                                .text_size(px(15.0))
                                .line_height(px(22.0))
                                .text_color(colors.primary)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                                        cx.emit(TodosEvent::OpenNote {
                                            session: open_session.clone(),
                                            block: index,
                                        });
                                    }),
                                )
                                .child(item.text.clone()),
                        )
                        .children(chips),
                );
            }
            column = column.child(list);
        }
        div()
            .id("todos-page")
            .size_full()
            .overflow_y_scroll()
            .bg(colors.work_surface_nested())
            .font_family(crate::fonts::ui_family())
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .px(px(48.0))
                    .pt(px(56.0))
                    .pb(px(160.0))
                    .child(column),
            )
    }
}

trait AlphaExt {
    fn alpha(self, a: f32) -> Self;
}

impl AlphaExt for gpui::Rgba {
    fn alpha(self, a: f32) -> Self {
        gpui::Rgba {
            a: self.a * a,
            ..self
        }
    }
}

/// Every note, not only those with open to-dos, is a search away.
fn search_notes_button(colors: SemanticColors) -> impl IntoElement {
    let shortcut = crate::commands::command(crate::commands::CommandId::SearchNotes)
        .shortcut_label()
        .unwrap_or_default();
    div()
        .id("todos-search-notes")
        .debug_selector(|| "todos-search-notes".into())
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(ubra_ui::Radius::CHIP))
        .cursor_pointer()
        .hover(move |style| style.bg(ubra_ui::Fill::hover(colors, true)))
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(crate::commands::SearchNotes), cx);
        })
        .child(sf_symbol("magnifyingglass", 12.0, colors.secondary))
        .child(
            div()
                .text_size(px(12.5))
                .text_color(colors.secondary)
                .child("Search notes"),
        )
        .child(
            div()
                .text_size(px(12.5))
                .text_color(colors.tertiary)
                .child(shortcut),
        )
}
