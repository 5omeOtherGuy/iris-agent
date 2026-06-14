# Iris Agent Guidelines

This file only adds Iris-specific rules. The global `~/.pi/agent/AGENTS.md` already covers planning, TDD, verification, security, reuse-before-handrolling, git safety, and concise reporting.

## Project context

- **Iris** is the terminal-first coding agent product.
- **Nexus** is the local agent runtime core.
- **Iris CLI** is the terminal interface.
- This is a Rust project. Do not assume architecture, commands, dependencies, or runtime behavior that is not documented or implemented. When code and docs conflict, prefer implemented code and call out the stale doc.
- Follow `docs/ROADMAP.md` for sequencing. Treat `docs/FEATURES.md` as capability inventory, not current scope. Do not implement future milestone systems unless explicitly asked.

## Architecture rules

- Keep a logical ownership boundary between Nexus runtime concerns and Iris CLI terminal-interface concerns. For MVP, this can be simple modules in one crate/binary; do not introduce separate crates, processes, or broad abstraction layers only to satisfy this rule.
- Nexus owns the model loop, tool execution protocol, conversation state, provider-neutral message/tool contracts, workspace and shell safety policy, and later context/mode/subagent behavior when those milestones are reached.
- Iris CLI owns terminal input/output, display, command-line parsing, interactive prompts, and collecting approval decisions requested by Nexus.
- The CLI may perform UX-oriented or syntactic validation, but Nexus must remain the enforcement point for tool execution, approvals, workspace paths, and shell commands.
- Organize code by feature/domain and ownership boundary, not by generic type buckets.
- Keep provider/auth identifiers separate from API/transport identifiers: provider/auth IDs describe credential/account sources (for example `openai-codex`), while API/transport IDs describe wire protocols or endpoint families (for example `openai-codex-responses`).
- Provider adapters live under `src/providers/{provider}_{api}.rs` (for example `src/providers/openai_codex_responses.rs`). Auth flows and token stores live under `src/auth/{provider}.rs` (for example `src/auth/openai_codex.rs`).
- Nexus/runtime code must not depend on provider-specific payload names, auth details, endpoints, or transport-specific types.
- Prefer cohesive implementation files under roughly **400 lines** where practical; do not split files solely to satisfy a line count. Tests, dense Rust types, or tightly coupled logic may justify larger files.

## Implementation standards

- Keep modules small, cohesive, and explicit. Avoid both god files and premature micro-modules.
- Define integration seams and contracts before connecting Nexus, providers, tools, approvals, or Iris CLI.
- Validate user, model, provider, tool, path, and shell-command inputs at system boundaries.
- Do not use emojis in code, comments, documentation, logs, or user-facing CLI output unless explicitly requested.
- For Agent Kernel MVP work, prioritize tests for workspace path safety, tool result/error encoding, edit behavior, approval handling, and fake-provider tool-call loops.

## Available skills

- `tdd-workflow` — use for behavior changes, bug fixes, and refactors.
- `git-workflow` — use for branch/commit/merge strategy or git workflow decisions.
- `rust-patterns` — use before designing or editing non-trivial Rust modules.
- `rust-testing` — use before adding or changing Rust tests.
- `documentation-codemap-specialist` — use when generating codemaps or updating docs from code.

## Review standard

For significant architecture reviews or implementation-plan reviews, include component scores when useful, with a stated rubric and overall score. Skip scoring for routine coding tasks, small changes, or status updates. Evaluate consistency, modularity, integration seams, internal/external reuse, abstraction level, blast radius, test strategy, and scope risk.
