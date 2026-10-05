//! A terminal whose job stops at a question (`Proceed? [y/N]`, `Password:`,
//! a script's `read`) needs the user exactly as an Agent at a permission
//! prompt does, and stops needing them the moment it is answered. A job that
//! is only quiet, and a full-screen program reading keys, never does.
//!
//! Real shells on real PTYs, for both ways a local session owns one.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ubra_engine::session::{HolderConfig, Session, SessionSpec};
use ubra_engine::{Authority, ManifestEngine, PtySpec};
use ubra_proto::{NeedsInputKind, NeedsInputSource, SessionStatus};

fn engine() -> Arc<ManifestEngine> {
    let dir = ubra_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ubra-line-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn shell(id: &str, root: &Path, holder: Option<HolderConfig>) -> Session {
    let spec = SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-i".into()], root)
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("PS1", "$ ")
            .size(80, 24),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder,
        remote: None,
        defer_launch: false,
    };
    Session::spawn(spec, engine()).expect("spawn")
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn needs_input(session: &Session) -> bool {
    matches!(session.status(), SessionStatus::NeedsInput(_))
}

/// Watches a job for longer than the settle and every sample cadence, and
/// fails the moment it is flagged.
fn never_flagged(session: &Session, what: &str) {
    let until = Instant::now() + Duration::from_millis(2_500);
    while Instant::now() < until {
        assert!(
            !needs_input(session),
            "{what} was flagged as needing input: {:?} {:?}",
            session.view().needs_input,
            session.screen_lines()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn at_prompt(session: &Session) -> bool {
    session.status() == SessionStatus::Idle
        && session
            .screen_lines()
            .last()
            .is_some_and(|line| line.trim_end() == "$")
}

fn exercise(session: &mut Session) {
    wait_until("the prompt", || at_prompt(session));

    // A script asks, and the terminal needs the user.
    session
        .write_input(b"sh -c 'printf \"Proceed? [y/N] \"; read answer; sleep 1'\r")
        .unwrap();
    wait_until("the question to be flagged", || needs_input(session));
    let view = session.view();
    assert_eq!(
        view.status,
        SessionStatus::NeedsInput(NeedsInputKind::Question)
    );
    let detail = view.needs_input.expect("detail");
    assert_eq!(detail.source, NeedsInputSource::TerminalLine);
    assert_eq!(
        detail.summary,
        "Proceed? [y/N]",
        "screen: {:?}",
        session.screen_lines()
    );
    assert!(!detail.secret);

    // Answering clears it at once, although the job runs on.
    session.write_input(b"y\r").unwrap();
    wait_until("the answer to clear it", || !needs_input(session));
    assert!(session.view().needs_input.is_none());
    wait_until("the job to end", || at_prompt(session));

    // A password prompt is flagged with none of the screen's text.
    session
        .write_input(b"sh -c 'stty -echo; printf \"Password: \"; read p; stty echo'\r")
        .unwrap();
    wait_until("the password prompt to be flagged", || needs_input(session));
    let detail = session.view().needs_input.expect("detail");
    assert_eq!(detail.summary, "Waiting for a password");
    assert_eq!(detail.prompt_excerpt, None);
    assert!(detail.secret);
    // Interrupting the job ends the question too.
    session.write_input(b"\x03").unwrap();
    wait_until("the interrupted job to clear it", || {
        !needs_input(session) && at_prompt(session)
    });

    // Quiet is not waiting.
    session.write_input(b"sleep 30\r").unwrap();
    wait_until("sleep to run", || {
        session.status() == SessionStatus::Working
    });
    never_flagged(session, "`sleep`");
    session.write_input(b"\x03").unwrap();
    wait_until("the prompt after sleep", || at_prompt(session));

    // Neither is a raw-mode reader waiting on a key.
    session
        .write_input(b"stty raw -echo; dd bs=1 count=1 2>/dev/null; stty sane\r")
        .unwrap();
    wait_until("the raw reader to run", || {
        session.status() == SessionStatus::Working
    });
    never_flagged(session, "a raw-mode reader");
    session.write_input(b"q").unwrap();
    wait_until("the raw reader to finish", || {
        session.status() == SessionStatus::Idle
    });

    let _ = session.terminate(Duration::from_secs(2));
}

#[test]
fn a_question_in_a_directly_owned_terminal_needs_the_user() {
    let root = root("direct");
    let mut session = shell("line_direct", &root, None);
    exercise(&mut session);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_question_in_a_held_terminal_needs_the_user() {
    let root = root("held");
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_ubra-holder")),
    };
    let mut session = shell("line_held", &root, Some(holder));
    exercise(&mut session);
    let _ = std::fs::remove_dir_all(&root);
}
