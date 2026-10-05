//! Opt-in: the real Pi coding agent driven through a private Engine.
//!
//! Nothing here needs a model account. Pi talks to
//! `fixtures/fake_pi_api.py` (spawned on a free 127.0.0.1 port and
//! registered as a custom provider in the temp HOME's `~/.pi/agent/models.json`),
//! which scripts slow streams and a `bash` tool call from keywords in the
//! prompt. HOME, the project and the Engine socket all live in a temp dir
//! removed on drop; the developer's `~/.pi` is never read or written.
//!
//! `UBRA_PI_BIN_DIR` must hold a `pi` executable, and `node` and `python3`
//! must be on PATH:
//!
//! ```sh
//! npm install --prefix /tmp/pi @earendil-works/pi-coding-agent
//! UBRA_PI_BIN_DIR=/tmp/pi/node_modules/.bin \
//!   cargo test -p ubra-engine --test pi_real -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The tests set process-wide environment the Engine hands to its children,
//! so they must run with `--test-threads=1`.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ubra_engine::control::ControlServer;
use ubra_engine::detect::ManifestEngine;
use ubra_engine::registry::Registry;
use ubra_proto::ControlMessage;

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Client {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let message = ControlMessage::Request {
            id: self.next,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).unwrap();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            if let ControlMessage::Response { id, result, .. } =
                serde_json::from_str::<ControlMessage>(&line).unwrap()
                && id == self.next
            {
                return result.map_err(|error| format!("{error:?}"));
            }
        }
    }

    fn status(&mut self, id: &str) -> String {
        let list = self.call("session.list", json!({})).unwrap();
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == id)
            .map(|record| record["status"].to_string())
            .unwrap_or_default()
    }

    fn screen(&mut self, id: &str) -> String {
        self.call("session.read_screen", json!({ "sessionID": id }))
            .map(|result| result["text"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    fn send(&mut self, id: &str, text: &str) {
        self.call(
            "session.send_text",
            json!({ "sessionID": id, "text": text, "submit": true }),
        )
        .expect("send_text");
    }

    /// Samples status every 100 ms until `done` holds or `within` passes,
    /// printing each transition with its screen. Returns every status seen.
    fn watch(
        &mut self,
        id: &str,
        label: &str,
        within: Duration,
        mut done: impl FnMut(&str, &str) -> bool,
    ) -> Vec<String> {
        let start = Instant::now();
        let mut seen: Vec<String> = Vec::new();
        loop {
            let status = self.status(id);
            let screen = self.screen(id);
            if seen.last() != Some(&status) {
                println!(
                    "[{label} +{}ms] {status}\n{}",
                    start.elapsed().as_millis(),
                    indent(&screen)
                );
                seen.push(status.clone());
            }
            if done(&status, &screen) || start.elapsed() > within {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn indent(screen: &str) -> String {
    screen
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| format!("    | {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

struct Fixture {
    temp: tempfile::TempDir,
    project: PathBuf,
    api: Option<Child>,
}

impl Fixture {
    fn requests(&self) -> String {
        std::fs::read_to_string(self.temp.path().join("api.log")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(api) = &mut self.api {
            let _ = api.kill();
            let _ = api.wait();
        }
    }
}

/// A private HOME and project, with the Engine's inherited environment
/// pointed at them and at the Pi under test. With `fake_api`, the HOME's Pi
/// config selects a custom provider served by the fake API; without it, Pi
/// starts as on first run: no settings and no model. `PI_OFFLINE` stops
/// catalog refreshes and update checks either way. None when not opted in.
fn fixture(fake_api: bool) -> Option<Fixture> {
    let Some(bin) = std::env::var_os("UBRA_PI_BIN_DIR") else {
        eprintln!("UBRA_PI_BIN_DIR unset; skipping");
        return None;
    };
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let api = fake_api.then(|| {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_pi_api.py"))
            .arg(port.to_string())
            .arg(temp.path().join("api.log"))
            .spawn()
            .expect("python3 for the fake OpenAI API");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "fake OpenAI API never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        (child, port)
    });
    if let Some((_, port)) = &api {
        let models = json!({ "providers": { "fake": {
            "baseUrl": format!("http://127.0.0.1:{port}/v1"),
            "api": "openai-completions",
            "apiKey": "fake-key-for-ubra-e2e",
            "compat": { "supportsDeveloperRole": false, "supportsReasoningEffort": false },
            "models": [{ "id": "fake-model", "reasoning": false }],
        }}});
        std::fs::write(home.join(".pi/agent/models.json"), models.to_string()).unwrap();
        let settings = json!({
            "defaultProvider": "fake",
            "defaultModel": "fake-model",
            "enableInstallTelemetry": false,
        });
        std::fs::write(home.join(".pi/agent/settings.json"), settings.to_string()).unwrap();
    }
    let path = format!(
        "{}:{}",
        Path::new(&bin).display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // SAFETY: --test-threads=1, and set before the Engine spawns anything.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        std::env::set_var("PI_OFFLINE", "1");
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "PI_CODING_AGENT_DIR",
            "PI_CODING_AGENT_SESSION_DIR",
        ] {
            std::env::remove_var(name);
        }
    }
    Some(Fixture {
        temp,
        project,
        api: api.map(|(child, _)| child),
    })
}

fn start(temp: &Path) -> Client {
    let dir = ubra_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .unwrap();
    let (engine, _) = ManifestEngine::load_dir(&dir).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_logs_dir(temp.join("logs")),
    );
    let listener = server.bind().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let stream = UnixStream::connect(temp.join("daemon.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .unwrap();
    Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream),
        next: 0,
    }
}

fn spawn(
    client: &mut Client,
    project: &Path,
    prompt: Option<&str>,
) -> (String, Result<(), String>) {
    let mut params = json!({
        "kind": { "pi": {} },
        "cwd": project,
        "initialCols": 100,
        "initialRows": 30,
    });
    if let Some(prompt) = prompt {
        params["initialPrompt"] = json!(prompt);
    }
    match client.call("session.spawn", params) {
        Ok(record) => (record["id"].as_str().unwrap().to_string(), Ok(())),
        Err(error) => {
            let list = client.call("session.list", json!({})).unwrap();
            let id = list["sessions"][0]["id"].as_str().unwrap().to_string();
            (id, Err(error))
        }
    }
}

/// Initial prompt, a streamed turn, a follow-up that runs a tool (Pi never
/// asks before running one), then kill and resume.
#[test]
#[ignore = "needs UBRA_PI_BIN_DIR and a real Pi"]
fn prompts_stream_run_tools_and_resume() {
    let Some(fixture) = fixture(true) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let mut failures = Vec::new();

    let (id, delivered) = spawn(
        &mut client,
        &fixture.project,
        Some("Reply with the word PINEAPPLE please."),
    );
    if let Err(error) = delivered {
        failures.push(format!("initial prompt: {error}"));
    }
    let seen = client.watch(&id, "initial", Duration::from_secs(15), |status, screen| {
        status.contains("idle") && screen.contains("PINEAPPLE\n")
    });
    if !seen.last().is_some_and(|status| status.contains("idle")) {
        failures.push(format!(
            "the answered initial prompt never read as idle: {seen:?}"
        ));
    }
    if !fixture
        .requests()
        .contains("last_user='Reply with the word PINEAPPLE please.'")
    {
        failures.push(format!(
            "the initial prompt never reached Pi:\n{}",
            fixture.requests()
        ));
    }

    client.send(&id, "SLOW stream something");
    let seen = client.watch(&id, "stream", Duration::from_secs(15), |status, screen| {
        status.contains("idle") && screen.contains("SLOWDONE")
    });
    if !seen.iter().any(|status| status.contains("working")) {
        failures.push(format!("a streamed turn never read as working: {seen:?}"));
    }
    if !seen.last().is_some_and(|status| status.contains("idle")) {
        failures.push(format!("the streamed turn never read as idle: {seen:?}"));
    }
    if !client.screen(&id).contains("SLOWDONE") {
        failures.push("the streamed turn never finished".into());
    }

    client.send(&id, "RUNCMD for me");
    let seen = client.watch(&id, "tool", Duration::from_secs(15), |status, screen| {
        status.contains("idle") && screen.contains("DONECMD")
    });
    if !seen.last().is_some_and(|status| status.contains("idle")) {
        failures.push(format!("the tool turn never read as idle: {seen:?}"));
    }
    if !fixture.project.join("ubra-e2e-file").exists() {
        failures.push("the bash tool call never ran".into());
    }
    let requests = fixture.requests();
    if !requests.contains("last_user='RUNCMD for me'")
        || !requests.contains("last_user='SLOW stream something'")
    {
        failures.push(format!(
            "the follow-ups did not arrive on their own:\n{requests}"
        ));
    }

    let _ = client.call("session.kill", json!({ "sessionID": id }));
    client.watch(&id, "killed", Duration::from_secs(5), |status, _| {
        status.contains("exited")
    });
    if let Err(error) = client.call("session.resume", json!({ "sessionID": id })) {
        failures.push(format!("resume: {error}"));
    }
    let seen = client.watch(&id, "resumed", Duration::from_secs(15), |status, screen| {
        status.contains("idle") && screen.contains("DONECMD")
    });
    if !client.screen(&id).contains("DONECMD") {
        failures.push("the resumed tab does not show the earlier conversation".into());
    }
    if !seen.last().is_some_and(|status| status.contains("idle")) {
        failures.push(format!("the resumed tab never read as idle: {seen:?}"));
    }
    // The resumed conversation carries the earlier turns to the model.
    client.send(&id, "Say MANGO now");
    client.watch(
        &id,
        "after-resume",
        Duration::from_secs(10),
        |status, screen| status.contains("idle") && screen.contains("MANGO\n"),
    );
    let requests = fixture.requests();
    let last = requests.lines().rev().find(|line| line.contains("MANGO"));
    if !last.is_some_and(|line| line.contains("PINEAPPLE") && line.contains("RUNCMD")) {
        failures.push(format!(
            "the resumed conversation lost its history:\n{requests}"
        ));
    }
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    assert!(
        failures.is_empty(),
        "failures:\n  {}",
        failures.join("\n  ")
    );
}

/// First run with no model configured: Pi cannot answer anything, so the
/// tab must not sit on Working as if it were.
#[test]
#[ignore = "needs UBRA_PI_BIN_DIR and a real Pi"]
fn first_run_without_a_model_is_not_working() {
    let Some(fixture) = fixture(false) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let (id, _) = spawn(&mut client, &fixture.project, None);
    let seen = client.watch(&id, "no-model", Duration::from_secs(10), |_, _| false);
    let status = client.status(&id);
    println!("[no-model final] {status}\n{}", indent(&client.screen(&id)));
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    assert!(
        !status.contains("working") && !status.contains("starting"),
        "no model, yet the tab reads as {status}: {seen:?}"
    );
}

/// A folder with project-local Pi resources makes Pi ask whether to trust
/// it before loading them. That is a question for the user.
#[test]
#[ignore = "needs UBRA_PI_BIN_DIR and a real Pi"]
fn the_project_trust_prompt_needs_input() {
    let Some(fixture) = fixture(true) else {
        return;
    };
    std::fs::create_dir_all(fixture.project.join(".pi")).unwrap();
    std::fs::write(fixture.project.join(".pi/settings.json"), "{}").unwrap();
    let mut client = start(fixture.temp.path());
    let (id, _) = spawn(&mut client, &fixture.project, None);
    let seen = client.watch(&id, "trust", Duration::from_secs(15), |status, _| {
        status.contains("needsInput") || status.contains("idle")
    });
    let screen = client.screen(&id);
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    if !screen.contains("Trust project folder?") {
        // Project trust arrived in Pi 0.9x; older builds load `.pi` freely.
        eprintln!("this Pi never asked about project trust; skipping");
        return;
    }
    assert!(
        seen.last()
            .is_some_and(|status| status.contains("needsInput")),
        "the trust prompt never read as needing input: {seen:?}"
    );
}

/// An initial prompt into a folder Pi wants trusted: the trust prompt is
/// answered and the prompt still reaches the model.
#[test]
#[ignore = "needs UBRA_PI_BIN_DIR and a real Pi"]
fn an_initial_prompt_survives_the_trust_prompt() {
    let Some(fixture) = fixture(true) else {
        return;
    };
    std::fs::create_dir_all(fixture.project.join(".pi")).unwrap();
    std::fs::write(fixture.project.join(".pi/settings.json"), "{}").unwrap();
    let mut client = start(fixture.temp.path());
    let (id, delivered) = spawn(
        &mut client,
        &fixture.project,
        Some("Reply with the word PINEAPPLE please."),
    );
    client.watch(
        &id,
        "untrusted",
        Duration::from_secs(15),
        |status, screen| status.contains("idle") && screen.contains("PINEAPPLE\n"),
    );
    let requests = fixture.requests();
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    assert!(delivered.is_ok(), "initial prompt: {delivered:?}");
    assert!(
        requests.contains("PINEAPPLE"),
        "the prompt never reached Pi:\n{requests}"
    );
}
