//! The "Search notes" page of the command palette, and the note rows ⌘K
//! mixes into its results. Every note in the notes folder is listed: live
//! note Sessions, archived ones (dimmed) and files no Session claims. The
//! index is the shared To-dos model's, built off the main thread and kept
//! fresh by its directory watcher; a keystroke only ranks what is in memory.
use super::*;
use crate::notes::search::{NoteEntry, NoteHit, NotesSearch};
use crate::notes::todos::TodosModel;
use gpui::Entity;
use std::collections::HashMap;
use ubra_proto::ProjectId;
use ubra_ui::{AgentLogo, Typo};

/// Two lines (title, then the matching snippet), so taller than a command
/// row. Nine command rows of list height show six notes.
pub(super) const NOTE_ROW_HEIGHT: f32 = 54.0;
/// Notes ⌘K mixes in above its commands while typing.
const PALETTE_NOTE_LIMIT: usize = 3;
/// Agent marks a row shows for the sessions its to-dos link.
const LINKED_MARKS: usize = 3;

/// What opening a note does, by whether a Session holds it. Every home
/// ends in the right inspector's detail page; none moves the selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoteHome {
    Live(SessionId),
    /// Unarchived, then shown.
    Archived(SessionId),
    /// Adopted as a note Session, then shown.
    Orphan,
}

impl NoteHome {
    fn telemetry(&self) -> &'static str {
        match self {
            Self::Live(_) => "live",
            Self::Archived(_) => "archived",
            Self::Orphan => "orphan",
        }
    }
}

/// Asks the window to put the caret on a block of a note it just showed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NoteOpened {
    pub note_id: String,
    /// The scope of the note shown; `None` is global. Scopes are separate
    /// stores, so the id alone does not name the note.
    pub workspace: Option<ProjectId>,
    pub block: Option<usize>,
}

impl gpui::EventEmitter<NoteOpened> for NavigationOverlay {}

#[derive(Default)]
pub(super) struct NotesPage {
    model: Option<Entity<TodosModel>>,
    entries: Arc<Vec<NoteEntry>>,
    pub(super) hits: Vec<NoteHit>,
    search: NotesSearch,
    /// (Scope, note id) → the Session holding it and whether that
    /// Session is archived. Rebuilt from the store whenever the page
    /// refreshes.
    homes: HashMap<(Option<ProjectId>, String), (SessionId, bool)>,
    _observe: Option<gpui::Subscription>,
}

impl NavigationOverlay {
    /// The shared index, created and observed on first use. Opening asks
    /// it to re-read anything the watcher marked changed.
    fn notes_model(&mut self, cx: &mut Context<Self>) -> Entity<TodosModel> {
        if let Some(model) = &self.notes.model {
            return model.clone();
        }
        let model = TodosModel::global(&self._runtime, cx);
        self.notes._observe = Some(cx.observe(&model, |this, _, cx| {
            if matches!(this.overlay, Some(Overlay::Notes | Overlay::CommandPalette)) {
                this.refresh_notes(cx);
            }
        }));
        self.notes.model = Some(model.clone());
        model
    }

    /// Opening search re-reads the notes folder: a note written since the
    /// last read must be findable even if the directory watcher missed it.
    /// Unchanged files are reused, so this costs one directory listing.
    pub(super) fn reread_notes(&mut self, cx: &mut Context<Self>) {
        let model = self.notes_model(cx);
        model.update(cx, |model, _| model.invalidate());
        self.refresh_notes(cx);
    }

    /// Re-reads the index and the store's note Sessions, keeping the
    /// highlight on the note it was on.
    pub(super) fn refresh_notes(&mut self, cx: &mut Context<Self>) {
        let model = self.notes_model(cx);
        model.update(cx, |model, cx| model.sync(cx));
        let entries = model.read(cx).notes();
        let homes = {
            let store = self.store.read().expect("session store lock poisoned");
            store
                .sessions()
                .values()
                .filter(|record| record.is_note())
                .filter_map(|record| {
                    Some((
                        (record.note_workspace.clone(), record.note_id.clone()?),
                        (record.id.clone(), record.is_archived()),
                    ))
                })
                .collect()
        };
        let unchanged = Arc::ptr_eq(&entries, &self.notes.entries) && homes == self.notes.homes;
        if unchanged {
            return;
        }
        self.notes.entries = entries;
        self.notes.homes = homes;
        match self.overlay {
            Some(Overlay::Notes) => {
                let selected = self
                    .highlighted_note()
                    .map(|(entry, _)| (entry.workspace.clone(), entry.id.clone()));
                self.filter_notes();
                if let Some(index) = selected.and_then(|(workspace, id)| {
                    self.notes.hits.iter().position(|hit| {
                        let entry = &self.notes.entries[hit.entry];
                        entry.workspace == workspace && entry.id == id
                    })
                }) {
                    self.highlight = index;
                }
            }
            Some(Overlay::CommandPalette) if !self.query.text().trim().is_empty() => {
                let highlighted = self.highlighted_command();
                self.refresh_command_items();
                self.restore_highlight(highlighted.as_ref());
            }
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn filter_notes(&mut self) {
        let entries = Arc::clone(&self.notes.entries);
        self.notes.hits = self
            .notes
            .search
            .rank(&entries, self.query.text(), usize::MAX);
        self.highlight = self.highlight.min(self.notes.hits.len().saturating_sub(1));
    }

    pub(super) fn notes_ready(&self, cx: &App) -> bool {
        self.notes
            .model
            .as_ref()
            .is_none_or(|model| model.read(cx).notes_ready())
    }

    pub(super) fn notes_empty_label(&self, cx: &App) -> &'static str {
        if !self.notes_ready(cx) {
            "Finding notes…"
        } else if self.notes.entries.is_empty() {
            "No notes yet"
        } else {
            "No matches"
        }
    }

    fn highlighted_note(&self) -> Option<(&NoteEntry, &NoteHit)> {
        let hit = self.notes.hits.get(self.highlight)?;
        Some((self.notes.entries.get(hit.entry)?, hit))
    }

    fn note_home(&self, workspace: &Option<ProjectId>, note_id: &str) -> NoteHome {
        match self
            .notes
            .homes
            .get(&(workspace.clone(), note_id.to_owned()))
        {
            Some((id, false)) => NoteHome::Live(id.clone()),
            Some((id, true)) => NoteHome::Archived(id.clone()),
            None => NoteHome::Orphan,
        }
    }

    /// Opens the highlighted note. Return puts the caret on the block that
    /// matched; ⌘Return opens the note where it was left.
    pub(super) fn open_highlighted_note(
        &mut self,
        keep_caret: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((entry, hit)) = self.highlighted_note() else {
            return;
        };
        let note_id = entry.id.clone();
        let workspace = entry.workspace.clone();
        let block = if keep_caret {
            None
        } else {
            hit.snippet.as_ref().map(|snippet| snippet.block)
        };
        self.open_note(note_id, workspace, block, window, cx);
    }

    pub(super) fn open_note(
        &mut self,
        note_id: String,
        workspace: Option<ProjectId>,
        block: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let home = self.note_home(&workspace, &note_id);
        {
            let mut store = self.store.write().expect("session store lock poisoned");
            match &home {
                // The window opens the detail page from `NoteOpened`; the
                // selection never moves for a note.
                NoteHome::Live(_) => {}
                NoteHome::Archived(id) => store.revive_sessions_unselected(vec![id.clone()]),
                NoteHome::Orphan => store.open_note_file(note_id.clone(), workspace.clone()),
            }
        }
        crate::telemetry::notes_event("notes.search.result_opened", home.telemetry());
        self.close_overlay(window, cx);
        cx.emit(NoteOpened {
            note_id,
            workspace,
            block,
        });
    }

    /// The notes ⌘K shows above its commands for a non-empty query.
    pub(super) fn palette_note_actions(&mut self) -> Vec<Ranked<PaletteAction>> {
        if self.query.text().trim().is_empty() {
            return Vec::new();
        }
        let entries = Arc::clone(&self.notes.entries);
        self.notes
            .search
            .rank(&entries, self.query.text(), PALETTE_NOTE_LIMIT)
            .into_iter()
            .map(|hit| {
                let entry = &entries[hit.entry];
                let archived = matches!(
                    self.note_home(&entry.workspace, &entry.id),
                    NoteHome::Archived(_)
                );
                Ranked {
                    item: PaletteAction {
                        id: format!("note:{}", entry.id),
                        title: entry.title.clone(),
                        system_image: "doc.text",
                        shortcut: None,
                        detail: Some(if archived { "Archived note" } else { "Note" }.into()),
                        enabled: true,
                        is_default: false,
                        agent_kind: None,
                        command: PaletteCommand::OpenNote {
                            note_id: entry.id.clone(),
                            workspace: entry.workspace.clone(),
                            block: hit.snippet.as_ref().map(|snippet| snippet.block),
                        },
                        keywords: String::new(),
                    },
                    title_matches: hit.title_ranges,
                    score: 0,
                }
            })
            .collect()
    }

    pub(super) fn render_note_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors();
        let hit = self.notes.hits[index].clone();
        let entry = &self.notes.entries[hit.entry];
        let selected = index == self.highlight;
        let archived = matches!(
            self.note_home(&entry.workspace, &entry.id),
            NoteHome::Archived(_)
        );
        let project = entry
            .project
            .as_deref()
            .and_then(|root| Path::new(root).file_name())
            .map(|name| name.to_string_lossy().into_owned());
        let mut meta: Vec<String> = Vec::new();
        if archived {
            meta.push("Archived".to_owned());
        }
        if let Some(project) = &project {
            meta.push(project.clone());
        }
        if entry.open_todos > 0 {
            meta.push(format!(
                "{} to-do{}",
                entry.open_todos,
                if entry.open_todos == 1 { "" } else { "s" }
            ));
        }
        let detail = {
            let mut lines = vec![entry.title.clone()];
            if !meta.is_empty() {
                lines.push(meta.join(" · "));
            }
            if archived {
                lines.push("Opening restores it from the archive".to_owned());
            }
            lines.join("\n")
        };
        let marks: Vec<ubra_ui::AgentKind> = {
            let store = self.store.read().expect("session store lock poisoned");
            entry
                .linked
                .iter()
                .filter_map(|id| store.sessions().get(&SessionId(id.clone())))
                .map(|record| ui_agent_kind(record.effective_kind()))
                .take(LINKED_MARKS)
                .collect()
        };
        // The second line: where the query matched, else how the note opens.
        let (snippet, snippet_ranges) = match &hit.snippet {
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
        let age = relative_time_ms(entry.modified_ms);
        let title_tone = if selected {
            colors.primary
        } else {
            colors.text(ubra_ui::TextTone::Unselected)
        };
        let note_agent = ui_agent_kind(&ubra_proto::AgentKind::NOTE);
        div()
            .h(px(NOTE_ROW_HEIGHT))
            .px(px(6.0))
            .py(px(2.0))
            .child(
                div()
                    .id(("note-row", index))
                    .debug_selector(move || format!("note-row-{index}"))
                    .group("note-row")
                    .h_full()
                    .px(px(9.0))
                    .rounded(px(Radius::inner(
                        super::PALETTE_RADIUS,
                        super::PALETTE_ROW_INSET,
                    )))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .glass_menu_row(colors, selected)
                    .cursor_pointer()
                    .active(move |style| style.opacity(0.74))
                    .warm_tooltip(move |_, cx| {
                        cx.new(|_| PaletteTooltip(detail.clone(), colors)).into()
                    })
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && this.highlight != index {
                            this.highlight = index;
                            cx.notify();
                        }
                    }))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            let keep_caret = event.modifiers().platform;
                            this.in_main(window, cx, move |this, window, cx| {
                                this.highlight = index;
                                this.open_highlighted_note(keep_caret, window, cx);
                            })
                        }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .when(archived, |logo| logo.opacity(0.55))
                            .child(AgentLogo::new(note_agent, 28.0, colors).badged(false)),
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
                                                title_tone
                                            })
                                            .truncate()
                                            .child(highlighted_label(
                                                entry.title.clone(),
                                                &hit.title_ranges,
                                            )),
                                    )
                                    .when(!meta.is_empty(), |line| {
                                        line.child(
                                            div()
                                                .flex_none()
                                                .text_size(px(Typo::META.size))
                                                .text_color(colors.tertiary)
                                                .child(meta.join(" · ")),
                                        )
                                    })
                                    .children(marks.into_iter().map(|agent| {
                                        div().flex_none().child(
                                            AgentLogo::new(agent, 16.0, colors).badged(false),
                                        )
                                    })),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size + 1.0))
                                    .text_color(colors.tertiary)
                                    .when(archived, |line| line.opacity(0.7))
                                    .truncate()
                                    .child(highlighted_label(snippet, &snippet_ranges)),
                            ),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .w(px(KEYCAP_WIDTH))
                            .h(px(KEYCAP_HEIGHT))
                            .flex()
                            .items_center()
                            .justify_end()
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .when(selected, |age| age.invisible())
                                    .group_hover("note-row", |style| style.invisible())
                                    .child(age),
                            )
                            .child(
                                keycap(colors)
                                    .debug_selector(move || format!("note-return-{index}"))
                                    .absolute()
                                    .right_0()
                                    .top_0()
                                    .when(!selected, |cue| cue.invisible())
                                    .group_hover("note-row", |style| style.visible())
                                    .child(Icon::new(IconName::Return, 14.0, colors.secondary)),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn relative_time_ms(milliseconds: u64) -> String {
    super::history_page::relative_time(milliseconds as f64)
}
