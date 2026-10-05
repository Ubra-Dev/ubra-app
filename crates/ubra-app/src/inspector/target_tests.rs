use super::*;
use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
use gpui::{Modifiers, TestAppContext};

fn fixture() -> (Arc<StoreRuntime>, SessionId, SessionId) {
    let runtime = Arc::new(StoreRuntime::inert());
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let a = fixture
        .selected_session_id
        .clone()
        .expect("active fixture session");
    let b = fixture
        .list
        .sessions
        .iter()
        .find(|s| s.id != a && !s.is_note() && !s.is_archived())
        .expect("second fixture session")
        .id
        .clone();
    let mut store = runtime.store.write().expect("store");
    store.hydrate(fixture.list);
    store.select(a.clone());
    drop(store);
    (runtime, a, b)
}
fn runtime_owner() -> Arc<tokio::runtime::Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    )
}

#[gpui::test]
fn pin_survives_active_change_and_archival_never_falls_back(cx: &mut TestAppContext) {
    let (runtime, a, b) = fixture();
    let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), runtime_owner(), cx));
    inspector.update(cx, |i, cx| {
        i.set_inspection_target(InspectorTarget::Pinned(a.clone()), Some(a.clone()), cx);
        i.set_session_context(Some(Some(a.clone())), cx);
    });
    runtime.store.write().expect("store").select(b.clone());
    inspector.update(cx, |i, cx| {
        i.set_inspection_target(InspectorTarget::Pinned(a.clone()), Some(b.clone()), cx);
        i.refresh_if_context_changed(cx);
        assert_eq!(i.selected_session().unwrap().id, a);
        assert_eq!(i.sidebar.active, Some(b.clone()));
    });
    runtime
        .store
        .write()
        .expect("store")
        .archive_sessions(vec![a.clone()]);
    inspector.update(cx, |i, cx| {
        i.refresh_if_context_changed(cx);
        assert!(i.selected_session().is_none());
        assert_eq!(i.sidebar.target, InspectorTarget::Pinned(a.clone()));
        assert_eq!(
            runtime.store.read().expect("store").selected_session_id(),
            Some(&b)
        );
        i.set_inspection_target(InspectorTarget::FollowActive, Some(b.clone()), cx);
        i.set_session_context(Some(Some(b.clone())), cx);
        assert_eq!(i.selected_session().unwrap().id, b);
    });
}

#[gpui::test]
fn deleted_pin_and_empty_saved_workspace_have_no_fallback(cx: &mut TestAppContext) {
    let (runtime, a, b) = fixture();
    let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), runtime_owner(), cx));
    inspector.update(cx, |i, cx| i.set_session_context(Some(Some(a.clone())), cx));
    runtime
        .store
        .write()
        .expect("store")
        .remove_session_record(&a);
    runtime.store.write().expect("store").select(b);
    inspector.update(cx, |i, cx| {
        i.refresh_if_context_changed(cx);
        assert!(i.selected_session().is_none());
        i.set_session_context(Some(None), cx);
        assert!(i.selected_session().is_none());
    });
}

#[gpui::test]
fn authored_note_pin_is_unavailable_as_a_recipient_without_losing_notes_state(
    cx: &mut TestAppContext,
) {
    let (runtime, a, b) = fixture();
    let mut note = runtime
        .store
        .read()
        .expect("store")
        .sessions()
        .get(&b)
        .unwrap()
        .as_ref()
        .clone();
    note.kind = ProtoAgentKind::NOTE;
    note.foreground_agent = None;
    note.note_id = Some("authored-note".into());
    runtime.store.write().expect("store").upsert_session(note);
    let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), runtime_owner(), cx));
    inspector.update(cx, |i, cx| {
        i.set_session_context(Some(Some(a.clone())), cx);
        i.select_workspace(WorkspaceSurface::Notes, cx);
        i.notes.query_mut().insert("authored");
        i.notes.set_detail(Some(NoteDetail::Open {
            session: b.clone(),
            note_id: "authored-note".into(),
        }));
        i.set_inspection_target(InspectorTarget::Pinned(b.clone()), Some(a), cx);
        i.set_session_context(Some(Some(b.clone())), cx);
        assert!(i.selected_session().is_none());
        assert_eq!(i.sidebar.target, InspectorTarget::Pinned(b.clone()));
        assert_eq!(i.workspace_selected, Some(WorkspaceSurface::Notes));
        assert_eq!(i.notes.query_text(), "authored");
        assert_eq!(i.open_note_session(), Some(b));
    });
}

#[test]
fn transition_events_are_scoped_to_the_captured_session_and_surface() {
    let a = SessionId::new("A");
    let b = SessionId::new("B");
    let run = ubra_client::EventEnvelope {
        name: "run.updated".into(),
        seq: 1,
        params: serde_json::json!({"sessionID":"A","runId":"r","revision":2}),
    };
    assert!(sidebar::event_applies(
        &run,
        &a,
        Some(WorkspaceSurface::Runs)
    ));
    assert!(!sidebar::event_applies(
        &run,
        &b,
        Some(WorkspaceSurface::Runs)
    ));
    assert!(!sidebar::event_applies(
        &run,
        &a,
        Some(WorkspaceSurface::Tasks)
    ));
    let task = ubra_client::EventEnvelope {
        name: "task.updated".into(),
        seq: 2,
        params: serde_json::json!({"session_id":"B","sender_id":"A","task_id":"t","revision":3}),
    };
    assert!(sidebar::event_applies(
        &task,
        &a,
        Some(WorkspaceSurface::Tasks)
    ));
    assert!(sidebar::event_applies(
        &task,
        &b,
        Some(WorkspaceSurface::Tasks)
    ));
}

#[test]
fn remote_detected_ports_never_become_local_preview_destinations() {
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let mut session = fixture
        .list
        .sessions
        .into_iter()
        .find(|s| !s.is_note())
        .unwrap();
    session.listening_ports = Some(vec![ubra_proto::PortInfo {
        port: 4321,
        process_name: "server".into(),
    }]);
    session.artifacts = None;
    session.pull_requests = None;
    assert_eq!(
        artifact_count(&session),
        0,
        "ports belong to Browser, not PR links"
    );
    assert_eq!(
        detected_browser_urls(&session).first().unwrap().1,
        "http://localhost:4321"
    );
    session.host = Some("remote-host".into());
    assert!(detected_browser_urls(&session).is_empty());
}

struct Harness {
    inspector: Entity<WorkbenchInspector>,
}
impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inspector.clone())
    }
}

#[gpui::test]
fn every_catalog_destination_selects_and_files_loads_real_source(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = "fn selector_source() {}\n";
    std::fs::write(directory.path().join("selector-source.rs"), source).unwrap();
    let runtime = Arc::new(StoreRuntime::inert());
    let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let a = fixture.selected_session_id.clone().unwrap();
    let session = fixture
        .list
        .sessions
        .iter_mut()
        .find(|s| s.id == a)
        .unwrap();
    session.cwd = directory.path().to_string_lossy().into_owned();
    session.host = None;
    {
        let mut store = runtime.store.write().unwrap();
        store.hydrate(fixture.list);
        store.select(a.clone());
    }
    let (harness, cx) = cx.add_window_view(move |_, cx| {
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, runtime_owner(), cx));
        inspector.update(cx, |i, cx| i.set_session_context(Some(Some(a)), cx));
        Harness { inspector }
    });
    cx.simulate_resize(gpui::size(px(440.0), px(900.0)));
    let inspector = harness.read_with(cx, |h, _| h.inspector.clone());
    for surface in WorkspaceSurface::CATALOG {
        inspector.update(cx, |i, cx| i.select_workspace(surface, cx));
        cx.run_until_parked();
        assert_eq!(
            inspector.read_with(cx, |i, _| i.workspace_selected),
            Some(surface)
        );
        if surface == WorkspaceSurface::Files {
            // Exercise the actual file backend, not a seeded or fake
            // successful viewer.
            inspector.update(cx, |inspector, cx| {
                inspector.open_file_reference(directory.path(), "selector-source.rs:1", cx);
            });
            cx.run_until_parked();
            inspector.read_with(cx, |inspector, cx| {
                inspector.code_viewer.read_with(cx, |viewer, _| {
                    assert_eq!(viewer.tab_label().as_deref(), Some("selector-source.rs"));
                    assert_eq!(
                        viewer.export_prompt_snapshot().unwrap().content.as_ref(),
                        source
                    );
                });
            });
        }
    }
}

#[gpui::test]
fn context_remove_and_keyboard_clear_mutate_the_shared_next_prompt_queue(cx: &mut TestAppContext) {
    let (runtime, a, _) = fixture();
    let attachment =
        crate::prompt_draft::PromptAttachment::instructions("Authored instruction").unwrap();
    let attachment_id = runtime.prompt_drafts.stage(&a, attachment).unwrap();
    let drafts = runtime.prompt_drafts.clone();
    let target = a.clone();
    let (harness, cx) = cx.add_window_view(move |_, cx| {
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, runtime_owner(), cx));
        inspector.update(cx, |i, cx| {
            i.set_session_context(Some(Some(a)), cx);
            i.select_workspace(WorkspaceSurface::Context, cx);
        });
        Harness { inspector }
    });
    cx.simulate_resize(gpui::size(px(440.0), px(900.0)));
    cx.run_until_parked();
    let bounds = cx
        .debug_bounds(&format!("context-remove-{attachment_id}"))
        .unwrap();
    cx.simulate_click(bounds.center(), Modifiers::none());
    assert!(drafts.shared_snapshot(&target).attachments.is_empty());
    drafts
        .stage(
            &target,
            crate::prompt_draft::PromptAttachment::instructions("Second instruction").unwrap(),
        )
        .unwrap();
    let inspector = harness.read_with(cx, |h, _| h.inspector.clone());
    inspector.update(cx, |_, cx| cx.notify());
    inspector.update_in(cx, |i, window, cx| {
        window.focus(&i.sidebar.context_clear_focus, cx)
    });
    cx.simulate_keystrokes("enter");
    assert!(drafts.shared_snapshot(&target).attachments.is_empty());
}

fn changed_note(
    runtime: &StoreRuntime,
    recipient: &SessionId,
) -> (
    tempfile::TempDir,
    Arc<ubra_notes::store::NoteStore>,
    crate::prompt_draft::NoteSource,
) {
    let directory = tempfile::tempdir().unwrap();
    let notes = Arc::new(ubra_notes::store::NoteStore::open(directory.path()).unwrap());
    let (note_id, _) = notes
        .create(
            ubra_notes::store::parse_note("# Source\n\nINITIAL_BODY\n").doc,
            None,
        )
        .unwrap();
    let source = crate::prompt_draft::NoteSource {
        workspace: None,
        note_id,
        session_id: SessionId::new("authored-source-note"),
    };
    runtime.prompt_drafts.register_live_note(
        source.clone(),
        notes.clone(),
        "# Source\n\nINITIAL_BODY\n".into(),
    );
    runtime
        .prompt_drafts
        .stage(
            recipient,
            crate::prompt_draft::PromptAttachment::note(source.clone()),
        )
        .unwrap();
    runtime.prompt_drafts.register_live_note(
        source.clone(),
        notes.clone(),
        "# Source\n\nDISPLAYED_BODY\n".into(),
    );
    (directory, notes, source)
}

#[gpui::test]
fn context_refresh_cannot_authorize_until_the_exact_snapshot_finishes_its_frame(
    cx: &mut TestAppContext,
) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let (runtime, recipient, _) = fixture();
    let (_directory, notes, source) = changed_note(&runtime, &recipient);
    let drafts = runtime.prompt_drafts.clone();
    let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, runtime_owner(), cx));
    inspector.update(cx, |inspector, cx| {
        inspector.set_session_context(Some(Some(recipient.clone())), cx);
        inspector.select_workspace(WorkspaceSurface::Context, cx);
        inspector.set_visible(true, cx);
    });
    // Finish the real background loader while this entity has never had a window.
    cx.run_until_parked();
    let loaded = drafts.shared_snapshot(&recipient);
    assert!(loaded.attachments[0].content.contains("DISPLAYED_BODY"));
    assert!(drafts.prepare(&recipient, false, None).is_err());

    let window = cx.open_window(gpui::size(px(440.0), px(900.0)), |_, _| Harness {
        inspector: inspector.clone(),
    });
    // Opening a fake-platform window performs the production render/layout/paint.
    // Painting alone must still leave send blocked until the frame callback runs.
    assert!(drafts.prepare(&recipient, false, None).is_err());
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.simulate_next_frame(cx) > 0);
    })
    .unwrap();
    let prepared = drafts.prepare(&recipient, false, None).unwrap();
    assert_eq!(prepared.submitted.revision, loaded.revision);
    assert!(prepared.params.text.contains("DISPLAYED_BODY"));
    assert!(!prepared.params.text.contains("INITIAL_BODY"));
    drop(prepared);

    // An unchanged painted snapshot never enqueues another acknowledgment loop.
    cx.update_window(window.into(), |_, window, cx| {
        window.refresh();
        window.draw(cx).clear();
        assert_eq!(window.simulate_next_frame(cx), 0);
    })
    .unwrap();

    // If another snapshot paints while acceptance is pending, only one callback
    // stays queued, and the newer snapshot gets its own subsequent acceptance.
    drafts.register_live_note(
        source.clone(),
        notes.clone(),
        "# Source\n\nINTERMEDIATE_BODY\n".into(),
    );
    inspector.update(cx, |inspector, cx| inspector.refresh_context_preview(cx));
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.refresh();
        window.draw(cx).clear();
    })
    .unwrap();
    drafts.register_live_note(source, notes, "# Source\n\nLATEST_BODY\n".into());
    inspector.update(cx, |inspector, cx| inspector.refresh_context_preview(cx));
    cx.run_until_parked();
    let latest = drafts.shared_snapshot(&recipient);
    cx.update_window(window.into(), |_, window, cx| {
        window.refresh();
        window.draw(cx).clear();
        assert_eq!(window.simulate_next_frame(cx), 1);
    })
    .unwrap();
    assert!(drafts.prepare(&recipient, false, None).is_err());
    cx.update_window(window.into(), |_, window, cx| {
        window.refresh();
        window.draw(cx).clear();
        assert_eq!(window.simulate_next_frame(cx), 1);
    })
    .unwrap();
    let prepared = drafts.prepare(&recipient, false, None).unwrap();
    assert_eq!(prepared.submitted.revision, latest.revision);
    assert!(prepared.params.text.contains("LATEST_BODY"));
    assert!(!prepared.params.text.contains("INTERMEDIATE_BODY"));
}

#[gpui::test]
fn context_frame_rejects_hidden_switched_stale_or_changed_note_previews(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    for interruption in [
        "target",
        "target-and-return",
        "surface",
        "hide",
        "hide-and-return",
        "generation",
        "draft-revision",
        "live-note",
        "persisted-note",
    ] {
        let (runtime, recipient, other) = fixture();
        let (_directory, notes, source) = changed_note(&runtime, &recipient);
        let drafts = runtime.prompt_drafts.clone();
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, runtime_owner(), cx));
        inspector.update(cx, |inspector, cx| {
            inspector.set_session_context(Some(Some(recipient.clone())), cx);
            inspector.select_workspace(WorkspaceSurface::Context, cx);
            inspector.set_visible(true, cx);
        });
        cx.run_until_parked();
        assert!(drafts.prepare(&recipient, false, None).is_err());
        let window = cx.open_window(gpui::size(px(440.0), px(900.0)), |_, _| Harness {
            inspector: inspector.clone(),
        });
        // Mutate only after the real snapshot has painted, before its acceptance.
        inspector.update(cx, |inspector, cx| match interruption {
            "target" => inspector.set_session_context(Some(Some(other)), cx),
            "target-and-return" => {
                inspector.set_session_context(Some(Some(other)), cx);
                inspector.set_session_context(Some(Some(recipient.clone())), cx);
                inspector.select_workspace(WorkspaceSurface::Context, cx);
            }
            "surface" => {
                inspector.select_workspace(WorkspaceSurface::Usage, cx);
                inspector.select_workspace(WorkspaceSurface::Context, cx);
            }
            "hide" => inspector.set_visible(false, cx),
            "hide-and-return" => {
                inspector.set_visible(false, cx);
                inspector.set_visible(true, cx);
            }
            "generation" => {
                inspector.sidebar.generation = inspector.sidebar.generation.wrapping_add(1);
            }
            "draft-revision" => drafts.set_text(&recipient, "Changed composer text".into()),
            "live-note" => drafts.register_live_note(
                source.clone(),
                notes.clone(),
                "# Source\n\nUNSEEN_BODY\n".into(),
            ),
            "persisted-note" => {
                drafts.release_live_note(&source);
                notes
                    .save(
                        &source.note_id,
                        &ubra_notes::store::parse_note("# Source\n\nUNSEEN_BODY\n"),
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        });
        cx.update_window(window.into(), |_, window, cx| {
            assert!(window.simulate_next_frame(cx) > 0);
        })
        .unwrap();
        assert!(
            drafts.prepare(&recipient, false, None).is_err(),
            "{interruption} authorized context that was not displayed",
        );
    }
}
