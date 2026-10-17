# Security Policy

## Supported versions

Security fixes target the latest minor release line (currently `0.3.x`) and
the default branch. Older lines receive no fixes.

## Reporting a vulnerability

Report suspected vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/5omeOtherGuy/iris-agent/security/advisories/new).
Do not open a public issue with exploit details, credentials, tokens, logs, or
private session data.

Helpful report details:

- affected component (Nexus runtime, provider/auth, tool, or CLI)
- impact and attack scenario
- reproduction steps using non-secret sample data
- affected version, commit, or installation source

## What happens next

- Reports are acknowledged within 7 days.
- Fixes go through a private security advisory: fix, release, publish the
  advisory, and credit the reporter.
- Iris enforces workspace path safety and shell-command policy in Nexus;
  reports about sandbox escape, path traversal, or approval bypass are
  especially welcome. Fixes include deterministic tests when practical and
  avoid publishing sensitive reproduction data.
