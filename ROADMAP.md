# Roadmap

Ubra should be the best way to work with coding agents. Agents are good enough
now that the bottleneck is you: how many you can keep track of and how much
context you can hold. Everything below is about removing that bottleneck.

This is direction, not a release calendar. Specific work is tracked in
[Issues](https://github.com/Ubra-Dev/ubra-app/issues).

## Next

- **Notes.** Write a PRD in a note, break it into to-dos, and send any to-do to
  an agent. Agents write notes back. You stay at the level of the plan while
  the agents do the work.
- **Windows.** A native Windows build is in progress, alongside macOS and the
  Linux beta.
- **Bigger swarms.** Today you can run dozens of agents in parallel. The goal is
  hundreds, with status, review, and coordination that still fit in one head.

## Always

- **Sessions never die.** Persistence, Engine upgrades, and recovery that hold
  up under updates, reconnects, crashes, and memory pressure.
- **Every agent, done properly.** Launch, resume, and status detection for each
  agent, tested against real terminal fixtures.
- **Your machines.** Local or any SSH host you control, with no tmux, no sudo,
  and no hosted relay.
- **Releases you can trust.** Green CI, provenance and supply-chain checks,
  signed and notarized builds, and the Homebrew tap.

## Boundaries

No Ubra account and no hosted relay for your sessions. Diagnostics are covered
by the [privacy notice](PRIVACY.md) and can be turned off. Agent processes run
with your user permissions; ubra does not sandbox them.

For proposals, describe the workflow and the smallest useful improvement in
[Discussions](https://github.com/Ubra-Dev/ubra-app/discussions) or a
[feature request](https://github.com/Ubra-Dev/ubra-app/issues/new?template=feature_request.yml).
