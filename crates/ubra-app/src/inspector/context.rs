use super::sidebar::{button, edit_query, field, message, panel};
use super::*;
use crate::prompt_draft::{AttachmentSource, PromptAttachment};

/// Identity of an actually painted Context snapshot, scoped to its display frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ContextPreviewFrame {
    recipient: SessionId,
    generation: u64,
    revision: u64,
    window: gpui::AnyWindowHandle,
    transition: u64,
    visibility: u64,
}

impl WorkbenchInspector {
    pub(super) fn render_context(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(session) = self.selected_session() else {
            return panel("context-panel", colors)
                .child(message("No inspected recipient", colors))
                .into_any_element();
        };
        let id = session.id;
        let draft = self.runtime.prompt_drafts.shared_snapshot(&id);
        let instructions = self
            .sidebar
            .instructions
            .entry(id.clone())
            .or_default()
            .display("│")
            .0;
        let mut view=panel("context-panel",colors)
            .child(message(format!("Next explicit prompt to {} · {} · draft revision {}",session.title,id.0,draft.revision),colors))
            .child(message("Staging does not send. Remove and Clear change this recipient's real next-prompt queue. Notes stay editable only in Notes.",colors));
        if !draft.text.is_empty() {
            view = view
                .child(section_label("Composer text", colors))
                .child(message(draft.text.clone(), colors));
        }
        let target = id.clone();
        view = view.child(button(
            "context-compose",
            format!("Compose prompt for {}", session.title),
            colors,
            window,
            cx,
            move |_, _, cx| cx.emit(InspectorEvent::ComposePrompt(target.clone())),
        ));
        let target = id.clone();
        view = view.child(sidebar::button_with_focus(
            "context-clear",
            "Clear pending context",
            colors,
            self.sidebar.context_clear_focus.clone(),
            cx,
            move |this, _, cx| {
                this.runtime.prompt_drafts.clear(&target);
                this.sidebar
                    .preview_offsets
                    .retain(|(session, _), _| session != &target);
                cx.notify();
            },
        ));
        let target = id.clone();
        view = view.child(button(
            "context-refresh",
            "Refresh source previews",
            colors,
            window,
            cx,
            move |this, _, cx| {
                if this.selected_session().as_ref().map(|s| &s.id) == Some(&target) {
                    this.refresh_context_preview(cx);
                }
            },
        ));
        if let Some(error) = self.sidebar.feedback.get(&id) {
            view = view.child(message(error.clone(), colors));
        }
        if draft.attachments.is_empty() {
            view=view.child(message("No pending attachments. Attach from Files, a selected quote, or Notes; add authored instructions below.",colors));
        }
        for attachment in &draft.attachments {
            view = view.child(
                div()
                    .id(SharedString::from(format!(
                        "context-attachment-{}",
                        attachment.id
                    )))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(message(attachment.label.clone(), colors))
                    .child(message(
                        format!(
                            "Source {:?} · revision {}{}",
                            attachment.source,
                            attachment.source_revision.as_deref().unwrap_or("snapshot"),
                            if attachment.local_only {
                                " · local recipient only"
                            } else {
                                ""
                            }
                        ),
                        colors,
                    )),
            );
            if let Some(error) = &attachment.error {
                view = view.child(message(
                    format!("Unavailable source; sending is blocked: {error}"),
                    colors,
                ));
            }
            let key = (id.clone(), attachment.id.clone());
            let offset = self
                .sidebar
                .preview_offsets
                .get(&key)
                .copied()
                .unwrap_or(0)
                .min(attachment.content.len());
            let (start, end) = preview_range(&attachment.content, offset);
            view = view
                .child(message(
                    format!(
                        "Preview bytes {start}–{end} of {}",
                        attachment.content.len()
                    ),
                    colors,
                ))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "context-preview-{}",
                            attachment.id
                        )))
                        .min_w_0()
                        .overflow_x_scroll()
                        .font_family(crate::fonts::mono_family())
                        .text_sm()
                        .child(attachment.content[start..end].to_owned()),
                );
            if end < attachment.content.len() {
                let key = key.clone();
                view = view.child(button(
                    format!("context-next-{}", attachment.id),
                    "Next preview page",
                    colors,
                    window,
                    cx,
                    move |this, _, cx| {
                        this.sidebar.preview_offsets.insert(key.clone(), end);
                        cx.notify();
                    },
                ));
            }
            if start > 0 {
                view = view.child(button(
                    format!("context-beginning-{}", attachment.id),
                    "Preview beginning",
                    colors,
                    window,
                    cx,
                    move |this, _, cx| {
                        this.sidebar.preview_offsets.remove(&key);
                        cx.notify();
                    },
                ));
            }
            if let AttachmentSource::Note(source) = &attachment.source {
                let source = source.clone();
                view = view.child(button(
                    format!("context-open-note-{}", attachment.id),
                    "Open in Notes",
                    colors,
                    window,
                    cx,
                    move |_, _, cx| {
                        cx.emit(InspectorEvent::OpenNote {
                            note_id: source.note_id.clone(),
                            workspace: source.workspace.clone(),
                        })
                    },
                ));
            }
            let target = id.clone();
            let attachment_id = attachment.id.clone();
            view = view.child(button(
                format!("context-remove-{}", attachment.id),
                "Remove",
                colors,
                window,
                cx,
                move |this, _, cx| {
                    this.runtime.prompt_drafts.remove(&target, &attachment_id);
                    this.sidebar
                        .preview_offsets
                        .remove(&(target.clone(), attachment_id.clone()));
                    cx.notify();
                },
            ));
        }
        let target = id.clone();
        view = view.child(field(
            format!("context-instructions-{}", id.0),
            "Authored instructions (ordinary prompt text)",
            instructions,
            Some(self.sidebar.context_focus.clone()),
            colors,
            window,
            cx,
            move |this, key, cx| {
                edit_query(
                    this.sidebar.instructions.entry(target.clone()).or_default(),
                    key,
                    cx,
                )
            },
        ));
        let target = id.clone();
        view = view.child(button(
            "context-add-instructions",
            format!("Attach instructions to {}", session.title),
            colors,
            window,
            cx,
            move |this, _, cx| {
                let text = this
                    .sidebar
                    .instructions
                    .entry(target.clone())
                    .or_default()
                    .text();
                match PromptAttachment::instructions(text)
                    .and_then(|a| this.runtime.prompt_drafts.stage(&target, a))
                {
                    Ok(_) => {
                        this.sidebar
                            .instructions
                            .entry(target.clone())
                            .or_default()
                            .clear();
                        this.sidebar.feedback.remove(&target);
                    }
                    Err(e) => {
                        this.sidebar.feedback.insert(target.clone(), e);
                    }
                }
                cx.notify();
            },
        ));
        let frame = ContextPreviewFrame {
            recipient: id,
            generation: self.sidebar.generation,
            revision: draft.revision,
            window: window.window_handle(),
            transition: self.tab_transition_generation,
            visibility: self.context_preview_epoch,
        };
        let inspector = cx.weak_entity();
        view.relative()
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        if !bounds.intersects(&window.content_mask().bounds) {
                            return;
                        }
                        let _ = inspector.update(cx, |this, cx| {
                            if !this.accepts_context_frame(&frame, window)
                                || this.context_preview_pending.is_some()
                                || this.context_preview_frame.as_ref() == Some(&frame)
                            {
                                return;
                            }
                            this.context_preview_pending = Some(frame.clone());
                            cx.on_next_frame(window, move |this, window, cx| {
                                if this.context_preview_pending.as_ref() != Some(&frame) {
                                    return;
                                }
                                this.context_preview_pending = None;
                                if !this.accepts_context_frame(&frame, window) {
                                    // A newer frame may have painted while the one bounded
                                    // callback was pending. Let that current view schedule its
                                    // own acceptance; never refresh sources from a frame hook.
                                    if this.visible
                                        && this.workspace_selected
                                            == Some(WorkspaceSurface::Context)
                                    {
                                        cx.notify();
                                    }
                                    return;
                                }
                                // Acknowledge only the painted revision. prepare() refreshes
                                // Note sources under the draft lock and refuses any later edit,
                                // including a live or persisted edit not yet loaded by this view.
                                this.context_preview_frame = Some(frame.clone());
                                if !this
                                    .runtime
                                    .prompt_drafts
                                    .acknowledge_preview(&frame.recipient, draft.revision)
                                {
                                    cx.notify();
                                }
                            });
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }

    fn accepts_context_frame(&self, frame: &ContextPreviewFrame, window: &Window) -> bool {
        self.visible
            && self.workspace_selected == Some(WorkspaceSurface::Context)
            && self.accepts_sidebar_response(&frame.recipient, frame.generation)
            && self.tab_transition_generation == frame.transition
            && self.context_preview_epoch == frame.visibility
            && window.window_handle() == frame.window
    }

    pub(super) fn refresh_context_preview(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_session().map(|s| s.id) else {
            return;
        };
        let drafts = self.runtime.prompt_drafts.clone();
        let target = id.clone();
        let generation = self.sidebar.generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move { drafts.refresh_sources_shared(&target) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.visible
                    && this.workspace_selected == Some(WorkspaceSurface::Context)
                    && this.accepts_sidebar_response(&id, generation)
                {
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(super) fn attach_displayed_file(&mut self, recipient: SessionId, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session().filter(|s| s.id == recipient) else {
            return;
        };
        if session.host.is_some() {
            self.sidebar.feedback.insert(
                session.id,
                "Local file snapshots cannot be attached to a remote recipient".into(),
            );
            cx.notify();
            return;
        }
        match self
            .code_viewer
            .read(cx)
            .export_prompt_snapshot()
            .and_then(|a| self.runtime.prompt_drafts.stage(&session.id, a))
        {
            Ok(_) => {
                self.sidebar.feedback.remove(&session.id);
            }
            Err(e) => {
                self.sidebar.feedback.insert(session.id, e);
            }
        }
        cx.notify();
    }
}

fn preview_range(text: &str, offset: usize) -> (usize, usize) {
    let mut start = offset.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + 16 * 1024).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_pages_preserve_multibyte_content_without_overlapping_bytes() {
        let text = "漢字".repeat(5000);
        let mut offset = 0;
        let mut restored = String::new();
        while offset < text.len() {
            let (start, end) = preview_range(&text, offset);
            assert!(end > start);
            assert!(end - start <= 16 * 1024);
            restored.push_str(&text[start..end]);
            offset = end;
        }
        assert_eq!(restored, text);
    }
}
