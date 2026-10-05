//! Opt-in: the real OpenCode TUI driven through a private Engine.
//!
//! Nothing here needs a provider account. OpenCode is configured with a
//! custom `@ai-sdk/openai-compatible` provider pointing at
//! `fixtures/fake_openai_api.py` (spawned on a free 127.0.0.1 port), which
//! scripts slow streams and a bash tool call from keywords in the prompt.
//! The same server is OpenCode's HTTP(S) proxy and logs then refuses every
//! other host, so no request leaves the machine. HOME and every XDG
//! directory, the project and the Engine socket live in a temp dir removed on
//! drop; the developer's `~/.config/opencode` and `~/.local/share/opencode`
//! are never read or written.
//!
//! `UBRA_OPENCODE_BIN_DIR` must hold an `opencode` executable, and `python3`
//! must be on PATH:
//!
//! ```sh
//! npm install --prefix /tmp/oc opencode-ai
//! UBRA_OPENCODE_BIN_DIR=/tmp/oc/node_modules/.bin \
//!   cargo test -p ubra-engine --test opencode_real -- --ignored --nocapture --test-threads=1
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
    api: Child,
}

impl Fixture {
    fn requests(&self) -> String {
        std::fs::read_to_string(self.temp.path().join("api.log")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.api.kill();
        let _ = self.api.wait();
    }
}

/// The fake provider as OpenCode's default model, bash behind a permission
/// prompt, and every update, share and download switched off.
fn config(port: u16) -> String {
    json!({
        "$schema": "https://opencode.ai/config.json",
        "model": "fake/fake-model",
        "small_model": "fake/fake-model",
        "autoupdate": false,
        "share": "disabled",
        "permission": { "bash": "ask" },
        "provider": {
            "fake": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Fake",
                "options": {
                    "baseURL": format!("http://127.0.0.1:{port}/v1"),
                    "apiKey": "fake-key-for-ubra-e2e",
                },
                "models": { "fake-model": { "name": "Fake Model" } },
            }
        }
    })
    .to_string()
}

/// A private HOME, XDG tree and project, with the Engine's inherited
/// environment pointed at them and at the OpenCode under test. `configured`
/// writes the fake provider config; without it OpenCode starts as on a fresh
/// machine. None when not opted in.
fn fixture(configured: bool) -> Option<Fixture> {
    let Some(bin) = std::env::var_os("UBRA_OPENCODE_BIN_DIR") else {
        eprintln!("UBRA_OPENCODE_BIN_DIR unset; skipping");
        return None;
    };
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let xdg = [
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("XDG_STATE_HOME", home.join(".local/state")),
        ("XDG_CACHE_HOME", home.join(".cache")),
    ];
    for (_, dir) in &xdg {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::create_dir_all(&project).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    if configured {
        std::fs::create_dir_all(home.join(".config/opencode")).unwrap();
        std::fs::write(home.join(".config/opencode/opencode.json"), config(port)).unwrap();
    }
    let api = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_openai_api.py"))
        .arg(port.to_string())
        .arg(temp.path().join("api.log"))
        .spawn()
        .expect("python3 for the fake OpenAI-compatible API");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "fake API never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    let path = format!(
        "{}:{}",
        Path::new(&bin).display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let proxy = format!("http://127.0.0.1:{port}");
    // SAFETY: --test-threads=1, and set before the Engine spawns anything.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        for (name, dir) in &xdg {
            std::env::set_var(name, dir);
        }
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            std::env::set_var(name, &proxy);
        }
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
        for name in [
            "OPENCODE_DISABLE_AUTOUPDATE",
            "OPENCODE_DISABLE_MODELS_FETCH",
            "OPENCODE_DISABLE_DEFAULT_PLUGINS",
            "OPENCODE_DISABLE_LSP_DOWNLOAD",
            "OPENCODE_DISABLE_SHARE",
        ] {
            std::env::set_var(name, "1");
        }
        for name in [
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_DIR",
            "OPENCODE_CONFIG_CONTENT",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
        ] {
            std::env::remove_var(name);
        }
    }
    Some(Fixture { temp, project, api })
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
        "kind": { "opencode": {} },
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

/// Records a failure unless the last status a watch saw was idle.
fn settled(failures: &mut Vec<String>, what: &str, seen: &[String]) {
    if !seen.last().is_some_and(|status| status.contains("idle")) {
        failures.push(format!("{what} never settled idle: {seen:?}"));
    }
}

/// Initial prompt, a streamed turn, a follow-up that needs bash permission,
/// approval the way the app sends it, then kill and resume.
#[test]
#[ignore = "needs UBRA_OPENCODE_BIN_DIR and a real OpenCode"]
fn prompts_stream_ask_permission_and_resume() {
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
    let seen = client.watch(&id, "initial", Duration::from_secs(20), |status, screen| {
        status.contains("idle") && screen.contains("PINEAPPLE\n")
    });
    settled(&mut failures, "the initial turn", &seen);
    if !fixture
        .requests()
        .contains("tools=True last_user='Reply with the word PINEAPPLE please.'")
    {
        failures.push(format!(
            "the initial prompt never reached the API:\n{}",
            fixture.requests()
        ));
    }

    client.send(&id, "SLOW stream something");
    let seen = client.watch(&id, "stream", Duration::from_secs(20), |status, screen| {
        status.contains("idle") && screen.contains("SLOWDONE")
    });
    if !seen.iter().any(|status| status.contains("working")) {
        failures.push(format!("a streamed turn never read as working: {seen:?}"));
    }
    settled(&mut failures, "the streamed turn", &seen);
    if !client.screen(&id).contains("SLOWDONE") {
        failures.push("the streamed turn never finished".into());
    }

    client.send(&id, "RUNCMD for me");
    let seen = client.watch(&id, "permission", Duration::from_secs(15), |status, _| {
        status.contains("needsInput")
    });
    if !seen.iter().any(|status| status.contains("permission")) {
        failures.push(format!(
            "the bash permission prompt never read as a permission: {seen:?}"
        ));
    }
    if !fixture
        .requests()
        .contains("tools=True last_user='RUNCMD for me'")
    {
        failures.push(format!(
            "the follow-up did not arrive on its own:\n{}",
            fixture.requests()
        ));
    }
    // The app's Approve for opencode: the manifest's `approve` keystroke.
    client.send(&id, "");
    let seen = client.watch(
        &id,
        "approved",
        Duration::from_secs(15),
        |status, screen| status.contains("idle") && screen.contains("DONECMD"),
    );
    settled(&mut failures, "the approved turn", &seen);
    if !fixture.project.join("ubra-e2e-file").exists() {
        failures.push("approving did not run the bash command".into());
    }

    let _ = client.call("session.kill", json!({ "sessionID": id }));
    client.watch(&id, "killed", Duration::from_secs(5), |status, _| {
        status.contains("exited")
    });
    if let Err(error) = client.call("session.resume", json!({ "sessionID": id })) {
        failures.push(format!("resume: {error}"));
    }
    let seen = client.watch(&id, "resumed", Duration::from_secs(20), |status, screen| {
        status.contains("idle") && screen.contains("DONECMD")
    });
    settled(&mut failures, "the resumed session", &seen);
    if !client.screen(&id).contains("DONECMD") {
        failures.push(format!(
            "the resumed tab does not show the earlier conversation:\n{}",
            indent(&client.screen(&id))
        ));
    }
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    let requests = fixture.requests();
    println!("requests:\n{requests}");
    assert!(
        failures.is_empty(),
        "failures:\n  {}",
        failures.join("\n  ")
    );
}

/// A fresh machine with no provider configured: OpenCode opens on its home
/// screen with a free default model and a hint to run /connect. That is a
/// composer ready for input, so it must settle as idle rather than sit
/// starting or read as working.
#[test]
#[ignore = "needs UBRA_OPENCODE_BIN_DIR and a real OpenCode"]
fn first_run_without_a_provider_settles_idle() {
    let Some(fixture) = fixture(false) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let (id, _) = spawn(&mut client, &fixture.project, None);
    let seen = client.watch(&id, "first-run", Duration::from_secs(20), |status, _| {
        status.contains("idle")
    });
    let screen = client.screen(&id);
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    println!("requests:\n{}", fixture.requests());
    assert!(
        screen.contains("ctrl+p commands"),
        "OpenCode did not reach its home screen; the scenario is stale:\n{}",
        indent(&screen)
    );
    assert!(
        seen.last().is_some_and(|status| status.contains("idle")),
        "the first-run home screen never read as idle: {seen:?}"
    );
}
