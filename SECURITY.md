# Security Policy

## Supported versions

Iris is currently pre-1.0. Security fixes target the default branch and the next published version.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub Security Advisories for this repository. If that is not available, open a minimal issue that says a private security report is needed and do not include exploit details, credentials, tokens, logs, or private session data.

Helpful report details:

- affected component (Nexus runtime, provider/auth, tool, or CLI)
- impact and attack scenario
- reproduction steps using non-secret sample data
- affected version, commit, or installation source

## Handling expectations

Security reports are prioritized over routine feature work. Iris enforces workspace path safety and shell-command policy in Nexus; reports about sandbox escape, path traversal, or approval bypass are especially welcome. Fixes should include deterministic tests when practical and should avoid publishing sensitive reproduction data.
