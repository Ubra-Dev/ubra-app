use super::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;

/// A stand-in agent CLI: shows the argv it got, then stays up like a TUI.
fn forking_agent(temp: &Path) -> PathBuf {
    let script = temp.join("fake-agent");
    std::fs::write(&script, "#!/bin/sh\necho \"FORKED $*\"\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn manifest(binary: &Path, home: &Path) -> crate::detect::Manifest {
    serde_json::from_value(json!({
        "schemaVersion": 2,
        "id": "probe",
        "version": "test",
        "statusModel": "full",
        "agent": {
            "binary": binary.to_string_lossy(),
            // A known shell and home: the spawn must not source the
            // developer's own login configuration.
            "env": { "SHELL": "/bin/sh", "HOME": home.to_string_lossy(), "ENV": "" },
            "conversation": {
                "fork": {"exactArgs": ["fork", "{id}"], "latestArgs": ["fork", "--last"]}
            },
        },
        "rules": [],
    }))
    .expect("manifest")
}

#[test]
fn fork_keeps_the_sources_own_kind_and_conversation_beside_a_foreground_agent() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let agent = forking_agent(temp.path());
    let engine = Arc::new(crate::detect::ManifestEngine::new(vec![manifest(
        &agent, &home,
    )]));
    let registry = Arc::new(Mutex::new(Registry::new(
        engine,
        temp.path().join("state.json"),
    )));
    let server = Arc::new(ControlServer::new(
        Arc::clone(&registry),
        temp.path().join("daemon.sock"),
    ));

    let mut source = test_record("s_source");
    source.kind = ubra_proto::AgentKind::new("probe");
    source.cwd = temp.path().to_string_lossy().into_owned();
    source.agent_session_id = Some("conv-source".into());
    // Codex started by hand in the source's reclaimed login shell: display
    // borrows it, but the fork must still carry the source's own kind and
    // conversation — never another agent's grammar around this provider id.
    source.foreground_agent = Some(ubra_proto::AgentKind::CODEX);
    registry.lock().unwrap().insert_record(source);

    let forked: ubra_proto::SessionRecord = serde_json::from_value(ok_of(call(
        &server,
        "session.fork",
        Some(json!({"sessionID": "s_source"})),
    )))
    .expect("fork record");
    assert_eq!(forked.kind, ubra_proto::AgentKind::new("probe"));
    assert_eq!(
        forked.parent.as_ref().map(|id| id.0.as_str()),
        Some("s_source")
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = registry
            .lock()
            .unwrap()
            .get(&forked.id.0)
            .map(|session| session.screen_lines().join("\n"))
            .unwrap_or_default();
        if text.contains("fork conv-source") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "screen never showed the fork argv:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
