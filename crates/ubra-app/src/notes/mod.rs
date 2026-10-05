//! Ubra Notes in the main window's trailing inspector.
//!
//! A note is a Session whose kind is `note`, but notes never appear in the
//! navigation sidebar and never take the main-pane selection. Only the
//! inspector's Notes detail page hosts this editor. Sessions started from
//! a note remain its children and re-root in the navigation sidebar since
//! their parent is hidden. The pane owns the open file, saves continuously,
//! synchronizes its Session title, and reloads CLI/agent writes. Closing or
//! deleting a detail releases the editor so delayed saves cannot revive it.

pub(crate) mod chip;
pub(crate) mod editor_view;
pub(crate) mod panel;
pub(crate) mod search;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod todos;
mod versions;
pub(crate) mod work_item;
#[cfg(test)]
pub(crate) mod work_item_tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, KeyBinding, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use ubra_notes::edit::Editor;
use ubra_notes::markdown::FrontMatter;
use ubra_notes::mention::{self as mentions, Candidate, MentionTarget};
use ubra_notes::store::{self, Note, NoteStore};
use ubra_proto::{ProjectId, SessionId};
use ubra_ui::SemanticColors;

use crate::prompt_draft::NoteSource;
use crate::store::StoreRuntime;
use editor_view::{EditorEvent, MentionDirectory, MentionEntry, NoteEditorView};

gpui::actions!(ubra_notes, [AttachNote, AttachNoteSelection]);

const SAVE_DEBOUNCE: Duration = Duration::from_millis(350);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(120);

pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = editor_view::key_bindings();
    bindings.push(KeyBinding::new(
        "cmd-alt-a",
        AttachNote,
        Some(editor_view::EDITOR_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "cmd-alt-shift-a",
        AttachNoteSelection,
        Some(editor_view::EDITOR_CONTEXT),
    ));
    bindings
}

struct OpenNote {
    session: SessionId,
    id: String,
    /// The store holding this note's file (the global notes or one
    /// workspace's folder), resolved from the Session's scope when shown.
    store: Arc<NoteStore>,
    /// The scope `store` serves; `None` is global.
    workspace: Option<ProjectId>,
    front: FrontMatter,
    editor: Entity<NoteEditorView>,
    /// Markdown last written to (or read from) disk, to tell our own writes
    /// from outside edits when the watcher fires.
    saved: String,
    dirty: bool,
    /// The title last pushed to the Session, so renames go out once.
    synced_title: String,
    /// Markdown an autosave is writing off the main thread right now: the
    /// watcher and a flush take the file holding it as our own write.
    in_flight: Option<String>,
    /// Typing arrived while a write was in flight: save again when it lands.
    resave: bool,
    _subscription: Subscription,
}

enum PaneState {
    Empty,
    Open(OpenNote),
    Missing { session: SessionId, id: String },
}

pub(crate) struct NotePane {
    runtime: Arc<StoreRuntime>,
    store: Option<Arc<NoteStore>>,
    /// Workspace notes stores, opened on first use from the global store.
    scoped: HashMap<ProjectId, Arc<NoteStore>>,
    state: PaneState,
    focus: FocusHandle,
    attachment_recipient: Option<(SessionId, String)>,
    focus_on_show: bool,
    save_task: Task<()>,
    _watch_task: Task<()>,
    _watcher: Option<notify::RecommendedWatcher>,
    /// Keeps mention chips' session status and the `@` menu live.
    _sessions_task: Task<()>,
    error: Option<SharedString>,
    /// The open note's Version History panel, while it is shown.
    versions: Option<versions::VersionPanel>,
    /// Fixture palette; live panes follow the store's theme.
    colors_override: Option<SemanticColors>,
    /// Prompts Start sent to mentioned sessions, for tests.
    #[cfg(test)]
    pub(crate) sent_for_test: Vec<crate::notifications::SendTextCommand>,
}

impl Focusable for NotePane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            PaneState::Open(open) => open.editor.read(cx).focus_handle(cx),
            _ => self.focus.clone(),
        }
    }
}

fn editor_view_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(crate) enum NotePaneEvent {
    /// Escape with nothing left to dismiss in the editor.
    Dismiss,
    /// A mention chip was clicked: show that session (an agent, a
    /// terminal, or another note).
    Reveal(SessionId),
    /// A source reference with its explicit captured prompt recipient.
    Attach {
        recipient: SessionId,
        source: NoteSource,
    },
    AttachSelection {
        recipient: SessionId,
        quote: crate::quote::Quote,
    },
}

impl gpui::EventEmitter<NotePaneEvent> for NotePane {}

impl NotePane {
    pub(crate) fn new(runtime: Arc<StoreRuntime>, cx: &mut Context<Self>) -> Self {
        let store = NoteStore::resolve_dir()
            .and_then(|dir| NoteStore::open(dir).ok())
            .map(Arc::new);
        Self::with_store(runtime, store, true, cx)
    }

    /// `watch` is off only in deterministic tests, whose scheduler rejects
    /// wakeups from the file watcher's own thread.
    pub(crate) fn with_store(
        runtime: Arc<StoreRuntime>,
        store: Option<Arc<NoteStore>>,
        watch: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (watcher, watch_task) = match (&store, watch) {
            (Some(store), true) => Self::watch(store, cx),
            _ => (None, Task::ready(())),
        };
        let sessions_task = if watch {
            Self::follow_sessions(&runtime, cx)
        } else {
            Task::ready(())
        };
        Self {
            runtime,
            store,
            scoped: HashMap::new(),
            state: PaneState::Empty,
            attachment_recipient: None,
            focus: cx.focus_handle(),
            focus_on_show: false,
            save_task: Task::ready(()),
            _watch_task: watch_task,
            _watcher: watcher,
            _sessions_task: sessions_task,
            error: None,
            versions: None,
            colors_override: None,
            #[cfg(test)]
            sent_for_test: Vec::new(),
        }
    }

    pub(crate) fn set_attachment_recipient(
        &mut self,
        recipient: Option<(SessionId, String)>,
        cx: &mut Context<Self>,
    ) {
        if self.attachment_recipient != recipient {
            self.attachment_recipient = recipient;
            cx.notify();
        }
    }

    pub(crate) fn attachment_source(&self) -> Option<NoteSource> {
        let PaneState::Open(open) = &self.state else {
            return None;
        };
        Some(NoteSource {
            workspace: open.workspace.clone(),
            note_id: open.id.clone(),
            session_id: open.session.clone(),
        })
    }

    /// Current authored body, without private front matter or stale autosave.
    pub(crate) fn body_snapshot(&self, cx: &App) -> Option<(NoteSource, String)> {
        let PaneState::Open(open) = &self.state else {
            return None;
        };
        Some((
            self.attachment_source()?,
            open.editor.read(cx).body_snapshot(),
        ))
    }

    pub(crate) fn selection_snapshot(&self, cx: &App) -> Option<(NoteSource, String)> {
        let PaneState::Open(open) = &self.state else {
            return None;
        };
        let selected = open.editor.read(cx).selection_snapshot();
        (!selected.trim().is_empty())
            .then(|| (self.attachment_source().expect("open source"), selected))
    }

    fn publish_live_body(&self, cx: &App) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        if let Some((source, body)) = self.body_snapshot(cx) {
            self.runtime
                .prompt_drafts
                .register_live_note(source, open.store.clone(), body);
        }
    }

    fn attachment_target(&mut self, cx: &mut Context<Self>) -> Option<SessionId> {
        let Some((recipient, _)) = self.attachment_recipient.clone() else {
            self.error = Some("Select an available prompt recipient before attaching".into());
            cx.notify();
            return None;
        };
        let valid = self
            .runtime
            .store
            .read()
            .expect("store")
            .sessions()
            .get(&recipient)
            .is_some_and(|record| {
                !record.is_note()
                    && !record.is_archived()
                    && !matches!(record.status, ubra_proto::SessionStatus::Exited(_))
            });
        if !valid {
            self.error = Some("This prompt recipient is no longer available".into());
            cx.notify();
            return None;
        }
        Some(recipient)
    }

    fn attach_note(&mut self, _: &AttachNote, _: &mut Window, cx: &mut Context<Self>) {
        let Some(recipient) = self.attachment_target(cx) else {
            return;
        };
        self.publish_live_body(cx);
        if let Some(source) = self.attachment_source() {
            cx.emit(NotePaneEvent::Attach { recipient, source });
        }
    }

    fn attach_note_selection(
        &mut self,
        _: &AttachNoteSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(recipient) = self.attachment_target(cx) else {
            return;
        };
        let Some((source, content)) = self.selection_snapshot(cx) else {
            self.error = Some("Select authored note text before attaching a selection".into());
            cx.notify();
            return;
        };
        let (_, body) = self.body_snapshot(cx).expect("open selection source");
        let quote = crate::quote::Quote::new(
            crate::quote::QuoteSource::NoteSelection {
                session_id: source.session_id,
                workspace: source.workspace,
                note_id: source.note_id,
                source_revision: crate::prompt_draft::revision(&body),
            },
            content,
        )
        .expect("nonempty exported selection");
        cx.emit(NotePaneEvent::AttachSelection { recipient, quote });
    }

    fn colors(&self) -> SemanticColors {
        self.colors_override.unwrap_or_else(|| {
            crate::app_theme::colors_in(&self.runtime.store.read().expect("store"))
        })
    }

    #[cfg(test)]
    pub(crate) fn editor_for_test(&self) -> Option<Entity<NoteEditorView>> {
        match &self.state {
            PaneState::Open(open) => Some(open.editor.clone()),
            _ => None,
        }
    }

    /// Focus the editor the next time a note is shown (a new note, a click).
    pub(crate) fn request_focus(&mut self) {
        self.focus_on_show = true;
    }

    /// Drops the open note without saving, when it is `note_id` for
    /// `session`. Trashing deletes the file, so a later save would resurrect
    /// it; anything else stays untouched.
    pub(crate) fn discard_open(
        &mut self,
        session: &SessionId,
        note_id: &str,
        cx: &mut Context<Self>,
    ) {
        let open = match &self.state {
            PaneState::Open(open) => open.session == *session && open.id == note_id,
            PaneState::Missing { session: open, id } => open == session && id == note_id,
            PaneState::Empty => false,
        };
        if !open {
            return;
        }
        if let Some(source) = self.attachment_source() {
            self.runtime.prompt_drafts.release_live_note(&source);
        }
        self.state = PaneState::Empty;
        self.versions = None;
        self.save_task = Task::ready(());
        self.error = None;
        self.focus_on_show = false;
        cx.notify();
    }

    /// Shows `note_id` for `session`; switching away saves the previous one.
    pub(crate) fn show(
        &mut self,
        session: &SessionId,
        note_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same = match &self.state {
            PaneState::Open(open) => open.session == *session && open.id == note_id,
            PaneState::Missing { session: s, id } => s == session && id == note_id,
            PaneState::Empty => false,
        };
        if !same {
            self.save(cx);
            self.load(session, note_id, window, cx);
        }
        // Arrived from a session's header: show the to-do it works on.
        let reveal = self
            .runtime
            .store
            .write()
            .expect("store")
            .take_note_reveal(session);
        if let (Some(child), PaneState::Open(open)) = (reveal, &self.state) {
            let editor = open.editor.clone();
            if editor.update(cx, |view, cx| view.reveal_session(&child.0, cx)) {
                self.focus_on_show = true;
            }
        }
        if std::mem::take(&mut self.focus_on_show) {
            let handle = self.focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    /// Caret to the end of a note block (index in the file, title excluded)
    /// and focus the editor, as a jump from the To-dos page.
    pub(crate) fn reveal_block(
        &mut self,
        block: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        open.editor.update(cx, |view, cx| {
            let index = (block + 1).min(view.editor.blocks().len() - 1);
            let len = view.editor.block(index).text.len();
            view.editor
                .set_caret(ubra_notes::edit::Pos::new(index, len));
            cx.notify();
        });
        let handle = self.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// The store for one scope, opening and watching workspace folders
    /// on first use. `None` without a global store, or when the folder
    /// cannot be opened.
    fn store_for(&mut self, workspace: &Option<ProjectId>) -> Option<Arc<NoteStore>> {
        let Some(id) = workspace else {
            return self.store.clone();
        };
        if let Some(store) = self.scoped.get(id) {
            return Some(store.clone());
        }
        let global = self.store.clone()?;
        let store = Arc::new(NoteStore::open_workspace(global.dir(), &id.0).ok()?);
        if let Some(watcher) = self._watcher.as_mut() {
            use notify::{RecursiveMode, Watcher};
            let _ = watcher.watch(store.dir(), RecursiveMode::NonRecursive);
        }
        self.scoped.insert(id.clone(), store.clone());
        Some(store)
    }

    fn load(
        &mut self,
        session: &SessionId,
        note_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(source) = self.attachment_source() {
            self.runtime.prompt_drafts.release_live_note(&source);
        }
        self.versions = None;
        let workspace = self
            .runtime
            .store
            .read()
            .expect("store")
            .sessions()
            .get(session)
            .and_then(|record| record.note_workspace.clone());
        let notes = self.store_for(&workspace);
        let source = notes
            .as_ref()
            .and_then(|store| store.path_for(note_id).ok())
            .and_then(|path| std::fs::read_to_string(path).ok());
        let (Some(store), Some(source)) = (notes, source) else {
            self.state = PaneState::Missing {
                session: session.clone(),
                id: note_id.to_owned(),
            };
            cx.notify();
            return;
        };
        let note = store::parse_note(&source);
        let colors = self.colors();
        let assets = Some(editor_view::AssetHome {
            store: store.clone(),
            note_id: note_id.to_owned(),
        });
        let editor = cx.new(|cx| {
            let mut view = NoteEditorView::new(Editor::new(&note.doc), colors, cx);
            if let Some(assets) = assets {
                view.set_asset_home(assets);
            }
            view.fold_started_work();
            view
        });
        let subscription = cx.subscribe_in(&editor, window, |this, _, event, _, cx| match event {
            EditorEvent::Changed => this.schedule_save(cx),
            EditorEvent::Dismiss => {
                // Escape closes the history panel before it leaves the note.
                if this.versions_open() {
                    this.close_versions(cx);
                    return;
                }
                // The host flushes and releases the editor on dismissal.
                cx.emit(NotePaneEvent::Dismiss);
            }
            EditorEvent::OpenMention(target) => this.open_mention(target, cx),
            EditorEvent::Work(request) => this.on_work(request, cx),
        });
        let synced_title = self
            .runtime
            .store
            .read()
            .expect("store")
            .sessions()
            .get(session)
            .map(|record| record.title.clone())
            .unwrap_or_default();
        self.state = PaneState::Open(OpenNote {
            session: session.clone(),
            id: note_id.to_owned(),
            store,
            workspace,
            front: note.front,
            editor,
            saved: source,
            dirty: false,
            synced_title,
            in_flight: None,
            resave: false,
            _subscription: subscription,
        });
        self.publish_live_body(cx);
        self.push_mentions(cx);
        self.push_work(cx);
        cx.notify();
    }

    fn follow_sessions(runtime: &Arc<StoreRuntime>, cx: &mut Context<Self>) -> Task<()> {
        let mut changes = runtime.changes();
        cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this
                            .update(cx, |this, cx| {
                                this.push_mentions(cx);
                                this.push_work(cx);
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        })
    }

    /// Everything the open note can mention, rebuilt from the session store.
    pub(crate) fn mention_directory(&self) -> MentionDirectory {
        let open = match &self.state {
            PaneState::Open(open) => Some(open.session.clone()),
            _ => None,
        };
        let store = self.runtime.store.read().expect("store");
        let entries = mention_entries(&store, open.as_ref());
        MentionDirectory { entries }
    }

    /// Hands the open editor a fresh directory; a no-op when nothing changed.
    pub(crate) fn push_mentions(&mut self, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        let editor = open.editor.clone();
        let directory = self.mention_directory();
        editor.update(cx, |view, cx| view.set_mentions(directory, cx));
    }

    fn open_mention(&mut self, target: &MentionTarget, cx: &mut Context<Self>) {
        let prefer = match &self.state {
            PaneState::Open(open) => Some(open.workspace.clone()),
            _ => None,
        };
        let session = {
            let store = self.runtime.store.read().expect("store");
            match target {
                MentionTarget::Session(id) => {
                    let id = SessionId::new(id.clone());
                    store.sessions().contains_key(&id).then_some(id)
                }
                MentionTarget::Note(note) => {
                    let mut same_scope = None;
                    let mut any = None;
                    for record in store.sessions().values() {
                        if !(record.is_note() && record.note_id.as_deref() == Some(note)) {
                            continue;
                        }
                        // An id alone is ambiguous across scopes: prefer the
                        // open note's scope, else the first Session found.
                        if prefer
                            .as_ref()
                            .is_some_and(|scope| &record.note_workspace == scope)
                        {
                            same_scope = Some(record.id.clone());
                            break;
                        }
                        if any.is_none() {
                            any = Some(record.id.clone());
                        }
                    }
                    same_scope.or(any)
                }
            }
        };
        if let Some(session) = session {
            self.save(cx);
            cx.emit(NotePaneEvent::Reveal(session));
        }
    }

    fn watch(
        store: &Arc<NoteStore>,
        cx: &mut Context<Self>,
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
                if this.update(cx, |this, cx| this.reconcile(cx)).is_err() {
                    break;
                }
            }
        });
        (watcher, task)
    }

    /// An outside write to the open note (CLI append, an agent, another
    /// editor). A clean note reloads; with unsaved typing the write is merged
    /// into the editor as one undo step, so neither side is lost.
    pub(crate) fn reconcile(&mut self, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        let Some(path) = open.store.path_for(&open.id).ok() else {
            return;
        };
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let session = open.session.clone();
                let id = open.id.clone();
                self.discard_open(&session, &id, cx);
                cx.emit(NotePaneEvent::Dismiss);
                return;
            }
            Err(_) => return,
        };
        if source == open.saved || open.in_flight.as_deref() == Some(source.as_str()) {
            return;
        }
        self.absorb_outside(source, cx);
    }

    /// Takes the file's current `source` into the open note: a reload when
    /// there is no unsaved typing, else a three-way merge against what the
    /// editor last loaded or saved.
    fn absorb_outside(&mut self, source: String, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &mut self.state else {
            return;
        };
        // A direct edit of the file (another editor, an agent's own file
        // tools) is kept in history and cannot break the note's identity:
        // the store repairs the front matter from the last known one.
        let source = open.store.notice_outside_change(&open.id).unwrap_or(source);
        let mut theirs = store::parse_note(&source);
        store::repair_front(&mut theirs.front, &open.id, &open.front);
        if open.dirty {
            let base = store::parse_note(&open.saved).doc;
            open.editor.update(cx, |view, cx| {
                let merged = ubra_notes::merge::merge3(&base, &view.editor.document(), &theirs.doc);
                view.editor.absorb(merged, ubra_notes::history::now_ms());
                cx.notify();
            });
        } else {
            let editor = Editor::new(&theirs.doc);
            open.editor.update(cx, |view, cx| view.reload(editor, cx));
        }
        open.saved = source;
        open.front = theirs.front;
        self.publish_live_body(cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.publish_live_body(cx);
        if let PaneState::Open(open) = &mut self.state {
            open.dirty = true;
        }
        self.save_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.save_with(true, cx));
        });
    }

    /// Writes the open note now, on this thread: before switching notes and
    /// on the way out, where the file must be current when this returns.
    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        self.save_with(false, cx);
    }

    /// `background`: the autosave, whose store write runs off the main
    /// thread. Otherwise a flush, done here and now.
    fn save_with(&mut self, background: bool, cx: &mut Context<Self>) {
        if background {
            self.save_in_background(cx);
            return;
        }
        let PaneState::Open(open) = &self.state else {
            return;
        };
        let store = open.store.clone();
        // Save only over the version this editor knows; an outside write in
        // between is merged in first, then saved over. Bounded: a file that
        // keeps changing under us is retried on the next save.
        let mut note = None;
        for _ in 0..3 {
            let PaneState::Open(open) = &mut self.state else {
                return;
            };
            let current = Note {
                front: open.front.clone(),
                doc: open.editor.read(cx).editor.document(),
            };
            let markdown = current.to_markdown();
            if markdown == open.saved {
                open.dirty = false;
                note = Some(current);
                break;
            }
            match store.save_if_unchanged(&open.id, &current, &open.saved) {
                Ok(store::SaveOutcome::Saved { source }) => {
                    open.saved = source;
                    open.dirty = false;
                    self.error = None;
                    note = Some(current);
                    break;
                }
                Ok(store::SaveOutcome::Conflict {
                    current: Some(outside),
                }) if open.in_flight.as_deref() == Some(outside.as_str()) => {
                    // Our own autosave landed first: it is the known version.
                    open.saved = outside;
                }
                Ok(store::SaveOutcome::Conflict {
                    current: Some(outside),
                }) => {
                    open.dirty = true;
                    self.absorb_outside(outside, cx);
                }
                Ok(store::SaveOutcome::Conflict { current: None }) => {
                    let session = open.session.clone();
                    let id = open.id.clone();
                    self.discard_open(&session, &id, cx);
                    cx.emit(NotePaneEvent::Dismiss);
                    return;
                }
                Err(err) => {
                    self.error = Some(format!("Couldn't save this note: {err}").into());
                    return;
                }
            }
        }
        let Some(note) = note else {
            return;
        };
        self.sync_title(&note.doc.title, cx);
    }

    /// The sidebar row is the Session's title; keep it the note's title.
    fn sync_title(&mut self, title: &str, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &mut self.state else {
            return;
        };
        let title = title.trim();
        let title = if title.is_empty() { "Untitled" } else { title };
        if title != open.synced_title {
            open.synced_title = title.to_owned();
            self.runtime
                .store
                .write()
                .expect("store")
                .rename(open.session.clone(), title);
        }
        cx.notify();
    }

    /// The autosave. The note is serialized here, but the store's
    /// merge-safe write (compare, atomic rename, fsync, history: several ms
    /// on a long note) runs off the main thread, so it never lands inside a
    /// keystroke's frame. One write is in flight at a time, so writes cannot
    /// land out of order; typing meanwhile saves again when it lands.
    fn save_in_background(&mut self, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &mut self.state else {
            return;
        };
        let store = open.store.clone();
        if open.in_flight.is_some() {
            open.resave = true;
            return;
        }
        let current = Note {
            front: open.front.clone(),
            doc: open.editor.read(cx).editor.document(),
        };
        let markdown = current.to_markdown();
        if markdown == open.saved {
            open.dirty = false;
            let title = current.doc.title.clone();
            self.sync_title(&title, cx);
            return;
        }
        let expected = open.saved.clone();
        let id = open.id.clone();
        open.in_flight = Some(markdown);
        let title = current.doc.title.clone();
        let write_id = id.clone();
        let editor_id = open.editor.entity_id();
        let write =
            cx.background_spawn(
                async move { store.save_if_unchanged(&write_id, &current, &expected) },
            );
        cx.spawn(async move |this, cx| {
            let outcome = write.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_background_save(&id, editor_id, &title, outcome, cx)
            });
        })
        .detach();
    }

    fn finish_background_save(
        &mut self,
        id: &str,
        editor_id: gpui::EntityId,
        title: &str,
        outcome: std::io::Result<store::SaveOutcome>,
        cx: &mut Context<Self>,
    ) {
        let PaneState::Open(open) = &mut self.state else {
            return;
        };
        if open.id != id || open.editor.entity_id() != editor_id {
            return;
        }
        let written = open.in_flight.take();
        let resave = std::mem::take(&mut open.resave);
        match outcome {
            Ok(store::SaveOutcome::Saved { source }) => {
                open.saved = source;
                self.error = None;
                self.sync_title(title, cx);
            }
            Ok(store::SaveOutcome::Conflict {
                current: Some(outside),
            }) => {
                if written.as_deref() != Some(outside.as_str()) {
                    open.dirty = true;
                    self.absorb_outside(outside, cx);
                }
                self.schedule_save(cx);
                return;
            }
            Ok(store::SaveOutcome::Conflict { current: None }) => {
                let session = open.session.clone();
                let id = open.id.clone();
                self.discard_open(&session, &id, cx);
                cx.emit(NotePaneEvent::Dismiss);
                return;
            }
            Err(err) => {
                self.error = Some(format!("Couldn't save this note: {err}").into());
            }
        }
        if resave {
            self.schedule_save(cx);
        }
        cx.notify();
    }
}

impl Drop for NotePane {
    fn drop(&mut self) {
        if let Some(source) = self.attachment_source() {
            self.runtime.prompt_drafts.release_live_note(&source);
        }
    }
}

impl Render for NotePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors();
        let versions = self.versions_element(_window, cx);
        let attachment_action = self
            .attachment_recipient
            .as_ref()
            .map(|(_, label)| format!("Attach to {label}"));
        let root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if this.versions_open() && event.keystroke.key == "escape" {
                    this.close_versions(cx);
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if !matches!(this.state, PaneState::Open(_)) && event.keystroke.key == "escape" {
                    cx.emit(NotePaneEvent::Dismiss);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(Self::open_versions))
            .on_action(cx.listener(Self::attach_note))
            .on_action(cx.listener(Self::attach_note_selection))
            .bg(colors.work_surface_nested())
            .font_family(crate::fonts::ui_family());
        let root = match &self.state {
            PaneState::Open(open) => {
                open.editor.update(cx, |view, _| view.set_colors(colors));
                root
                    .child(
                        div().flex().flex_wrap().items_center().justify_end().gap(px(4.0)).px(px(10.0)).py(px(6.0))
                            .child(
                                div().id("notes-attach-context").debug_selector(|| "notes-attach-context".into()).px(px(8.0)).py(px(5.0)).rounded(px(6.0))
                                    .text_size(px(11.0)).text_color(colors.secondary)
                                    .when_some(attachment_action.clone(), |button, label| {
                                        button.role(gpui::Role::Button).aria_label(label.clone()).tab_index(0)
                                            .hover(move |button| button.bg(ubra_ui::Fill::subtle(colors)))
                                            .focus_visible(move |style| style.bg(ubra_ui::Fill::subtle(colors)))
                                            .on_click(cx.listener(|this, _, window, cx| this.attach_note(&AttachNote, window, cx)))
                                            .child(label)
                                    })
                                    .when(attachment_action.is_none(), |button| button.child("No prompt recipient"))
                            )
                            .when(attachment_action.is_some(), |toolbar| toolbar.child(
                                div().id("notes-attach-selection").debug_selector(|| "notes-attach-selection".into()).px(px(8.0)).py(px(5.0)).rounded(px(6.0))
                                    .text_size(px(11.0)).text_color(colors.secondary)
                                    .role(gpui::Role::Button).aria_label(format!("Attach selected note text to {}", self.attachment_recipient.as_ref().expect("recipient").1)).tab_index(0)
                                    .hover(move |button| button.bg(ubra_ui::Fill::subtle(colors)))
                                    .focus_visible(move |style| style.bg(ubra_ui::Fill::subtle(colors)))
                                    .on_click(cx.listener(|this, _, window, cx| this.attach_note_selection(&AttachNoteSelection, window, cx)))
                                    .child("Attach selection")
                            ))
                    )
                    .child(div().flex_1().min_h(px(0.0)).child(open.editor.clone()))
            }
            PaneState::Missing { .. } => root.track_focus(&self.focus).child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(colors.secondary)
                            .child("This note's file is gone"),
                    )
                    .child(div().text_size(px(12.0)).text_color(colors.tertiary).child(
                        "It may have been moved or deleted outside Ubra. Go back to the notes list.",
                    )),
            ),
            PaneState::Empty => root.track_focus(&self.focus),
        };
        let root = root.when_some(versions, |el, panel| el.child(panel));
        root.when_some(self.error.clone(), |el, error| {
            el.child(
                div()
                    .absolute()
                    .bottom(px(14.0))
                    .right(px(14.0))
                    .px(px(12.0))
                    .py(px(7.0))
                    .rounded(px(8.0))
                    .bg(ubra_ui::Ink::DANGER)
                    .text_color(gpui::white())
                    .text_size(px(12.0))
                    .child(error),
            )
        })
    }
}

/// A session mention's text. The row and chip already wear the agent's
/// logo, so its name only steals width from the title; a session with no
/// title yet still needs a word, so it says the agent.
pub(crate) fn mention_label(agent: &str, title: &str) -> String {
    if title.trim().is_empty() {
        mentions::session_label(agent, "")
    } else {
        mentions::session_label("", title)
    }
}

/// Live sessions first (most recently active), then notes, each as the
/// `@` menu offers it. Notes are Sessions too; they are mentioned by their
/// file id so the link survives the Session being archived and restored.
fn mention_entries(
    store: &crate::store::SessionStore,
    open: Option<&SessionId>,
) -> Vec<MentionEntry> {
    let mut records: Vec<_> = store
        .sessions()
        .values()
        .filter(|s| !s.is_archived() && Some(&s.id) != open)
        .collect();
    records.sort_by(|a, b| b.updated_at.0.total_cmp(&a.updated_at.0));
    let project_of = |record: &ubra_proto::SessionRecord| {
        store
            .projects()
            .get(&record.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };
    let (notes, sessions): (Vec<_>, Vec<_>) = records.into_iter().partition(|s| s.is_note());
    let mut entries: Vec<MentionEntry> = sessions
        .into_iter()
        .map(|session| {
            let kind = session.effective_kind();
            let agent = crate::notifications::display_name(kind, store.agent_descriptor(kind));
            let title = crate::switcher::display_title_str(session);
            let project = project_of(session);
            let detail = match (&session.host, project.is_empty()) {
                (Some(host), false) => format!("{project} · {host}"),
                (Some(host), true) => host.clone(),
                (None, false) => project.clone(),
                (None, true) => {
                    crate::quick_open::home_relative(std::path::Path::new(&session.cwd))
                }
            };
            MentionEntry {
                candidate: Candidate {
                    target: MentionTarget::Session(session.id.0.clone()),
                    label: mention_label(agent, title),
                    keywords: format!(
                        "{agent} {project} {}",
                        session.git_branch.as_deref().unwrap_or("")
                    ),
                },
                agent: Some(crate::session_presentation::ui_agent_kind(kind)),
                status: Some(crate::session_presentation::status_state(session, false)),
                detail: detail.into(),
            }
        })
        .collect();
    entries.extend(notes.into_iter().filter_map(|note| {
        let id = note.note_id.clone()?;
        let project = project_of(note);
        Some(MentionEntry {
            candidate: Candidate {
                target: MentionTarget::Note(id),
                label: mentions::note_label(&note.title),
                keywords: format!("note {project}"),
            },
            agent: None,
            status: None,
            detail: project.into(),
        })
    }));
    entries
}
