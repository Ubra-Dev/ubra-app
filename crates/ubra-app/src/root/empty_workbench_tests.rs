use super::*;
use gpui::{HeadlessAppContext, Modifiers, size};
use ubra_proto::workspace::*;

/// End-to-end through the real UI and the real Engine: the window-owned wizard
/// must launch exactly the advertised sessions into one workspace and then get
/// out of the way. Fixture receipts alone would not prove this path.
#[test]
#[ignore = "native wizard launch with a disposable Engine, PTYs and screenshots"]
fn empty_workbench_launches_the_selected_layout_from_the_ui() {
    let fixture = crate::workspace_fixture::LiveWorkspace::start();
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
        ($selector:expr) => {
            cx.debug_bounds(root.into(), $selector)
                .unwrap()
                .unwrap_or_else(|| panic!("{} was never painted", $selector))
        };
    }
    macro_rules! click {
        ($selector:expr) => {{
            let point = bounds!($selector).center();
            cx.update_window(root.into(), |_, window, cx| {
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
    let capture = |cx: &mut HeadlessAppContext, name: &str| {
        if let Some(directory) = std::env::var_os("UBRA_EMPTY_WORKBENCH_SCREENSHOTS") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            cx.capture_screenshot(root.into())
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
    assert!(bounds!("empty-wizard-card").size.width > px(0.0));
    capture(&mut cx, "empty-workbench-project.png");
    assert_eq!(
        sessions().len(),
        before,
        "opening the wizard must not launch anything"
    );

    // Page one: choose a real project root, then advance without launching.
    click!("empty-folder");
    click!("empty-picker-row-0");
    // The agent roster is a list of brands, so its rows carry the marks.
    click!("empty-agent");
    capture(&mut cx, "empty-workbench-agents.png");
    cx.update_window(root.into(), |_, window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("escape").unwrap(), cx);
    })
    .unwrap();
    settle!();
    assert_eq!(
        sessions().len(),
        before,
        "choosing a folder must not launch anything"
    );
    click!("empty-get-started");
    assert!(
        cx.debug_bounds(root.into(), "empty-launch")
            .unwrap()
            .is_some(),
        "Get Started advances to the layout page"
    );
    click!("empty-preset-0");
    capture(&mut cx, "empty-workbench-layout.png");
    click!("empty-more-layouts");
    click!("empty-preset-7");
    capture(&mut cx, "empty-workbench-sixteen.png");
    click!("empty-preset-0");
    assert_eq!(
        sessions().len(),
        before,
        "selecting a layout must not launch anything"
    );

    // Confirmation is the only admission point.
    click!("empty-launch");
    wait!(sessions().len() == before + 1, "one launched session");
    wait!(
        update!(|root: &mut RootView, _, _| root.empty_workbench.is_none()),
        "the wizard to close after a successful launch"
    );
    settle!();

    let launched = sessions();
    assert_eq!(
        launched.len(),
        before + 1,
        "exactly one session was admitted, without respawn"
    );
    let root_pane = fixture
        .directory
        .path()
        .join("Ubra")
        .canonicalize()
        .unwrap();
    let shell = launched
        .iter()
        .filter(|record| record.kind == AgentKind::SHELL && record.host.is_none())
        .find(|record| std::path::Path::new(&record.cwd) == root_pane);
    assert!(
        shell.is_some(),
        "the launched session runs in the chosen project root: {:?}",
        launched
            .iter()
            .map(|record| record.cwd.clone())
            .collect::<Vec<_>>()
    );
    let selected = fixture
        .services
        .store
        .store
        .read()
        .unwrap()
        .selected_session_id()
        .cloned();
    assert_eq!(
        selected.as_ref(),
        shell.map(|record| &record.id),
        "the launched session becomes the selected session"
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
