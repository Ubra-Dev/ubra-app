//! Opt-in: official xAI Grok Build (tested with 1.0.46), through a private Engine.
//!
//! This is NOT npm's community @vibe-kit/grok-cli. The bundled manifest's
//! official setup URL, login command and --continue grammar target Grok Build.
//! Install https://x.ai/cli/install.sh into a scratch HOME with GROK_BIN_DIR
//! redirected into scratch and that directory already on PATH (prevents the
//! installer creating global symlinks). Never use the real HOME for installation.
//!
//! A temporary ~/.grok/config.toml selects an OpenAI-compatible custom model
//! served by fixtures/fake_grok_api.py on loopback. The same server refuses
//! outbound HTTP(S) proxy requests. HOME, GROK_HOME, all XDG directories, the
//! project and Engine socket are temporary. No real credentials are needed.
//!
//! With an official `grok` in UBRA_GROK_BIN_DIR and python3 on PATH:
//! ```sh
//! UBRA_GROK_BIN_DIR=/tmp/grok/bin CARGO_TARGET_DIR=/tmp/grok/target \
//!   cargo test -p ubra-engine --test grok_real -- --ignored --nocapture --test-threads=1
//! ```
//! Run serially: these opt-in tests set the Engine's process-wide environment.
//! Real OAuth, post-login delivery, remote sessions and two conversations in
//! one directory (--continue selects the latest) are deliberately not covered.

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
                println!("[{label} final] {status}\n{}", indent(&screen));
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

/// The fake provider as Grok's default model, bash behind a permission
/// prompt, updates and prompt suggestions switched off.
fn config(port: u16) -> String {
    format!(
        r#"
[cli]
auto_update = false
[models]
default = "fake"
session_summary = "fake"
[model.fake]
model = "fake-model"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "fake-key-for-ubra-e2e"
api_backend = "chat_completions"
[ui]
prompt_suggestions = false
default_selected_permission = "allow_once"
[permission]
ask = ["Bash(*)"]
"#
    )
}

/// A private HOME, XDG tree and project, with the Engine's inherited
/// environment pointed at them and at the Grok under test. `configured`
/// writes the fake provider config; without it Grok starts as on a fresh
/// machine. None when not opted in.
fn fixture(configured: bool) -> Option<Fixture> {
    let Some(bin) = std::env::var_os("UBRA_GROK_BIN_DIR") else {
        eprintln!("UBRA_GROK_BIN_DIR unset; skipping");
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
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        std::fs::write(home.join(".grok/config.toml"), config(port)).unwrap();
    }
    let api = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_grok_api.py"))
        .arg(port.to_string())
        .arg(temp.path().join("api.log"))
        .spawn()
        .expect("python3 for the fake OpenAI-compatible API");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "fake API never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        Path::new(&bin).join("grok").is_file(),
        "UBRA_GROK_BIN_DIR must contain grok"
    );
    let path = format!(
        "{}:/usr/bin:/bin:/usr/sbin:/sbin",
        Path::new(&bin).display()
    );
    let proxy = format!("http://127.0.0.1:{port}");
    // SAFETY: --test-threads=1, and set before the Engine spawns anything.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        std::env::set_var("SHELL", "/bin/bash");
        for (name, dir) in &xdg {
            std::env::set_var(name, dir);
        }
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            std::env::set_var(name, &proxy);
        }
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
        for (name, _) in std::env::vars_os().collect::<Vec<_>>() {
            if name.to_string_lossy().starts_with("GROK_")
                || ["XAI_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"]
                    .iter()
                    .any(|key| name == *key)
            {
                std::env::remove_var(name);
            }
        }
        std::env::set_var("GROK_HOME", home.join(".grok"));
        std::env::set_var("GROK_DISABLE_AUTOUPDATER", "1");
        std::env::set_var("BROWSER", "/usr/bin/false");
        std::env::set_var("GROK_PROMPT_SUGGESTIONS", "0");
        std::env::set_var(
            "GROK_XAI_API_BASE_URL",
            format!("http://127.0.0.1:{port}/v1"),
        );
        if configured {
            std::env::set_var("XAI_API_KEY", "fake-key-for-ubra-e2e");
        }
    }
    let help = Command::new(Path::new(&bin).join("grok"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("--continue")
            && help.contains("most recent session for the current working directory")
    );
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
        "kind": { "grok": {} },
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
#[ignore = "needs UBRA_GROK_BIN_DIR and a real Grok"]
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
        status.contains("idle") && screen.contains("Worked for")
    });
    settled(&mut failures, "the initial turn", &seen);
    if client.screen(&id).matches("PINEAPPLE").count() < 2 {
        failures.push("the initial reply did not appear beside the user prompt".into());
    }
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
    if fixture.project.join("ubra-e2e-file").exists() {
        failures.push("the command ran before approval".into());
    }
    // The app's Approve for grok: the manifest's `approve` keystroke.
    let (manifests, _) =
        ManifestEngine::load_dir(&ubra_engine::detect::bundled_manifest_dir()).unwrap();
    let approve = &manifests.raw_agent("grok").unwrap()["approve"];
    if approve.is_null() {
        failures.push("Grok has no manifest Approve action".into());
    }
    client
        .call(
            "session.send_text",
            json!({
                "sessionID": id, "text": approve["text"].as_str().unwrap_or(""),
                "submit": approve["submit"].as_bool().unwrap_or(true),
            }),
        )
        .unwrap();
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
    client.send(&id, "ASKQUESTION choose a color");
    let seen = client.watch(
        &id,
        "question",
        Duration::from_secs(10),
        |status, screen| status.contains("needsInput") && screen.contains("Pick a test color"),
    );
    if !seen.last().is_some_and(|s| s.contains("needsInput")) {
        failures.push(format!("question never needed input: {seen:?}"));
    }
    client.send(&id, "");
    let seen = client.watch(
        &id,
        "answered",
        Duration::from_secs(10),
        |status, screen| status.contains("idle") && screen.contains("DONEQUESTION"),
    );
    settled(&mut failures, "answered question", &seen);
    if !client.screen(&id).contains("DONEQUESTION") {
        failures.push("the question answer never reached the API".into());
    }
    let history = fixture
        .requests()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|request| {
            request["agent_request"] == true
                && request["user_messages"].as_array().is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|m| m.as_str().is_some_and(|m| m.contains("ASKQUESTION")))
                })
        })
        .unwrap_or(Value::Null);
    for prompt in ["PINEAPPLE", "SLOW", "RUNCMD"] {
        if !history["user_messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|m| m.as_str().is_some_and(|m| m.contains(prompt)))
        }) {
            failures.push(format!("resume lost {prompt}: {history}"));
        }
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

/// No API key/config: the login menu requires input; never initiate sign-in.
#[test]
#[ignore = "needs UBRA_GROK_BIN_DIR and a real Grok"]
fn first_run_without_a_provider_needs_input() {
    let Some(fixture) = fixture(false) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let (id, _) = spawn(&mut client, &fixture.project, None);
    let seen = client.watch(&id, "first-run", Duration::from_secs(20), |status, _| {
        status.contains("needsInput")
    });
    let screen = client.screen(&id);
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    println!("requests:\n{}", fixture.requests());
    assert!(
        screen.contains("Login with Grok"),
        "Grok did not reach its home screen; the scenario is stale:\n{}",
        indent(&screen)
    );
    assert!(
        seen.last()
            .is_some_and(|status| status.contains("needsInput")),
        "the first-run home screen never needed input: {seen:?}"
    );
}

#[test]
#[ignore = "needs UBRA_GROK_BIN_DIR and a real Grok"]
fn unauthenticated_prompt_is_retained_and_not_reported_delivered() {
    let Some(fixture) = fixture(false) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let (id, delivered) = spawn(
        &mut client,
        &fixture.project,
        Some("PINEAPPLE initial prompt"),
    );
    println!("delivery: {delivered:?}");
    client.watch(&id, "first-run-prompt", Duration::from_secs(3), |_, _| {
        false
    });
    let screen = client.screen(&id);
    client
        .call("session.kill", json!({"sessionID":id}))
        .unwrap();
    assert!(
        delivered
            .unwrap_err()
            .contains("initial_prompt_delivery_failed")
    );
    assert!(screen.contains("Login with Grok"));
    let records = client.call("session.list", json!({})).unwrap();
    assert_eq!(
        records["sessions"][0]["originatingPrompt"],
        "PINEAPPLE initial prompt"
    );
}
