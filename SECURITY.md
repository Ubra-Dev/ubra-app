# Security policy

## Supported versions

Security fixes are made for the latest ubra release. Because the app can launch
processes with your user account's privileges, keeping ubra and the
coding-agent CLIs it launches current is important.

## Report a vulnerability

Please use [GitHub's private vulnerability reporting](https://github.com/Ubra-Dev/ubra-app/security/advisories/new).
Do not include an exploit, private terminal output, tokens, or personal paths in
a public issue.

Include the affected ubra and operating-system versions, a minimal reproduction, the
impact you believe is possible, and any suggested mitigation. You should receive
an acknowledgement within seven days. Timing for a fix or disclosure depends on
severity and complexity; the maintainer will coordinate that with the reporter.

For ordinary bugs, use the [bug report form](https://github.com/Ubra-Dev/ubra-app/issues/new?template=bug_report.yml).

## Scope and trust model

ubra intentionally runs local shells, coding agents, MCP tools, and optional
remote-node commands. A tool doing something the user explicitly authorized is
not itself an ubra vulnerability. Permission-boundary bypasses, unsafe update or
IPC behavior, credential disclosure, session isolation failures, and unintended
remote execution are in scope.

See the [security model](docs/SECURITY-MODEL.md) for the boundaries ubra does
and does not provide.
