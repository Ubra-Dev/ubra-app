//! Opt-in: real GitHub Copilot CLI through a private Engine and loopback API.
//!
//! `COPILOT_PROVIDER_BASE_URL` + `COPILOT_PROVIDER_TYPE=openai` select the
//! scripted `fixtures/fake_copilot_api.py`. BYOK needs no GitHub account.
//! Model tests use COPILOT_OFFLINE=true; login tests use the refusing local
//! HTTP(S) proxy. HOME, COPILOT_HOME, GH_CONFIG_DIR and all XDG directories
//! are temporary. No developer credentials or dotfiles are used.
//!
//! Tested with @github/copilot 1.0.90. Install into scratch, then run:
//!
//! ```sh
//! fish -c 'nvm use 24; npm install --prefix /tmp/copilot @github/copilot@1.0.90'
//! UBRA_COPILOT_BIN_DIR=/tmp/copilot/node_modules/.bin \
//!   cargo test -p ubra-engine --test copilot_real -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Requires node and python3 on PATH. Tests mutate process environment, so
//! --test-threads=1 is mandatory. The manifest's --continue resumes the latest
//! conversation; independent same-directory conversation identity is not tested.

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

/// A private HOME, XDG tree and project, with the Engine's inherited
/// environment pointed at them and at the Copilot under test. `configured`
/// selects the fake provider; without it Copilot starts as on a fresh
/// machine. None when not opted in.
fn fixture(configured: bool, trusted: bool) -> Option<Fixture> {
    let Some(bin) = std::env::var_os("UBRA_COPILOT_BIN_DIR") else {
        eprintln!("UBRA_COPILOT_BIN_DIR unset; skipping");
        return None;
    };
    assert!(
        Path::new(&bin).join("copilot").is_file(),
        "UBRA_COPILOT_BIN_DIR must contain copilot"
    );
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
    std::fs::create_dir_all(home.join(".copilot")).unwrap();
    let settings = json!({
        "banner": "never", "showTipsOnStartup": false,
        "trustedFolders": if trusted { vec![project.clone()] } else { vec![] },
        "ide": {"autoConnect": false}, "memory": false,
    });
    std::fs::write(home.join(".copilot/config.json"), settings.to_string()).unwrap();
    let api = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_copilot_api.py"))
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
        // Never inherit credentials, provider commands, or another Copilot home.
        let names: Vec<_> = std::env::vars_os()
            .map(|(k, _)| k)
            .filter(|k| {
                let k = k.to_string_lossy();
                k.starts_with("COPILOT_")
                    || k.starts_with("OTEL_")
                    || k.starts_with("GITHUB_")
                    || k.starts_with("GH_")
                    || matches!(
                        k.as_ref(),
                        "OPENAI_API_KEY" | "ANTHROPIC_API_KEY" | "NODE_OPTIONS"
                    )
            })
            .collect();
        for name in names {
            std::env::remove_var(name);
        }
        std::env::set_var("COPILOT_HOME", home.join(".copilot"));
        std::env::set_var("GH_CONFIG_DIR", home.join(".config/gh"));
        std::env::set_var("COPILOT_AUTO_UPDATE", "false");
        if configured {
            std::env::set_var("COPILOT_OFFLINE", "true");
            std::env::set_var("COPILOT_PROVIDER_BASE_URL", format!("{proxy}/v1"));
            std::env::set_var("COPILOT_PROVIDER_TYPE", "openai");
            std::env::set_var("COPILOT_PROVIDER_API_KEY", "fake-key-for-ubra-e2e");
            std::env::set_var("COPILOT_MODEL", "fake-model");
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
        "kind": { "copilot": {} },
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
#[ignore = "needs UBRA_COPILOT_BIN_DIR and a real Copilot"]
fn prompts_stream_ask_permission_and_resume() {
    let Some(fixture) = fixture(true, true) else {
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
        status.contains("idle") && screen.contains("● PINEAPPLE")
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
    // Use the exact catalog keystroke consumed by the app's Approve action.
    let (catalog, _) =
        ManifestEngine::load_dir(&ubra_engine::detect::bundled_manifest_dir()).unwrap();
    let agent: ubra_proto::AgentDescriptor =
        serde_json::from_value(catalog.raw_agent("copilot").unwrap().clone()).unwrap();
    if let Some(approve) = agent.approve {
        client
            .call(
                "session.send_text",
                json!({"sessionID": id, "text": approve.text, "submit": approve.submit}),
            )
            .unwrap();
    } else {
        failures.push("the manifest has no Approve keystroke".into());
        client.send(&id, "");
    }
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
    let killed = client.watch(&id, "killed", Duration::from_secs(5), |status, _| {
        status.contains("exited")
    });
    if !killed.last().unwrap().contains("exited") {
        failures.push("kill did not exit".into());
    }
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
    client.send(&id, "Reply with RESUMEFRUIT please.");
    let seen = client.watch(
        &id,
        "resume-followup",
        Duration::from_secs(15),
        |status, screen| status.contains("idle") && screen.contains("● RESUMEFRUIT"),
    );
    settled(&mut failures, "the resume follow-up", &seen);
    if !client.screen(&id).contains("● RESUMEFRUIT") {
        failures.push("the resumed follow-up never finished".into());
    }
    let requests = fixture.requests();
    let history = requests
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("HISTORY "))
        .unwrap_or("[]");
    let history: Vec<String> = serde_json::from_str(history).unwrap();
    if history
        != [
            "Reply with the word PINEAPPLE please.",
            "SLOW stream something",
            "RUNCMD for me",
            "Reply with RESUMEFRUIT please.",
        ]
    {
        failures.push(format!(
            "resume did not preserve separate turns: {history:?}"
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

/// With no credentials/provider, the CLI offers /login above a ready composer.
/// Opening /login must then report the account selector as needing input.
#[test]
#[ignore = "needs UBRA_COPILOT_BIN_DIR and a real Copilot"]
fn first_run_without_a_provider_settles_idle() {
    let Some(fixture) = fixture(false, true) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let (id, _) = spawn(&mut client, &fixture.project, None);
    let seen = client.watch(&id, "first-run", Duration::from_secs(20), |status, _| {
        status.contains("idle")
    });
    let screen = client.screen(&id);
    client.send(&id, "/login");
    let login = client.watch(&id, "login", Duration::from_secs(10), |status, _| {
        status.contains("needsInput")
    });
    let login_screen = client.screen(&id);
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    assert!(
        login.last().unwrap().contains("question")
            && login_screen.contains("What account do you want to log into?"),
        "{login:?}\n{login_screen}"
    );
    println!("requests:\n{}", fixture.requests());
    assert!(
        screen.contains("Please use /login to sign in to use Copilot"),
        "Copilot did not reach its home screen; the scenario is stale:\n{}",
        indent(&screen)
    );
    assert!(
        seen.last().is_some_and(|status| status.contains("idle")),
        "the first-run home screen never read as idle: {seen:?}"
    );
}

#[test]
#[ignore = "needs UBRA_COPILOT_BIN_DIR and a real Copilot"]
fn folder_trust_needs_input() {
    let Some(fixture) = fixture(true, false) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let (id, result) = spawn(&mut client, &fixture.project, None);
    result.unwrap();
    let seen = client.watch(&id, "trust", Duration::from_secs(15), |status, screen| {
        status.contains("needsInput") && screen.contains("Do you trust the files")
    });
    let screen = client.screen(&id);
    let _ = client.call("session.kill", json!({"sessionID": id}));
    assert!(screen.contains("Do you trust the files"), "{screen}");
    assert!(seen.last().unwrap().contains("permission"), "{seen:?}");
    assert!(fixture.requests().is_empty());
}

#[test]
#[ignore = "needs UBRA_COPILOT_BIN_DIR and a real Copilot"]
fn initial_prompt_survives_folder_trust() {
    let Some(fixture) = fixture(true, false) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let (id, result) = spawn(
        &mut client,
        &fixture.project,
        Some("Reply with TRUSTFRUIT please."),
    );
    let seen = client.watch(
        &id,
        "trust-prompt",
        Duration::from_secs(15),
        |status, screen| status.contains("idle") && screen.contains("● TRUSTFRUIT"),
    );
    let _ = client.call("session.kill", json!({"sessionID": id}));
    assert!(result.is_ok(), "{result:?}");
    assert!(
        fixture
            .requests()
            .contains("last_user='Reply with TRUSTFRUIT please.'"),
        "{}",
        fixture.requests()
    );
    assert!(seen.last().unwrap().contains("idle"), "{seen:?}");
    // The injector selected one-session trust, not "remember this folder".
    let (id, result) = spawn(&mut client, &fixture.project, None);
    result.unwrap();
    let seen = client.watch(
        &id,
        "trust-not-persisted",
        Duration::from_secs(10),
        |status, screen| status.contains("needsInput") && screen.contains("Do you trust the files"),
    );
    let screen = client.screen(&id);
    let _ = client.call("session.kill", json!({"sessionID": id}));
    assert!(
        seen.last().unwrap().contains("permission") && screen.contains("Do you trust the files"),
        "{seen:?}\n{screen}"
    );
}
