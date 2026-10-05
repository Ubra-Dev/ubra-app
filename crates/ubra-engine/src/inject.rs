//! Spawn-time config injection: what turns a bare agent CLI into a
//! Ubra-connected session.
//!
//! Ported from the local half of `InjectionBuilder`. The daemon writes two
//! files at startup — a Claude hooks settings file and a Claude MCP config —
//! whose contents reference `$UBRA_CLI` / the CLI's sibling `ubra-mcp`,
//! then appends per-launch flags (`--settings`, `--mcp-config`, Codex `-c`
//! overrides) for whichever mechanisms the agent's manifest opted into. This
//! is what makes a Claude session hook-driven rather than screen-detected,
//! and what gives every agent the `ubra` MCP tools.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::agent::InjectionSpec;

/// Environment variable names shared with every hook and MCP shim.
pub const SESSION_ID_ENV: &str = "UBRA_SESSION_ID";
pub const SOCKET_ENV: &str = "UBRA_SOCKET";
pub const CLI_ENV: &str = "UBRA_CLI";

/// A random v4 UUID in the lowercase-hex form Claude accepts as
/// `--session-id`. Minting it ourselves is what makes resume possible later
/// without the agent ever reporting an id.
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("random");
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h = |range: std::ops::Range<usize>| {
        bytes[range]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    format!(
        "{}-{}-{}-{}-{}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    )
}

/// The static hooks file injected into every Claude session via `--settings`.
/// Commands read `$UBRA_CLI` / `$UBRA_SOCKET` from the PTY env, so the
/// file content is identical for all sessions and safe to write once.
pub fn write_claude_hooks_file(inject_dir: &Path) -> io::Result<()> {
    const EVENTS: [&str; 11] = [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionRequest",
        "Notification",
        "Stop",
        "SubagentStart",
        "SubagentStop",
        "SessionEnd",
    ];
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        hooks.insert(
            event.to_string(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": format!("\"${CLI_ENV}\" hook {event}"),
                    "timeout": 10,
                }]
            }]),
        );
    }
    write_atomic(
        &inject_dir.join("claude-hooks.json"),
        &serde_json::to_vec(&json!({ "hooks": hooks }))?,
    )
}

/// The Claude `--mcp-config` file: the `ubra` stdio server backed by the
/// CLI's sibling `ubra-mcp` proxy (or the CLI itself as a fallback).
pub fn write_claude_mcp_file(inject_dir: &Path, cli_path: &Path) -> io::Result<()> {
    let (command, args) = mcp_launch(cli_path);
    write_atomic(
        &inject_dir.join("claude-mcp.json"),
        &serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "ubra": { "type": "stdio", "command": command, "args": args }
            }
        }))?,
    )
}

fn mcp_launch(cli_path: &Path) -> (String, Vec<String>) {
    let proxy = cli_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("ubra-mcp");
    if is_executable(&proxy) {
        (proxy.to_string_lossy().into_owned(), Vec::new())
    } else {
        (
            cli_path.to_string_lossy().into_owned(),
            vec!["mcp-stdio".into()],
        )
    }
}

/// Folder of the Claude Code plugin that carries Ubra's skills.
pub const CLAUDE_SKILLS_PLUGIN_DIR: &str = "claude-skills-plugin";

/// Writes `<inject>/claude-skills-plugin/`: a Claude Code plugin named `ubra`
/// whose `skills/<name>/SKILL.md` load as native skills (`ubra:scheduling`,
/// …). Static like the hooks and MCP files, so one copy serves every session.
/// Staged and swapped whole, so an Engine upgrade never leaves a skill from an
/// older build behind.
pub fn write_claude_skills_plugin(inject_dir: &Path) -> io::Result<PathBuf> {
    let plugin_dir = inject_dir.join(CLAUDE_SKILLS_PLUGIN_DIR);
    let staging = inject_dir.join(format!(
        ".{CLAUDE_SKILLS_PLUGIN_DIR}.{}.tmp",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join(".claude-plugin"))?;
    write_atomic(
        &staging.join(".claude-plugin/plugin.json"),
        &serde_json::to_vec_pretty(&json!({
            "name": "ubra",
            "version": "0.1.0",
            "description": "How to use Ubra from inside a Ubra session: scheduling, notes, and coordinating other agents.",
            "author": { "name": "Ubra" },
        }))?,
    )?;
    for skill in ubra_proto::skills::ALL {
        let folder = staging.join("skills").join(skill.name);
        std::fs::create_dir_all(&folder)?;
        write_atomic(&folder.join("SKILL.md"), skill.skill_md().as_bytes())?;
    }
    let backup = inject_dir.join(format!(
        ".{CLAUDE_SKILLS_PLUGIN_DIR}.{}.old",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&backup);
    if plugin_dir.exists() {
        std::fs::rename(&plugin_dir, &backup)?;
    }
    match std::fs::rename(&staging, &plugin_dir) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(plugin_dir)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            if backup.exists() {
                let _ = std::fs::rename(&backup, &plugin_dir);
            }
            Err(error)
        }
    }
}

/// Session identity needed to bake Cursor's per-launch plugin (MCP env + hooks).
pub struct CursorInject<'a> {
    pub session_id: &'a str,
    pub socket_path: &'a Path,
}

/// Per-launch injection arguments for the mechanisms a manifest opted into.
pub fn injection_args(
    injection: &InjectionSpec,
    inject_dir: &Path,
    cli_path: &Path,
) -> Vec<String> {
    injection_args_with_cursor(injection, inject_dir, cli_path, None)
}

/// Like [`injection_args`], and when `cursor` is set also writes/appends the
/// session-local Cursor `--plugin-dir` (MCP + stop hook).
pub fn injection_args_with_cursor(
    injection: &InjectionSpec,
    inject_dir: &Path,
    cli_path: &Path,
    cursor: Option<CursorInject<'_>>,
) -> Vec<String> {
    let mut argv = Vec::new();
    if injection.claude_hooks {
        let hooks = inject_dir.join("claude-hooks.json");
        if hooks.exists() {
            argv.push("--settings".into());
            argv.push(hooks.to_string_lossy().into_owned());
        }
    }
    if injection.claude_mcp {
        let mcp = inject_dir.join("claude-mcp.json");
        if mcp.exists() {
            argv.push("--mcp-config".into());
            argv.push(mcp.to_string_lossy().into_owned());
            // The skills explain those tools, so they travel together.
            let skills = inject_dir.join(CLAUDE_SKILLS_PLUGIN_DIR);
            if skills.join(".claude-plugin/plugin.json").exists() {
                argv.push("--plugin-dir".into());
                argv.push(skills.to_string_lossy().into_owned());
            }
        }
    }
    if injection.codex_notify {
        argv.push("-c".into());
        argv.push(format!(
            "notify=[{}, \"notify\"]",
            toml_string(&cli_path.to_string_lossy())
        ));
    }
    if injection.codex_mcp {
        let (command, args) = mcp_launch(cli_path);
        let encoded_args = args
            .iter()
            .map(|arg| toml_string(arg))
            .collect::<Vec<_>>()
            .join(",");
        argv.push("-c".into());
        argv.push(format!(
            "mcp_servers.ubra.command={}",
            toml_string(&command)
        ));
        argv.push("-c".into());
        argv.push(format!("mcp_servers.ubra.args=[{encoded_args}]"));
    }
    if (injection.cursor_mcp || injection.cursor_hooks)
        && let Some(cursor) = cursor
        && let Ok(plugin_dir) = write_cursor_plugin(
            inject_dir,
            cli_path,
            cursor.session_id,
            cursor.socket_path,
            injection.cursor_mcp,
            injection.cursor_hooks,
        )
    {
        argv.push("--plugin-dir".into());
        argv.push(plugin_dir.to_string_lossy().into_owned());
    }
    argv
}

/// Writes `<inject>/cursor-plugin/<session>/` with plugin manifest, optional
/// `mcp.json`, and optional `hooks/hooks.json`. Returns the plugin directory.
///
/// Always stages into a fresh temp dir and replaces the live plugin tree so a
/// resume that disables MCP or hooks cannot leave the previous files active.
pub fn write_cursor_plugin(
    inject_dir: &Path,
    cli_path: &Path,
    session_id: &str,
    socket_path: &Path,
    mcp: bool,
    hooks: bool,
) -> io::Result<PathBuf> {
    let plugin_root = inject_dir.join("cursor-plugin");
    std::fs::create_dir_all(&plugin_root)?;
    let plugin_dir = plugin_root.join(session_id);
    let staging = plugin_root.join(format!(".{session_id}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join(".cursor-plugin"))?;
    write_atomic(
        &staging.join(".cursor-plugin/plugin.json"),
        &serde_json::to_vec_pretty(&json!({
            "name": "ubra",
            "version": "0.1.0",
            "description": "Ubra agent orchestration for Cursor sessions",
        }))?,
    )?;

    if mcp {
        let (target, target_args) = mcp_launch(cli_path);
        // Cursor splits `command` on spaces before spawning it. Keep the
        // App Support path in argv, where JSON preserves it as one argument.
        let args = std::iter::once(target)
            .chain(target_args)
            .collect::<Vec<_>>();
        write_atomic(
            &staging.join("mcp.json"),
            &serde_json::to_vec_pretty(&json!({
                "mcpServers": {
                    "ubra": {
                        "type": "stdio",
                        "command": "/usr/bin/env",
                        "args": args,
                        "env": {
                            "UBRA_SESSION_ID": session_id,
                            "UBRA_SOCKET": socket_path.to_string_lossy(),
                            "UBRA_CLI": cli_path.to_string_lossy(),
                        }
                    }
                }
            }))?,
        )?;
    }

    if hooks {
        std::fs::create_dir_all(staging.join("hooks"))?;
        // Absolute CLI path: Cursor does not expand $UBRA_CLI in hook cmds.
        let quoted = shell_single_quote(&cli_path.to_string_lossy());
        write_atomic(
            &staging.join("hooks/hooks.json"),
            &serde_json::to_vec_pretty(&json!({
                "version": 1,
                "hooks": {
                    "stop": [{ "command": format!("{quoted} hook Stop") }],
                    "beforeSubmitPrompt": [{
                        "command": format!("{quoted} hook UserPromptSubmit")
                    }],
                }
            }))?,
        )?;
    }

    let backup = plugin_root.join(format!(".{session_id}.{}.old", std::process::id()));
    let _ = std::fs::remove_dir_all(&backup);
    if plugin_dir.exists() {
        std::fs::rename(&plugin_dir, &backup)?;
    }
    match std::fs::rename(&staging, &plugin_dir) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(plugin_dir)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            if backup.exists() {
                let _ = std::fs::rename(&backup, &plugin_dir);
            }
            Err(error)
        }
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Claude Code's project-directory slug for a working directory: `/` and `.`
/// replaced by `-`, verified against real dirs under `~/.claude/projects`.
pub fn claude_project_slug(cwd: &str) -> String {
    cwd.replace(['/', '.'], "-")
}

/// `~/.claude/projects/<slug>/<uuid>.jsonl` — predictable only for Claude,
/// because only Claude lets the caller choose the session UUID *and* derives
/// its jsonl path from the cwd.
pub fn claude_transcript_path(home: &Path, cwd: &str, session_uuid: &str) -> PathBuf {
    home.join(".claude/projects")
        .join(claude_project_slug(cwd))
        .join(format!("{session_uuid}.jsonl"))
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_v4_and_unique() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4', "version nibble: {a}");
        assert!(
            matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant nibble: {a}"
        );
    }

    #[test]
    fn the_hooks_file_matches_the_swift_shape() {
        let temp = tempfile::tempdir().expect("temp");
        write_claude_hooks_file(temp.path()).expect("write");
        let parsed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("claude-hooks.json")).expect("read"),
        )
        .expect("parse");
        let stop = &parsed["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(stop["type"], "command");
        assert_eq!(stop["command"], "\"$UBRA_CLI\" hook Stop");
        assert_eq!(stop["timeout"], 10);
        assert!(parsed["hooks"]["SubagentStop"].is_array());
    }

    #[test]
    fn the_mcp_file_prefers_the_sibling_proxy() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("bin/ubra");
        std::fs::create_dir_all(cli.parent().unwrap()).expect("mkdir");
        std::fs::write(&cli, "#!/bin/sh\n").expect("cli");

        // No proxy: fall back to `ubra mcp-stdio`.
        write_claude_mcp_file(temp.path(), &cli).expect("write");
        let parsed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("claude-mcp.json")).expect("read"),
        )
        .expect("parse");
        assert_eq!(parsed["mcpServers"]["ubra"]["args"][0], "mcp-stdio");

        // With an executable sibling, it becomes the command.
        let proxy = temp.path().join("bin/ubra-mcp");
        std::fs::write(&proxy, "#!/bin/sh\n").expect("proxy");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        write_claude_mcp_file(temp.path(), &cli).expect("write");
        let parsed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("claude-mcp.json")).expect("read"),
        )
        .expect("parse");
        assert_eq!(
            parsed["mcpServers"]["ubra"]["command"],
            proxy.to_string_lossy().as_ref()
        );
        assert_eq!(
            parsed["mcpServers"]["ubra"]["args"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
    }

    /// Writes the real plugin to `UBRA_SKILLS_OUT` so it can be checked with
    /// `claude plugin validate` and loaded with `claude --plugin-dir`.
    #[test]
    #[ignore = "writes the Claude skills plugin to UBRA_SKILLS_OUT"]
    fn write_skills_plugin_for_inspection() {
        let out = std::env::var_os("UBRA_SKILLS_OUT").expect("set UBRA_SKILLS_OUT");
        let dir = write_claude_skills_plugin(Path::new(&out)).expect("write");
        eprintln!("{}", dir.display());
    }

    #[test]
    fn the_skills_plugin_holds_every_ubra_skill_and_replaces_stale_ones() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = write_claude_skills_plugin(temp.path()).expect("write");
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(".claude-plugin/plugin.json")).expect("manifest"),
        )
        .expect("json");
        assert_eq!(manifest["name"], "ubra", "skills load as ubra:<name>");
        for skill in ubra_proto::skills::ALL {
            let md = std::fs::read_to_string(dir.join("skills").join(skill.name).join("SKILL.md"))
                .expect("skill file");
            assert!(md.starts_with(&format!("---\nname: {}\n", skill.name)));
            assert!(md.contains(skill.markdown));
        }
        // A skill an older build wrote must not survive an upgrade.
        std::fs::create_dir_all(dir.join("skills/retired")).expect("stale");
        write_claude_skills_plugin(temp.path()).expect("rewrite");
        assert!(!dir.join("skills/retired").exists());
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .expect("list")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
    }

    #[test]
    fn injection_args_cover_all_four_mechanisms() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("ubra");
        write_claude_hooks_file(temp.path()).expect("hooks");
        write_claude_mcp_file(temp.path(), &cli).expect("mcp");

        let claude = InjectionSpec {
            claude_hooks: true,
            claude_mcp: true,
            ..Default::default()
        };
        let args = injection_args(&claude, temp.path(), &cli);
        assert_eq!(args[0], "--settings");
        assert!(args[1].ends_with("claude-hooks.json"));
        assert_eq!(args[2], "--mcp-config");
        assert!(args[3].ends_with("claude-mcp.json"));
        // Without the skills plugin on disk, no dangling --plugin-dir.
        assert_eq!(args.len(), 4, "{args:?}");
        write_claude_skills_plugin(temp.path()).expect("skills");
        let args = injection_args(&claude, temp.path(), &cli);
        assert_eq!(args[4], "--plugin-dir");
        assert!(args[5].ends_with(CLAUDE_SKILLS_PLUGIN_DIR));

        let codex = InjectionSpec {
            codex_notify: true,
            codex_mcp: true,
            ..Default::default()
        };
        let args = injection_args(&codex, temp.path(), &cli);
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("notify=["), "{args:?}");
        assert!(
            args.iter()
                .any(|arg| arg.starts_with("mcp_servers.ubra.command=")),
            "{args:?}"
        );
    }

    #[test]
    fn cursor_plugin_is_launch_scoped_with_baked_env_and_stop_hook() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("Application Support/ubra");
        std::fs::create_dir_all(cli.parent().unwrap()).expect("mkdir");
        std::fs::write(&cli, b"#!/bin/sh\n").expect("cli");
        let proxy = cli.with_file_name("ubra-mcp");
        std::fs::write(&proxy, b"#!/bin/sh\n").expect("proxy");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&cli).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&cli, perms).unwrap();
            std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let socket = temp.path().join("d.sock");
        let cursor = InjectionSpec {
            cursor_mcp: true,
            cursor_hooks: true,
            ..Default::default()
        };
        let args = injection_args_with_cursor(
            &cursor,
            temp.path(),
            &cli,
            Some(CursorInject {
                session_id: "s_test",
                socket_path: &socket,
            }),
        );
        assert_eq!(args[0], "--plugin-dir");
        assert!(args[1].ends_with("cursor-plugin/s_test"), "{args:?}");

        let plugin = Path::new(&args[1]);
        let mcp: serde_json::Value =
            serde_json::from_slice(&std::fs::read(plugin.join("mcp.json")).expect("mcp.json"))
                .expect("json");
        assert_eq!(mcp["mcpServers"]["ubra"]["command"], "/usr/bin/env");
        assert_eq!(
            mcp["mcpServers"]["ubra"]["args"][0],
            proxy.to_string_lossy().as_ref()
        );
        assert_eq!(
            mcp["mcpServers"]["ubra"]["env"]["UBRA_SESSION_ID"],
            "s_test"
        );
        assert_eq!(
            mcp["mcpServers"]["ubra"]["env"]["UBRA_SOCKET"],
            socket.to_string_lossy().as_ref()
        );

        let hooks: serde_json::Value =
            serde_json::from_slice(&std::fs::read(plugin.join("hooks/hooks.json")).expect("hooks"))
                .expect("json");
        let stop = hooks["hooks"]["stop"][0]["command"].as_str().expect("cmd");
        assert!(stop.contains("hook Stop"), "{stop}");
        assert!(stop.contains("ubra"), "{stop}");
    }

    #[test]
    fn cursor_plugin_rewrite_drops_disabled_components() {
        let temp = tempfile::tempdir().expect("temp");
        let cli = temp.path().join("bin/ubra");
        std::fs::create_dir_all(cli.parent().unwrap()).expect("mkdir");
        std::fs::write(&cli, b"#!/bin/sh\n").expect("cli");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let socket = temp.path().join("d.sock");
        let both = InjectionSpec {
            cursor_mcp: true,
            cursor_hooks: true,
            ..Default::default()
        };
        let args = injection_args_with_cursor(
            &both,
            temp.path(),
            &cli,
            Some(CursorInject {
                session_id: "s_reuse",
                socket_path: &socket,
            }),
        );
        let plugin = Path::new(&args[1]);
        assert!(plugin.join("mcp.json").is_file());
        assert!(plugin.join("hooks/hooks.json").is_file());

        let hooks_only = InjectionSpec {
            cursor_mcp: false,
            cursor_hooks: true,
            ..Default::default()
        };
        let _ = injection_args_with_cursor(
            &hooks_only,
            temp.path(),
            &cli,
            Some(CursorInject {
                session_id: "s_reuse",
                socket_path: &socket,
            }),
        );
        assert!(
            !plugin.join("mcp.json").exists(),
            "disabled mcp.json must not survive a rewrite"
        );
        assert!(plugin.join("hooks/hooks.json").is_file());
    }

    #[test]
    fn the_transcript_slug_matches_claudes_rule() {
        assert_eq!(
            claude_project_slug("/Users/giga/.claude/worktrees/x"),
            "-Users-giga--claude-worktrees-x"
        );
        let path = claude_transcript_path(Path::new("/Users/giga"), "/tmp/repo", "abc-123");
        assert_eq!(
            path,
            Path::new("/Users/giga/.claude/projects/-tmp-repo/abc-123.jsonl")
        );
    }
}
