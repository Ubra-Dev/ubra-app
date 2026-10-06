# Contributing

Ubra values reliable sessions, a compact interface, and clear behavior. A good
contribution solves a specific problem and keeps those properties intact.

## Choose a change

New here? Browse the [good first issues](https://github.com/Ubra-Dev/ubra-app/issues?q=is%3Aissue%20is%3Aopen%20label%3A%22good%20first%20issue%22).
Each one includes a starting point, a bounded scope, and verification steps.

- **Bugs:** include reproduction steps, expected behavior, and your environment
  in a [bug report](https://github.com/Ubra-Dev/ubra-app/issues/new?template=bug_report.yml).
- **Fixes and docs:** open a focused PR. An issue is useful context, not a
  prerequisite for a small change.
- **Agent support:** start with the [manifest guide](docs/AGENT-MANIFESTS.md).
  Launch commands and status rules live in JSON under
  [`crates/ubra-engine/manifests/`](crates/ubra-engine/manifests/).
- **Larger changes:** discuss the problem first, especially when adding a new
  trust boundary, persistent format, dependency, or compatibility commitment.
  Use [Discussions](https://github.com/Ubra-Dev/ubra-app/discussions) for early
  ideas and a [feature request](https://github.com/Ubra-Dev/ubra-app/issues/new?template=feature_request.yml)
  for a concrete proposal.

## Set up

Fork and clone the repository. Install Rust through rustup; the workspace's
[`rust-toolchain.toml`](rust-toolchain.toml) selects the compiler.

On macOS, use macOS 15 or newer with the Xcode command-line tools. On Linux,
follow the build and packaging flow in [PACKAGING.md](PACKAGING.md).

```sh
cargo build --workspace
cargo test -p ubra-engine    # choose the package you changed
```

The first build compiles GPUI from a pinned Zed revision. Subsequent builds
are incremental. On macOS, run `./scripts/dev.sh` from the repository root to
try your change in an app bundle. The dev app shares sessions and preferences
with the installed app; see the [README](README.md).

## Find the code

All desktop behavior lives in the Rust workspace under [`crates/`](crates/).
Read [AGENTS.md](AGENTS.md) before changing it.

| Area | Crate under `crates/` |
| :--- | :--- |
| Desktop interface | `ubra-app` |
| Sessions, worktrees, status, and orchestration | `ubra-engine` |
| Wire types and local client | `ubra-proto`, `ubra-client` |
| Terminal rendering and shared parsing | `ubra-term`, `ubra-terminal-state` |
| Remote Helper | `ubra-remote` |
| Automation CLI and MCP server | `ubra-mcp` |

The Engine owns session records; Holders keep the PTYs and agent processes
alive. Read the [remote architecture](REMOTE_PORT.md) before changing
remote sessions, SSH, Holders, terminal state, or packaging.

## Verify

Start with the narrowest relevant package or test. Before handing off Rust
changes, run these from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

`./scripts/check.sh` runs formatting, Clippy, tests, shell syntax guards, and
the dependency-license policy. The release build above is a separate check.

Engine tests create real PTYs, processes, and Git repositories. Tests
against a real SSH host must be opt-in and document setup and cleanup.

For documentation-only changes, check links and rendered output. Explain any
checks you could not run. Never include private prompts, credentials, or raw
session logs in fixtures or screenshots.

### Native Settings and project setup

Settings and the project setup wizard are separate native windows owned by the
invoking workbench. Each workbench keeps one window per surface; repeated
requests focus that window and route the requested Settings page or agent host
into it. Native ownership blocks the workbench until dismissal and closes its
children when the workbench closes. Escape and system close dismiss the
topmost nested Settings layer first. Project setup is optional only when the
unfiltered sidebar contains a project; session-list hydration and remembered
project history alone do not qualify. With no projects, setup has no X and
rejects Escape, close shortcuts, system close, and clicks outside its bounds.
An admitted launch remains non-dismissible until completion.

Settings must paint its content without waiting for macOS Login Items or wake
helper status. Read-only status probes run in the background; only the affected
controls show a checking state and remain unavailable until actual facts arrive.
Pending probes do not rewrite saved preferences. Reopening or a user action
invalidates older results so they cannot overwrite newer state.

Sidebar `+` opens onboarding without a Finder prompt. Its folder control opens
the chooser only on request. Existing local project folders, including symlink
aliases, are rejected; use the sidebar to open them instead. Validation repeats
against current Engine projects before any workspace/session creation. An
explicit folder rejection leaves the draft editable; ordinary admitted-launch
failures do not. Normal session/preset launches into existing projects remain
allowed.

Both child windows have no native titlebar or traffic lights, retain their
platform window titles for accessibility, and use matching circular in-content
close controls where dismissal is allowed. They request the native blurred
material and paint translucent theme surfaces. The main workbench's material
preference is unchanged. For an isolated visual check without attaching real
sessions, launch the development app with
`UBRA_SIDEBAR_PREVIEW=1 UBRA_SIDEBAR_SCENARIO=typical`; open Settings from the
sidebar footer, then onboarding from the sidebar header `+`. It must not open a
folder chooser until its folder control is activated. Check both light and dark
themes, absence of native titlebar chrome, matching close controls, nested
dismissal, and focus
restoration. With an empty sidebar, check that setup offers no close control and
cannot be dismissed. The GPUI tests cover separate-window routing, deduplication,
owner cleanup, and required setup persistence; the ignored live-Engine wizard
test covers launch admission.

Run the native AppKit ownership smoke on a macOS desktop:
`cargo test -p ubra-app --test owned_dialog_appkit -- --ignored`.
Run the disposable-Engine launch scenario with
`cargo test -p ubra-app empty_workbench_launches_the_selected_layout_from_the_ui -- --ignored`.
The duplicate-import admission scenario is
`cargo test -p ubra-app fresh_projects_reject_import_but_existing_project_launch_remains_allowed -- --ignored`.
Both Engine scenarios create temporary projects, PTYs, and an Engine and clean
them up through their fixtures; neither uses a configured remote host.

On macOS, ownership is an AppKit sheet attached to the explicitly requested
workbench, not the globally active window. Linux requires native window-manager
modality: advertised EWMH modal support on X11 or `xdg_wm_dialog_v1` on Wayland.
Unsupported environments report a window-opening error instead of displaying a
modeless fallback or disabling the workbench in app code.

## Open a pull request

Keep one purpose per PR. Describe the problem, the resulting behavior, and
how you verified it. Link an issue when one exists. Include a screenshot or
short recording for interface changes.

If you change session lifecycle or persistence, explain what happens to running
sessions during restart, reconnect, and upgrade. If you change a protocol or
stored format, document compatibility. Update the relevant user guide when
behavior or setup changes.

CI must pass before merge. Reviews weigh correctness and session continuity
first, then performance and interface clarity. Keep new controls and
configuration justified by the problem they solve.

Contributions use [Apache 2.0](LICENSE); there is no CLA.
[Security](SECURITY.md) explains private vulnerability reporting.
