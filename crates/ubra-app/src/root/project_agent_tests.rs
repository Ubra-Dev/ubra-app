use super::*;
use gpui::{HeadlessAppContext, size};

#[gpui::test]
fn single_agent_startup_restores_pane_header_before_new_session(cx: &mut gpui::TestAppContext) {
    use ubra_proto::workspace::{WorkspaceMutation, WorkspaceMutationParams};
    // The disposable Engine delivers real socket responses from Tokio workers.
    cx.executor().allow_parking();
    for delayed_sessions in [false, true] {
        let fixture = crate::workspace_fixture::LiveWorkspace::start();
        let runtime = fixture.services.store.clone();
        let listing = fixture.services.tokio.block_on(async {
            let client = runtime.client();
            let review = SessionId::new("review");
            client.kill(&review).await.unwrap();
            client.remove(&review).await.unwrap();
            let snapshot = client.workspaces().await.unwrap();
            client
                .mutate_workspace(&WorkspaceMutationParams {
                    expected_revision: snapshot.revision,
                    mutation: WorkspaceMutation::RemoveWorkspace {
                        workspace_id: fixture.workspace.clone(),
                    },
                })
                .await
                .unwrap();
            client.sessions().await.unwrap()
        });
        assert_eq!(listing.sessions.len(), 1);
        let hydration_gate = delayed_sessions.then(|| fixture.hold_catalog());
        let runtime = if delayed_sessions {
            let _entered = fixture.services.tokio.enter();
            Arc::new(
                crate::store::StoreRuntime::start(
                    Arc::new(ubra_client::DaemonClient::with_socket_path(
                        fixture.directory.path().join("engine.sock"),
                    )),
                    fixture.directory.path().join("cold-prefs.json"),
                )
                .unwrap(),
            )
        } else {
            runtime
        };
        {
            let mut store = runtime.store.write().expect("store");
            store
                .update_preferences(|prefs| {
                    prefs.active_workspace = None;
                    prefs.confirm_before_closing_session = false;
                })
                .unwrap();
            if !delayed_sessions {
                store.hydrate(listing);
            } else {
                assert!(!store.has_hydrated_sessions());
            }
        }
        let services = Arc::new(crate::AppServices {
            store: runtime.clone(),
            usage_tx: fixture.services.usage_tx.clone(),
            updates: crate::updates::inert(),
            dev_build: None,
            daemon_startup: None,
            tokio: fixture.services.tokio.clone(),
        });
        let mut app = cx.clone();
        let (root, view) = app.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        if delayed_sessions {
            view.run_until_parked();
            assert!(root.read_with(view, |root, _| root.active_workspace.is_none()));
        }
        drop(hydration_gate);
        let deadline = Instant::now() + Duration::from_secs(5);
        while root.read_with(view, |root, _| root.active_workspace.is_none()) {
            assert!(
                Instant::now() < deadline,
                "startup never opened the agent layout"
            );
            view.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
        }
        view.run_until_parked();
        assert_eq!(
            root.read_with(view, |root, cx| root.active_session_id(cx)),
            Some(SessionId::new("build")),
        );
        let snapshot = fixture
            .services
            .tokio
            .block_on(runtime.client().workspaces())
            .unwrap();
        let tab = &snapshot.workspaces[0].tabs[0];
        let pane = &tab.focused_pane;
        for selector in [
            format!("split-pane-right-{}", pane.0),
            format!("split-pane-bottom-{}", pane.0),
            format!("remove-pane-{}", pane.0),
        ] {
            assert!(view.debug_bounds(&selector).is_some(), "missing {selector}");
        }
        let split = view
            .debug_bounds(&format!("split-pane-right-{}", pane.0))
            .unwrap();
        view.simulate_mouse_move(split.center(), None, gpui::Modifiers::default());
        view.run_until_parked();
        assert!(
            view.debug_bounds("split-agent-menu").is_some(),
            "split did not offer agents"
        );
        let close = view
            .debug_bounds(&format!("remove-pane-{}", pane.0))
            .unwrap();
        view.simulate_click(close.center(), gpui::Modifiers::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            view.run_until_parked();
            let closed = runtime
                .store
                .read()
                .expect("store")
                .sessions()
                .get(&SessionId::new("build"))
                .is_none_or(|session| {
                    matches!(session.status, ubra_proto::SessionStatus::Exited(_))
                });
            if closed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Close did not stop the selected agent"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        view.update(|window, _| window.remove_window());
    }
}

#[test]
#[ignore = "native project/agent navigation with disposable Engine, PTYs and screenshots"]
fn project_agents_remain_visible_and_open_preserved_layouts() {
    let fixture = crate::workspace_fixture::LiveWorkspace::start();
    fixture
        .services
        .store
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| {
            prefs.terminal_theme = if std::env::var_os("UBRA_VISUAL_LIGHT").is_some() {
                "github-light"
            } else {
                "ubra"
            }
            .into();
        })
        .unwrap();
    let driver = fixture.continuous_output();
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(ubra_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
        commands::bind_keys(cx, &Default::default());
    });
    let services = fixture.services.clone();
    let root = cx
        .open_window(size(px(1100.0), px(720.0)), |window, cx| {
            cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
        })
        .unwrap();
    macro_rules! update {
        ($body:expr) => {
            cx.update_window(root.into(), |view, window, cx| {
                view.downcast::<RootView>()
                    .unwrap()
                    .update(cx, |root, cx| ($body)(root, window, cx))
            })
            .unwrap()
        };
    }
    macro_rules! settle {
        () => {
            for _ in 0..25 {
                cx.advance_clock(Duration::from_millis(16));
                cx.update_window(root.into(), |_, window, cx| window.simulate_next_frame(cx))
                    .unwrap();
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(4));
            }
        };
    }
    macro_rules! wait {
        ($condition:expr) => {{
            let deadline = Instant::now() + Duration::from_secs(10);
            while !$condition {
                assert!(Instant::now() < deadline, "project navigation deadline");
                settle!();
            }
        }};
    }
    let snapshot = || {
        fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().workspaces())
            .unwrap()
    };
    let initial = snapshot();
    let original_layout = initial.workspaces[0].tabs[0].layout.clone();
    let original_tab = initial.workspaces[0].tabs[0].id.clone();
    let capture = |cx: &mut HeadlessAppContext, name: &str| {
        if let Some(directory) = std::env::var_os("UBRA_PROJECT_AGENT_SCREENSHOTS") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            cx.capture_screenshot(root.into())
                .unwrap()
                .save(directory.join(name))
                .unwrap();
        }
    };
    settle!();
    // These bounds only exist if the ordinary agent rows were actually painted.
    let point = update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
        let sidebar = root.sidebar.read(cx);
        assert!(
            sidebar
                .project_agent_center_for_test(&SessionId::new("build"))
                .is_some()
        );
        sidebar
            .project_agent_center_for_test(&SessionId::new("review"))
            .expect("review agent remains visible in workspace")
    });
    cx.update_window(root.into(), |_, window, cx| {
        window.simulate_mouse_move(point, cx);
        window.dispatch_event(
            gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                button: MouseButton::Left,
                position: point,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                button: MouseButton::Left,
                position: point,
                modifiers: Default::default(),
                click_count: 1,
            }),
            cx,
        );
    })
    .unwrap();
    wait!(
        update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root.active_session_id(cx))
            == Some(SessionId::new("review"))
    );
    let after_click = snapshot();
    assert_eq!(after_click.workspaces.len(), 1);
    assert_eq!(after_click.workspaces[0].tabs[0].id, original_tab);
    assert_eq!(after_click.workspaces[0].tabs[0].layout, original_layout);
    fixture.verify_process_identity();
    settle!();
    capture(&mut cx, "project-agents-vertical.png");
    update!(
        |root: &mut RootView, _, cx: &mut Context<RootView>| root.sidebar.update(
            cx,
            |sidebar, cx| {
                sidebar
                    .set_tab_orientation(crate::store::TabOrientation::Horizontal, cx)
                    .unwrap();
                sidebar.reveal(cx);
            }
        )
    );
    settle!();
    capture(&mut cx, "project-agents-horizontal.png");
    // The Projects menu is anchored to the header and must not reveal the sidebar
    // or change the live terminal's geometry when opened or dismissed.
    update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root
        .sidebar
        .update(cx, |sidebar, cx| sidebar.conceal(cx)));
    settle!();
    let (picker, geometry) = update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
        assert!(!root.sidebar.read(cx).is_visible());
        (
            root.sidebar
                .read(cx)
                .project_picker_center_for_test()
                .unwrap(),
            root.active_terminal(cx)
                .unwrap()
                .read(cx)
                .geometry_for_test()
                .0,
        )
    });
    cx.update_window(root.into(), |_, window, cx| {
        window.simulate_mouse_move(picker, cx);
        window.dispatch_event(
            gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                button: MouseButton::Left,
                position: picker,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                button: MouseButton::Left,
                position: picker,
                modifiers: Default::default(),
                click_count: 1,
            }),
            cx,
        );
    })
    .unwrap();
    settle!();
    update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
        assert!(root.sidebar.read(cx).project_picker_is_open_for_test());
        assert!(!root.sidebar.read(cx).is_visible());
        assert!(!root.sidebar.read(cx).is_peeking());
        assert_eq!(
            root.active_terminal(cx)
                .unwrap()
                .read(cx)
                .geometry_for_test()
                .0,
            geometry
        );
    });
    capture(&mut cx, "projects-dropdown.png");
    cx.update_window(root.into(), |_, window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("escape").unwrap(), cx);
    })
    .unwrap();
    settle!();
    update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
        assert!(!root.sidebar.read(cx).project_picker_is_open_for_test());
        assert!(!root.sidebar.read(cx).is_visible());
        assert_eq!(
            root.active_terminal(cx)
                .unwrap()
                .read(cx)
                .geometry_for_test()
                .0,
            geometry
        );
    });
    capture(&mut cx, "toolbar-open.png");
    for visible in [false, true] {
        cx.update_window(root.into(), |_, window, cx| {
            window.dispatch_keystroke(gpui::Keystroke::parse("cmd-b").unwrap(), cx);
        })
        .unwrap();
        settle!();
        update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
            assert_eq!(root.sidebar.read(cx).horizontal_tabs_visible(), visible);
            assert!(!root.sidebar.read(cx).is_visible());
            assert_eq!(root.active_session_id(cx), Some(SessionId::new("review")));
        });
        fixture.verify_process_identity();
        if !visible {
            capture(&mut cx, "toolbar-hidden.png");
        }
    }
    assert_eq!(snapshot().workspaces[0].tabs[0].layout, original_layout);
    // Leave the explicit layout, then use the same agent-first navigation.
    // The Engine adopts the one unambiguous project layout, preserving the split.
    update!(
        |root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>| {
            root.sidebar
                .update(cx, |sidebar, cx| sidebar.activate_workspace(None, cx));
            root.open_workspace_launch_session(SessionId::new("build"), window, cx);
        }
    );
    wait!(snapshot().workspaces[0].project_id.is_some());
    wait!(
        update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root.active_session_id(cx))
            == Some(SessionId::new("build"))
    );
    let adopted = snapshot();
    assert_eq!(adopted.workspaces.len(), 1);
    assert_eq!(adopted.workspaces[0].tabs[0].layout, original_layout);
    assert_eq!(adopted.workspaces[0].tabs[0].id, original_tab);
    fixture.verify_process_identity();
    assert!(driver.ticks() > 0);
    drop(driver);
    cx.update_window(root.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
