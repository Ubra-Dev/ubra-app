//! A Claude spawn with an initial prompt must not sit out the whole
//! workspace-trust watch when the folder is already trusted. Claude runs no
//! hooks before trust, so its first hook ends the watch; before that exit
//! every such spawn RPC took 20s longer than the delivery itself.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use ubra_engine::control::ControlServer;
use ubra_engine::detect::ManifestEngine;
use ubra_engine::registry::Registry;
use ubra_proto::{ControlError, ControlMessage};

struct Control {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Control {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect");
        let reader = BufReader::new(stream.try_clone().expect("clone"));
        Self {
            stream,
            reader,
            next_id: 1,
        }
    }

    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ControlError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        })
        .expect("encode");
        bytes.push(b'\n');
        self.stream.write_all(&bytes).expect("write");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read reply");
        match serde_json::from_str::<ControlMessage>(&line).expect("decode") {
            ControlMessage::Response { result, .. } => result,
            other => panic!("{method} failed: {other:?}"),
        }
    }
}

/// The bundled Claude manifest, minus its binary so the fixture argv runs.
fn claude_engine(temp: &Path) -> Arc<ManifestEngine> {
    let manifests = temp.join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(ubra_engine::detect::bundled_manifest_dir().join("claude-code.json"))
            .unwrap(),
    )
    .unwrap();
    manifest["agent"].as_object_mut().unwrap().remove("binary");
    std::fs::write(
        manifests.join("claude-code.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let (engine, _) = ManifestEngine::load_dir(&manifests).unwrap();
    Arc::new(engine)
}

fn start_server(temp: &Path) -> Arc<ControlServer> {
    let registry = Arc::new(Mutex::new(Registry::new(
        claude_engine(temp),
        temp.join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_logs_dir(temp.join("logs")),
    );
    let listener = server.bind().expect("bind");
    {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let server = Arc::clone(&server);
                std::thread::spawn(move || {
                    let _ = server.serve(stream);
                });
            }
        });
    }
    server
}

#[test]
fn a_hook_ends_the_trust_watch_so_a_trusted_spawn_returns_promptly() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let socket = server.socket_path().to_path_buf();

    // Stand-in for Claude in a trusted folder: no trust picker, just a
    // composer, and a SessionStart hook once it is up.
    let hooks = {
        let socket = socket.clone();
        std::thread::spawn(move || {
            let mut control = Control::connect(&socket);
            let deadline = Instant::now() + Duration::from_secs(10);
            let id = loop {
                let listed = control.request("session.list", json!({})).expect("list");
                if let Some(id) = listed["sessions"][0]["id"].as_str() {
                    break id.to_owned();
                }
                assert!(Instant::now() < deadline, "the session never appeared");
                std::thread::sleep(Duration::from_millis(50));
            };
            std::thread::sleep(Duration::from_millis(500));
            control
                .request(
                    "hook.report",
                    json!({
                        "kind": "claude-hook", "ubraSessionID": id,
                        "event": "SessionStart",
                        "payload": {"hook_event_name": "SessionStart", "session_id": "c-1"},
                    }),
                )
                .expect("hook");
            id
        })
    };

    let mut control = Control::connect(&socket);
    let started = Instant::now();
    let result = control.request(
        "session.spawn",
        json!({
            "kind": { "claude-code": {} }, "cwd": "/tmp",
            "argv": ["/bin/sh", "-c", r#"stty -echo; printf '\033[?2004h> '; exec cat"#],
            "initialPrompt": "acknowledge this prompt",
        }),
    );
    let elapsed = started.elapsed();
    let id = hooks.join().expect("hook thread");
    let _ = control.request("session.kill", json!({ "sessionID": id }));

    assert!(result.is_ok(), "prompt delivery failed: {result:?}");
    assert!(
        elapsed < Duration::from_secs(12),
        "spawn waited out the trust watch after the hook: {elapsed:?}"
    );
}

/// Claude Code 2.1's picker: "No, exit" listed first, unnumbered and focused;
/// Enter there exits 1, the down arrow moves to "Yes". Typing "1" does
/// nothing. Ubra used to answer "1" and Enter, so every first launch in a new
/// folder quit with code 1 a few seconds in.
const CLAUDE_2_1_TRUST_PICKER: &str = r#"stty raw -echo
draw() { printf '\033[2J\033[H Security guide\r\n\r\n %s No, exit\r\n %s Yes, I trust this folder\r\n\r\n Enter to confirm · Esc to cancel\r\n' "$1" "$2"; }
focus=no; draw '❯' ' '
while :; do
  c=$(dd bs=1 count=1 2>/dev/null)
  case "$c" in
    B) focus=yes; draw ' ' '❯' ;;
    A) focus=no; draw '❯' ' ' ;;
    "$(printf '\r')")
      if [ "$focus" = yes ]; then stty sane; printf '\033[2J\033[Htrusted> '; exec cat; fi
      exit 1 ;;
  esac
done"#;

#[test]
fn the_trust_watch_moves_to_yes_before_it_presses_enter() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(server.socket_path());
    let record = control
        .request(
            "session.spawn",
            json!({
                "kind": { "claude-code": {} }, "cwd": "/tmp",
                "argv": ["/bin/sh", "-c", CLAUDE_2_1_TRUST_PICKER],
            }),
        )
        .expect("spawn");
    let id = record["id"].as_str().expect("session id").to_owned();

    let deadline = Instant::now() + Duration::from_secs(10);
    let screen = loop {
        let screen = control
            .request("session.read_screen", json!({ "sessionID": id }))
            .map(|result| result["text"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default();
        if screen.contains("trusted>") || Instant::now() > deadline {
            break screen;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let listed = control.request("session.list", json!({})).expect("list");
    let _ = control.request("session.kill", json!({ "sessionID": id }));

    assert!(
        screen.contains("trusted>"),
        "the picker was not answered with Yes: {screen:?} {listed}"
    );
}

/// Opt-in: the real `claude` on PATH, in a fresh folder, answered by the trust
/// watch. Runs against a throwaway `CLAUDE_CONFIG_DIR` with onboarding marked
/// done and a fake API key, so it needs no sign-in, sends no prompt and never
/// touches `~/.claude`; both temp directories are removed afterwards.
/// `UBRA_REAL_CLAUDE=1 cargo test -p ubra-engine --test workspace_trust -- --ignored --nocapture`
#[test]
#[ignore = "drives the real claude CLI; set UBRA_REAL_CLAUDE=1"]
fn real_claude_reaches_its_composer_in_an_untrusted_folder() {
    if std::env::var_os("UBRA_REAL_CLAUDE").is_none() {
        return;
    }
    let temp = tempfile::tempdir().expect("temp");
    let config = temp.path().join("claude-config");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let key = "sk-ant-ubra-test-0000000000000000";
    std::fs::write(
        config.join(".claude.json"),
        serde_json::to_vec(&json!({
            "hasCompletedOnboarding": true,
            "theme": "dark",
            "customApiKeyResponses": { "approved": [&key[key.len() - 20..]], "rejected": [] },
        }))
        .unwrap(),
    )
    .unwrap();
    let server = start_server(temp.path());
    let mut control = Control::connect(server.socket_path());
    let record = control
        .request(
            "session.spawn",
            json!({
                "kind": { "claude-code": {} }, "cwd": project,
                "argv": [
                    "/usr/bin/env",
                    format!("CLAUDE_CONFIG_DIR={}", config.display()),
                    format!("ANTHROPIC_API_KEY={key}"),
                    "DISABLE_AUTOUPDATER=1",
                    "claude",
                ],
            }),
        )
        .expect("spawn");
    let id = record["id"].as_str().expect("session id").to_owned();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut asked = false;
    let screen = loop {
        let screen = control
            .request("session.read_screen", json!({ "sessionID": id }))
            .map(|result| result["text"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default();
        asked |= screen.contains("trust this folder");
        let composer = screen.contains("? for shortcuts") && !screen.contains("trust this folder");
        if composer || Instant::now() > deadline {
            break screen;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let listed = control.request("session.list", json!({})).expect("list");
    let _ = control.request("session.kill", json!({ "sessionID": id }));
    eprintln!("trust picker seen: {asked}\n{screen}");

    assert!(
        asked,
        "Claude never asked to trust a fresh folder: {screen}"
    );
    assert!(
        screen.contains("? for shortcuts") && !screen.contains("trust this folder"),
        "Claude did not reach its composer: {screen} {listed}"
    );
}

/// Opt-in: a newcomer's whole first run against the real `claude` on PATH.
/// Ubra picks the light text style because the client said its window is
/// light, the test plays the user and sits on the safety notes past the
/// 20-second trust window (signing in takes at least that long), and the
/// trust picker that follows must still be answered. Uses a throwaway
/// `CLAUDE_CONFIG_DIR` with a fake API key, so it needs no sign-in and never
/// touches `~/.claude`.
/// `UBRA_REAL_CLAUDE=1 cargo test -p ubra-engine --test workspace_trust -- --ignored --nocapture`
#[test]
#[ignore = "drives the real claude CLI; set UBRA_REAL_CLAUDE=1"]
fn real_claude_first_run_is_guided_to_its_composer() {
    if std::env::var_os("UBRA_REAL_CLAUDE").is_none() {
        return;
    }
    let temp = tempfile::tempdir().expect("temp");
    let config = temp.path().join("claude-config");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let key = "sk-ant-ubra-test-0000000000000000";
    std::fs::write(
        config.join(".claude.json"),
        serde_json::to_vec(&json!({
            "customApiKeyResponses": { "approved": [&key[key.len() - 20..]], "rejected": [] },
        }))
        .unwrap(),
    )
    .unwrap();
    let server = start_server(temp.path());
    let mut control = Control::connect(server.socket_path());
    let record = control
        .request(
            "session.spawn",
            json!({
                "kind": { "claude-code": {} }, "cwd": project,
                "appearance": "light",
                "argv": [
                    "/usr/bin/env",
                    format!("CLAUDE_CONFIG_DIR={}", config.display()),
                    format!("ANTHROPIC_API_KEY={key}"),
                    "DISABLE_AUTOUPDATER=1",
                    "claude",
                ],
            }),
        )
        .expect("spawn");
    let id = record["id"].as_str().expect("session id").to_owned();
    let read = |control: &mut Control| {
        control
            .request("session.read_screen", json!({ "sessionID": id }))
            .map(|result| result["text"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default()
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    let notes = loop {
        let screen = read(&mut control);
        if screen.contains("Security notes") || Instant::now() > deadline {
            break screen;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        notes.contains("Security notes"),
        "the text-style question was not answered: {notes}"
    );
    std::thread::sleep(Duration::from_secs(22));
    control
        .request(
            "session.send_key",
            json!({ "sessionID": id, "key": { "named": "enter" } }),
        )
        .expect("press Enter on the notes");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut asked = false;
    let screen = loop {
        let screen = read(&mut control);
        asked |= screen.contains("trust this folder");
        let composer = screen.contains("? for shortcuts") && !screen.contains("trust this folder");
        if composer || Instant::now() > deadline {
            break screen;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = control.request("session.kill", json!({ "sessionID": id }));
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config.join("settings.json")).unwrap()).unwrap();
    eprintln!(
        "trust picker seen: {asked}, theme: {}\n{screen}",
        saved["theme"]
    );

    assert_eq!(saved["theme"], "light", "Claude saved another text style");
    assert!(
        asked,
        "Claude never asked to trust a fresh folder: {screen}"
    );
    assert!(
        screen.contains("? for shortcuts") && !screen.contains("trust this folder"),
        "the trust picker after a long first run was left to the user: {screen}"
    );
}
