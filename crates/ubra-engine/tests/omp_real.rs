//! Opt-in: the real omp (Oh My Pi) coding agent driven through a private
//! Engine.
//!
//! Nothing here needs a model account. omp talks to `fixtures/fake_pi_api.py`
//! (spawned on a free 127.0.0.1 port and registered as a custom provider in the
//! temp HOME's `~/.omp/agent/models.yml`, with `modelRoles.default` selecting it
//! in `~/.omp/agent/config.yml`), which scripts a slow streamed reply from
//! keywords in the prompt. HOME, the project and the Engine socket all live in
//! a temp dir removed on drop; the developer's `~/.omp` is never read or
//! written.
//!
//! `UBRA_OMP_BIN_DIR` must hold an `omp` executable, and `node` and `python3`
//! must be on PATH:
//!
//! ```sh
//! curl -fsSL https://omp.sh/install | sh
//! UBRA_OMP_BIN_DIR=~/.local/bin \
//!   cargo test -p ubra-engine --test omp_real -- --ignored --nocapture --test-threads=1
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
/// pointed at them and at the omp under test. With `fake_api`, the HOME's omp
/// config registers a custom provider served by the fake API and selects it as
/// the default role model; without it, omp starts as on first run: no provider
/// and no model. `PI_NO_TITLE` is on either way: it only suppresses the
/// LLM-written session *name*, while the run-state separator this suite reads
/// (`π ⠋`/`π >`, `tui.titleState`) stays on. None when not opted in.
fn fixture(fake_api: bool) -> Option<Fixture> {
    let Some(bin) = std::env::var_os("UBRA_OMP_BIN_DIR") else {
        eprintln!("UBRA_OMP_BIN_DIR unset; skipping");
        return None;
    };
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(home.join(".omp/agent")).unwrap();
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
        // omp's own custom-provider surface: `providers.<name>` in models.yml,
        // with the role model chosen in config.yml. Verified against
        // src/config/models-config-schema-bundle.ts and
        // src/config/model-registry.ts in the installed package.
        std::fs::write(
            home.join(".omp/agent/models.yml"),
            format!(
                "providers:\n  fake:\n    api: openai-completions\n    baseUrl: http://127.0.0.1:{port}/v1\n    apiKey: fake-key-for-ubra-e2e\n    models:\n      - id: fake-model\n        name: Fake Model\n"
            ),
        )
        .unwrap();
    }
    // `setupVersion` is omp's own onboarding marker: below
    // `CURRENT_SETUP_VERSION` (2) it runs the sign-in wizard instead of the
    // session, which no model config can skip. The signing-in test in tests/
    // omp_real.rs caught exactly that.
    let mut config = String::from("setupVersion: 2\n");
    if api.is_some() {
        config.push_str("modelRoles:\n  default: fake/fake-model\n");
    }
    std::fs::write(home.join(".omp/agent/config.yml"), config).unwrap();
    let path = format!(
        "{}:{}",
        Path::new(&bin).display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // SAFETY: --test-threads=1, and set before the Engine spawns anything.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        std::env::set_var("PI_NO_TITLE", "1");
        for name in [
            "PI_CONFIG_DIR",
            "PI_CONFIG_FILES",
            "PI_CODING_AGENT_DIR",
            "PI_CODING_AGENT_SESSION_DIR",
            "OMP_PROFILE",
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
        "kind": { "omp": {} },
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

/// omp's run state lives in its OSC 0 title, so a streamed turn is the whole
/// point: the tab must read as Working while the stream runs and come back to
/// Idle when it ends.
#[test]
#[ignore = "needs UBRA_OMP_BIN_DIR and a real omp"]
fn omp_streams_a_turn_and_reports_working_then_idle() {
    let Some(fixture) = fixture(true) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let mut failures = Vec::new();

    let (id, delivered) = spawn(&mut client, &fixture.project, Some("SLOW stream something"));
    if let Err(error) = delivered {
        failures.push(format!("initial prompt: {error}"));
    }
    let seen = client.watch(&id, "stream", Duration::from_secs(30), |status, screen| {
        status.contains("idle") && screen.contains("SLOWDONE")
    });
    if !seen.iter().any(|status| status.contains("working")) {
        failures.push(format!("the streamed turn never read as working: {seen:?}"));
    }
    if !seen.last().is_some_and(|status| status.contains("idle")) {
        failures.push(format!("the streamed turn never read as idle: {seen:?}"));
    }
    if !client.screen(&id).contains("SLOWDONE") {
        failures.push("the streamed turn never finished".into());
    }
    // Delivery proof is the reply itself: the fake API streams `SLOWDONE` only
    // for a prompt whose text contains SLOW. Its `last_user=` log field cannot
    // be used here — omp prepends a `<system-reminder>` block to the first user
    // message, and the fixture truncates that field at 80 characters.
    if !fixture.requests().contains("POST /v1/chat/completions") {
        failures.push(format!(
            "the prompt never reached the model:\n{}",
            fixture.requests()
        ));
    }
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    assert!(
        failures.is_empty(),
        "failures:\n  {}",
        failures.join("\n  ")
    );
}

/// First run with no provider configured: omp cannot answer anything, so the
/// tab must not sit on Working as if it were.
#[test]
#[ignore = "needs UBRA_OMP_BIN_DIR and a real omp"]
fn omp_without_a_model_is_not_working() {
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
        "no provider, yet the tab reads as {status}: {seen:?}"
    );
}
