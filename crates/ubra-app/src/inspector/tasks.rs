use super::sidebar::{
    PageLoad, PageRequest, Snapshot, button, edit_query, field, message, panel, update_snapshot,
};
use super::*;
use ubra_proto::tasks::*;

#[derive(Default)]
pub(super) struct TasksState {
    pub snapshot: Snapshot<SessionTasksResult>,
    pub snapshot_error: Option<String>,
    pub checklists: Snapshot<Vec<crate::prompt_draft::LinkedChecklist>>,
    pub checklist_error: Option<String>,
    pub page_load: PageLoad<i64>,
    pub checklist_offset: usize,
    pub answers: HashMap<String, QueryEditor>,
    pub busy: bool,
    pub feedback: Option<String>,
}

impl TasksState {
    pub(super) fn settle_page(
        &mut self,
        id: &SessionId,
        request: &PageRequest<i64>,
        result: Result<SessionTasksResult, String>,
    ) -> bool {
        if !self.page_load.settle(request) {
            return false;
        }
        let result = result.and_then(|page| {
            if page.session_id == id.0 {
                Ok(page)
            } else {
                Err("Task page identity mismatch".into())
            }
        });
        self.snapshot_error = update_snapshot(&mut self.snapshot, result);
        true
    }

    fn apply_checklists(
        &mut self,
        result: Result<Vec<crate::prompt_draft::LinkedChecklist>, String>,
    ) {
        self.checklist_error = update_snapshot(&mut self.checklists, result);
    }
}
impl WorkbenchInspector {
    fn choose_current_task(
        &mut self,
        id: SessionId,
        task_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let state = self.sidebar.tasks.entry(id.clone()).or_default();
        if state.busy {
            return;
        }
        state.busy = true;
        state.page_load.latest();
        self.sidebar.generation = self.sidebar.generation.wrapping_add(1);
        self.sidebar.load = None;
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        cx.spawn(async move |this, cx| {
            let request_id = id.clone();
            let result = tokio
                .spawn(async move {
                    client
                        .set_current_task(&request_id, task_id)
                        .await
                        .map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| v);
            let _ = this.update(cx, |this, cx| {
                let state = this.sidebar.tasks.entry(id.clone()).or_default();
                state.busy = false;
                match result {
                    Ok(page) if page.session_id == id.0 => {
                        state.page_load.latest();
                        state.snapshot_error = None;
                        state.snapshot = Snapshot::Ready(page);
                    }
                    Ok(_) => state.feedback = Some("Task mutation identity mismatch".into()),
                    Err(e) => state.feedback = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn answer_task(&mut self, id: SessionId, task_id: String, cx: &mut Context<Self>) {
        let state = self.sidebar.tasks.entry(id.clone()).or_default();
        if state.busy {
            return;
        }
        let text = state
            .answers
            .entry(task_id.clone())
            .or_default()
            .text()
            .to_owned();
        if text.trim().is_empty() {
            state.feedback = Some("Write an answer first".into());
            cx.notify();
            return;
        }
        let allowed = matches!(&state.snapshot,Snapshot::Ready(page) if page.tasks.iter().chain(page.current_task.iter()).any(|t|t.task_id==task_id && t.sender_id==id.0 && t.status==TaskStatus::Blocked));
        if !allowed {
            state.feedback = Some("Only this task's sender can answer its blocker".into());
            cx.notify();
            return;
        }
        state.busy = true;
        let params = TaskAnswerParams {
            caller_id: id.0.clone(),
            task_id: task_id.clone(),
            text,
        };
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        cx.spawn(async move |this, cx| {
            let result = tokio
                .spawn(async move { client.task_answer(params).await.map_err(|e| e.to_string()) })
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| v);
            let _ = this.update(cx, |this, cx| {
                let state = this.sidebar.tasks.entry(id.clone()).or_default();
                state.busy = false;
                match result {
                    Ok(answer)
                        if answer.task.task_id == task_id && answer.task.sender_id == id.0 =>
                    {
                        state.feedback = Some(format!("Answer delivery: {}", answer.delivery));
                        if answer.ok {
                            state.answers.remove(&task_id);
                        }
                    }
                    Ok(_) => state.feedback = Some("Task answer identity mismatch".into()),
                    Err(e) => state.feedback = Some(e),
                }
                if this.selected_session().as_ref().map(|s| &s.id) == Some(&id) {
                    this.refresh_sidebar(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn load_older_tasks(&mut self, id: SessionId, cursor: i64, cx: &mut Context<Self>) {
        let state = self.sidebar.tasks.entry(id.clone()).or_default();
        if state.page_load.loading && state.page_load.cursor == Some(cursor) {
            return;
        }
        let request = state.page_load.begin(Some(cursor));
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let params = SessionTasksParams {
                session_id: id.0.clone(),
                limit: Some(50),
                cursor: Some(cursor),
            };
            let result = tokio
                .spawn(async move {
                    client
                        .session_tasks_page(params)
                        .await
                        .map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| v);
            let _ = this.update(cx, |this, cx| {
                if this.selected_session().as_ref().map(|s| &s.id) != Some(&id) {
                    return;
                }
                let state = this.sidebar.tasks.entry(id.clone()).or_default();
                if !state.settle_page(&id, &request, result) {
                    return;
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn refresh_linked_checklists(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_session().map(|s| s.id) else {
            return;
        };
        let snapshot = &mut self.sidebar.tasks.entry(id.clone()).or_default().checklists;
        if !matches!(snapshot, Snapshot::Ready(_)) {
            *snapshot = Snapshot::Loading;
        }
        let runtime = self.runtime.clone();
        let target = id.clone();
        let generation = self.sidebar.generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { runtime.linked_checklists(&target) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.accepts_sidebar_response(&id, generation) {
                    return;
                }
                this.sidebar
                    .tasks
                    .entry(id)
                    .or_default()
                    .apply_checklists(result);
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn render_tasks(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(session) = self.selected_session() else {
            return panel("tasks-panel", colors)
                .child(message("No inspected session", colors))
                .into_any_element();
        };
        let id = session.id;
        let state = self.sidebar.tasks.entry(id.clone()).or_default();
        let refresh_target = id.clone();
        let mut view=panel("tasks-panel",colors).child(message("Task receipts are explicit. Idle, turn end, and terminal prose do not complete tasks.",colors))
            .child(sidebar::button_with_focus("tasks-refresh","Newest receipt page",colors,self.sidebar.tasks_focus.clone(),cx,move|this,_,cx|{this.sidebar.tasks.entry(refresh_target.clone()).or_default().page_load.latest();this.refresh_sidebar_for(&refresh_target,cx);}));
        if let Some(feedback) = &state.feedback {
            view = view.child(message(feedback.clone(), colors));
        }
        if let Some(error) = &state.snapshot_error {
            view = view.child(message(
                format!("Receipt page refresh failed: {error}. Previous observed data, if shown, is stale. Retry with Newest receipt page."),
                colors,
            ));
        }
        match &state.snapshot {
            Snapshot::Empty => {
                view = view.child(message("Open or refresh to read task receipts", colors))
            }
            Snapshot::Loading => view = view.child(message("Loading receipts…", colors)),
            Snapshot::Failed(e) => {
                view = view.child(message(
                    format!("Could not read task receipts: {e}"),
                    colors,
                ))
            }
            Snapshot::Ready(page) => {
                view = view.child(section_label("Current acknowledged task", colors));
                if let Some(task) = &page.current_task {
                    view = view.child(message(
                        format!(
                            "{} · {:?} · {}",
                            task.title.as_deref().unwrap_or(&task.task_id),
                            task.status,
                            task.task_id
                        ),
                        colors,
                    ));
                    let target = id.clone();
                    view = view.child(button(
                        "task-clear-current",
                        "Clear current selection",
                        colors,
                        window,
                        cx,
                        move |this, _, cx| this.choose_current_task(target.clone(), None, cx),
                    ));
                } else {
                    view = view.child(message("No task explicitly selected", colors));
                }
                for (heading, terminal, blocked) in [
                    ("Pending and acknowledged", false, false),
                    ("Blocked questions and results", false, true),
                    ("Completed, failed and cancelled", true, false),
                ] {
                    view = view.child(section_label(heading, colors));
                    let mut count = 0;
                    for task in page
                        .tasks
                        .iter()
                        .chain(page.current_task.iter())
                        .filter(|t| {
                            t.status.is_terminal() == terminal
                                && (terminal || (t.status == TaskStatus::Blocked) == blocked)
                        })
                    {
                        count += 1;
                        view = view.child(
                            div()
                                .id(SharedString::from(format!("task-receipt-{}", task.task_id)))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(message(
                                    format!(
                                        "{} · {:?}",
                                        task.title.as_deref().unwrap_or(&task.task_id),
                                        task.status
                                    ),
                                    colors,
                                ))
                                .child(message(
                                    format!(
                                        "{} · sender {} → assignee {} · revision {} · delivery {}",
                                        task.task_id,
                                        task.sender_id,
                                        task.session_id,
                                        task.revision,
                                        task.delivery
                                    ),
                                    colors,
                                )),
                        );
                        if let Some(at) = task.updated_at_ms {
                            view = view.child(message(
                                format!("Updated {}", relative_time(at as f64)),
                                colors,
                            ));
                        }
                        if let Some(result) = &task.result {
                            view = view.child(message(result.clone(), colors));
                        }
                        for update in task.updates.iter().filter(|u| u.text.is_some()) {
                            view = view.child(message(
                                format!(
                                    "{} · {} · {}",
                                    update.kind,
                                    update.by,
                                    update.text.as_deref().unwrap_or_default()
                                ),
                                colors,
                            ));
                        }
                        if task.session_id == id.0
                            && page.current_task_id.as_deref() != Some(task.task_id.as_str())
                            && matches!(task.status, TaskStatus::Acknowledged | TaskStatus::Blocked)
                        {
                            let target = id.clone();
                            let tid = task.task_id.clone();
                            view = view.child(button(
                                format!("task-current-{tid}"),
                                "Make current",
                                colors,
                                window,
                                cx,
                                move |this, _, cx| {
                                    this.choose_current_task(target.clone(), Some(tid.clone()), cx)
                                },
                            ));
                        }
                        if task.sender_id == id.0 && task.status == TaskStatus::Blocked {
                            let tid = task.task_id.clone();
                            let text = state
                                .answers
                                .get(&tid)
                                .map(|q| q.display("│").0)
                                .unwrap_or_default();
                            let target = id.clone();
                            let edit_id = tid.clone();
                            view = view.child(field(
                                format!("task-answer-field-{tid}"),
                                "Answer as task sender",
                                text,
                                None,
                                colors,
                                window,
                                cx,
                                move |this, key, cx| {
                                    edit_query(
                                        this.sidebar
                                            .tasks
                                            .entry(target.clone())
                                            .or_default()
                                            .answers
                                            .entry(edit_id.clone())
                                            .or_default(),
                                        key,
                                        cx,
                                    )
                                },
                            ));
                            let target = id.clone();
                            view = view.child(button(
                                format!("task-answer-{tid}"),
                                "Send answer",
                                colors,
                                window,
                                cx,
                                move |this, _, cx| {
                                    this.answer_task(target.clone(), tid.clone(), cx)
                                },
                            ));
                        }
                    }
                    if count == 0 {
                        view = view.child(message("No receipts in this section", colors));
                    }
                }
                if let Some(cursor) = page.next_cursor {
                    let target = id.clone();
                    view = view.child(button(
                        "tasks-load-older",
                        if state.page_load.loading {
                            "Loading older page…"
                        } else {
                            "Older receipt page"
                        },
                        colors,
                        window,
                        cx,
                        move |this, _, cx| this.load_older_tasks(target.clone(), cursor, cx),
                    ));
                }
            }
        }
        view = view.child(section_label("Authored Notes checklists", colors));
        if let Some(error) = &state.checklist_error {
            view = view.child(message(
                format!("Authored checklist refresh failed: {error}. Previous observed items, if shown, are stale."),
                colors,
            ));
            view = view.child(button(
                "checklists-retry",
                "Retry authored checklists",
                colors,
                window,
                cx,
                |this, _, cx| this.refresh_linked_checklists(cx),
            ));
        }
        match &state.checklists {
            Snapshot::Ready(rows) if rows.is_empty() => {
                view = view.child(message(
                    "No authored Notes checklist linked to this session",
                    colors,
                ))
            }
            Snapshot::Ready(rows) => {
                let offset =
                    (state.checklist_offset / 50 * 50).min(rows.len().saturating_sub(1) / 50 * 50);
                view = view.child(message(
                    format!(
                        "Authored items {}–{} of {}",
                        offset + 1,
                        (offset + 50).min(rows.len()),
                        rows.len()
                    ),
                    colors,
                ));
                for row in rows.iter().skip(offset).take(50) {
                    view = view.child(message(
                        format!(
                            "{} · {} · {}",
                            if row.checked {
                                "✓ Completed authored item"
                            } else {
                                "○ Unchecked authored item"
                            },
                            row.title,
                            row.text
                        ),
                        colors,
                    ));
                    let source = row.source.clone();
                    let block = row.block;
                    view = view.child(button(
                        format!("task-checklist-{}-{block}", source.session_id.0),
                        "Open checklist item in Notes",
                        colors,
                        window,
                        cx,
                        move |_, _, cx| {
                            cx.emit(InspectorEvent::OpenChecklist {
                                source: source.clone(),
                                block,
                            })
                        },
                    ));
                }
                if offset + 50 < rows.len() {
                    let target = id.clone();
                    view = view.child(button(
                        "checklists-next",
                        "Next authored checklist page",
                        colors,
                        window,
                        cx,
                        move |this, _, cx| {
                            this.sidebar
                                .tasks
                                .entry(target.clone())
                                .or_default()
                                .checklist_offset = offset + 50;
                            cx.notify();
                        },
                    ));
                }
                if offset > 0 {
                    let target = id.clone();
                    view = view.child(button(
                        "checklists-previous",
                        "Previous authored checklist page",
                        colors,
                        window,
                        cx,
                        move |this, _, cx| {
                            this.sidebar
                                .tasks
                                .entry(target.clone())
                                .or_default()
                                .checklist_offset = offset.saturating_sub(50);
                            cx.notify();
                        },
                    ));
                }
            }
            Snapshot::Failed(e) => {
                view = view.child(message(
                    format!("Could not read authored checklists: {e}"),
                    colors,
                ))
            }
            _ => view = view.child(message("Reading linked authored checklists…", colors)),
        }
        view.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &SessionId, next_cursor: Option<i64>) -> SessionTasksResult {
        SessionTasksResult {
            session_id: id.0.clone(),
            tasks: Vec::new(),
            current_task_id: None,
            current_task: None,
            next_cursor,
        }
    }

    #[test]
    fn older_navigation_rejects_delayed_newest_completion_in_either_order() {
        let id = SessionId::new("A");
        for newest_finishes_first in [true, false] {
            let mut state = TasksState::default();
            let newest = state.page_load.refresh().unwrap();
            let older = state.page_load.begin(Some(50));
            if newest_finishes_first {
                assert!(!state.settle_page(&id, &newest, Ok(page(&id, Some(50)))));
                assert!(state.page_load.loading);
            }
            assert!(state.settle_page(&id, &older, Ok(page(&id, Some(100)))));
            if !newest_finishes_first {
                assert!(!state.settle_page(&id, &newest, Err("late newest failure".into())));
            }
            let Snapshot::Ready(page) = &state.snapshot else {
                panic!("older receipts lost");
            };
            assert_eq!(page.next_cursor, Some(100));
            assert!(state.snapshot_error.is_none());
            assert!(!state.page_load.loading);
        }
    }

    #[test]
    fn background_refresh_preserves_older_cursor_and_explicit_latest_wins() {
        let id = SessionId::new("A");
        let mut state = TasksState::default();
        let older = state.page_load.begin(Some(50));
        assert!(state.page_load.refresh().is_none());
        assert!(state.settle_page(&id, &older, Ok(page(&id, Some(100)))));
        let refresh = state.page_load.refresh().unwrap();
        assert_eq!(state.page_load.cursor, Some(50));
        state.page_load.latest();
        let latest = state.page_load.refresh().unwrap();
        assert!(!state.settle_page(&id, &refresh, Ok(page(&id, Some(100)))));
        assert!(state.page_load.loading);
        assert!(state.settle_page(&id, &latest, Ok(page(&id, Some(50)))));
        assert!(state.page_load.cursor.is_none());
    }

    #[test]
    fn scope_reset_rejects_previous_scope_and_refresh_errors_keep_ready_receipts() {
        let id = SessionId::new("A");
        let mut state = TasksState::default();
        let old = state.page_load.begin(Some(50));
        state.page_load.cancel();
        let current = state.page_load.refresh().unwrap();
        assert!(!state.settle_page(&id, &old, Err("previous scope".into())));
        assert!(state.page_load.loading);
        assert!(state.settle_page(&id, &current, Ok(page(&id, Some(100)))));
        let refresh = state.page_load.refresh().unwrap();
        assert!(state.settle_page(&id, &refresh, Err("offline".into())));
        assert!(matches!(&state.snapshot, Snapshot::Ready(page) if page.next_cursor == Some(100)));
        assert_eq!(state.snapshot_error.as_deref(), Some("offline"));
        let mismatch = state.page_load.refresh().unwrap();
        assert!(state.settle_page(&id, &mismatch, Ok(page(&SessionId::new("B"), None))));
        assert!(matches!(&state.snapshot, Snapshot::Ready(page) if page.session_id == id.0));
        assert_eq!(
            state.snapshot_error.as_deref(),
            Some("Task page identity mismatch")
        );
        let mut initial = TasksState::default();
        let request = initial.page_load.refresh().unwrap();
        assert!(initial.settle_page(&id, &request, Err("initial failure".into())));
        assert!(matches!(&initial.snapshot, Snapshot::Failed(error) if error == "initial failure"));
    }

    #[test]
    fn authored_checklist_refresh_retains_items_and_initial_failure_is_distinct() {
        let row = crate::prompt_draft::LinkedChecklist {
            source: crate::prompt_draft::NoteSource {
                workspace: None,
                note_id: "authored-note".into(),
                session_id: SessionId::new("note-session"),
            },
            title: "Release checklist".into(),
            block: 2,
            text: "Review captured output".into(),
            checked: false,
        };
        let mut state = TasksState::default();
        state.apply_checklists(Ok(vec![row.clone()]));
        state.apply_checklists(Err("note unavailable".into()));
        assert!(
            matches!(&state.checklists, Snapshot::Ready(rows) if rows == std::slice::from_ref(&row))
        );
        assert_eq!(state.checklist_error.as_deref(), Some("note unavailable"));
        let mut completed = row;
        completed.checked = true;
        state.apply_checklists(Ok(vec![completed.clone()]));
        assert!(state.checklist_error.is_none());
        assert!(matches!(&state.checklists, Snapshot::Ready(rows) if rows == &[completed]));
        let mut initial = TasksState::default();
        initial.apply_checklists(Err("initial failure".into()));
        assert!(
            matches!(&initial.checklists, Snapshot::Failed(error) if error == "initial failure")
        );
    }
}
