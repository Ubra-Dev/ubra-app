//! Production-sidebar input and rendering coverage for the app-wide inbox.
use super::tests::test_services;
use super::*;
use crate::sidebar::SidebarPreviewFixture;
use gpui::{Modifiers, size};

/// Mount the production root once, with an adjacent input probe rather than an
/// overlay. RootView owns all keyboard routing and notification presentation.
struct NotificationWheelHarness {
    root: Entity<RootView>,
    scrolls: Arc<std::sync::atomic::AtomicUsize>,
    clicks: Arc<std::sync::atomic::AtomicUsize>,
    _root_changed: Subscription,
}

impl NotificationWheelHarness {
    fn new(root: Entity<RootView>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&root, |_, _, cx| cx.notify());
        Self {
            root,
            scrolls: Arc::default(),
            clicks: Arc::default(),
            _root_changed: subscription,
        }
    }
}

impl Render for NotificationWheelHarness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let scrolls = self.scrolls.clone();
        let probe_scrolls = self.scrolls.clone();
        let clicks = self.clicks.clone();
        div()
            .size_full()
            .flex()
            .on_scroll_wheel(move |_, _, _| {
                scrolls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
            .child(div().flex_1().min_w_0().h_full().child(self.root.clone()))
            .child(
                div()
                    .id("notification-outside-probe")
                    .debug_selector(|| "notification-outside-probe".into())
                    .w(px(24.0))
                    .h_full()
                    .flex_none()
                    .on_scroll_wheel(move |_, _, cx| {
                        probe_scrolls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        cx.stop_propagation();
                    })
                    .on_click(move |_, _, _| {
                        clicks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }),
            )
    }
}

fn notification_services(count: usize) -> Arc<AppServices> {
    let services = test_services();
    let mut list = SidebarPreviewFixture::make(PreviewScenario::Typical).list;
    let base = list.sessions[0].clone();
    list.sessions.clear();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    let titles = [
        "Summarize this week's customer interviews",
        "Refresh the launch checklist in Notion",
        "Polish the command palette",
        "Prepare the September release notes",
        "Compare the onboarding flows",
        "Review the new landing page",
        "Organize the design feedback",
        "Draft the team update",
    ];
    let mut store = services.store.store.write().unwrap();
    store.set_notification_surface_visible(false);
    for index in (0..count).rev() {
        let mut session = base.clone();
        session.id = SessionId::new(format!("notification-preview-{index}"));
        session.title = titles[index % titles.len()].into();
        session.status = if index % 4 == 1 {
            SessionStatus::NeedsInput(ubra_proto::NeedsInputKind::Question)
        } else {
            SessionStatus::Idle
        };
        session.last_turn_completed_at = Some(ubra_proto::DateMillis(
            now - (index + 1) as f64 * 3_600_000.0,
        ));
        session.last_seen_at = None;
        session.updated_at = session.last_turn_completed_at.unwrap();
        use ubra_proto::attention::{
            ATTENTION_VERSION, AttentionEvent, AttentionKind, AttentionState,
        };
        session.attention_state = Some(AttentionState {
            version: ATTENTION_VERSION,
            epoch: session.id.0.clone(),
            sequence: 1,
            turn: 1,
            working: false,
            last_native_completion: None,
            observed_at: None,
            active_tools: Default::default(),
            events: vec![AttentionEvent {
                sequence: 1,
                turn: 1,
                kind: if index % 4 == 1 {
                    AttentionKind::Request
                } else {
                    AttentionKind::Completion
                },
                occurred_at: session.updated_at,
                resolved: false,
                blocking: true,
                detail: None,
            }],
            native_requests: Default::default(),
            native_completions: Default::default(),
        });
        list.sessions.push(session);
        store.hydrate(list.clone());
    }
    assert_eq!(store.notifications().entries().len(), count);
    drop(store);
    services
}

fn notifications_displayed(root: &RootView) -> bool {
    root.inspector_open && root.right_sidebar_content == RightSidebarContent::Notifications
}

#[gpui::test]
fn notification_sidebar_contains_wheel_but_not_outside_input(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let (harness, cx) = cx.add_window_view(move |window, cx| {
        let root = cx.new(|cx| {
            let mut root = RootView::new(
                notification_services(200),
                false,
                PreviewScenario::Artifacts,
                window,
                cx,
            );
            root.toggle_notifications(window, cx);
            root
        });
        NotificationWheelHarness::new(root, cx)
    });
    cx.simulate_resize(size(px(1000.0), px(700.0)));
    cx.run_until_parked();
    let root = harness.read_with(cx, |view, _| view.root.clone());
    let list = cx.debug_bounds("notification-list").unwrap();
    for delta in [-40.0, -100_000.0, -40.0, 100_000.0, 40.0] {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: list.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(delta))),
            ..Default::default()
        });
        cx.run_until_parked();
    }
    assert_eq!(
        harness.read_with(cx, |view, _| view
            .scrolls
            .load(std::sync::atomic::Ordering::Relaxed)),
        0
    );
    let outside = cx
        .debug_bounds("notification-outside-probe")
        .unwrap()
        .center();
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: outside,
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-40.0))),
        ..Default::default()
    });
    cx.simulate_click(outside, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        harness.read_with(cx, |view, _| view
            .scrolls
            .load(std::sync::atomic::Ordering::Relaxed)),
        1
    );
    assert_eq!(
        harness.read_with(cx, |view, _| view
            .clicks
            .load(std::sync::atomic::Ordering::Relaxed)),
        1
    );
    assert!(
        root.read_with(cx, |root, _| notifications_displayed(root)),
        "outside input must not dismiss the docked inbox"
    );
    assert!(cx.debug_bounds("notification-dismiss-layer").is_none());
}

#[gpui::test]
fn notification_list_scrolls_and_actions_stay_inside(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let services = notification_services(200);
    let store = services.store.clone();
    let (harness, cx) = cx.add_window_view(move |window, cx| {
        let root = cx.new(|cx| {
            let mut root = RootView::new(services, false, PreviewScenario::Artifacts, window, cx);
            root.toggle_notifications(window, cx);
            root
        });
        NotificationWheelHarness::new(root, cx)
    });
    cx.simulate_resize(size(px(1000.0), px(700.0)));
    cx.run_until_parked();
    let root = harness.read_with(cx, |view, _| view.root.clone());
    assert!(cx.debug_bounds("notification-row-0").is_some());
    assert!(
        cx.debug_bounds("notification-row-20").is_none(),
        "offscreen rows should not be built"
    );
    let list = cx.debug_bounds("notification-list").unwrap();
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: list.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-120.0))),
        ..Default::default()
    });
    cx.run_until_parked();
    assert!(
        root.read_with(cx, |root, _| root
            .notification_scroll
            .0
            .borrow()
            .base_handle
            .offset()
            .y
            < px(0.0)),
        "the inbox itself must scroll"
    );
    assert_eq!(
        harness.read_with(cx, |view, _| view
            .scrolls
            .load(std::sync::atomic::Ordering::Relaxed)),
        0
    );
    root.update_in(cx, |root, window, cx| {
        root.notification_focus.focus(window, cx)
    });
    for _ in 0..12 {
        cx.simulate_keystrokes("down");
    }
    cx.run_until_parked();
    assert_eq!(root.read_with(cx, |root, _| root.notification_selected), 12);
    let row = cx.debug_bounds("notification-row-12").unwrap();
    assert!(row.top() >= list.top() && row.bottom() <= list.bottom());
    let selected = store.store.read().unwrap().notifications().entries()[12].clone();
    {
        let position = cx.debug_bounds("notification-mute-12").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert!(
        store
            .store
            .read()
            .unwrap()
            .preferences()
            .muted_notification_sessions
            .contains(&selected.session_id.0)
    );
    assert!(root.read_with(cx, |root, _| notifications_displayed(root)));
    {
        let position = cx.debug_bounds("notification-read-12").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert!(
        store
            .store
            .read()
            .unwrap()
            .notifications()
            .entries()
            .iter()
            .find(|entry| entry.id == selected.id)
            .unwrap()
            .read
    );
    assert!(root.read_with(cx, |root, _| root.notification_selected
        < root.notification_rows().len()));
    {
        let position = cx.debug_bounds("notification-filter").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert_eq!(
        root.read_with(cx, |root, _| root.notification_rows().len()),
        200
    );
    assert_eq!(root.read_with(cx, |root, _| root.notification_selected), 0);
    assert_eq!(
        root.read_with(cx, |root, _| root
            .notification_scroll
            .0
            .borrow()
            .base_handle
            .offset()
            .y),
        px(0.0)
    );
    {
        let position = cx.debug_bounds("notification-options").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert!(root.read_with(cx, |root, _| root.notification_options_open
        && notifications_displayed(root)));
    {
        let position = cx.debug_bounds("notification-read-all").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert_eq!(
        store.store.read().unwrap().notifications().unread_count(),
        0
    );
    {
        let position = cx.debug_bounds("notification-filter").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert!(root.read_with(cx, |root, _| root.notification_rows().is_empty()));
    assert_eq!(root.read_with(cx, |root, _| root.notification_selected), 0);
    assert!(
        cx.debug_bounds("notification-list").is_some(),
        "empty state remains in the sidebar list region"
    );
    {
        let position = cx.debug_bounds("notification-clear").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert!(
        store
            .store
            .read()
            .unwrap()
            .notifications()
            .entries()
            .is_empty()
    );
    assert!(root.read_with(cx, |root, _| notifications_displayed(root)));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!root.read_with(cx, |root, _| root.inspector_open));
}

#[gpui::test]
fn notification_no_session_rail_preserves_global_feed_and_unread(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let services = notification_services(200);
    let store = services.store.clone();
    let entries = store
        .store
        .read()
        .unwrap()
        .notifications()
        .entries()
        .to_vec();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let mut root = RootView::new_with_selection(
            services,
            false,
            PreviewScenario::Artifacts,
            None,
            Some(None),
            window,
            cx,
        );
        root.inspector_toggled_at = None;
        root.set_inspector_open(false, cx);
        root
    });
    cx.simulate_resize(size(px(1200.0), px(800.0)));
    cx.run_until_parked();
    assert!(root.read_with(cx, |root, cx| root.active_session_id(cx).is_none()));
    assert!(cx.debug_bounds("notification-panel").is_none());
    {
        let position = cx
            .debug_bounds("INSPECTOR_STRIP_Notifications")
            .unwrap()
            .center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    root.read_with(cx, |root, cx| {
        assert!(notifications_displayed(root));
        assert!(!root.preview);
        assert!(root.inspector.is_some());
        assert!(root.active_session_id(cx).is_none());
        assert_eq!(root.notification_rows().len(), 200);
    });
    let after = store.store.read().unwrap();
    assert_eq!(
        after.notifications().unread_count(),
        200,
        "opening the app-wide feed must not read it"
    );
    assert_eq!(
        after
            .notifications()
            .entries()
            .iter()
            .map(|entry| (&entry.id, entry.read))
            .collect::<Vec<_>>(),
        entries
            .iter()
            .map(|entry| (&entry.id, entry.read))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        after.sessions().len(),
        200,
        "no-session selection must not delete the fixture"
    );
    assert!(after.projects().len() > 1);
    drop(after);
    assert!(cx.debug_bounds("notification-dismiss-layer").is_none());
    {
        let position = cx
            .debug_bounds("INSPECTOR_STRIP_Notifications")
            .unwrap()
            .center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert!(!root.read_with(cx, |root, _| root.inspector_open));
    root.update_in(cx, |root, window, cx| {
        root.open_notification(SessionId::new(""), None, window, cx);
        root.open_notification(SessionId::new(""), None, window, cx);
    });
    cx.run_until_parked();
    assert!(
        root.read_with(cx, |root, _| notifications_displayed(root)),
        "empty-session native opens are idempotent"
    );
    assert_eq!(
        store.store.read().unwrap().notifications().unread_count(),
        200
    );
}

#[gpui::test]
fn notification_event_focus_and_sidebar_close_do_not_close_terminal_session(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| {
        cx.set_reduce_motion(true);
        commands::bind_keys(cx, &Default::default());
    });
    let services = notification_services(200);
    let store = services.store.clone();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Artifacts, window, cx)
    });
    cx.simulate_resize(size(px(1200.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| root.show_notifications(window, cx));
    cx.run_until_parked();
    let event = store.store.read().unwrap().notifications().entries()[1].clone();
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert!(notifications_displayed(root));
        assert_eq!(root.active_session_id(cx), Some(event.session_id.clone()));
        assert!(
            root.active_terminal(cx)
                .expect("production terminal")
                .read(cx)
                .is_focused(window)
        );
        assert!(!root.notification_focus.contains_focused(window, cx));
    });
    assert!(
        store
            .store
            .read()
            .unwrap()
            .notifications()
            .entries()
            .iter()
            .find(|entry| entry.id == event.id)
            .unwrap()
            .read
    );
    let selected = root.read_with(cx, |root, _| root.notification_selected);
    cx.simulate_keystrokes("down up");
    cx.run_until_parked();
    assert_eq!(
        root.read_with(cx, |root, _| root.notification_selected),
        selected,
        "terminal arrows must not navigate the still-open feed"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        root.read_with(cx, |root, _| notifications_displayed(root)),
        "terminal Escape is not sidebar dismissal"
    );
    root.update_in(cx, |root, window, cx| {
        root.notification_focus.focus(window, cx)
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!root.read_with(cx, |root, _| root.inspector_open));
    assert!(
        store
            .store
            .read()
            .unwrap()
            .sessions()
            .contains_key(&event.session_id)
    );
    assert!(root.read_with(cx, |root, _| {
        root.window_store
            .read()
            .expect("store")
            .pending_close()
            .is_none()
    }));
    root.update_in(cx, |root, window, cx| root.toggle_notifications(window, cx));
    cx.run_until_parked();
    // A focused toolbar button is also owned by the notification pane.
    {
        let position = cx.debug_bounds("notification-options").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    assert!(!root.read_with(cx, |root, _| root.inspector_open));
    assert!(
        store
            .store
            .read()
            .unwrap()
            .sessions()
            .contains_key(&event.session_id)
    );
    assert!(root.read_with(cx, |root, _| {
        root.window_store
            .read()
            .expect("store")
            .pending_close()
            .is_none()
    }));
    root.update_in(cx, |root, window, cx| {
        assert!(
            root.active_terminal(cx)
                .expect("terminal")
                .read(cx)
                .is_focused(window)
        );
        root.show_notifications(window, cx);
        root.focus_active_terminal(window, cx);
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    let pending = root.read_with(cx, |root, _| {
        root.window_store
            .read()
            .expect("store")
            .pending_close()
            .expect("normal session-close confirmation")
            .ids
            .clone()
    });
    assert_eq!(
        pending,
        vec![event.session_id.clone()],
        "terminal Cmd+W must reach the existing close policy, not hide Notifications",
    );
    assert!(cx.has_pending_prompt());
    assert!(root.read_with(cx, |root, _| notifications_displayed(root)));
}

#[gpui::test]
fn notification_toolbar_keyboard_and_entry_identity_survive_filter_reordering(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let services = notification_services(200);
    let store = services.store.clone();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Artifacts, window, cx)
    });
    cx.simulate_resize(size(px(1200.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| root.show_notifications(window, cx));
    cx.run_until_parked();
    let first = store.store.read().unwrap().notifications().entries()[0].clone();
    let second = store.store.read().unwrap().notifications().entries()[1].clone();
    let third = store.store.read().unwrap().notifications().entries()[2].clone();
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    {
        let position = cx.debug_bounds("notification-mute-1").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    assert!(
        store
            .store
            .read()
            .unwrap()
            .preferences()
            .muted_notification_sessions
            .contains(&second.session_id.0)
    );
    root.update_in(cx, |root, _, cx| {
        root.window_store
            .write()
            .unwrap()
            .set_notification_read(&first.id, true);
        root.notification_selected = 0;
        cx.notify();
    });
    cx.run_until_parked();
    // The focused control moved from filtered row 1 to row 0. Enter must
    // still invoke the same entry's command, not the replacement at index 1.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let prefs = store.store.read().unwrap().preferences().clone();
    assert!(
        !prefs
            .muted_notification_sessions
            .contains(&second.session_id.0)
    );
    assert!(
        !prefs
            .muted_notification_sessions
            .contains(&third.session_id.0)
    );
    assert!(root.read_with(cx, |root, _| notifications_displayed(root)));
    {
        let position = cx.debug_bounds("notification-filter").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.simulate_keystrokes("space");
    cx.run_until_parked();
    assert!(root.read_with(cx, |root, _| root.notification_filter_unread));
    assert_eq!(root.read_with(cx, |root, _| root.notification_selected), 0);
    {
        let position = cx.debug_bounds("notification-options").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    let initial = store.store.read().unwrap().preferences().clone();
    {
        let position = cx.debug_bounds("notification-alerts").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.simulate_keystrokes("space");
    cx.run_until_parked();
    assert_eq!(
        store
            .store
            .read()
            .unwrap()
            .preferences()
            .status_notifications,
        initial.status_notifications
    );
    {
        let position = cx.debug_bounds("notification-sounds").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        store.store.read().unwrap().preferences().status_sounds,
        initial.status_sounds
    );
    assert_eq!(
        store.store.read().unwrap().notifications().unread_count(),
        199,
        "control Enter must not open/read the selected event"
    );
    {
        let position = cx.debug_bounds("notification-clear").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        store
            .store
            .read()
            .unwrap()
            .notifications()
            .entries()
            .is_empty()
    );
    assert_eq!(root.read_with(cx, |root, _| root.notification_selected), 0);
    assert!(root.read_with(cx, |root, _| notifications_displayed(root)));
}

#[gpui::test]
fn notification_project_change_keeps_global_feed_over_remembered_closed_inspector(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let services = notification_services(200);
    let store = services.store.clone();
    let other = {
        let mut store = store.store.write().unwrap();
        let mut other = store
            .sessions()
            .get(&SessionId::new("notification-preview-199"))
            .unwrap()
            .as_ref()
            .clone();
        let project = store
            .projects()
            .values()
            .find(|project| project.id != other.project_id)
            .unwrap()
            .clone();
        other.project_id = project.id.clone();
        other.cwd = project.root.clone();
        store.upsert_session(other.clone());
        store
            .update_preferences(|prefs| {
                prefs.inspector_projects.insert(
                    other.project_id.0.clone(),
                    crate::store::InspectorProjectState {
                        open: false,
                        tab: crate::store::InspectorTab::Info,
                    },
                );
            })
            .unwrap();
        other
    };
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let mut root = RootView::new_with_selection(
            services,
            false,
            PreviewScenario::Artifacts,
            None,
            Some(Some(SessionId::new("notification-preview-0"))),
            window,
            cx,
        );
        root.toggle_notifications(window, cx);
        root
    });
    cx.simulate_resize(size(px(1200.0), px(800.0)));
    cx.run_until_parked();
    {
        let position = cx.debug_bounds("notification-filter").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    let before = store
        .store
        .read()
        .unwrap()
        .notifications()
        .entries()
        .iter()
        .map(|entry| (entry.id.clone(), entry.read))
        .collect::<Vec<_>>();
    root.update_in(cx, |root, _, cx| {
        root.window_store.write().unwrap().select(other.id.clone());
        root.sync_inspector_context(cx);
        cx.notify();
    });
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert!(notifications_displayed(root));
        assert_eq!(root.active_project_id(cx), Some(other.project_id.0.clone()));
        assert!(!root.notification_filter_unread);
        assert_eq!(root.notification_rows().len(), 200);
        assert!(!root.inspector.as_ref().unwrap().read(cx).is_visible());
        assert!(
            !root
                .inspector
                .as_ref()
                .unwrap()
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        );
    });
    assert_eq!(
        store
            .store
            .read()
            .unwrap()
            .notifications()
            .entries()
            .iter()
            .map(|entry| (entry.id.clone(), entry.read))
            .collect::<Vec<_>>(),
        before,
        "project switching must not bulk-read or project-filter the inbox"
    );
    {
        let position = cx.debug_bounds("INSPECTOR_STRIP_Notes").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
    };
    cx.run_until_parked();
    root.read_with(cx, |root, cx| {
        assert!(root.inspector_open);
        assert_eq!(root.right_sidebar_content, RightSidebarContent::Workspace);
        let inspector = root.inspector.as_ref().unwrap().read(cx);
        assert!(inspector.is_visible());
        assert_eq!(
            inspector.selected_workspace(),
            Some(WorkspaceSurface::Notes)
        );
    });
}

#[gpui::test]
fn notification_narrow_options_and_full_height_list_are_contained_in_both_themes(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let services = notification_services(200);
    services
        .store
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| {
            prefs.sidebar_width = 200.0;
        })
        .unwrap();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let mut root = RootView::new(services, false, PreviewScenario::Artifacts, window, cx);
        root.inspector_width = 300.0;
        root.toggle_notifications(window, cx);
        root.notification_options_open = true;
        root
    });
    for theme in ["rose-pine", "github-light"] {
        root.update_in(cx, |root, _, cx| {
            root.window_store
                .write()
                .unwrap()
                .update_preferences(|prefs| {
                    prefs.follow_system_theme = false;
                    prefs.terminal_theme = theme.into();
                })
                .unwrap();
            cx.notify();
        });
        cx.simulate_resize(size(px(900.0), px(560.0)));
        cx.run_until_parked();
        let panel = cx.debug_bounds("notification-panel").unwrap();
        let list = cx.debug_bounds("notification-list").unwrap();
        assert_eq!(
            panel.size.width,
            px(299.0),
            "300px seam includes its separator"
        );
        assert!(panel.left() >= px(0.0) && panel.right() <= px(900.0));
        assert!(panel.top() >= px(0.0) && panel.bottom() <= px(560.0));
        assert!(list.left() >= panel.left() && list.right() <= panel.right());
        assert!(list.top() >= panel.top() && list.bottom() <= panel.bottom());
        assert!(list.size.height > px(0.0));
        for selector in [
            "notification-filter",
            "notification-read-all",
            "notification-options",
            "notification-alerts",
            "notification-sounds",
            "notification-test",
            "notification-clear",
        ] {
            let control = cx.debug_bounds(selector).unwrap();
            assert!(
                control.left() >= panel.left() && control.right() <= panel.right(),
                "{theme}: {selector} must fit the 300px sidebar"
            );
            assert!(
                control.top() >= panel.top() && control.bottom() <= panel.bottom(),
                "{theme}: {selector} must remain reachable"
            );
        }
        let old_sounds = root.read_with(cx, |root, _| {
            root.window_store
                .read()
                .unwrap()
                .preferences()
                .status_sounds
        });
        {
            let position = cx.debug_bounds("notification-sounds").unwrap().center();
            cx.simulate_click(position, Modifiers::default());
        };
        cx.run_until_parked();
        assert_eq!(
            root.read_with(cx, |root, _| root
                .window_store
                .read()
                .unwrap()
                .preferences()
                .status_sounds),
            !old_sounds
        );
        cx.simulate_resize(size(px(1200.0), px(800.0)));
        cx.run_until_parked();
        let panel = cx.debug_bounds("notification-panel").unwrap();
        let list = cx.debug_bounds("notification-list").unwrap();
        assert_eq!(panel.size.width, px(299.0));
        let first = cx.debug_bounds("notification-row-0").unwrap();
        assert!(
            list.size.height > first.size.height * 7.0,
            "a docked list must not retain the seven-row popup cap"
        );
        let eighth = cx.debug_bounds("notification-row-7").unwrap();
        assert!(
            eighth.top() >= list.top() && eighth.bottom() <= list.bottom(),
            "more than seven rows must be visibly available"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes the production notification sidebar screenshot artifact"]
fn render_notification_panel_preview_screenshot() {
    let output = std::path::PathBuf::from(
        std::env::var_os("UBRA_VISUAL_OUTPUT").expect("set UBRA_VISUAL_OUTPUT"),
    );
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(ubra_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
    });
    let services = notification_services(200);
    services
        .store
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| {
            prefs.follow_system_theme = false;
            prefs.terminal_theme = if std::env::var_os("UBRA_VISUAL_LIGHT").is_some() {
                "github-light".into()
            } else {
                "rose-pine".into()
            };
        })
        .unwrap();
    let window = cx
        .open_window(size(px(1200.0), px(800.0)), move |window, cx| {
            cx.new(|cx| {
                let mut root =
                    RootView::new(services, false, PreviewScenario::Artifacts, window, cx);
                if std::env::var_os("UBRA_VISUAL_NARROW").is_some() {
                    root.inspector_width = 300.0;
                }
                root.toggle_notifications(window, cx);
                root.notification_options_open = std::env::var_os("UBRA_VISUAL_OPTIONS").is_some();
                root
            })
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let screenshot = cx.capture_screenshot(window.into()).unwrap();
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    screenshot.save(output).unwrap();
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
