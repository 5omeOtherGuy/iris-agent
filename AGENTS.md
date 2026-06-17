# Iris Agent Guidelines

Iris = terminal-first coding agent product. Nexus = local agent runtime core. Iris CLI = terminal interface. Rust project; when code and docs conflict, trust the code and flag the stale doc.

(Maintainers: the full local playbook lives in the gitignored `AGENTS.local.md`. This committed file is the public, review-facing subset and is what Codex code review reads.)

## Architecture boundaries

- **Nexus owns** the model loop, tool execution protocol, conversation state, provider-neutral message/tool contracts, and workspace + shell safety policy. It is the enforcement point for tool execution, approvals, workspace paths, and shell commands.
- **Iris CLI owns** terminal I/O, display, arg parsing, interactive prompts, and collecting approval decisions Nexus requests. The CLI may do UX/syntactic validation but never replaces Nexus enforcement.
- Keep provider/auth IDs (credential source, e.g. `openai-codex`) separate from API/transport IDs (wire protocol, e.g. `openai-codex-responses`).
- Provider adapters: `src/providers/{provider}_{api}.rs`. Auth/token stores: `src/auth/{provider}.rs`. Nexus/runtime code must not depend on provider-specific payloads, auth details, endpoints, or transport types.
- Organize by feature/domain and ownership boundary, not generic type buckets. Prefer cohesive files under ~400 lines; do not split solely for line count.

## Review guidelines

- Flag terminal/UI logic that leaked into Nexus, or tool-execution/approval/path/shell enforcement that leaked into the CLI.
- Reject provider-specific names, auth details, endpoints, or transport types in Nexus/runtime code.
- Require validation of user/model/provider/tool/path/shell inputs at system boundaries.
- Security-critical: workspace path traversal, shell-command policy bypass, and approval-gate bypass are blocking issues.
- Runtime must use Tokio async provider streams, a turn-level `CancellationToken`, stream-vs-cancel and tool-vs-cancel races, a child token per tool, valid transcript/error data on abort, and explicit safe-parallel (otherwise sequential) tools. Flag any bespoke runtime, WebSocket/session-reuse machinery, or a single giant agent file.
- Behavior changes need tests, especially: workspace path safety, tool result/error encoding, edit behavior, approval handling, provider-stream cancellation, tool cancellation, sequential-by-default tools, and explicit safe parallel tools.
- No emojis in code, comments, docs, logs, or CLI output unless explicitly requested.
- Prefer the smallest correct change; call out scope creep, premature abstraction, and reinvented stdlib/dependency functionality.

## Git workflow

- Solo trunk-based: small safe changes go directly to `main`; short-lived branches for risky/multi-step work. Never rewrite or force-push `main`.
- Conventional commit subjects and PR titles, e.g. `feat(auth): add OpenAI Codex login`.
- Before pushing code, run `bash scripts/gate.sh` (fmt + clippy + test); it mirrors CI. Inspect CI failures with `gh run view --log-failed`.
- Comment `@codex review` on a PR to request a Codex review.
