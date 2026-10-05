//! Resuming a session starts its new child at once, at the size the previous
//! run was using, instead of waiting out the deferred-launch fallback for a
//! size the App will not send (its pane did not change).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ubra_engine::control::ControlServer;
use ubra_engine::session::HolderConfig;
use ubra_engine::{ManifestEngine, Registry};
use ubra_proto::ControlMessage;

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn resume_launches_at_the_previous_size_without_waiting_for_the_app() {
    let temp = tempfile::tempdir().unwrap();
    let (engine, _) =
        ManifestEngine::load_dir(&ubra_engine::detect::bundled_manifest_dir()).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.path().join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
            .with_logs_dir(temp.path().join("logs"))
            .with_holder(HolderConfig {
                holders_dir: temp.path().join("holders"),
                executable: env!("CARGO_BIN_EXE_ubra-holder").into(),
            }),
    );
    let listener = server.bind().unwrap();
    let serving = Arc::clone(&server);
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let server = Arc::clone(&serving);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let control = UnixStream::connect(server.socket_path()).unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut writer = control.try_clone().unwrap();
    let mut replies = BufReader::new(control);
    let mut request = |id, method: &str, params: Value| {
        let message = ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).unwrap();
        let mut line = String::new();
        replies.read_line(&mut line).unwrap();
        match serde_json::from_str::<ControlMessage>(&line).unwrap() {
            ControlMessage::Response {
                result: Ok(value), ..
            } => value,
            other => panic!("{method} failed: {other:?}"),
        }
    };

    // The previous run: sized by the App to its pane, then gone.
    let spawned = request(
        1,
        "session.spawn",
        json!({ "kind": { "shell": {} }, "cwd": "/tmp", "argv": ["/bin/sh", "-c", "read line"] }),
    );
    let id = spawned["id"].as_str().unwrap().to_owned();
    let child = || {
        registry
            .lock()
            .unwrap()
            .get(&id)
            .map_or(0, ubra_engine::session::Session::child_pid)
    };
    wait_until("first child", Duration::from_secs(5), || child() != 0);
    {
        let registry = registry.lock().unwrap();
        let session = registry.get(&id).unwrap();
        session.resize(132, 41).unwrap();
        session.write_input(b"\n").unwrap();
    }
    let first_pid = child();
    wait_until("first run exits", Duration::from_secs(5), || {
        registry
            .lock()
            .unwrap()
            .record(&id)
            .is_some_and(|record| matches!(record.status, ubra_proto::SessionStatus::Exited(_)))
    });

    let started = Instant::now();
    request(2, "session.resume", json!({ "sessionID": id }));
    wait_until("resumed child", Duration::from_secs(5), || {
        let pid = child();
        pid != 0 && pid != first_pid
    });
    let launched_after = started.elapsed();
    assert!(
        launched_after < Duration::from_millis(300),
        "the resumed child waited {launched_after:?}, as if for a size the App never sends"
    );
    assert_eq!(
        registry.lock().unwrap().get(&id).unwrap().screen_size(),
        (132, 41),
        "the resumed child starts at the size its previous run was using"
    );

    let _ = registry
        .lock()
        .unwrap()
        .terminate(&id, Duration::from_secs(2));
}
