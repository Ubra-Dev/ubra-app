//! Version history for the open note: a small glass panel listing earlier
//! versions (when, by whom, what changed) with a read-only preview, and a
//! Restore that asks first through the system alert sheet.
//!
//! The panel belongs to the note pane, not the editor: it floats over the
//! note like the pane's other menus and never takes the keyboard from the
//! editor. Escape closes it. Restoring saves pending typing first, and the
//! text it replaces stays in history, so a restore can be undone the same way.

use gpui::{
    Anchor, AnyElement, Context, MouseButton, MouseDownEvent, SharedString, Window, anchored,
    deferred, div, point, prelude::*, px,
};
use ubra_notes::doc::{BlockKind, Document};
use ubra_notes::history::{self, Author, Version};

use super::{NotePane, PaneState};
use crate::floating;

const PANEL_WIDTH: f32 = 580.0;
const LIST_WIDTH: f32 = 236.0;
const PANEL_HEIGHT: f32 = 360.0;
const PANEL_MARGIN: f32 = 12.0;
/// Enough to judge a version at a glance; the whole text is one restore away.
const PREVIEW_BLOCKS: usize = 200;

pub(super) struct VersionPanel {
    note_id: String,
    title: String,
    pub(super) versions: Vec<Version>,
    selected: usize,
    /// The selected version, shown as readable text rather than Markdown.
    pub(super) preview: Document,
}

const PANEL: floating::Target<NotePane> = floating::Target {
    key: "note-version-history",
    radius: floating::MENU_RADIUS,
    content: NotePane::versions_content,
    dismiss: |this, _, cx| this.close_versions(cx),
};

impl NotePane {
    /// Opens the panel for the open note (the "Version history…" command).
    pub(crate) fn open_versions(
        &mut self,
        _: &crate::commands::NoteVersionHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Pending typing becomes the newest version before it is compared.
        self.save(cx);
        let PaneState::Open(open) = &self.state else {
            return;
        };
        let store = open.store.clone();
        let versions = store.history().list(&open.id).unwrap_or_default();
        let title = open.editor.read(cx).editor.title().trim().to_owned();
        let mut panel = VersionPanel {
            note_id: open.id.clone(),
            title: if title.is_empty() {
                "Untitled".into()
            } else {
                title
            },
            versions,
            selected: 0,
            preview: Document::new("", Vec::new()),
        };
        panel.preview = preview(&store, &panel);
        self.versions = Some(panel);
        cx.notify();
    }

    pub(super) fn close_versions(&mut self, cx: &mut Context<Self>) {
        if self.versions.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn versions_open(&self) -> bool {
        self.versions.is_some()
    }

    pub(crate) fn select_version(&mut self, index: usize, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        let store = open.store.clone();
        let Some(panel) = &mut self.versions else {
            return;
        };
        if index < panel.versions.len() && index != panel.selected {
            panel.selected = index;
            panel.preview = preview(&store, panel);
            cx.notify();
        }
    }

    /// Asks, then restores the selected version.
    fn confirm_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &self.versions else {
            return;
        };
        let Some(version) = panel.versions.get(panel.selected) else {
            return;
        };
        let answer = window.prompt(
            gpui::PromptLevel::Info,
            &format!("Restore the version from {}?", when(version.id)),
            Some("The note's current text is kept in its version history, so you can go back."),
            &[
                gpui::PromptButton::ok("Restore"),
                gpui::PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        let version = version.id;
        let editor_id = match &self.state {
            PaneState::Open(open) => open.editor.entity_id(),
            _ => return,
        };
        cx.spawn(async move |this, cx| {
            if answer.await.ok() == Some(0) {
                let _ = this.update(cx, |this, cx| {
                    if matches!(&this.state, PaneState::Open(open) if open.editor.entity_id() == editor_id) {
                        this.restore_version(version, cx);
                    }
                });
            }
        })
        .detach();
    }

    pub(super) fn restore_version(&mut self, version: u64, cx: &mut Context<Self>) {
        let PaneState::Open(open) = &self.state else {
            return;
        };
        let store = open.store.clone();
        let Some(note_id) = self.versions.as_ref().map(|panel| panel.note_id.clone()) else {
            return;
        };
        self.reconcile(cx);
        self.save(cx);
        // Saving detects a removed file and releases the editor. Do not
        // recreate it from history after a trash or external deletion.
        if !matches!(&self.state, PaneState::Open(open) if open.id == note_id) {
            return;
        }
        match store.restore_version(&note_id, version, &Author::User) {
            Ok(_) => {
                self.versions = None;
                self.reconcile(cx);
                self.error = None;
            }
            Err(err) => {
                self.error = Some(format!("Couldn't restore that version: {err}").into());
            }
        }
        cx.notify();
    }

    /// The panel for this frame, anchored at the top right of the note.
    pub(super) fn versions_element(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.versions.as_ref()?;
        let viewport = window.viewport_size();
        let position = point(viewport.width - px(PANEL_MARGIN), px(PANEL_MARGIN + 40.0));
        let dismiss =
            |this: &mut Self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>| {
                this.close_versions(cx);
            };
        let scrim = deferred(
            anchored().position(point(px(0.0), px(0.0))).child(
                div()
                    .w(viewport.width)
                    .h(viewport.height)
                    .occlude()
                    .on_mouse_down(MouseButton::Left, cx.listener(dismiss))
                    .on_mouse_down(MouseButton::Right, cx.listener(dismiss)),
            ),
        )
        .with_priority(1);
        let host = div().absolute().inset_0();
        let colors = self.colors();
        if floating::uses_panels(false, colors, cx) {
            let probe = Self::versions_content(self, cx)?;
            let panel = floating::host_element(
                PANEL,
                probe,
                PANEL_WIDTH,
                floating::PopupOrigin::Control {
                    position,
                    anchor: Anchor::TopRight,
                },
                PANEL_MARGIN,
                window,
                cx,
            );
            return Some(host.child(panel).child(scrim).into_any_element());
        }
        let content = ubra_ui::FloatingSurface::new(colors, self.versions_body(cx)?)
            .radius(floating::MENU_RADIUS);
        Some(
            host.child(scrim)
                .child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopRight)
                            .position(position)
                            .snap_to_window_with_margin(px(PANEL_MARGIN))
                            .child(div().occlude().child(content)),
                    )
                    .with_priority(2),
                )
                .into_any_element(),
        )
    }

    fn versions_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = self.colors();
        let body = self.versions_body(cx)?;
        Some(floating::surface(colors, floating::MENU_RADIUS, PANEL_WIDTH, body).into_any_element())
    }

    fn versions_body(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let colors = self.colors();
        let titles: std::collections::HashMap<String, String> = {
            let store = self.runtime.store.read().expect("store");
            store
                .sessions()
                .values()
                .map(|record| (record.id.0.clone(), record.title.clone()))
                .collect()
        };
        let agent_title = |id: &str| titles.get(id).cloned();
        let panel = self.versions.as_ref()?;
        let header = div()
            .px(px(floating::MENU_ROW_MARGIN + floating::MENU_ROW_INSET))
            .pt(px(10.0))
            .pb(px(6.0))
            .flex()
            .flex_col()
            .gap(px(1.0))
            .child(
                div()
                    .text_size(px(ubra_ui::Typo::ROW.size))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(colors.primary)
                    .child("Version History"),
            )
            .child(
                div()
                    .text_size(px(ubra_ui::Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(SharedString::from(panel.title.clone())),
            );
        if panel.versions.is_empty() {
            return Some(
                div().w(px(PANEL_WIDTH)).pb(px(12.0)).child(header).child(
                    div()
                        .px(px(floating::MENU_ROW_MARGIN + floating::MENU_ROW_INSET))
                        .text_size(px(ubra_ui::Typo::ROW.size))
                        .text_color(colors.secondary)
                        .child("No earlier versions yet. Ubra keeps one as you write, and before anyone else changes this note."),
                ),
            );
        }
        let mut list = div()
            .id("note-version-list")
            .w(px(LIST_WIDTH))
            .h_full()
            .flex_none()
            .overflow_y_scroll()
            .py(px(floating::MENU_PADDING_Y));
        for (index, version) in panel.versions.iter().enumerate() {
            let on = index == panel.selected;
            let row = floating::menu_row(
                ("note-version", index),
                crate::icons::sf_symbol(author_icon(&version.author), 12.0, colors.secondary),
                colors,
                on,
            )
            .h(px(44.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(ubra_ui::Typo::ROW.size))
                            .text_color(colors.primary)
                            .truncate()
                            .child(format!(
                                "{} · {}",
                                when(version.id),
                                who(&version.author, agent_title)
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(ubra_ui::Typo::META.size))
                            .text_color(colors.tertiary)
                            .truncate()
                            .child(version.summary.clone()),
                    ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.select_version(index, cx)),
            );
            list = list.child(row);
        }
        let preview = div()
            .id("note-version-preview")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px(px(14.0))
            .py(px(10.0))
            .text_size(px(ubra_ui::Typo::ROW.size))
            .line_height(px(19.0))
            .text_color(colors.secondary)
            .whitespace_normal()
            .child(preview_element(&panel.preview, colors));
        let restore = floating::menu_row(
            "note-version-restore",
            crate::icons::sf_symbol("arrow.counterclockwise", 12.0, colors.primary),
            colors,
            false,
        )
        .child(
            div()
                .text_size(px(ubra_ui::Typo::ROW.size))
                .text_color(colors.primary)
                .child("Restore This Version…"),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, window, cx| this.confirm_restore(window, cx)),
        );
        Some(
            div()
                .w(px(PANEL_WIDTH))
                .flex()
                .flex_col()
                .child(header)
                .child(ubra_ui::HairlineDivider::horizontal(colors))
                .child(
                    div()
                        .h(px(PANEL_HEIGHT))
                        .flex()
                        .child(list)
                        .child(ubra_ui::HairlineDivider::vertical(colors))
                        .child(preview),
                )
                .child(floating::menu_separator(colors))
                .child(div().pb(px(floating::MENU_PADDING_Y)).child(restore)),
        )
    }
}

fn preview(store: &ubra_notes::store::NoteStore, panel: &VersionPanel) -> Document {
    let Some(version) = panel.versions.get(panel.selected) else {
        return Document::new("", Vec::new());
    };
    match store.history().read(&panel.note_id, version.id) {
        Ok(source) => ubra_notes::markdown::parse(&source).1,
        Err(_) => Document::new("This version can't be read.", Vec::new()),
    }
}

/// A version's text the way the note reads, without Markdown marks: its
/// title, headings, to-dos with their boxes, lists, and quotes.
fn preview_element(doc: &Document, colors: ubra_ui::SemanticColors) -> gpui::Div {
    let mut body = div().flex().flex_col().gap(px(4.0));
    if !doc.title.is_empty() {
        body = body.child(
            div()
                .pb(px(4.0))
                .text_size(px(17.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(colors.primary)
                .child(doc.title.clone()),
        );
    }
    let mut number = 0;
    for block in doc.blocks.iter().take(PREVIEW_BLOCKS) {
        number = if block.kind == BlockKind::Numbered {
            number + 1
        } else {
            0
        };
        let marker = match block.kind {
            BlockKind::Todo { checked: true } => "☑ ".to_owned(),
            BlockKind::Todo { checked: false } => "☐ ".to_owned(),
            BlockKind::Bullet => "• ".to_owned(),
            BlockKind::Numbered => format!("{number}. "),
            _ => String::new(),
        };
        let line = div()
            .pl(px(f32::from(block.indent) * 16.0))
            .text_color(colors.secondary)
            .child(format!("{marker}{}", block.text));
        let line = match block.kind {
            BlockKind::Heading(_) => line
                .pt(px(6.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(colors.primary),
            BlockKind::Quote => line.italic().text_color(colors.tertiary),
            BlockKind::Todo { checked: true } => line.text_color(colors.tertiary),
            BlockKind::Code => line.font_family(crate::fonts::mono_family()),
            BlockKind::Divider => div().child(ubra_ui::HairlineDivider::horizontal(colors)),
            _ => line,
        };
        body = body.child(line);
    }
    body
}

/// "just now", "12 minutes ago", "3 hours ago", "2 days ago", else the date.
pub(super) fn when(ms: u64) -> String {
    let now = history::now_ms();
    let seconds = now.saturating_sub(ms) / 1000;
    let plural = |n: u64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match seconds {
        0..=59 => "Just now".into(),
        60..=3_599 => plural(seconds / 60, "minute"),
        3_600..=86_399 => plural(seconds / 3_600, "hour"),
        86_400..=604_799 => plural(seconds / 86_400, "day"),
        _ => history::describe_time(ms)[..10].to_owned(),
    }
}

/// "You", "Command line", or the agent's name as the sidebar shows it.
fn who(author: &Author, agent_title: impl Fn(&str) -> Option<String>) -> String {
    match author {
        Author::User => "You".into(),
        Author::Cli => "Command line".into(),
        Author::File => "Edited file".into(),
        Author::Session(id) => agent_title(id).unwrap_or_else(|| "An agent".into()),
    }
}

fn author_icon(author: &Author) -> &'static str {
    match author {
        Author::User => "doc.text",
        Author::Cli => "terminal",
        Author::File => "pencil",
        Author::Session(_) => "sparkle",
    }
}
