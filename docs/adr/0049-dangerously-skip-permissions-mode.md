# ADR-0049: `--dangerously-skip-permissions` bypasses the approval gate

**Date**: 2026-07-05
**Status**: accepted
**Deciders**: iris-agent maintainers, operator

## Context

ADR-0032 made the safety floors (destructive, dirty-file, sandbox,
repository-control, nothing-self-waives) non-bypassable: no preset, grant layer,
or config could auto-run a destructive or dirty-file call. That invariant is
correct for interactive use, and approval-gate bypass is this repo's blocking
security class.

Operators running Iris inside a throwaway sandbox (a container, a CI job, a
disposable VM) sometimes want the opposite trade: no prompts at all, for every
call, and accept full responsibility. Claude Code ships
`--dangerously-skip-permissions`; Codex ships a full-auto mode. Iris has no
equivalent, so those users either babysit prompts or drop the safety floors
some other way.

This ADR is a deliberate, operator-sanctioned exception to ADR-0032's
non-bypassable-floor invariant. It does not weaken the floors for any other
session.

## Decision

Add an explicit `--dangerously-skip-permissions` mode. When active, Nexus
auto-approves EVERY gated tool call at the top of the approval decision path,
before any floor, grant, or preset is consulted, and emits a distinct audit
event. The mode is guarded by hard constraints:

1. **Activation is operator-controlled.** The CLI flag is the explicit startup
   path. `/approval dangerously-skip-permissions` and the settings menu select the
   same runtime mode at an inter-turn boundary. The selected permission mode is
   stored in global-only `defaultApproval`; project settings are ignored for this
   key, and the per-cwd trust store (ADR-0027), env vars, and repo-committed state
   cannot enable it. A malicious repository has no path to granting itself
   approval (upholds ADR-0032's repository-control floor for activation).

2. **Default plus transcript persistence.** `defaultApproval` stores the last
   selected permission mode (`strict`, `auto`, `never`, or
   `dangerously-skip-permissions`) for new processes and `/new`. A session
   transcript may also carry `dangerousMode` markers; on resume, the last marker
   overrides the global default for that session. Missing markers inherit the
   global default; turning the mode off appends an explicit disabled marker. In
   skip mode Nexus never writes to `session_allowed` or the project
   `PolicyStoreSink`.

3. **Auditable, never silent.** Each bypass is emitted as
   `AgentEvent::ToolAutoApprovedDangerous`, distinct from the ordinary
   `ToolAutoApproved` so it is greppable and not confused with a policy
   auto-approval (ADR-0020 taxonomy).

4. **Loud and recorded.** Session start prints a one-time stderr banner
   (`ALL PERMISSION CHECKS DISABLED ...`) and appends a `dangerousMode`
   transcript entry, so a resumed or audited session shows the mode was active.

5. **Only the approval gate is skipped.** Workspace path safety,
   read-before-mutate (ADR-0007), the mutation guard's snapshot/restore, and all
   non-approval tool validation are unchanged. Skip mode does not disable other
   safety systems.

Scope is one skip check at the top of the approval decision path; the
floor/grant/preset logic below is untouched.

## Alternatives Considered

### Activate from project config / trust-store flag
- **Pros**: Set once per repository.
- **Cons**: A repo-committed or persisted value could grant a cloned repository
  approval-gate bypass — exactly the repository-control floor ADR-0032 forbids.
- **Why not**: Rejected. Activation may persist only in the user-global
  `defaultApproval` setting or in the same session's transcript markers; project
  config and trust stores cannot enable it.

### Add a `full-access` approval preset
- **Pros**: Reuses the ADR-0032 preset axis.
- **Cons**: A preset that silently overrides floors blurs the auto/never model.
- **Why not**: This is a deliberate floor exception, not a normal preset. It is
  surfaced as an exclusive permission mode: selecting `strict`, `auto`, or
  `never` clears skip-permissions; selecting `dangerously-skip-permissions`
  enables it.

## Consequences

- Operators get a true no-prompt mode for sandboxed/CI use.
- The bypass is explicit, loud, audited per-call, and persisted only in
  user-global defaults plus same-session transcript state — project files still
  cannot opt a repository into it.
- Non-approval safety (paths, read-before-mutate, mutation guard) still applies,
  so skip mode is not "disable all safety".
- Risk: a user runs it outside a sandbox. Mitigated by the flag name, the help
  danger warning, the startup banner, and the transcript record.
