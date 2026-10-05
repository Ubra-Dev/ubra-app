# Ubra

**A native desktop workspace for running and coordinating the coding-agent CLIs you already use.**

Keep parallel agent sessions visible, give independent tasks separate Git worktrees, and review changes in context. Ubra is a desktop application—not an agent model or provider, and not a hosted service. Spend less time switching context and untangling collisions while staying in control of what your agents run.

[Install options](#install) · [Get started](#get-started) · [Source code](https://github.com/Ubra-Dev/ubra-app)

## Why Ubra

Running agents in separate terminals makes it harder to see who needs input and which changes belong together. Ubra brings sessions for a project into one workspace, so you can coordinate parallel work and inspect the results without losing the surrounding context.

## What you can do

- **Coordinate sessions:** Launch multiple sessions in a project, arrange them in panes, see working, needs-input, and finished status, and receive notifications.
- **Separate parallel changes:** Assign tasks to distinct Git worktrees and branches to reduce edit collisions. Worktrees are organization tools, not security sandboxes.
- **Review alongside the work:** Inspect diffs, stage and commit changes, and follow pull request checks in context.
- **Delegate through MCP:** Use Ubra's built-in MCP server to let supported agents delegate work and coordinate with other sessions.
- **Keep sessions running:** Sessions can outlive the desktop app and survive an Engine restart.
- **Keep notes with the work:** Use notes alongside agent tasks.
- **Choose where work runs:** Run sessions locally or connect to supported SSH hosts.

Resume and fork behavior depends on the agent CLI.

## Install

There are currently no published GitHub Releases, so DMG and Linux release downloads are not available yet. Homebrew installation becomes available when a release is published. You can build Ubra from source using the instructions in [PACKAGING.md](PACKAGING.md).

### macOS

Ubra supports macOS 15 or newer on Apple silicon and Intel. When releases are available, install with the [Homebrew tap](https://github.com/Ubra-Dev/ubra-app):

```sh
brew install --cask Ubra-Dev/ubra-app/ubra
```

Alternatively, download the DMG from [GitHub Releases](https://github.com/Ubra-Dev/ubra-app/releases) when an artifact is published, then move Ubra to Applications.

### Linux beta

The supported and tested matrix is Ubuntu 22.04 and 24.04 on x86_64 and aarch64, using X11 or Wayland and a Vulkan-capable driver. Linux packages may not accompany every release. `zenity` must be on `PATH` for close dialogs; the Debian package declares it, while AppImage users must provide it.

See [LINUX.md](LINUX.md) for supported details, package verification, installation, troubleshooting, and beta limitations.

## Get started

1. Install and launch Ubra.
2. Choose a project folder and an installed agent CLI.
3. Select a pane layout and confirm to start the sessions.

Install and configure coding-agent CLIs and their provider accounts separately; Ubra runs the CLIs available to your user.

## Agents and integrations

Ubra detects locally installed supported coding-agent CLIs and runs them in terminal sessions. Bring your own CLI, provider account, and credentials. Integration depth varies by agent; Claude Code and Codex have the deepest status and resume integrations, but support is not identical across agents.

See [Agent manifests](docs/AGENT-MANIFESTS.md) for the catalog and custom manifest details.

## Local, remote, and privacy

Sessions run locally or over SSH on supported remote targets: Linux x86_64/aarch64 and macOS arm64. Intel macOS and Rosetta are not supported for the remote Helper. SSH is the transport; remote sessions do not require tmux, sudo, an Ubra account, or a hosted session relay. See [REMOTE_PORT.md](REMOTE_PORT.md) for the support boundary and architecture.

Agents, shells, and MCP servers run with your user permissions. Ubra is not a sandbox; worktrees do not restrict what an agent can access. See the [security model](docs/SECURITY-MODEL.md).
Ubra collects diagnostics unless sharing is disabled. Diagnostics do not include terminal input or output, prompts, or file contents. See [PRIVACY.md](PRIVACY.md) for exact fields, controls, local storage, and network activity.

## Contribute

Read the [contributor guide](CONTRIBUTING.md) for setup and review expectations. Report bugs and propose ideas in [Issues](https://github.com/Ubra-Dev/ubra-app/issues), or see the [Roadmap](ROADMAP.md).

## Attribution and license

Ubra is an independent derivative of the Apache-2.0-licensed [Diri](https://github.com/cristicretu/diri) project. Ubra is not affiliated with, sponsored by, or endorsed by Diri or its maintainers. See [LICENSE](LICENSE) and [NOTICE](NOTICE) for license and attribution details.

[Privacy](PRIVACY.md) · [Security](SECURITY.md)
