//! `ubra note` — capture into and read Ubra Notes from any shell or agent.
//!
//! A new note is created through the Engine as a note Session, so it appears
//! in the sidebar (under the agent that wrote it). When no Engine is running
//! the note is written as a plain file instead; everything else here edits
//! the files directly, under the store lock, and the open editor picks the
//! change up.

use std::io::{IsTerminal, Read};
use std::path::Path;

use ubra_mcp::bridge::NoteSpawn;
use ubra_notes::doc::Document;
use ubra_notes::handoff::{self, TodoSelector};
use ubra_notes::history::Author;
use ubra_notes::markdown;
use ubra_notes::mention::{self, MentionTarget};
use ubra_notes::store::{self, NoteMeta, NoteStore, Resolve};

use super::CliError;

pub(crate) const HELP: &str = "ubra note TEXT                     write a quick note for this project (the first line is its title)\n  \
ubra note add [TEXT] [--title T] [--project FOLDER | --inbox] [--pin] [--open]\n                                        write a note (Markdown; reads what you pipe in when TEXT is left out).\n                                        It appears in the sidebar under the agent that wrote it; --open shows it\n  \
ubra note append NOTE TEXT          add Markdown to the end of a note (NOTE is its id or part of its title)\n  \
ubra note todo TEXT [--to NOTE] [--project FOLDER | --inbox]   add a to-do (to this project's \"To-dos\" note unless --to)\n  \
ubra note list [--project FOLDER] [--mentions SESSION|me] [--all] [--json]\n  \
ubra note check NOTE TODO [--undo]   tick a to-do (TODO is its text or part of it)\n  \
ubra note link NOTE TODO SESSION    put a session's @-chip on a to-do\n  \
ubra note show NOTE                 print a note as Markdown\n  \
ubra note edit NOTE --old TEXT --new TEXT [--all]   change text in place (exact match; an empty --new deletes)\n  \
ubra note replace-section NOTE HEADING   replace everything under a heading with what you pipe in\n  \
ubra note history NOTE [VERSION]    list a note's earlier versions, or print one\n  \
ubra note restore NOTE VERSION      bring back an earlier version (the current text is kept in history)\n  \
ubra note path                      print the folder where notes are kept\n  \
--workspace ID reads and writes one project workspace's notes (a project id); without it\n  \
(or with --global) every command uses the global notes";

pub(crate) fn run(arguments: &[String]) -> Result<(), CliError> {
    let store = open_store_for(scoped_args(arguments)?.as_deref())?;
    let Some(first) = arguments.first().map(String::as_str) else {
        println!("{HELP}");
        return Ok(());
    };
    let rest = &arguments[1..];
    match first {
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            Ok(())
        }
        "add" | "new" | "create" => add(&store, rest),
        "history" | "versions" => history(&store, rest),
        "restore" => restore(&store, rest),
        "edit" => edit(&store, rest),
        "replace-section" => replace_section(&store, rest),
        "append" => append(&store, rest),
        "todo" => todo(&store, rest),
        "list" | "ls" => list(&store, rest),
        "show" | "cat" => show(&store, rest),
        "check" | "done" => check(&store, rest),
        "link" => link(&store, rest),
        "path" => {
            println!("{}", store.dir().display());
            Ok(())
        }
        _ => add(&store, arguments),
    }
}

fn open_store_for(workspace: Option<&str>) -> Result<NoteStore, CliError> {
    let dir = NoteStore::resolve_dir()
        .ok_or_else(|| CliError::failure("cannot find a home directory for notes"))?;
    if let Some(id) = workspace {
        return NoteStore::open_workspace(&dir, id).map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidInput => {
                CliError::failure(format!("{id:?} is not a workspace id"))
            }
            _ => CliError::failure(format!("cannot open notes: {e}")),
        });
    }
    NoteStore::open(dir).map_err(|e| CliError::failure(format!("cannot open notes: {e}")))
}

/// The scope for this invocation, parsed up front so the right store is
/// open before dispatch: `--workspace` wins, `--global` states the
/// default, and both together is a mistake.
fn scoped_args(arguments: &[String]) -> Result<Option<String>, CliError> {
    let parsed = flags(arguments)?;
    if parsed.workspace.is_some() && parsed.global {
        return Err(CliError::failure("pass --workspace or --global, not both"));
    }
    Ok(parsed.workspace)
}

struct Flags {
    positional: Vec<String>,
    title: Option<String>,
    project: Option<String>,
    to: Option<String>,
    mentions: Option<String>,
    workspace: Option<String>,
    global: bool,
    pin: bool,
    undo: bool,
    inbox: bool,
    open: bool,
    old: Option<String>,
    new: Option<String>,
    all: bool,
    json: bool,
}

fn flags(arguments: &[String]) -> Result<Flags, CliError> {
    let mut out = Flags {
        positional: Vec::new(),
        title: None,
        project: None,
        to: None,
        mentions: None,
        workspace: None,
        global: false,
        pin: false,
        undo: false,
        inbox: false,
        open: false,
        old: None,
        new: None,
        all: false,
        json: false,
    };
    let mut iter = arguments.iter();
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| CliError::failure(format!("{name} needs a value")))
        };
        match arg.as_str() {
            "--title" | "-t" => out.title = Some(value("--title")?),
            "--project" | "-p" => out.project = Some(project_root(&value("--project")?)?),
            "--workspace" => out.workspace = Some(value("--workspace")?),
            "--global" => out.global = true,
            "--to" => out.to = Some(value("--to")?),
            "--mentions" => out.mentions = Some(value("--mentions")?),
            "--undo" => out.undo = true,
            "--inbox" => out.inbox = true,
            "--open" => out.open = true,
            "--old" => out.old = Some(value("--old")?),
            "--new" => out.new = Some(value("--new")?),
            "--pin" => out.pin = true,
            "--all" => out.all = true,
            "--json" => out.json = true,
            _ => out.positional.push(arg.clone()),
        }
    }
    Ok(out)
}

/// A project is named by its root path, as Ubra projects are.
fn project_root(path: &str) -> Result<String, CliError> {
    let path = Path::new(path);
    let absolute = std::fs::canonicalize(path)
        .map_err(|e| CliError::failure(format!("project {}: {e}", path.display())))?;
    Ok(absolute.to_string_lossy().into_owned())
}

fn text_or_stdin(positional: &[String]) -> Result<String, CliError> {
    if !positional.is_empty() {
        return Ok(positional.join(" "));
    }
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(CliError::failure(
            "nothing to write: pass TEXT or pipe it on stdin",
        ));
    }
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .map_err(|e| CliError::failure(format!("reading stdin: {e}")))?;
    Ok(text)
}

fn add(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let text = text_or_stdin(&flags.positional)?;
    let (title, body) = match flags.title.clone() {
        Some(title) => (title, text),
        None => {
            let text = text.trim_start();
            let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
            (
                first.trim().trim_start_matches("# ").to_owned(),
                rest.to_owned(),
            )
        }
    };
    let place = placement(&flags)?;
    let parent = std::env::var(ubra_proto::paths::ENV_SESSION_ID).ok();
    let (id, session) = create_note(
        store,
        &title,
        &body,
        &place,
        parent.as_deref(),
        flags.workspace.as_deref(),
    )?;
    if flags.open {
        match &session {
            Some(session) => {
                super::bridge()
                    .request(
                        ubra_proto::Method::SESSION_REVEAL,
                        serde_json::json!({ "sessionID": session }),
                        std::time::Duration::from_secs(3),
                    )
                    .map_err(|e| {
                        CliError::failure(format!("wrote the note but could not open it: {e}"))
                    })?;
            }
            None => eprintln!("ubra: the note is saved, but Ubra isn't running to show it"),
        }
    }
    if flags.pin {
        store
            .update(&id, &Author::from_env(), |note| {
                note.front.set_flag(ubra_notes::store::KEY_PINNED, true);
                Ok(())
            })
            .map_err(|e| CliError::failure(format!("cannot save note: {e}")))?;
    }
    println!("{id}");
    Ok(())
}

/// Where a new note belongs.
enum Placement {
    /// A project root: the note becomes a sidebar Session in that project.
    Project(String),
    /// `--inbox`: a file with no project and no Session.
    Inbox,
}

/// `--inbox`, else `--project`, else the calling agent's project, else the
/// current directory, the way `ubra session spawn` treats its cwd.
fn placement(flags: &Flags) -> Result<Placement, CliError> {
    if flags.inbox {
        return Ok(Placement::Inbox);
    }
    if let Some(project) = &flags.project {
        return Ok(Placement::Project(project.clone()));
    }
    if let Some(root) = caller_project_root() {
        return Ok(Placement::Project(root));
    }
    let here = std::env::current_dir()
        .map_err(|e| CliError::failure(format!("current directory: {e}")))?;
    project_root(&here.to_string_lossy()).map(Placement::Project)
}

/// An agent's worktree is not its project; its record's project root is.
fn caller_project_root() -> Option<String> {
    let caller = std::env::var(ubra_proto::paths::ENV_SESSION_ID).ok()?;
    let listing = super::bridge()
        .request(
            ubra_proto::Method::SESSION_LIST,
            serde_json::json!({}),
            std::time::Duration::from_secs(3),
        )
        .ok()?;
    let listing: ubra_proto::SessionListResult = serde_json::from_value(listing).ok()?;
    let record = listing.sessions.iter().find(|r| r.id.0 == caller)?;
    listing
        .projects
        .into_iter()
        .find(|project| project.id == record.project_id && project.host.is_none())
        .map(|project| project.root)
}

/// Creates a note through the Engine so it gets a sidebar Session (under
/// `parent`), or as a plain file when no Engine can take it. Returns the
/// note id either way.
fn create_note(
    store: &NoteStore,
    title: &str,
    body: &str,
    place: &Placement,
    parent: Option<&str>,
    workspace: Option<&str>,
) -> Result<(String, Option<String>), CliError> {
    let project = match place {
        Placement::Project(root) => {
            match super::bridge()
                .spawn_note(root, title, body, parent, workspace)
                .map_err(|e| CliError::failure(format!("cannot create note: {e}")))?
            {
                NoteSpawn::Created(record) => {
                    let id = record.note_id.expect("spawn_note checks note_id");
                    return Ok((id, Some(record.id.0)));
                }
                NoteSpawn::Unavailable(reason) => {
                    eprintln!(
                        "ubra: {reason}; saved the note as a file only, it has no sidebar entry yet"
                    );
                    Some(root.as_str())
                }
            }
        }
        Placement::Inbox => None,
    };
    let (_, parsed) = markdown::parse(&format!("\n{body}"));
    let doc = Document::new(title, parsed.blocks);
    store
        .create_for_session(doc, project, None, &Author::from_env())
        .map(|(id, _)| (id, None))
        .map_err(|e| CliError::failure(format!("cannot create note: {e}")))
}

fn append(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let Some((target, text)) = flags.positional.split_first() else {
        return Err(CliError::failure("usage: ubra note append NOTE TEXT"));
    };
    let meta = resolve(store, target)?;
    let text = text_or_stdin(text)?;
    store
        .append(&meta.id, &text, &Author::from_env())
        .map_err(|e| CliError::failure(format!("cannot append: {e}")))?;
    println!("{}", meta.id);
    Ok(())
}

fn todo(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let text = text_or_stdin(&flags.positional)?;
    let items: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| format!("- [ ] {}", line.trim_start_matches("- [ ] ")))
        .collect();
    if items.is_empty() {
        return Err(CliError::failure("nothing to add"));
    }
    let meta = match &flags.to {
        Some(target) => resolve(store, target)?,
        None => inbox_todos(store, &placement(&flags)?, flags.workspace.as_deref())?,
    };
    store
        .append(&meta.id, &items.join("\n"), &Author::from_env())
        .map_err(|e| CliError::failure(format!("cannot append: {e}")))?;
    println!("{}", meta.id);
    Ok(())
}

/// The note loose to-dos collect in: "To-dos" in the project (or Inbox).
/// It belongs to the project, not to whichever agent added the first item.
fn inbox_todos(
    store: &NoteStore,
    place: &Placement,
    workspace: Option<&str>,
) -> Result<NoteMeta, CliError> {
    let project = match place {
        Placement::Project(root) => Some(root.as_str()),
        Placement::Inbox => None,
    };
    let notes = store
        .list()
        .map_err(|e| CliError::failure(format!("cannot list notes: {e}")))?;
    if let Some(found) = notes
        .into_iter()
        .find(|n| !n.archived && n.title == "To-dos" && n.project.as_deref() == project)
    {
        return Ok(found);
    }
    let (id, _) = create_note(store, "To-dos", "", place, None, workspace)?;
    store
        .meta(&id)
        .map_err(|e| CliError::failure(format!("cannot read note: {e}")))
}

/// An exact id, else the single note whose title contains `query`.
fn resolve(store: &NoteStore, query: &str) -> Result<NoteMeta, CliError> {
    let notes = store
        .list()
        .map_err(|e| CliError::failure(format!("cannot list notes: {e}")))?;
    match store::resolve(&notes, query) {
        Resolve::Found(note) => Ok(*note),
        Resolve::NotFound => Err(CliError::not_found(format!("no note matches \"{query}\""))),
        Resolve::Ambiguous(many) => Err(CliError::failure(format!(
            "\"{query}\" matches {} notes: {}",
            many.len(),
            many.iter()
                .map(|n| format!("{} ({})", n.display_title(), n.id))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn list(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    // Direct mentions only; the list_notes MCP tool also follows ancestors.
    let mentioned = match flags.mentions.as_deref() {
        None => None,
        Some("me") => Some(
            std::env::var(ubra_proto::paths::ENV_SESSION_ID).map_err(|_| {
                CliError::failure("--mentions me needs UBRA_SESSION_ID (run inside a Ubra session)")
            })?,
        ),
        Some(id) => Some(id.to_owned()),
    };
    let notes = store
        .list()
        .map_err(|e| CliError::failure(format!("cannot list notes: {e}")))?;
    let notes: Vec<&NoteMeta> = notes
        .iter()
        .filter(|n| flags.all || !n.archived)
        .filter(|n| flags.project.is_none() || n.project == flags.project)
        .filter(|n| {
            mentioned
                .as_ref()
                .is_none_or(|id| n.mentions.contains(&MentionTarget::Session(id.clone())))
        })
        .collect();
    if flags.json {
        let rows: Vec<serde_json::Value> = notes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "title": n.title,
                    "path": n.path,
                    "project": n.project,
                    "pinned": n.pinned,
                    "archived": n.archived,
                    "todos": { "done": n.todos_done, "total": n.todos_total },
                    "open_todos": n.open_todos.iter().map(|(_, t)| t).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return Ok(());
    }
    for n in notes {
        let todos = if n.todos_total > 0 {
            format!("  [{}/{}]", n.todos_done, n.todos_total)
        } else {
            String::new()
        };
        let pin = if n.pinned { "📌 " } else { "" };
        println!("{}  {pin}{}{todos}", n.id, n.display_title());
    }
    Ok(())
}

fn show(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let Some(target) = flags.positional.first() else {
        return Err(CliError::failure("usage: ubra note show NOTE"));
    };
    let meta = resolve(store, target)?;
    let note = store
        .load(&meta.id)
        .map_err(|e| CliError::failure(format!("cannot read note: {e}")))?;
    // The body without front matter reads best in a terminal or a prompt.
    let body = markdown::write(&Default::default(), &note.doc);
    print!("{body}");
    Ok(())
}

fn check(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let [target, todo] = flags.positional.as_slice() else {
        return Err(CliError::failure(
            "usage: ubra note check NOTE TODO [--undo]",
        ));
    };
    let meta = resolve(store, target)?;
    edit_todo(store, &meta.id, todo, |note, index| {
        handoff::set_checked(note, index, !flags.undo);
    })
}

fn link(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let [target, todo, session] = flags.positional.as_slice() else {
        return Err(CliError::failure("usage: ubra note link NOTE TODO SESSION"));
    };
    let meta = resolve(store, target)?;
    let label = session_label(session);
    edit_todo(store, &meta.id, todo, |note, index| {
        handoff::link_session(note, index, &label, session);
    })
}

fn edit_todo(
    store: &NoteStore,
    id: &str,
    todo: &str,
    edit: impl FnOnce(&mut ubra_notes::store::Note, usize),
) -> Result<(), CliError> {
    let selector = match todo.parse::<usize>() {
        Ok(index) => TodoSelector::Index(index),
        Err(_) => TodoSelector::Text(todo.to_owned()),
    };
    let (_, index) = store
        .update(id, &Author::from_env(), |note| {
            let index = handoff::find_todo(note, &selector)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
            edit(note, index);
            Ok(index)
        })
        .map_err(|e| CliError::failure(format!("cannot update note: {e}")))?;
    println!("{id} {index}");
    Ok(())
}

/// "kind: title" from the Engine when it is running, else the bare id.
fn session_label(session: &str) -> String {
    let listing = super::bridge().request(
        ubra_proto::Method::SESSION_LIST,
        serde_json::json!({}),
        std::time::Duration::from_secs(3),
    );
    listing
        .ok()
        .and_then(|listing| serde_json::from_value::<ubra_proto::SessionListResult>(listing).ok())
        .and_then(|listing| {
            listing
                .sessions
                .into_iter()
                .find(|record| record.id.0 == session)
                .map(|record| mention::session_label(record.effective_kind().id(), &record.title))
        })
        .unwrap_or_else(|| mention::session_label("", session))
}

fn history(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let (target, version) = match flags.positional.as_slice() {
        [target] => (target, None),
        [target, version] => (target, Some(parse_version(version)?)),
        _ => {
            return Err(CliError::failure("usage: ubra note history NOTE [VERSION]"));
        }
    };
    let meta = resolve(store, target)?;
    let history = store.history();
    if let Some(version) = version {
        let text = history
            .read(&meta.id, version)
            .map_err(|e| CliError::not_found(format!("{e}")))?;
        print!("{}", ubra_notes::history::body(&text));
        return Ok(());
    }
    let versions = history
        .list(&meta.id)
        .map_err(|e| CliError::failure(format!("cannot read the history: {e}")))?;
    if flags.json {
        let rows: Vec<serde_json::Value> = versions
            .iter()
            .map(|v| {
                serde_json::json!({
                    "version": v.id,
                    "when": ubra_notes::history::describe_time(v.id),
                    "by": v.author.describe(),
                    "size": v.bytes,
                    "summary": v.summary,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return Ok(());
    }
    if versions.is_empty() {
        println!("{} has no earlier versions yet.", meta.display_title());
        return Ok(());
    }
    println!(
        "Versions of \"{}\" (newest first, times in UTC):",
        meta.display_title()
    );
    for v in versions {
        println!(
            "  {}  {}  by {:<16}  {}",
            v.id,
            ubra_notes::history::describe_time(v.id),
            v.author.describe(),
            v.summary
        );
    }
    println!("Print one with: ubra note history NOTE VERSION");
    Ok(())
}

/// Restoring is the person's call, never an agent's: refused inside an
/// agent's session, allowed from a plain terminal.
fn restore(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let [target, version] = flags.positional.as_slice() else {
        return Err(CliError::failure("usage: ubra note restore NOTE VERSION"));
    };
    let version = parse_version(version)?;
    if let Some(agent) = calling_agent() {
        return Err(CliError::failure(format!(
            "only the person restores versions; {agent} is an agent. Ask them to use Version history in the note, or run this in their own terminal."
        )));
    }
    let meta = resolve(store, target)?;
    store
        .restore_version(&meta.id, version, &Author::from_env())
        .map_err(|e| CliError::failure(format!("cannot restore: {e}")))?;
    println!(
        "Restored \"{}\" to the version from {} (UTC). The text it replaced is in its history.",
        meta.display_title(),
        ubra_notes::history::describe_time(version)
    );
    Ok(())
}

fn parse_version(text: &str) -> Result<u64, CliError> {
    text.parse().map_err(|_| {
        CliError::failure(format!(
            "{text} is not a version number (see ubra note history)"
        ))
    })
}

/// The agent Session this command runs in, if any. A terminal tab is the
/// person's own; without the Engine an unknown Session counts as an agent.
fn calling_agent() -> Option<String> {
    let caller = std::env::var(ubra_proto::paths::ENV_SESSION_ID).ok()?;
    let listing = super::bridge()
        .request(
            ubra_proto::Method::SESSION_LIST,
            serde_json::json!({}),
            std::time::Duration::from_secs(3),
        )
        .ok()
        .and_then(|value| serde_json::from_value::<ubra_proto::SessionListResult>(value).ok());
    let is_terminal = listing
        .as_ref()
        .and_then(|listing| listing.sessions.iter().find(|r| r.id.0 == caller))
        .is_some_and(|record| record.kind.id() == ubra_proto::AgentKind::SHELL_ID);
    (!is_terminal).then_some(caller)
}

/// `ubra note edit NOTE --old TEXT --new TEXT [--all]`: the same exact
/// text edit agents make, on the note as `ubra note show` prints it.
fn edit(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let ([target], Some(old), Some(new)) = (flags.positional.as_slice(), &flags.old, &flags.new)
    else {
        return Err(CliError::failure(
            "usage: ubra note edit NOTE --old TEXT --new TEXT [--all]",
        ));
    };
    let meta = resolve(store, target)?;
    let replace_all = flags.all;
    change(store, &meta, |note| {
        ubra_notes::text_edit::edit(note, old, new, replace_all)
    })
}

/// `ubra note replace-section NOTE HEADING` with the new Markdown on stdin.
fn replace_section(store: &NoteStore, arguments: &[String]) -> Result<(), CliError> {
    let flags = flags(arguments)?;
    let [target, heading] = flags.positional.as_slice() else {
        return Err(CliError::failure(
            "usage: ubra note replace-section NOTE HEADING < new.md",
        ));
    };
    let markdown = text_or_stdin(&[])?;
    let meta = resolve(store, target)?;
    change(store, &meta, |note| {
        ubra_notes::text_edit::replace_section(note, heading, &markdown)
    })
}

fn change(
    store: &NoteStore,
    meta: &NoteMeta,
    apply: impl FnOnce(&mut ubra_notes::store::Note) -> Result<ubra_notes::text_edit::Edited, String>,
) -> Result<(), CliError> {
    let (_, edited) = store
        .update(&meta.id, &Author::from_env(), |note| {
            apply(note).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
        })
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidInput => CliError::failure(e.to_string()),
            _ => CliError::failure(format!("cannot change the note: {e}")),
        })?;
    println!("{}", edited.excerpt);
    if edited.tolerant {
        eprintln!("ubra: matched the table row ignoring its spacing");
    }
    Ok(())
}
