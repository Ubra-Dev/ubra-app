#![cfg(unix)]
//! `ubra note` against a real Engine: new notes become note Sessions (so
//! they reach the sidebar), and only a missing Engine falls back to a file.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use ubra_engine::{ControlServer, ManifestEngine, Registry};
use ubra_proto::SessionRecord;

struct Server {
    registry: Arc<Mutex<Registry>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    socket: PathBuf,
}

impl Server {
    fn start(directory: &Path, notes: &Path) -> Self {
        let (engine, _) =
            ManifestEngine::load_dir(&ubra_engine::detect::bundled_manifest_dir()).unwrap();
        let mut registry = Registry::new(Arc::new(engine), directory.join("state.json"));
        registry.load().unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let socket = directory.join("engine.sock");
        let server = Arc::new(ControlServer::new(registry.clone(), &socket).with_notes_dir(notes));
        let listener = server.bind().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let server = server.clone();
                std::thread::spawn(move || {
                    let _ = server.serve(stream);
                });
            }
        });
        Self {
            registry,
            stop,
            thread: Some(thread),
            socket,
        }
    }

    fn notes(&self) -> Vec<SessionRecord> {
        self.registry
            .lock()
            .unwrap()
            .records()
            .into_iter()
            .filter(SessionRecord::is_note)
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn ubra(socket: &Path, notes: &Path, cwd: &Path, caller: Option<&str>, words: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ubra"));
    command
        .args(words)
        .current_dir(cwd)
        .env_clear()
        .env("UBRA_SOCKET", socket)
        .env("UBRA_NOTES_DIR", notes);
    if let Some(caller) = caller {
        command.env("UBRA_SESSION_ID", caller);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .trim()
        .to_owned()
}

struct Setup {
    _temp: tempfile::TempDir,
    notes: PathBuf,
    project: PathBuf,
}

fn setup() -> Setup {
    let temp = tempfile::tempdir().unwrap();
    let notes = temp.path().join("notes");
    let project = temp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    Setup {
        notes,
        project,
        _temp: temp,
    }
}

#[test]
fn a_new_note_becomes_a_note_session_in_the_current_project() {
    let setup = setup();
    let server = Server::start(setup._temp.path(), &setup.notes);
    let id = stdout(&ubra(
        &server.socket,
        &setup.notes,
        &setup.project,
        None,
        &["note", "add", "Launch plan\n\n- [ ] ship it"],
    ));

    let notes = server.notes();
    assert_eq!(notes.len(), 1);
    let record = &notes[0];
    assert_eq!(record.note_id.as_deref(), Some(id.as_str()));
    assert_eq!(record.title, "Launch plan");
    assert_eq!(record.cwd, setup.project.to_string_lossy());
    assert!(record.parent.is_none());
    let file = std::fs::read_to_string(setup.notes.join(format!("{id}.md"))).unwrap();
    assert!(file.contains("- [ ] ship it"), "{file}");
}

#[test]
fn an_agent_writing_a_note_becomes_its_parent_and_lends_its_project() {
    let setup = setup();
    let server = Server::start(setup._temp.path(), &setup.notes);
    let first = stdout(&ubra(
        &server.socket,
        &setup.notes,
        &setup.project,
        None,
        &["note", "add", "PRD"],
    ));
    let parent = server.notes()[0].id.0.clone();

    // Written from somewhere else (a worktree) by the session `parent`.
    let elsewhere = setup._temp.path().canonicalize().unwrap();
    let second = stdout(&ubra(
        &server.socket,
        &setup.notes,
        &elsewhere,
        Some(&parent),
        &["note", "add", "Findings"],
    ));
    assert_ne!(first, second);
    let child = server
        .notes()
        .into_iter()
        .find(|record| record.note_id.as_deref() == Some(second.as_str()))
        .unwrap();
    assert_eq!(
        child.parent.as_ref().map(|p| p.0.as_str()),
        Some(parent.as_str())
    );
    assert_eq!(child.cwd, setup.project.to_string_lossy());
}

#[test]
fn todos_collect_in_one_project_note_session() {
    let setup = setup();
    let server = Server::start(setup._temp.path(), &setup.notes);
    for item in ["one", "two"] {
        stdout(&ubra(
            &server.socket,
            &setup.notes,
            &setup.project,
            None,
            &["note", "todo", item],
        ));
    }
    let notes = server.notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "To-dos");
    let id = notes[0].note_id.clone().unwrap();
    let file = std::fs::read_to_string(setup.notes.join(format!("{id}.md"))).unwrap();
    assert!(file.contains("- [ ] one\n- [ ] two"), "{file}");
}

#[test]
fn inbox_notes_and_a_missing_engine_write_files_only() {
    let setup = setup();
    let server = Server::start(setup._temp.path(), &setup.notes);
    stdout(&ubra(
        &server.socket,
        &setup.notes,
        &setup.project,
        None,
        &["note", "add", "--inbox", "Loose thought"],
    ));
    assert!(server.notes().is_empty());

    let missing = setup._temp.path().join("no-engine.sock");
    let output = ubra(
        &missing,
        &setup.notes,
        &setup.project,
        None,
        &["note", "add", "Offline"],
    );
    let id = stdout(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no sidebar entry yet"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let file = std::fs::read_to_string(setup.notes.join(format!("{id}.md"))).unwrap();
    assert!(
        file.contains(&format!("project: {}", setup.project.display())),
        "{file}"
    );
    assert!(server.notes().is_empty());
}

#[test]
fn create_open_history_and_a_restore_only_the_person_may_run() {
    let setup = setup();
    let server = Server::start(setup._temp.path(), &setup.notes);
    let run = |caller: Option<&str>, words: &[&str]| {
        ubra(&server.socket, &setup.notes, &setup.project, caller, words)
    };
    let id = stdout(&run(
        None,
        &["note", "create", "--open", "Offsite\n\n- [ ] find a venue"],
    ));
    stdout(&run(None, &["note", "append", &id, "Budget: 5k"]));

    let listing = stdout(&run(None, &["note", "history", &id]));
    assert!(listing.contains("Versions of \"Offsite\""), "{listing}");
    assert!(listing.contains("by the command line"), "{listing}");
    let first: String = listing
        .lines()
        .rev()
        .find(|line| line.starts_with("  "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap()
        .to_owned();
    let old = stdout(&run(None, &["note", "history", &id, &first]));
    assert!(!old.contains("Budget"), "{old}");

    // An agent's session may read history but not restore it.
    let agent = server.notes()[0].id.0.clone(); // any non-terminal Session
    let refused = run(Some(&agent), &["note", "restore", &id, &first]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("only the person restores"));

    let restored = stdout(&run(None, &["note", "restore", &id, &first]));
    assert!(restored.starts_with("Restored \"Offsite\""), "{restored}");
    let file = std::fs::read_to_string(setup.notes.join(format!("{id}.md"))).unwrap();
    assert!(!file.contains("Budget"), "{file}");
    let after = stdout(&run(None, &["note", "history", &id]));
    assert!(after.contains("restored the version from"), "{after}");
}

#[test]
fn edit_and_replace_section_change_a_note_in_place() {
    let setup = setup();
    let missing = setup._temp.path().join("no-engine.sock");
    let run = |words: &[&str]| ubra(&missing, &setup.notes, &setup.project, None, words);
    let id = stdout(&run(&[
        "note",
        "add",
        "--inbox",
        "Release tracker\n\n| PR | State |\n| --- | --- |\n| #562 | Fix |\n\n## Status\n\nWaiting.",
    ]));
    let shown = stdout(&run(&["note", "show", &id]));
    let row = shown
        .lines()
        .find(|l| l.contains("#562"))
        .unwrap()
        .to_owned();
    let changed = stdout(&run(&[
        "note",
        "edit",
        &id,
        "--old",
        &row,
        "--new",
        &row.replace("Fix", "Done"),
    ]));
    assert!(changed.contains("#562 | Done"), "{changed}");

    let missing_text = run(&["note", "edit", &id, "--old", "nope", "--new", "x"]);
    assert!(!missing_text.status.success());
    assert!(String::from_utf8_lossy(&missing_text.stderr).contains("not found"));

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ubra"))
        .args(["note", "replace-section", &id, "Status"])
        .current_dir(&setup.project)
        .env_clear()
        .env("UBRA_SOCKET", &missing)
        .env("UBRA_NOTES_DIR", &setup.notes)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Shipped in 0.1.0.")
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
    let after = stdout(&run(&["note", "show", &id]));
    assert!(after.contains("#562 | Done"), "{after}");
    assert!(after.contains("## Status\n\nShipped in 0.1.0."), "{after}");
    assert!(!after.contains("Waiting."), "{after}");
}

#[test]
fn workspace_notes_live_in_their_folder_with_scoped_sessions() {
    let setup = setup();
    let server = Server::start(setup._temp.path(), &setup.notes);
    let id = stdout(&ubra(
        &server.socket,
        &setup.notes,
        &setup.project,
        None,
        &["note", "add", "Scoped plan", "--workspace", "p_ws"],
    ));

    // The Engine session remembers the scope, and the file lands in the
    // workspace folder, not beside the global notes.
    let notes = server.notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].note_id.as_deref(), Some(id.as_str()));
    assert_eq!(
        notes[0].note_workspace.as_ref().map(|id| id.0.as_str()),
        Some("p_ws")
    );
    let file = std::fs::read_to_string(
        setup
            .notes
            .join("workspaces")
            .join("p_ws")
            .join(format!("{id}.md")),
    )
    .unwrap();
    assert!(file.contains("Scoped plan"), "{file}");

    // Each scope lists only its own notes.
    let scoped = stdout(&ubra(
        &server.socket,
        &setup.notes,
        &setup.project,
        None,
        &["note", "list", "--workspace", "p_ws"],
    ));
    assert!(scoped.contains(&id), "{scoped}");
    let global = stdout(&ubra(
        &server.socket,
        &setup.notes,
        &setup.project,
        None,
        &["note", "list"],
    ));
    assert!(!global.contains(&id), "{global}");

    // `path` follows the scope.
    let path = stdout(&ubra(
        &server.socket,
        &setup.notes,
        &setup.project,
        None,
        &["note", "path", "--workspace", "p_ws"],
    ));
    assert!(path.ends_with("workspaces/p_ws"), "{path}");

    // Both scopes at once is refused before anything is touched.
    let output = ubra(
        &server.socket,
        &setup.notes,
        &setup.project,
        None,
        &["note", "list", "--workspace", "p_ws", "--global"],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not both"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
