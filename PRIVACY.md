# Privacy

ubra has no account system or advertising. It records diagnostics (crashes,
hangs, errors and timings) so bugs can be fixed from a report instead of a
reproduction, and shares them with the project unless you turn that off. The
project does not run a service that receives your terminal contents or session
history.

## Diagnostics

Every ubra process (the app, its Engine, and each session's Holder) keeps a
flight recorder: a local log of what it did, in `<state>/telemetry/spool`.
On macOS that is `~/Library/Application Support/Ubra/telemetry/spool`; on Linux
`~/.local/state/ubra/telemetry/spool`. The whole spool directory is capped at
64 MiB. Only the Engine uploads.

**What is recorded:** app and build versions; the operating system and CPU
architecture; crashes with their symbolized stack frames and source location;
hangs and slow frames; memory, CPU and open-file counts; how long sessions take
to start, attach and first draw; errors and their codes; whether copy, paste,
file drops and updates worked (with size classes such as "under 1 KB", never
contents); which commands ran (by name); once-only setup milestones (first
launch, first agent ready and whether it was installed from the welcome, first
and second agent session, first agent started by another agent, and a launch on
a later day); and identifiers that let a report be followed: session ids, agent
names, and agent conversation ids. Folders are recorded only as a one-way hash.
Errors are recorded as codes and classes. Free-form error messages, subprocess
stderr and panic payloads are excluded; stack symbols and source locations
remain available for diagnosing crashes.

**What is never recorded:** terminal output or input, prompts, pasted or copied
text, file contents, environment variables, command lines, URLs you open,
passwords or keys.

**Where it goes:** unless you turn sharing off, the Engine uploads the log
about once an hour (within a minute after a crash or other incident) to a
Cloudflare Worker operated by the project at `https://telemetry.getubra.com`.
Uploads carry a random install id, the short Support ID derived from it, and
the name you chose (your login name unless you change or clear it). Uploads are
deleted by the diagnostics service on a retention schedule operated by the
project. Only the maintainers can read them.

**Your controls:** Settings › General › Privacy has the switch (*Share
diagnostics to help fix bugs*), the name (*Name for bug reports*), your Support
ID, *Diagnostics on this Mac*, and *Send now*. Turning sharing off stops uploads
at the next cycle; recording stays local. Missing, unreadable or malformed
settings disable uploads. *Send now* uploads what has been recorded so far
immediately, even with sharing off: clicking it is a one-time choice to send.
Setting `UBRA_TELEMETRY=off` in ubra's environment turns recording off
entirely. Help › Report a Problem… marks the moment in the log, sends it right
away the same way, copies your Support ID and opens a GitHub issue with it
filled in.

## Data stored on your machine

ubra stores session state, terminal replay logs, host configuration,
preferences, usage summaries, and search/index data under these locations:

- macOS: `~/Library/Application Support/Ubra` (including the diagnostics log
  under `telemetry/`), and `~/Library/Caches/ubra/updates`.
- Linux: `~/.local/share/ubra`, `~/.local/state/ubra` (including the
  diagnostics log under `telemetry/`), `~/.config/ubra`, and `~/.cache/ubra`
  (including `updates`).

Terminal logs can contain prompts, command output, repository paths, and secrets
printed by a process. Treat them as sensitive. Before attaching diagnostics to
an issue, review and redact them. Archiving can intentionally preserve session
metadata. Deleting the directories above removes all ubra-managed local data
after ubra and its daemon are stopped.

## Network activity

ubra connects to GitHub Releases to check for and download updates, and to the
project's diagnostics service unless you turn sharing off (see above). It may
also make network connections when you explicitly use remote hosts, PR
monitoring, or a tool/agent that uses the network. Those
tools and services have their own privacy practices. ubra does not proxy their
traffic through an ubra-operated server.

Remote-node credentials remain in the mechanisms you configure (for example,
SSH configuration and your keychain); they are not sent to the ubra project.

## Process access

ubra is not sandboxed because its core function is to launch shells and coding
agents, create worktrees, and communicate with local tools. Child processes run
with your user account's privileges and may inherit environment variables. Only
run agents and MCP servers you trust, and review their permissions separately.

For vulnerability reports, follow [SECURITY.md](SECURITY.md).
