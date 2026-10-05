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
