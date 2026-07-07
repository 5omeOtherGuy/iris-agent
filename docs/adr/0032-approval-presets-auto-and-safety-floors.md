# ADR-0032: Approval presets, auto mode, and non-bypassable safety floors

**Date**: 2026-07-04
**Status**: accepted — amended by
[ADR-0052](0052-task-workflow-v2-opt-in-guard-and-integrated-settlement.md)
(task-scoped dirty-file grants apply to bash attribution as well as edit/write;
the dirty-tree guard remains a non-bypassable floor). v1 implemented
(Nexus-owned `strict`/`auto`/`never`
approval mode, `/approval` session control, and TUI status label). v1 auto-runs
only clean in-workspace `edit`/`write`; the destructive, dirty-file, and
repository-control floors are enforced. Auto bash (and its sandbox preflight
seam), repo-local persistence, and HOME-owned global defaults are deferred.
**Deciders**: Iris maintainers, Pi agent session

## Context

Iris currently prompts on most effectful tool calls. That behavior is safe, but
it is too noisy for normal coding work:

- `bash`, `edit`, and `write` all require approval.
- ADR-0010 disables persistent allow-always for mutating/effectful tools because
  a per-tool grant would authorize arbitrary later arguments.
- ADR-0027 adds a HOME-owned per-cwd project policy, but it is explicit grant
  storage, not an approval-mode system.
- ADR-0028 adds dirty-tree protection: pre-existing user changes are special
  and must not be silently modified or entangled.

The reference tools split the design space:

- Claude Code exposes named permission modes: default/on-request behavior,
  `acceptEdits`, `dontAsk`, dangerous bypass, and an AI-assisted auto mode.
- Codex is the closer model for Iris: it separates approval policy from the
  sandbox/permission profile, then offers presets that bundle the two for UX.
- Pi intentionally has no permission popups. Users shape risk mostly by tool
  exposure (`--tools`, `--exclude-tools`, `--no-tools`) or by writing their own
  extension policy. That is too coarse for Iris because Nexus already owns an
  approval choke point.

The settled direction is not "make Iris ask less by answering yes." It is a
runtime policy model with explicit presets and hard safety floors. Auto mode is
one preset in that model.

This ADR assumes the Landlock-backed shell sandbox needed for auto is fully
enforced. For bash, that means the runtime can prove the configured sandbox is
active before running the command. The current Iris Landlock policy confines
writes to the workspace/temp paths and denies TCP networking; reads are left
unrestricted. If Iris wants auto mode to make a confidentiality claim, the shell
sandbox must also grow workspace-read confinement or the UI must say clearly that
sandboxed bash may still read host files outside the workspace.

## Decision

Introduce approval presets as operator-controlled runtime policy. Presets are a
UX layer over two separate runtime axes:

1. **Approval policy**: when Iris asks, auto-approves, or denies.
2. **Sandbox posture**: what the runtime can confine independently of approval.

Nexus remains the enforcement point (ADR-0005). The TUI, CLI, settings, and
project trust store may choose or display a policy, but they do not enforce it
and they do not answer approval requests on Nexus's behalf.

### Presets

Iris will support these user-facing presets:

| Preset | Runtime meaning | Persistence |
| --- | --- | --- |
| `strict` / `on-request` | Current behavior. Prompt for every non-allowlisted gated call. | Default. May be selected for the session or as a HOME-owned global default. |
| `auto` | Run calls that Nexus can prove safe under the current sandbox and dirty-tree state; ask for the rest. | Session/global. Never repo-controlled. |
| `never` / `never-ask` | Do not show prompts. A call that would require a prompt is denied unless already covered by an explicit grant. | Session/global. Useful for non-interactive/read-only posture. |

A future `full-access` or `danger` preset is not part of this decision. If it is
added later, it must be explicit, visibly dangerous, and still state which of the
safety floors below remain in force. Auto is not full access.

`--approve` in print/headless mode is also not auto. It is an explicit operator
request to approve gated calls for that run. It must remain visually and
semantically separate from `auto`.

### Non-bypassable floors

Every approval mode, grant layer, and preset sits below these floors:

1. **Destructive floor.** Destructive or recoverability-destroying shell
   commands (`rm`, `git reset --hard`, `git clean`, `git restore`, force push,
   filesystem/device destroyers, and similar patterns) are never auto-approved
   and never project-granted. They require a fresh deliberate decision when a UI
   can ask; in never-ask mode they are denied.
2. **Dirty-file floor.** A pre-existing dirty or untracked user file is never
   silently modified. Auto mode does not approve it. Project policy cannot grant
   it. A prompt may approve it only for the current task scope.
3. **Sandbox floor.** Auto bash requires a proven enforced sandbox. If the
   sandbox is disabled, unavailable, degraded, or cannot be checked before the
   command, auto falls back to strict for bash.
4. **Repository-control floor.** Approval presets and loosening policy are never
   read from repo-committed config. HOME-owned global settings and the
   HOME-owned canonical-cwd trust store may carry policy; `.iris/settings.json`
   in a repo must not grant tool authority.
5. **Nothing self-waives.** The model, a provider, a repo file, or a tool result
   can request an action; none of them can loosen approval policy.

These floors deliberately override session allow-always, project policy, and
auto mode.

**v1 note (auto edit/write path safety).** Auto's inside-workspace check is an
approval-time classification that fails closed (outside-workspace or
symlink-escaping targets stay on the prompt path). It is not a write-time TOCTOU
boundary: the tool body re-resolves the path at execution, where workspace
confinement re-canonicalizes the deepest existing ancestor and bails on escape
when confinement is active. Auto therefore never bypasses an active confinement
and is strictly more conservative than execution. Closing the residual
open()-follows-symlink race is the execution path's job uniformly (it applies
equally to a strict-mode approved write), tracked with OS sandboxing (#253), not
this preset.

### Grant layers and precedence

Keep the existing allow layers, but make their relationship to presets explicit:

```
floor checks (destructive, dirty, sandbox availability, repo-control)
   └── session grants (ephemeral)
        └── project grants (HOME-owned, per canonical cwd)
             └── approval preset (strict / auto / never)
```

Session and project grants are explicit user intent. `auto` is runtime
classification. `never` only denies unresolved prompts; it does not revoke an
explicit session/project grant that already authorizes the exact class of call.

Project grants remain ADR-0027's shape:

- `write` and `edit` may be granted per tool for the project.
- `bash` grants are exact command strings or token-boundary prefixes, never a
  blanket bash grant.
- Destructive bash commands are not grantable.
- Dirty-file mutation is not grantable.
- The project store is HOME-owned and keyed by canonical cwd.

### Prompt outcomes

The approval prompt should describe the actual scope of the decision. The names
matter because users must not confuse a task-local dirty-file exception with an
allow-always policy.

Supported outcomes:

- **Allow once**: run this call only.
- **Allow this session**: only for tools that support persistent session grants.
  Mutating/effectful built-ins still opt out under ADR-0010 unless a future ADR
  scopes the grant by path or exact call.
- **Allow this project**: only for ADR-0027 grantable calls. Not offered for
  destructive commands or dirty-file approvals.
- **Allow this dirty file for this task**: task-scoped only.
- **Allow all current dirty files for this task**: task-scoped escalation for
  workflows where the user intentionally asks Iris to continue work in an
  already-modified tree.
- **Deny**: refuse the call.

Dirty-file prompts must not use the label "always." A dirty approval expires at
settlement with the baseline it was judged against. It is never saved to the
session allow-set, project policy, or global settings.

### Auto mode rules

Auto mode is deterministic. The first implementation does not use an AI
classifier. A classifier may be considered later only as an input to the same
Nexus policy, never as an authority that bypasses the floors.

Auto mode permits a call only when all applicable checks pass.

#### Read-only built-ins

Tools that do not require approval today (`read`, `grep`, `find`, `ls`,
`read_output`) remain unchanged. They are not special-cased by auto.

#### `edit` and `write`

Auto-approve `edit` and `write` when:

- the target path resolves inside the workspace under the active path-safety
  rules;
- the target is not in the dirty baseline, or it has already been approved for
  this task;
- the call is not otherwise blocked by tool validation.

If a target is outside the workspace, cannot be resolved safely, or touches a
protected dirty path, auto falls back to the approval path.

#### `bash`

Under the "fully enforced Landlock" premise, auto may approve ordinary sandboxed
bash, but only inside a narrow runtime contract:

- the bash action is the default foreground `run` action;
- no persistent shell session is requested;
- no background job action (`start`, `poll`, `finalize`, `cancel`, `list`,
  `reset`, `close`) is being used in the auto path;
- Nexus can prove before execution that the command will run under the enforced
  sandbox posture required by the selected preset;
- the destructive classifier does not flag the command;
- the dirty-tree guard says there are no unapproved protected dirty files that a
  general workspace-writing shell could touch.

If the protected dirty set is non-empty, a general auto-approved bash command is
not safe: Landlock confines it to the workspace, but does not know which
workspace file it will write. The command must prompt unless it is separately
classified as read-only by a deterministic shell-command allowlist. That
allowlist can include simple status/inspection commands (`pwd`, `ls`, `git
status`, `git diff`, `rg`, `grep`, `cat`, `head`, `tail`) only if parsing can
reject redirects, control operators, command substitution, aliases/functions,
persistent sessions, and other shell features that could write.

The conservative v1 is therefore:

- auto-run foreground, one-shot, non-destructive bash only when the shell
  sandbox is enforced and the task has no unapproved dirty files; and
- prompt for every other bash call.

### Never-ask mode

Never-ask is not bypass. It means "do not interrupt me." A call is handled as
follows:

1. If an explicit session/project grant authorizes it and no floor blocks it,
   run it.
2. If auto would have run it only because of runtime classification, do not run
   it unless the selected preset is `auto`.
3. If a prompt would be required, deny it and return a normal denied tool result
   to the model.

This matches Claude Code's useful `dontAsk` behavior without importing its
bypass mode.

### UI and configuration

Add an approval-mode control to the TUI and CLI:

- `/approval strict|auto|never` changes the session mode.
- The status line shows the active mode (`on-request`, `auto`, `never-ask`) with
  a distinct symbol/label. It must not reuse `always-approve` for auto.
- A picker may cycle modes, but it must not hide the active sandbox posture when
  auto depends on it.
- A HOME-owned global default may be added after the session mode works.
- Repo-local `.iris/settings.json` must not choose a loosening approval mode.

The `/trust` modal remains the project-policy editor. It may show that project
grants reduce prompts under strict or auto, but it is not the mode picker.

### Runtime integration

Add an approval-mode field to `Agent` in Nexus. The mode is installed by the
host at construction and can be changed at inter-turn boundaries.

The gated-call path should compute:

```
blocked_by_floor = destructive || dirty_gate || sandbox_required_but_unproven
explicit_allowed = session_allowed || project_allowed
auto_allowed = approval_mode == Auto && deterministic_auto_policy_allows(call)

if blocked_by_floor:
    prompt_or_deny_according_to_mode()
elif explicit_allowed || auto_allowed:
    emit ToolAutoApproved and run
else:
    prompt_or_deny_according_to_mode()
```

The real code should avoid a generic `blocked_by_floor` boolean when individual
floors need different UI text. The invariant is the same: a floor prevents silent
execution before any grant or preset is consulted.

Do not implement auto by wrapping the `ApprovalGate` and returning `Allow`.
That would make the front-end the policy owner and would blur auto approvals
with user approvals. Auto decisions are Nexus decisions and should be emitted as
`ToolAutoApproved` or a more specific future event.

Bash needs a pre-execution sandbox-status check. If the current shell sandbox can
only report its status after spawn, add a separate preflight API that proves the
sandbox posture before Nexus decides to auto-run.

### Test requirements

Behavior changes need tests at the Nexus/tool-policy layer, not only TUI tests:

- strict mode preserves current prompt behavior;
- auto runs clean in-workspace `write`/`edit` without invoking the approval
  gate;
- auto prompts or denies outside-workspace `write`/`edit`;
- auto never silently touches a dirty baseline path;
- dirty approvals are task-scoped and expire at settlement;
- dirty prompts do not offer project grants or persistent allow-always;
- destructive bash prompts in strict/auto and denies in never-ask;
- auto bash runs only when the sandbox preflight is enforced;
- auto bash falls back to prompt when dirty files exist;
- never-ask denies unresolved prompts but still honors explicit non-floor
  session/project grants;
- repo-local settings cannot select a loosening approval mode;
- TUI status labels distinguish `auto` from `always-approve`.

## Alternatives Considered

### Keep strict/on-request only
- **Pros**: Smallest runtime surface; current safety model is well understood.
- **Cons**: Prompt fatigue remains high and pushes users toward agents with less
  careful safety boundaries.
- **Why not**: Iris can reduce prompts without abandoning Nexus-owned safety.

### Copy Claude Code modes directly
- **Pros**: Familiar names and useful behaviors (`dontAsk`, `acceptEdits`,
  `auto`).
- **Cons**: Claude's dangerous bypass and classifier-centered auto do not match
  Iris's enforcement-first architecture. `acceptEdits` is too narrow once Iris
  has a real shell sandbox.
- **Why not**: Borrow the useful concepts, not the authority model.

### Copy Codex presets directly
- **Pros**: Good separation between approval policy and sandbox profile; close
  to the Iris direction.
- **Cons**: Codex's labels and profiles assume Codex's sandbox implementation
  and product surface. Iris has dirty-tree floors and project grants that need
  to be first-class.
- **Why not**: Use the shape (preset bundles over separate axes), not the exact
  profiles.

### Follow Pi and remove permission popups
- **Pros**: Minimal core; users can use containers or custom policy.
- **Cons**: Too coarse for Iris. Nexus already owns tool execution and approval
  policy, and Iris has git/dirty-tree safety promises that need runtime gates.
- **Why not**: Iris is choosing a stricter built-in safety contract than Pi.

### Implement auto in the UI by auto-answering prompts
- **Pros**: Fast to wire; no Nexus changes.
- **Cons**: Moves authorization to Tier 3, hides policy from tests, and makes
  auto indistinguishable from a user approval in the transcript.
- **Why not**: Violates ADR-0005. Nexus owns approval decisions.

### Allow persistent dirty-file allow-always
- **Pros**: Fewer prompts when working in a dirty tree.
- **Cons**: A dirty file's meaning is tied to the current baseline. Persisting
  approval across tasks, sessions, or project state would silently authorize
  mutations to future user work the user never reviewed.
- **Why not**: Dirty approvals are only coherent inside the task whose baseline
  exposed them. Use task-scoped escalation instead.

### Auto-approve all sandboxed bash
- **Pros**: Maximum prompt reduction under Landlock.
- **Cons**: Even a fully enforced workspace-write sandbox can still modify dirty
  files inside the workspace unless the dirty guard gates it. If reads remain
  unrestricted, bash can also copy host secrets into the transcript.
- **Why not**: Auto bash needs dirty-tree and confidentiality caveats. Enforced
  sandboxing is necessary, not sufficient.

## Consequences

### Positive
- Prompt volume drops for routine, sandboxed workspace work.
- The safety story stays deterministic and testable.
- The UI can explain modes in product terms while Nexus keeps one enforcement
  model.
- Project grants, auto mode, and never-ask mode compose instead of competing.
- Dirty user work remains protected even in auto.

### Negative
- The approval system gains another runtime axis and more state to display.
- Bash auto depends on a reliable sandbox preflight seam that does not fully
  exist today.
- Users may expect auto to mean "never ask" unless the UI labels the fallback
  behavior clearly.
- Dirty-tree work will still prompt, especially for bash, unless the user grants
  a task-scoped dirty escalation.

### Risks
- A sandbox-status false positive could auto-run unsafe bash. Mitigate with a
  fail-closed preflight API and tests for disabled/degraded sandbox states.
- A shell parser allowlist could miss write-capable syntax. Mitigate by starting
  with no broad read-only shell allowlist, or by using a parser with conservative
  rejection.
- Repo-local configuration could accidentally gain authority. Mitigate by tests
  that project settings cannot loosen approval mode and by keeping policy in
  HOME-owned stores.
- Users may confuse task-scoped dirty approvals with persistent grants. Mitigate
  with explicit labels: never call dirty escalation "always."
