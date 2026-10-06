use super::*;
use gpui::{HeadlessAppContext, Modifiers, size};
use ubra_proto::workspace::*;

/// End-to-end through headless GPUI and the real Engine: the dedicated wizard
/// window must launch exactly the advertised sessions into its owner's workspace
/// and then close. Fixture receipts alone would not prove this path.
#[test]
#[ignore = "headless wizard launch with a disposable Engine, PTYs and optional screenshots"]
fn empty_workbench_launches_the_selected_layout_from_the_ui() {
    let fixture = crate::workspace_fixture::LiveWorkspace::start();
    let new_project = fixture.directory.path().join("New project");
    std::fs::create_dir(&new_project).unwrap();
    fixture
        .services
        .store
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| {
            prefs.default_agent = AgentKind::SHELL;
            prefs.terminal_theme = "ubra".into();
        })
        .unwrap();
    // A saved-but-empty workspace: the wizard's per-workspace entry.
    let client = fixture.services.store.client().clone();
    let workspace = fixture.services.tokio.block_on(async {
        let before = client.workspaces().await.unwrap();
        let created = client
            .mutate_workspace(&WorkspaceMutationParams {
                expected_revision: before.revision,
                mutation: WorkspaceMutation::CreateWorkspace {
                    name: "Wizard empty".into(),
                },
            })
            .await
            .unwrap();
        created
            .workspaces
            .iter()
            .find(|candidate| !before.workspaces.iter().any(|old| old.id == candidate.id))
            .expect("created workspace")
            .id
            .clone()
    });
    fixture
        .services
        .store
        .store
        .write()
        .unwrap()
        .refresh_workspaces();
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
        .open_window(size(px(1280.0), px(860.0)), |window, cx| {
            let services = services.clone();
            let workspace = workspace.clone();
            cx.new(move |cx| {
                RootView::new_with_workspace(
                    services,
                    false,
                    PreviewScenario::Empty,
                    Some(Some(workspace)),
                    window,
                    cx,
                )
            })
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
            for _ in 0..20 {
                cx.advance_clock(Duration::from_millis(16));
                cx.update_window(root.into(), |_, window, cx| window.simulate_next_frame(cx))
                    .unwrap();
                if let Some(wizard) = update!(|root: &mut RootView, _, _| root.empty_workbench) {
                    cx.update_window(wizard.into(), |_, window, cx| {
                        window.simulate_next_frame(cx)
                    })
                    .unwrap();
                }
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(4));
            }
        };
    }
    macro_rules! wait {
        ($condition:expr, $label:expr) => {{
            let deadline = Instant::now() + Duration::from_secs(30);
            while !$condition {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {}",
                    $label
                );
                settle!();
            }
        }};
    }
    macro_rules! bounds {
        ($window:expr, $selector:expr) => {
            cx.debug_bounds($window.into(), $selector)
                .unwrap()
                .unwrap_or_else(|| panic!("{} was never painted", $selector))
        };
    }
    macro_rules! click {
        ($window:expr, $selector:expr) => {{
            let point = bounds!($window, $selector).center();
            cx.update_window($window.into(), |_, window, cx| {
                window.simulate_mouse_move(point, cx);
                window.dispatch_event(
                    gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                        button: MouseButton::Left,
                        position: point,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                        button: MouseButton::Left,
                        position: point,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            })
            .unwrap();
            settle!();
        }};
    }
    let sessions = || {
        fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().sessions())
            .unwrap()
            .sessions
    };
    let destination = |workspace: &WorkspaceId| {
        fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().workspaces())
            .unwrap()
            .workspaces
            .into_iter()
            .find(|candidate| &candidate.id == workspace)
            .expect("destination workspace survives")
    };
    let before = sessions().len();
    let capture = |cx: &mut HeadlessAppContext, wizard: gpui::AnyWindowHandle, name: &str| {
        if let Some(directory) = std::env::var_os("UBRA_EMPTY_WORKBENCH_SCREENSHOTS") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            cx.capture_screenshot(wizard)
                .unwrap()
                .save(directory.join(name))
                .unwrap();
        }
    };

    settle!();
    wait!(
        update!(|root: &mut RootView, _, _| root.empty_workbench.is_some()),
        "the empty workspace to open the wizard"
    );
    let wizard =
        update!(|root: &mut RootView, _, _| root.empty_workbench.expect("native wizard open"));
    assert_ne!(wizard.window_id(), root.window_id());
    cx.update_window(wizard.into(), |_, window, _| {
        assert_eq!(window.owned_dialog_parent(), Some(root.into()));
    })
    .unwrap();
    capture(&mut cx, wizard.into(), "empty-workbench-project.png");
    assert_eq!(
        sessions().len(),
        before,
        "opening the wizard must not launch anything"
    );

    // An already imported project cannot advance or admit another workspace.
    click!(wizard, "empty-folder");
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|_| Some(vec![fixture.directory.path().join("Ubra")]));
    settle!();
    click!(wizard, "empty-get-started");
    assert!(
        cx.debug_bounds(wizard.into(), "empty-launch")
            .unwrap()
            .is_none(),
        "an existing project cannot reach launch confirmation"
    );
    assert_eq!(sessions().len(), before);
    assert!(destination(&workspace).tabs.is_empty());

    // Choose a fresh folder from the onboarding control, then advance.
    click!(wizard, "empty-folder");
    cx.simulate_path_prompt_response(|_| Some(vec![new_project.clone()]));
    settle!();
    // Select Terminal explicitly; installed/MRU agents can differ by fixture host.
    click!(wizard, "empty-agent");
    capture(&mut cx, wizard.into(), "empty-workbench-agents.png");
    // Keyboard navigation scrolls off-screen rows into view before selecting.
    // A bounds-only mouse click can otherwise hit content below the clipped list.
    wait!(
        cx.update_window(wizard.into(), |view, window, cx| {
            let selected = view
                .downcast::<crate::empty_workbench::EmptyWorkbenchView>()
                .unwrap()
                .read(cx)
                .picker_agent_for_test()
                == Some(&AgentKind::SHELL);
            if !selected {
                window.dispatch_keystroke(gpui::Keystroke::parse("down").unwrap(), cx);
            }
            selected
        })
        .unwrap_or(false),
        "Terminal picker selection"
    );
    cx.update_window(wizard.into(), |_, window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("enter").unwrap(), cx);
    })
    .unwrap();
    settle!();
    assert_eq!(
        sessions().len(),
        before,
        "choosing a folder must not launch anything"
    );
    click!(wizard, "empty-get-started");
    assert!(
        cx.debug_bounds(wizard.into(), "empty-launch")
            .unwrap()
            .is_some(),
        "Get Started advances to the layout page"
    );
    click!(wizard, "empty-preset-0");
    capture(&mut cx, wizard.into(), "empty-workbench-layout.png");
    click!(wizard, "empty-more-layouts");
    click!(wizard, "empty-preset-7");
    capture(&mut cx, wizard.into(), "empty-workbench-sixteen.png");
    click!(wizard, "empty-preset-0");
    assert_eq!(
        sessions().len(),
        before,
        "selecting a layout must not launch anything"
    );

    // Confirmation is the only admission point.
    click!(wizard, "empty-launch");
    wait!(sessions().len() == before + 1, "one launched session");
    wait!(
        update!(|root: &mut RootView, _, _| root.empty_workbench.is_none()),
        "the wizard to close after a successful launch"
    );
    settle!();
    assert!(
        cx.update(|cx| !cx.windows().contains(&wizard.into())),
        "successful admission closes the native wizard window"
    );

    let launched = sessions();
    assert_eq!(
        launched.len(),
        before + 1,
        "exactly one session was admitted, without respawn"
    );
    let root_pane = new_project.canonicalize().unwrap();
    let shell = launched
        .iter()
        .filter(|record| record.kind == AgentKind::SHELL && record.host.is_none())
        .find(|record| std::path::Path::new(&record.cwd) == root_pane);
    assert!(
        shell.is_some(),
        "expected local Terminal in {root_pane:?}; actual sessions: {:?}",
        launched
            .iter()
            .map(|record| (&record.kind, &record.host, &record.cwd))
            .collect::<Vec<_>>()
    );
    let selected = update!(|root: &mut RootView, _, _| {
        root.window_store
            .read()
            .expect("window store")
            .selected_session_id()
            .cloned()
    });
    assert_eq!(
        selected.as_ref(),
        shell.map(|record| &record.id),
        "the launched session becomes selected in its owning workbench"
    );
    let saved = destination(&workspace);
    assert_eq!(
        saved.tabs.len(),
        1,
        "the selected empty workspace received exactly one tab"
    );
    let panes = match &saved.tabs[0].layout {
        LayoutNode::Pane { .. } => 1,
        _ => panic!("the Single preset must commit one pane"),
    };
    assert_eq!(panes, 1);
    assert!(
        saved.selected_tab.as_ref() == Some(&saved.tabs[0].id),
        "the launched tab becomes the selected tab"
    );
    fixture.verify_process_identity();
    cx.update_window(root.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| cx.windows().is_empty()));
}
