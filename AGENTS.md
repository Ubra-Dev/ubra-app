```markdown
# Ubra repository instructions

## Scope

These instructions apply to the whole repository. The active architecture decision baseline for remote work is `REMOTE_PORT.md`; read it before changing remote session behavior, SSH handling, PTYs, holders, terminal state, or packaging.

## Completed remote architecture baseline

- The remote refactor is complete. Maintain the bootstrapped Remote PTY Holder as Ubra's only remote session transport; future remote work extends this architecture rather than reopening the transport migration.
- Implement and maintain remote behavior entirely in this Rust workspace.
- Implement and verify every product behavior in the Rust workspace. Historical
  migration documents are context, not an alternate implementation baseline.
- `crates/ubra-engine/manifests` is the canonical Agent catalog. Keep at
  least the established 20 manifests and preserve the package-time count gate:
  a missing manifest does not error, it silently spawns a bare login shell.
- The former Rust SSH + `tmux` transport has been deleted. Never reintroduce it as `legacy_tmux`, a feature flag, a migration path, or a runtime fallback. Missing, corrupt, unsupported, or capability-incompatible Helper artifacts must fail closed with a structured error; an unavailable packaged transport reports `remote_transport_unavailable`.
- When implementation and `REMOTE_PORT.md` disagree, stop and resolve the design mismatch explicitly instead of silently choosing one.

## Rust workspace map

- `crates/ubra-engine`: authoritative local session engine, PTY/holder lifecycle, status reduction, host orchestration, and remote bootstrap/SSH seam.
- `crates/ubra-proto`: shared Rust data models and wire codecs. `remote_pty` is the authoritative versioned Remote Helper protocol; companion access is not part of the current remote transport.
- `crates/ubra-client`: local app-to-engine client. It should not execute SSH directly.
- `crates/ubra-term`: GPUI terminal renderer and client-side terminal interaction.
- `crates/ubra-app`: desktop UI. It requests remote actions through the local Engine.
- `crates/ubra-terminal-state`: shared headless terminal parser/Grid/Snapshot/Diff implementation used by the local Engine and remote Holder.
- `crates/ubra-remote`: minimal remote Helper binary. Keep it independent of GPUI, `ubra-app`, and `ubra-client`.

The Rust toolchain is pinned by `rust-toolchain.toml` to Rust 1.95.0, edition 2024.

## Remote architecture invariants

- SSH is the authenticated encrypted byte transport. Use `ssh -T` for Helper protocol channels; SSH must not own the Agent PTY.
- The current baseline must not require remote `tmux`, `screen`, `zellij`, Node.js, Python, `socat`, `nc`, `curl`, `wget`, or a preinstalled Ubra service.
- Reuse OpenSSH configuration and a finite-lived ControlMaster for performance only. A ControlMaster must never be required for session survival.
- Bootstrap is idempotent: probe platform, select an exact local artifact, upload to a nonce temp path, verify, then atomically rename. Versioned binaries coexist by protocol and Build ID.
- Never overwrite a live Helper version in place. GC must retain every Build ID referenced by a session.
- Never construct Agent launches by concatenating shell strings. Send structured `argv`, `cwd`, and environment over the protocol and exec the argv directly inside the remote PTY child.
- Bootstrap shell commands must be fixed and internally generated. Validate every path component before interpolation and do not follow untrusted symlinks.
- Capture the remote login/cwd environment on the remote host. Do not copy the local process environment wholesale or propagate local secrets and socket paths.
- The remote Holder owns only the PTY, Agent process tree, current terminal grid/modes/cursor, bounded output, exit facts, and controller lease.
- The local Rust Engine owns `SessionRecord`, manifests, status reduction, project/worktree state, GUI events, orchestration, lifecycle policy, and host management.
- Use exactly one independent Holder process and Unix socket per Session; do not add a multi-session Ubra Supervisor to the current baseline.
- A Holder may spawn one minimal liveness guard for its Agent process group. The guard may only wait for Holder pipe closure and kill that one process group; it must not own a PTY, socket, state, or orchestration.
- The app and client must verify the local Engine's explicit Rust identity during `Hello`; fail closed on missing, old, or unknown daemon identities.
- Agent manifests used by the Rust Engine are Rust-owned resources under `ubra-engine`; remote launch must not load another resource bundle or fall back to a different Holder.
- Share terminal parsing through a minimal `ubra-terminal-state` crate; do not make `ubra-remote` depend on the full Engine or create a second parser implementation.
- PTY reads must never block on the attached client. Bound the connection queue; when it falls behind, discard stale diffs and reseed it with a Full Snapshot.
- The completed baseline permits exactly one live attach/controller. A new attach atomically increments the controller epoch and revokes the old attach. Multiple read-only observers are deferred enhancements.
- Full Snapshot contains only the visible grid, cursor, modes, dimensions, and sequence. Bound scrollback to 4 MiB and serve it on demand with `Scroll`.
- Reject incompatible protocol majors, missing required capabilities, wrong session incarnations, oversized frames, and stale controller epochs with structured errors.
- `ubra-remote` must remain a Helper, not a second Ubra Engine. Hooks, MCP forwarding, artifacts, ports, usage, handoff, checkpoints, resource governance, and multiple observers remain separate enhancements unless `REMOTE_PORT.md` is explicitly revised.

## Performance and stability

- Correctness is a release gate: never trade session identity, input delivery, PTY draining, terminal accuracy, or process cleanup for throughput.
- Among correct designs, require measurements and prefer lower latency, CPU, memory, copies, and wakeups. Do not add a supervisor, fan-out, lock, or cache without benchmark evidence.
- Keep one owner/event loop for PTY read, terminal parse, diff construction, and attach write. Reuse buffers and communicate through bounded channels; do not place a cross-task `Arc<Mutex<Terminal>>` on the hot path.
- Coalesce active output for no more than 16 ms. With no attach, continue updating terminal state but do not construct or serialize diffs; do not poll or run heartbeats/GC in an idle Holder.
- Preserve the local Helper/UDS gates from `REMOTE_PORT.md`: snapshot p90 <= 100 ms, input-to-PTY p95 <= 10 ms, output-to-diff p90 <= 50 ms, and loopback interaction median <= 75 ms / p90 <= 150 ms.

## Security and durability

- Remote cache/state directories are owner-only (`0700`); state/log files and Unix sockets are owner-only (`0600`); Helper binaries are owner executable only (`0700`).
- Verify packaged artifact length, SHA-256, Build ID, and protocol before activation.
- Never log credentials, authentication responses, full environments, Agent prompts, or protocol payloads that may contain secrets.
- Keep SSH protocol stdin exclusively for Helper frames. On macOS, route OpenSSH prompts through the packaged Rust `ubra-ssh-askpass`; do not parse passwords or host-key answers in the Engine.
- A failed upload/install may clean up only its own nonce temp file. It must not delete validated binaries or session state.
- Never request elevation or host-wide configuration: no `sudo`, package installation, PAM/sshd changes, system services, persistent user units/LaunchAgents, or `loginctl enable-linger`.
- One local, opt-in exception: the macOS wake helper (`ubra-wake-helper`, `crates/ubra-engine/src/wake.rs`) is a bundled `SMAppService` daemon the user approves in System Settings. It may only schedule and cancel the calling user's ubra wake events and sleep an idle Mac the caller owns the console of. Never widen its requests, never register it without the user switching it on, and never use it on remote hosts.
- Detect persistence rather than assuming `setsid()` survives logout. Surface `native-detach`, `user-supervisor`, and `non-persistent` distinctly. Use only an already-available, no-configuration transient user supervisor; otherwise report non-persistent. Never fall back to `tmux`.

## Build and verification

Run Rust commands from the workspace root (this directory).

Verification is allowed when relevant to the change. Prefer the narrowest
useful checks, then run the broader gates when warranted:
- `cargo test -p <touched-crate>` for focused Rust tests
- `./scripts/check.sh` for formatting, clippy, and tests
- `cargo build --workspace --release` for a full release build

Do not run `cargo clean` or `cargo update` unless explicitly requested; these
can force expensive rebuilds or alter dependency resolution.

Remote tests should be deterministic and not require a developer's real SSH host by default. Prefer fake `ssh` executables, fixture homes, Unix sockets, and spawned test PTYs. Cover at least:

- noisy shell startup and environment-capture timeout/failure;
- supported/unsupported OS and architecture parsing;
- concurrent and interrupted bootstrap;
- corrupt artifact and protocol/build mismatch;
- detach/reconnect with an unchanged process identity and terminal snapshot;
- slow-attach snapshot recovery;
- controller lease revocation;
- normal exit, signal exit, and Holder failure;
- all persistence capability outcomes.

Any test requiring a real remote host must be opt-in and document its environment variables and cleanup behavior.

CI must also build and execute `ubra-remote probe` natively on Linux x86_64/aarch64 and macOS arm64, and run a disposable ordinary-user OpenSSH detach/reconnect soak. These are release gates for the completed transport. macOS x86_64 and Rosetta are not supported Remote Helper targets.

## Change discipline

- Keep protocol and on-disk changes versioned and additive for live Helper sessions. There is no legacy `tmux` compatibility boundary and no transport migration still in progress.
- Update `REMOTE_PORT.md` when a design decision, phase boundary, dependency, platform matrix, or acceptance condition changes.
- Do not describe deferred product enhancements as prerequisites for completion of the remote transport refactor.
- Keep unrelated user changes intact. Do not rewrite historical port documents merely to make them match the new remote direction.
- Add production dependencies only when they materially reduce protocol, terminal, platform, or security risk; explain the tradeoff in the change.

```