//! Activation milestones follow the sessions the Engine starts: the first
//! and second agent session and the first one another agent started are
//! each recorded once, and terminals or notes never count.
//!
//! One test per binary: the recorder is process-global.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ubra_engine::registry::Registry;
use ubra_engine::session::SessionSpec;
use ubra_engine::{Authority, ManifestEngine, PtySpec};
use ubra_telemetry::activation::{Activation, Milestone};

fn record(id: &str, kind: &str, cwd: &Path, parent: Option<&str>) -> ubra_proto::SessionRecord {
    let mut value = json!({"id": id, "kind": ubra_proto::AgentKind::new(kind), "cwd": cwd,
        "projectID": "fixture", "title": id, "titleSource": ubra_proto::TitleSource::Placeholder,
        "status": ubra_proto::SessionStatus::Idle, "resumability": ubra_proto::Resumability::Live,
        "createdAt": 0, "updatedAt": 0, "pinned": false});
    if let Some(parent) = parent {
        value["parent"] = json!(parent);
    }
    serde_json::from_value(value).unwrap()
}

/// What control.rs does after a spawn: the record goes live, then
/// `session.spawn` and the activation milestones are recorded.
fn spawn(registry: &mut Registry, root: &Path, id: &str, kind: &str, parent: Option<&str>) {
    let record = record(id, kind, root, parent);
    let spec = SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sleep".into(), "30".into()], root).size(80, 24),
        manifest_id: kind.into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: None,
        remote: None,
        defer_launch: false,
    };
    registry.spawn(spec, record).expect("spawn");
    let live = registry.record(id).expect("record");
    ubra_engine::telemetry::record_session_spawn(
        &live,
        "fresh",
        Duration::ZERO,
        registry.other_live_agent(id),
    );
}

fn activation_records(state: &Path) -> Vec<Value> {
    ubra_telemetry::flush(Duration::from_secs(2));
    let mut records = Vec::new();
    for entry in std::fs::read_dir(ubra_telemetry::spool::spool_dir(state))
        .into_iter()
        .flatten()
        .flatten()
    {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        records.extend(
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|record| {
                    record["k"]
                        .as_str()
                        .is_some_and(|kind| kind.starts_with("activation."))
                }),
        );
    }
    records
}

#[test]
fn sessions_reach_each_milestone_once() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    assert!(ubra_telemetry::init(
        ubra_telemetry::Process::Engine,
        &state
    ));
    let origin = ubra_telemetry::activation::init_origin(Some(root.path())).expect("origin");
    assert!(!origin.preexisting, "nothing existed before: a new install");

    let (engine, _) =
        ManifestEngine::load_dir(&ubra_engine::detect::bundled_manifest_dir()).expect("manifests");
    let mut registry = Registry::new(Arc::new(engine), state.join("state.json"));

    spawn(&mut registry, root.path(), "s_terminal", "shell", None);
    spawn(&mut registry, root.path(), "s_note", "note", None);
    assert!(
        activation_records(&state).is_empty(),
        "terminals and notes are not agent sessions"
    );

    spawn(&mut registry, root.path(), "s_first", "claude-code", None);
    spawn(
        &mut registry,
        root.path(),
        "s_helper",
        "codex",
        Some("s_first"),
    );
    spawn(&mut registry, root.path(), "s_third", "claude-code", None);
    spawn(
        &mut registry,
        root.path(),
        "s_helper2",
        "codex",
        Some("s_first"),
    );
    for id in [
        "s_terminal",
        "s_note",
        "s_first",
        "s_helper",
        "s_third",
        "s_helper2",
    ] {
        let _ = registry.terminate(id, Duration::ZERO);
    }

    let records = activation_records(&state);
    let kinds: Vec<&str> = records
        .iter()
        .map(|record| record["k"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "activation.first_session",
            "activation.second_session",
            "activation.first_helper"
        ],
        "{records:?}"
    );
    let first = &records[0]["f"];
    assert_eq!(first["agent"], "claude-code");
    assert_eq!(first["helper"], false);
    assert_eq!(first["preexisting"], false);
    assert_eq!(records[0]["p"], "engine");
    let second = &records[1]["f"];
    assert_eq!(second["agent"], "codex");
    assert_eq!(second["helper"], true);
    assert_eq!(
        second["concurrent"], true,
        "the first agent was still live (the terminal does not count)"
    );
    assert_eq!(records[2]["f"]["agent"], "codex");
    for record in &records {
        assert!(record["f"]["since_first_launch_s"].is_u64());
    }

    let markers = Activation::new(&state);
    for milestone in [
        Milestone::FirstSession,
        Milestone::SecondSession,
        Milestone::FirstHelper,
    ] {
        assert!(markers.reached(milestone).is_some(), "{milestone:?}");
    }
    assert_eq!(markers.reached(Milestone::FirstLaunch), None, "app-only");
}
