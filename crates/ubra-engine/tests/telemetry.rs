//! The Engine and its Holders leave a timeline that explains a session's
//! life: a launch that cannot start is an incident, an agent that dies right
//! after launch is an incident, and the Holder records the child it ran.
//!
//! One test per binary: the recorder is process-global.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use ubra_engine::session::{HolderConfig, Session, SessionSpec};
use ubra_engine::{Authority, ManifestEngine, PtySpec};

fn engine() -> Arc<ManifestEngine> {
    let dir = ubra_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn spec(id: &str, root: &Path, executable: PathBuf, script: &str) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .size(80, 24),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(HolderConfig {
            holders_dir: root.join("holders"),
            executable,
        }),
        remote: None,
        defer_launch: false,
    }
}

fn records(state: &Path) -> Vec<Value> {
    let dir = ubra_telemetry::spool::spool_dir(state);
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "open" || extension == "jsonl")
        {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            records.extend(
                text.lines()
                    .filter_map(|line| serde_json::from_str(line).ok()),
            );
        }
    }
    records
}

fn find<'a>(records: &'a [Value], kind: &str, session: &str) -> Option<&'a Value> {
    records
        .iter()
        .find(|record| record["k"] == kind && record["f"]["session"] == session)
}

fn wait_for(state: &Path, what: &str, mut found: impl FnMut(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        ubra_telemetry::flush(Duration::from_secs(1));
        let records = records(state);
        if found(&records) {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            records
                .iter()
                .map(|record| format!("{} {}", record["k"], record["f"]))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn launches_exits_and_holder_facts_reach_the_spool() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    assert!(ubra_telemetry::init(
        ubra_telemetry::Process::Engine,
        &state
    ));
    ubra_engine::telemetry::set_holder_state_dir(&state);
    // SAFETY: set before any Holder manager is launched by this binary; the
    // manager inherits it and retires promptly once its session is gone.
    unsafe { std::env::set_var("UBRA_HOLDER_IDLE_SECONDS", "1") };

    // A Holder executable that does not exist: the launch itself fails.
    let missing = root.path().join("no-such-holder");
    let failed = Session::spawn(
        spec("s_tel_missing", root.path(), missing, "true"),
        engine(),
    );
    assert!(failed.is_err());
    let records = wait_for(&state, "session.launch_failed", |records| {
        find(records, "session.launch_failed", "s_tel_missing").is_some()
    });
    let incident = find(&records, "session.launch_failed", "s_tel_missing").unwrap();
    assert_eq!(incident["s"], "incident");
    assert_eq!(incident["f"]["transport"], "held");
    assert_eq!(incident["f"]["agent"], "shell");

    // A real Holder whose child dies at once with status 3: the Engine records
    // the launch, the exit and an early-exit incident, and the Holder process
    // itself records the child it spawned and how it ended.
    let holder = PathBuf::from(env!("CARGO_BIN_EXE_ubra-holder"));
    let session = Session::spawn(
        spec("s_tel_early", root.path(), holder.clone(), "exit 3"),
        engine(),
    )
    .expect("spawn");
    let records = wait_for(&state, "early exit and holder facts", |records| {
        find(records, "session.early_exit", "s_tel_early").is_some()
            && find(records, "holder.exit", "s_tel_early").is_some()
    });
    drop(session);

    let launch = find(&records, "session.launch", "s_tel_early").expect("launch");
    assert_eq!(launch["p"], "engine");
    let exit = find(&records, "session.exit", "s_tel_early").expect("exit");
    assert_eq!(exit["f"]["code"], 3);
    assert_eq!(exit["f"]["requested"], false);
    let early = find(&records, "session.early_exit", "s_tel_early").unwrap();
    assert_eq!(early["s"], "incident");
    assert_eq!(early["f"]["kind"], "exit");
    let spawned = find(&records, "holder.spawn", "s_tel_early").expect("holder spawn");
    assert_eq!(spawned["p"], "holder");
    let held_exit = find(&records, "holder.exit", "s_tel_early").unwrap();
    assert_eq!(held_exit["f"]["code"], 3);
    assert!(
        ubra_telemetry::spool::spool_dir(&state)
            .join(ubra_telemetry::spool::URGENT_MARKER)
            .exists(),
        "an incident asks the uploader to send soon"
    );

    // A `returnToLoginShell` agent that fails at startup: the PTY lives on as
    // the login shell, so only the wrapper's report says how the agent ended,
    // and the early-exit probe carries that status instead of guessing.
    let wrapped = ubra_engine::agent::AgentDescriptor {
        binary: Some("/bin/sh".into()),
        return_to_login_shell: true,
        ..Default::default()
    };
    let pty = wrapped
        .spawn_spec(
            Path::new("/tmp"),
            [
                ("SHELL".to_string(), "/bin/sh".to_string()),
                (
                    "HOME".to_string(),
                    root.path().to_string_lossy().into_owned(),
                ),
            ],
            &["-c".into(), "exit 7".into()],
        )
        .expect("wrapped spec")
        .size(80, 24);
    let mut wrapped_spec = spec("s_tel_wrapped", root.path(), holder, "");
    wrapped_spec.pty = pty;
    // The early-exit probe watches deferred launches of wrapped manifests.
    wrapped_spec.manifest_id = "codex".into();
    wrapped_spec.defer_launch = true;
    let mut session = Session::spawn(wrapped_spec, engine()).expect("spawn wrapped");
    let records = wait_for(&state, "wrapped agent exit", |records| {
        find(records, "session.agent_exited", "s_tel_wrapped").is_some()
            && find(records, "session.early_exit", "s_tel_wrapped").is_some()
    });
    let _ = session.terminate(Duration::from_secs(1));
    drop(session);

    let exited = find(&records, "session.agent_exited", "s_tel_wrapped").unwrap();
    assert_eq!(exited["s"], "warn");
    assert_eq!(exited["f"]["source"], "wrapper");
    assert_eq!(exited["f"]["code"], 7);
    assert_eq!(exited["f"]["runtime_s"], 0);
    let early = find(&records, "session.early_exit", "s_tel_wrapped").unwrap();
    assert_eq!(early["f"]["kind"], "returned_to_shell");
    assert_eq!(early["f"]["code"], 7);
}
