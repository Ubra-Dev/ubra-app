//! One of every status-bar segment, lit at once, so the strip can be
//! clicked through in a running dev window:
//!
//! ```sh
//! UBRA_SIDEBAR_PREVIEW=1 UBRA_SIDEBAR_SCENARIO=statusbar scripts/dev.sh
//! ```
//!
//! The selected session carries every active-session segment (status, SSH
//! location, branch, worktree, progress, context, ports) while the chrome,
//! context, and update values it cannot own in preview mode are synthesized
//! by `RootView::sync_status_bar` for this scenario only. Sibling sessions
//! cover the remaining statuses; switch to each to see its pill.

use super::*;

pub(super) fn make(now: f64) -> SidebarPreviewFixture {
    let ubra = project("preview-ubra", "/Users/preview/Projects/ubra", "Ubra");
    let anara = project("preview-anara", "/Users/preview/Projects/anara", "Anara");

    // Selected: carries every per-session segment at once.
    let mut working: SessionRecord = session(
        "statusbar-working",
        AgentKind::CODEX,
        &ubra,
        "Rebuild the bottom status bar",
        SessionStatus::Working,
        Some("feature/status-bar-mockup"),
        now - minutes(18.0),
    )
    .into();
    working.host = Some("preview-ssh".into());
    working.worktree_path = Some("/Users/preview/Projects/ubra/.ubra/worktrees/status-bar".into());
    working.terminal_progress = Some(ubra_proto::TerminalProgress {
        state: ubra_proto::TerminalProgressState::Normal,
        percent: 42,
    });
    working.foreground_ports = Some(vec![
        PortInfo {
            port: 3000,
            process_name: "node".into(),
        },
        PortInfo {
            port: 3001,
            process_name: "vite".into(),
        },
    ]);

    let permission: SessionRecord = session(
        "statusbar-permission",
        AgentKind::CLAUDE_CODE,
        &ubra,
        "Prepare the signed 0.1.0 release",
        SessionStatus::NeedsInput(NeedsInputKind::Permission),
        Some("release/0.1.0"),
        now - minutes(42.0),
    )
    .needs_input(NeedsInputDetail {
        kind: NeedsInputKind::Permission,
        source: NeedsInputSource::ClaudePermissionHook,
        tool_name: Some("Bash".into()),
        summary: "Wants to publish the release tag".into(),
        prompt_excerpt: None,
        options: None,
        risk_hint: RiskHint::Network,
        secret: false,
        occurred_at: DateMillis(now - 45_000.0),
    })
    .into();

    let question: SessionRecord = session(
        "statusbar-question",
        AgentKind::CODEX,
        &anara,
        "Rework the document import flow",
        SessionStatus::NeedsInput(NeedsInputKind::Question),
        Some("import-flow"),
        now - minutes(31.0),
    )
    .needs_input(NeedsInputDetail {
        kind: NeedsInputKind::Question,
        source: NeedsInputSource::CodexNotify,
        tool_name: None,
        summary: "Which empty-state direction should I use?".into(),
        prompt_excerpt: None,
        options: Some(vec!["Editorial".into(), "Compact".into()]),
        risk_hint: RiskHint::Neutral,
        secret: false,
        occurred_at: DateMillis(now - 120_000.0),
    })
    .into();

    // Idle with an unseen completion: the done half of the attention queue.
    let mut done: SessionRecord = session(
        "statusbar-done",
        AgentKind::CLAUDE_CODE,
        &ubra,
        "Migrate the remaining tooltip copy",
        SessionStatus::Idle,
        Some("tooltips"),
        now - hours(1.0),
    )
    .into();
    done.last_turn_completed_at = Some(DateMillis(now - minutes(5.0)));
    done.last_seen_at = Some(DateMillis(now - minutes(10.0)));

    let mut idle: SessionRecord = session(
        "statusbar-idle",
        AgentKind::SHELL,
        &anara,
        "Dev server · localhost:3000",
        SessionStatus::Idle,
        Some("main"),
        now - hours(2.2),
    )
    .into();
    idle.last_seen_at = Some(DateMillis(now - 60_000.0));

    let ended: SessionRecord = session(
        "statusbar-ended",
        AgentKind::CURSOR,
        &anara,
        "Cursor accessibility pass",
        SessionStatus::Exited(ExitInfo {
            reason: ExitReason::Exited,
            code: Some(0),
            signal: None,
            system_restart: false,
        }),
        Some("accessibility"),
        now - hours(7.0),
    )
    .into();
    let sessions = vec![working.clone(), permission, question, done, idle, ended];
    let prefs = Prefs {
        sidebar_visible: true,
        sidebar_project_order: vec![ubra.id.clone(), anara.id.clone()],
        sidebar_session_order: sessions.iter().map(|session| session.id.clone()).collect(),
        ..Prefs::default()
    };
    SidebarPreviewFixture {
        list: SessionListResult {
            sessions,
            projects: vec![ubra, anara],
        },
        selected_session_id: Some(working.id),
        prefs,
    }
}
