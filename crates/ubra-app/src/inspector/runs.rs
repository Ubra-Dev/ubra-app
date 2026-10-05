use super::sidebar::{
    PageLoad, PageRequest, Snapshot, button, edit_query, field, message, panel, update_snapshot,
};
use super::*;
use ubra_proto::runs::*;

pub(super) struct RunsState {
    pub snapshot: Snapshot<RunListResult>,
    pub snapshot_error: Option<String>,
    pub executable: QueryEditor,
    pub arguments: Vec<QueryEditor>,
    pub kind: RunKind,
    pub busy: bool,
    pub feedback: Option<String>,
    pub expanded: Option<String>,
    pub output: Snapshot<RunOutputChunk>,
    pub output_offset: u64,
    pub output_error: Option<String>,
    pub output_loading: bool,
    pub output_generation: u64,
    pub page_load: PageLoad<String>,
    pub output_decoder: RunOutputDecoder,
    pub next_decoder: RunOutputDecoder,
    pub output_lines: Vec<SharedString>,
}
impl Default for RunsState {
    fn default() -> Self {
        Self {
            snapshot: Snapshot::Empty,
            snapshot_error: None,
            executable: QueryEditor::default(),
            arguments: Vec::new(),
            kind: RunKind::Command,
            busy: false,
            feedback: None,
            expanded: None,
            output: Snapshot::Empty,
            output_offset: 0,
            output_error: None,
            output_loading: false,
            output_generation: 0,
            output_decoder: RunOutputDecoder::default(),
            next_decoder: RunOutputDecoder::default(),
            output_lines: Vec::new(),
            page_load: PageLoad::default(),
        }
    }
}
impl RunsState {
    pub(super) fn settle_page(
        &mut self,
        id: &SessionId,
        request: &PageRequest<String>,
        result: Result<RunListResult, String>,
    ) -> bool {
        if !self.page_load.settle(request) {
            return false;
        }
        let result = result.and_then(|page| {
            if page.runs.iter().all(|run| &run.session_id == id) {
                Ok(page)
            } else {
                Err("Run page identity mismatch".into())
            }
        });
        self.snapshot_error = update_snapshot(&mut self.snapshot, result);
        true
    }

    fn apply_output_result(
        &mut self,
        id: &SessionId,
        run_id: &str,
        offset: u64,
        result: Result<RunOutputChunk, String>,
    ) {
        match result {
            Ok(chunk)
                if &chunk.session_id == id && chunk.run_id == run_id && chunk.offset == offset =>
            {
                self.install_output(chunk);
                self.output_error = None;
            }
            result => {
                let error = match result {
                    Ok(_) => "Output identity mismatch".into(),
                    Err(error) => error,
                };
                self.output_error = update_snapshot(&mut self.output, Err(error));
            }
        }
    }

    pub(super) fn reset_output(&mut self) {
        self.output_generation = self.output_generation.wrapping_add(1);
        self.output_offset = 0;
        self.output_loading = false;
        self.output = Snapshot::Empty;
        self.output_error = None;
        self.output_decoder = RunOutputDecoder::default();
        self.next_decoder = RunOutputDecoder::default();
        self.output_lines.clear();
    }
    fn install_output(&mut self, chunk: RunOutputChunk) {
        let mut decoder = self.output_decoder.clone();
        self.output_lines.clear();
        for part in &chunk.parts {
            let text = decoder.push(part.stream, &part.bytes);
            if !text.is_empty() {
                self.output_lines
                    .push(SharedString::from(format!("[{:?}] {text}", part.stream)));
            }
        }
        if chunk.eof {
            for stream in [RunOutputStream::Stdout, RunOutputStream::Stderr] {
                let text = decoder.finish(stream);
                if !text.is_empty() {
                    self.output_lines
                        .push(SharedString::from(format!("[{stream:?}] {text}")));
                }
            }
        }
        self.next_decoder = decoder;
        self.output = Snapshot::Ready(chunk);
    }
}
impl WorkbenchInspector {
    fn start_run(&mut self, recipient: SessionId, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session().filter(|s| s.id == recipient) else {
            return;
        };
        let state = self.sidebar.runs.entry(session.id.clone()).or_default();
        if state.busy {
            return;
        }
        if session.host.is_some() || session.is_note() || session.is_archived() {
            state.feedback =
                Some("Run requires a live local session; no local fallback is used".into());
            cx.notify();
            return;
        }
        let argv = match structured_argv(state) {
            Ok(argv) => argv,
            Err(error) => {
                state.feedback = Some(error);
                cx.notify();
                return;
            }
        };
        let params = RunStartParams {
            session_id: session.id.clone(),
            request_id: format!(
                "desktop-{}-{}",
                session.id.0,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            kind: state.kind,
            argv,
        };
        state.busy = true;
        state.feedback = None;
        let id = session.id;
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        cx.spawn(async move |this, cx| {
            let result = tokio
                .spawn(async move { client.run_start(params).await.map_err(|e| e.to_string()) })
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| v);
            let _ = this.update(cx, |this, cx| {
                let state = this.sidebar.runs.entry(id.clone()).or_default();
                state.busy = false;
                match result {
                    Ok(record) if record.session_id == id => {
                        state.page_load.latest();
                        state.reset_output();
                        state.expanded = Some(record.run_id.clone());
                        state.feedback = Some(format!(
                            "Recorded {} · {:?}{}",
                            record.run_id,
                            record.status,
                            record.error.map(|e| format!(" · {e}")).unwrap_or_default()
                        ));
                    }
                    Ok(_) => state.feedback = Some("Run start identity mismatch".into()),
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
    fn load_older_runs(&mut self, id: SessionId, cursor: String, cx: &mut Context<Self>) {
        let state = self.sidebar.runs.entry(id.clone()).or_default();
        if state.page_load.loading && state.page_load.cursor.as_ref() == Some(&cursor) {
            return;
        }
        let request = state.page_load.begin(Some(cursor.clone()));
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let target = id.clone();
            let result = tokio
                .spawn(async move {
                    client
                        .run_list(RunListParams {
                            session_id: target,
                            limit: Some(200),
                            cursor: Some(cursor),
                        })
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
                let state = this.sidebar.runs.entry(id.clone()).or_default();
                let succeeded = result
                    .as_ref()
                    .is_ok_and(|page| page.runs.iter().all(|r| r.session_id == id));
                if !state.settle_page(&id, &request, result) {
                    return;
                }
                if succeeded {
                    state.expanded = None;
                    state.reset_output();
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn refresh_expanded_run_output(&mut self, cx: &mut Context<Self>) {
        if !self.visible || self.workspace_selected != Some(WorkspaceSurface::Runs) {
            return;
        }
        let Some(id) = self.selected_session().map(|s| s.id) else {
            return;
        };
        let state = self.sidebar.runs.entry(id.clone()).or_default();
        let Some(run_id) = state.expanded.clone() else {
            return;
        };
        if state.output_loading {
            return;
        }
        state.output_loading = true;
        state.output_generation = state.output_generation.wrapping_add(1);
        let generation = state.output_generation;
        let offset = state.output_offset;
        if !matches!(state.output, Snapshot::Ready(_)) {
            state.output = Snapshot::Loading;
        }
        let params = RunReadOutputParams {
            session_id: id.clone(),
            run_id: run_id.clone(),
            offset,
            max_bytes: Some(MAX_RUN_READ_BYTES),
        };
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        cx.spawn(async move |this, cx| {
            let result = tokio
                .spawn(async move {
                    client
                        .run_read_output(params)
                        .await
                        .map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| v);
            let _ = this.update(cx, |this, cx| {
                let visible = this.visible
                    && this.workspace_selected == Some(WorkspaceSurface::Runs)
                    && this.selected_session().as_ref().map(|s| &s.id) == Some(&id);
                let Some(state) = this.sidebar.runs.get_mut(&id) else {
                    return;
                };
                if state.output_generation != generation
                    || state.expanded.as_deref() != Some(&run_id)
                    || state.output_offset != offset
                {
                    return;
                }
                state.output_loading = false;
                if !visible {
                    return;
                }
                state.apply_output_result(&id, &run_id, offset, result);
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn render_runs(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(session) = self.selected_session() else {
            return panel("runs-panel", colors)
                .child(message("No inspected session", colors))
                .into_any_element();
        };
        let can_run = session.host.is_none() && !session.is_note();
        let id = session.id;
        let state = self.sidebar.runs.entry(id.clone()).or_default();
        let mut view=panel("runs-panel",colors).child(message(format!("Recipient {} · cwd {}",id.0,session.cwd),colors))
            .child(message("Explicit local commands. Commands already running inside agents are not automatically captured.",colors));
        if can_run {
            for kind in [
                RunKind::Build,
                RunKind::Test,
                RunKind::Lint,
                RunKind::Command,
            ] {
                let target = id.clone();
                view = view.child(button(
                    format!("run-kind-{kind:?}"),
                    format!("{}{:?}", if state.kind == kind { "✓ " } else { "" }, kind),
                    colors,
                    window,
                    cx,
                    move |this, _, cx| {
                        this.sidebar.runs.entry(target.clone()).or_default().kind = kind;
                        cx.notify();
                    },
                ));
            }
            let target = id.clone();
            view = view.child(field(
                format!("run-executable-{}", id.0),
                "Executable",
                state.executable.display("│").0,
                Some(self.sidebar.runs_focus.clone()),
                colors,
                window,
                cx,
                move |this, key, cx| {
                    edit_query(
                        &mut this
                            .sidebar
                            .runs
                            .entry(target.clone())
                            .or_default()
                            .executable,
                        key,
                        cx,
                    )
                },
            ));
            for (ix, arg) in state.arguments.iter().enumerate() {
                let target = id.clone();
                view = view.child(field(
                    format!("run-argument-{}-{ix}", id.0),
                    format!("Argument {} (one exact argv entry)", ix + 1),
                    arg.display("│").0,
                    None,
                    colors,
                    window,
                    cx,
                    move |this, key, cx| {
                        if let Some(q) = this
                            .sidebar
                            .runs
                            .entry(target.clone())
                            .or_default()
                            .arguments
                            .get_mut(ix)
                        {
                            edit_query(q, key, cx);
                        }
                    },
                ));
            }
            let target = id.clone();
            view = view.child(button(
                "run-add-argument",
                "Add argument",
                colors,
                window,
                cx,
                move |this, _, cx| {
                    this.sidebar
                        .runs
                        .entry(target.clone())
                        .or_default()
                        .arguments
                        .push(QueryEditor::default());
                    cx.notify();
                },
            ));
            if !state.arguments.is_empty() {
                let target = id.clone();
                view = view.child(button(
                    "run-remove-argument",
                    "Remove last argument",
                    colors,
                    window,
                    cx,
                    move |this, _, cx| {
                        this.sidebar
                            .runs
                            .entry(target.clone())
                            .or_default()
                            .arguments
                            .pop();
                        cx.notify();
                    },
                ));
            }
            let target = id.clone();
            view = view.child(button(
                "run-start",
                if state.busy {
                    "Starting…"
                } else {
                    "Run for inspected session"
                },
                colors,
                window,
                cx,
                move |this, _, cx| this.start_run(target.clone(), cx),
            ));
        } else {
            view=view.child(message("This host has no command execution producer; runs never execute locally for a remote recipient",colors));
        }
        if let Some(feedback) = &state.feedback {
            view = view.child(message(feedback.clone(), colors));
        }
        if let Some(error) = &state.snapshot_error {
            view = view.child(message(
                format!("Execution page refresh failed: {error}. Previous observed data, if shown, is stale. Retry with Newest execution page."),
                colors,
            ));
        }
        let target = id.clone();
        view = view.child(sidebar::button_with_focus(
            "runs-refresh",
            "Newest execution page",
            colors,
            self.sidebar.runs_refresh_focus.clone(),
            cx,
            move |this, _, cx| {
                this.sidebar
                    .runs
                    .entry(target.clone())
                    .or_default()
                    .page_load
                    .latest();
                this.refresh_sidebar_for(&target, cx);
            },
        ));
        match &state.snapshot {
            Snapshot::Empty => {
                view = view.child(message(
                    "Open or refresh to read the execution journal",
                    colors,
                ))
            }
            Snapshot::Loading => view = view.child(message("Loading execution journal…", colors)),
            Snapshot::Failed(e) => {
                view = view.child(message(format!("Could not read runs: {e}"), colors))
            }
            Snapshot::Ready(page) => {
                if page.runs.is_empty() {
                    view = view.child(message(
                        "No recorded runs. Start an explicit local command above.",
                        colors,
                    ));
                }
                for record in &page.runs {
                    let run_id = record.run_id.clone();
                    let target = id.clone();
                    let expanded = state.expanded.as_deref() == Some(&run_id);
                    view = view.child(button(
                        format!("run-disclose-{run_id}"),
                        format!(
                            "{} {:?} · {:?} · {:?}",
                            if expanded { "▾" } else { "▸" },
                            record.kind,
                            record.status,
                            record.argv
                        ),
                        colors,
                        window,
                        cx,
                        move |this, _, cx| {
                            let s = this.sidebar.runs.entry(target.clone()).or_default();
                            let expanded = if s.expanded.as_deref() == Some(&run_id) {
                                None
                            } else {
                                Some(run_id.clone())
                            };
                            s.reset_output();
                            s.expanded = expanded;
                            this.refresh_expanded_run_output(cx);
                            cx.notify();
                        },
                    ));
                    view = view.child(message(
                        format!(
                            "{} · source {:?} · exit {} · signal {} · duration {}",
                            record.cwd,
                            record.producer,
                            record
                                .exit_code
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "not observed".into()),
                            record
                                .signal
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "not observed".into()),
                            record
                                .duration_ms
                                .map(|v| format!("{v} ms"))
                                .unwrap_or_else(|| "not observed".into())
                        ),
                        colors,
                    ));
                    view = view.child(message(
                        format!(
                            "Started {} · finished {}",
                            relative_time(record.started_at.0),
                            record
                                .finished_at
                                .map(|at| relative_time(at.0))
                                .unwrap_or_else(|| "not observed".into())
                        ),
                        colors,
                    ));
                    if let Some(error) = &record.error {
                        view = view.child(message(error.clone(), colors));
                    }
                    if expanded {
                        if record.output_truncated {
                            view = view.child(message(
                                "Captured output truncated at the 4 MiB retention limit",
                                colors,
                            ));
                        }
                        if state.output_loading {
                            view = view.child(message("Refreshing captured output…", colors));
                        }
                        if let Some(error) = &state.output_error {
                            view = view.child(message(
                                format!("Output refresh failed: {error}. Previous observed output, if shown, is stale."),
                                colors,
                            ));
                            view = view.child(button(
                                format!("run-output-retry-{}", record.run_id),
                                "Retry captured output",
                                colors,
                                window,
                                cx,
                                |this, _, cx| this.refresh_expanded_run_output(cx),
                            ));
                        }
                        match &state.output {
                            Snapshot::Ready(chunk) => {
                                view = view.child(message(
                                    format!(
                                        "Output bytes {}–{} · revision {}{}{}",
                                        chunk.offset,
                                        chunk.next_offset,
                                        chunk.revision,
                                        if chunk.eof { " · end" } else { "" },
                                        if chunk.revision < record.revision {
                                            " · prior output revision"
                                        } else {
                                            ""
                                        }
                                    ),
                                    colors,
                                ));
                                for (ix, line) in state.output_lines.iter().enumerate() {
                                    view = view.child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "run-output-{}-{ix}",
                                                record.run_id
                                            )))
                                            .min_w_0()
                                            .overflow_x_scroll()
                                            .font_family(crate::fonts::mono_family())
                                            .text_sm()
                                            .child(line.clone()),
                                    );
                                }
                                if !chunk.eof && chunk.next_offset > chunk.offset {
                                    let target = id.clone();
                                    let next = chunk.next_offset;
                                    view = view.child(button(
                                        "run-output-next",
                                        "Next output page",
                                        colors,
                                        window,
                                        cx,
                                        move |this, _, cx| {
                                            let s = this
                                                .sidebar
                                                .runs
                                                .entry(target.clone())
                                                .or_default();
                                            s.output_decoder = s.next_decoder.clone();
                                            s.output_offset = next;
                                            s.output_generation =
                                                s.output_generation.wrapping_add(1);
                                            s.output = Snapshot::Empty;
                                            s.output_error = None;
                                            s.output_lines.clear();
                                            s.output_loading = false;
                                            this.refresh_expanded_run_output(cx);
                                        },
                                    ));
                                }
                                if chunk.offset > 0 {
                                    let target = id.clone();
                                    view = view.child(button(
                                        "run-output-beginning",
                                        "Beginning",
                                        colors,
                                        window,
                                        cx,
                                        move |this, _, cx| {
                                            let s = this
                                                .sidebar
                                                .runs
                                                .entry(target.clone())
                                                .or_default();
                                            s.reset_output();
                                            this.refresh_expanded_run_output(cx);
                                        },
                                    ));
                                }
                            }
                            Snapshot::Failed(e) => {
                                view = view
                                    .child(message(format!("Could not read output: {e}"), colors))
                            }
                            _ => view = view.child(message("Loading captured output…", colors)),
                        }
                    }
                }
                view = view.child(message(
                    format!(
                        "Journal retention: {} runs per session · output reads ≤64 KiB",
                        page.retention_limit
                    ),
                    colors,
                ));
                if let Some(cursor) = &page.next_cursor {
                    let target = id.clone();
                    let cursor = cursor.clone();
                    view = view.child(button(
                        "runs-older-page",
                        if state.page_load.loading {
                            "Loading older page…"
                        } else {
                            "Older execution page"
                        },
                        colors,
                        window,
                        cx,
                        move |this, _, cx| this.load_older_runs(target.clone(), cursor.clone(), cx),
                    ));
                }
            }
        }
        if let Some(prs) = session.pull_requests {
            for pr in prs {
                let checks = sorted_pr_checks(&pr);
                if !checks.is_empty() {
                    let (rollup, _) = checks_rollup(&pr);
                    view = view.child(message(
                        format!(
                            "Provider checks · PR #{} · {} · {rollup}",
                            pr.number, pr.url
                        ),
                        colors,
                    ));
                }
                for check in &checks {
                    view = view.child(message(
                        format!(
                            "{} · {} · {}",
                            check.name,
                            check.result,
                            check.detail.as_deref().unwrap_or("detail not reported")
                        ),
                        colors,
                    ));
                    if let Some(url) = &check.url {
                        let url = url.clone();
                        view = view.child(button(
                            format!("provider-check-{}-{}", pr.number, check.name),
                            "Open provider evidence",
                            colors,
                            window,
                            cx,
                            move |_, _, cx| cx.open_url(&url),
                        ));
                    }
                }
            }
        }
        view.into_any_element()
    }
}

pub(super) fn structured_argv(state: &RunsState) -> Result<Vec<String>, String> {
    let executable = state.executable.text();
    if executable.is_empty() {
        return Err("Enter an executable".into());
    }
    let mut argv = Vec::with_capacity(state.arguments.len() + 1);
    argv.push(executable.to_owned());
    argv.extend(state.arguments.iter().map(|q| q.text().to_owned()));
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_command_is_created_without_an_executable() {
        assert!(structured_argv(&RunsState::default()).is_err());
    }
    fn chunk(offset: u64, bytes: &[u8], eof: bool) -> RunOutputChunk {
        RunOutputChunk {
            session_id: SessionId::new("A"),
            run_id: "r".into(),
            revision: 1,
            offset,
            next_offset: offset + bytes.len() as u64,
            parts: vec![RunOutputPart {
                sequence: offset,
                stream: RunOutputStream::Stdout,
                offset,
                byte_len: bytes.len() as u32,
                bytes: bytes.to_vec(),
            }],
            eof,
            truncated: false,
        }
    }
    #[test]
    fn decoder_carries_a_character_across_raw_byte_pages() {
        let bytes = "漢".as_bytes();
        let mut state = RunsState::default();
        state.install_output(chunk(0, &bytes[..1], false));
        assert!(state.output_lines.is_empty());
        state.output_decoder = state.next_decoder.clone();
        state.output_offset = 1;
        state.install_output(chunk(1, &bytes[1..], true));
        assert_eq!(state.output_lines.len(), 1);
        assert_eq!(state.output_lines[0].as_ref(), "[Stdout] 漢");
    }
    #[test]
    fn live_page_rereads_decode_from_page_start_without_duplicate_or_corrupted_text() {
        let bytes = "漢".as_bytes();
        let mut state = RunsState::default();
        state.install_output(chunk(0, &bytes[..1], false));
        state.install_output(chunk(0, bytes, false));
        let first = state.output_lines.clone();
        state.install_output(chunk(0, bytes, false));
        assert_eq!(state.output_lines, first);
        assert_eq!(state.output_lines[0].as_ref(), "[Stdout] 漢");
    }
    #[test]
    fn starting_another_run_discards_pending_utf8_from_the_previous_run() {
        let mut state = RunsState::default();
        state.install_output(chunk(0, &"漢".as_bytes()[..1], false));
        state.reset_output();
        state.install_output(chunk(0, b"A", true));
        assert_eq!(state.output_lines[0].as_ref(), "[Stdout] A");
    }

    fn page(next_cursor: Option<&str>) -> RunListResult {
        RunListResult {
            runs: Vec::new(),
            next_cursor: next_cursor.map(str::to_owned),
            retention_limit: 200,
        }
    }

    #[test]
    fn older_navigation_rejects_delayed_newest_completion_in_either_order() {
        let id = SessionId::new("A");
        for newest_finishes_first in [true, false] {
            let mut state = RunsState::default();
            let newest = state.page_load.refresh().unwrap();
            let older = state.page_load.begin(Some("older".into()));
            if newest_finishes_first {
                assert!(!state.settle_page(&id, &newest, Ok(page(None))));
                assert!(state.page_load.loading);
            }
            assert!(state.settle_page(&id, &older, Ok(page(Some("even-older")))));
            if !newest_finishes_first {
                assert!(!state.settle_page(&id, &newest, Err("delayed newest failure".into())));
            }
            let Snapshot::Ready(page) = &state.snapshot else {
                panic!("older page lost");
            };
            assert_eq!(page.next_cursor.as_deref(), Some("even-older"));
            assert!(state.snapshot_error.is_none());
            assert!(!state.page_load.loading);
        }
    }

    #[test]
    fn background_refresh_preserves_older_page_and_latest_supersedes_it() {
        let id = SessionId::new("A");
        let mut state = RunsState::default();
        let older = state.page_load.begin(Some("older".into()));
        assert!(state.page_load.refresh().is_none());
        assert!(state.settle_page(&id, &older, Ok(page(Some("even-older")))));
        let refresh = state.page_load.refresh().unwrap();
        assert_eq!(state.page_load.cursor.as_deref(), Some("older"));
        state.page_load.latest();
        let latest = state.page_load.refresh().unwrap();
        assert!(!state.settle_page(&id, &refresh, Ok(page(Some("even-older")))));
        assert!(state.page_load.loading);
        assert!(state.settle_page(&id, &latest, Ok(page(Some("older")))));
        assert!(state.page_load.cursor.is_none());
    }

    #[test]
    fn scope_reset_rejects_old_page_even_after_returning_to_same_session() {
        let id = SessionId::new("A");
        let mut state = RunsState::default();
        let old = state.page_load.begin(Some("older".into()));
        state.page_load.cancel();
        let current = state.page_load.refresh().unwrap();
        assert!(!state.settle_page(&id, &old, Err("previous scope".into())));
        assert!(state.page_load.loading);
        assert!(state.settle_page(&id, &current, Ok(page(None))));
        assert!(state.snapshot_error.is_none());
    }

    #[test]
    fn output_refresh_error_and_identity_mismatch_retain_observed_raw_page() {
        let id = SessionId::new("A");
        let mut state = RunsState::default();
        state.apply_output_result(&id, "r", 0, Ok(chunk(0, b"observed", true)));
        let observed = state.output_lines.clone();
        state.apply_output_result(&id, "r", 0, Err("offline".into()));
        assert!(
            matches!(&state.output, Snapshot::Ready(page) if page.parts[0].bytes == b"observed")
        );
        assert_eq!(state.output_lines, observed);
        assert_eq!(state.output_error.as_deref(), Some("offline"));
        let mut wrong = chunk(0, b"unrelated", true);
        wrong.run_id = "other-run".into();
        state.apply_output_result(&id, "r", 0, Ok(wrong));
        assert_eq!(state.output_lines, observed);
        assert_eq!(
            state.output_error.as_deref(),
            Some("Output identity mismatch")
        );
        state.apply_output_result(&id, "r", 0, Ok(chunk(0, b"recovered", true)));
        assert!(state.output_error.is_none());
        assert_eq!(state.output_lines[0].as_ref(), "[Stdout] recovered");
        state.reset_output();
        state.apply_output_result(&id, "other-run", 0, Err("initial failure".into()));
        assert!(matches!(&state.output, Snapshot::Failed(error) if error == "initial failure"));
        assert!(state.output_lines.is_empty());
    }

    #[test]
    fn run_page_refresh_failure_retains_ready_but_initial_failure_is_failed() {
        let id = SessionId::new("A");
        let mut state = RunsState::default();
        let first = state.page_load.refresh().unwrap();
        assert!(state.settle_page(&id, &first, Ok(page(Some("older")))));
        let refresh = state.page_load.refresh().unwrap();
        assert!(state.settle_page(&id, &refresh, Err("offline".into())));
        assert!(
            matches!(&state.snapshot, Snapshot::Ready(page) if page.next_cursor.as_deref() == Some("older"))
        );
        assert_eq!(state.snapshot_error.as_deref(), Some("offline"));
        let recovered = state.page_load.refresh().unwrap();
        assert!(state.settle_page(&id, &recovered, Ok(page(None))));
        assert!(state.snapshot_error.is_none());
        let mut initial = RunsState::default();
        let request = initial.page_load.refresh().unwrap();
        assert!(initial.settle_page(&id, &request, Err("initial failure".into())));
        assert!(matches!(&initial.snapshot, Snapshot::Failed(error) if error == "initial failure"));
    }
}
