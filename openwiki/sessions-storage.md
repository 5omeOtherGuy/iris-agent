# Sessions and Storage

Wayland owns session integration. Nexus owns in-memory conversation state and can
be seeded from a prior transcript.

## Transcript storage

Session logs are JSONL files under `~/.iris/sessions` unless `IRIS_SESSION_DIR`
overrides the root. The log stores headers, user and assistant messages, tool
calls/results, reasoning rows, model-selection audit entries, compaction
entries, fold entries, dangerous-mode audit entries, task metadata, and linked
resume metadata.

If a log cannot be opened, Iris warns and continues in memory.

## Resume

Resume loads a stored transcript, rebuilds provider-visible context, seeds a new
agent with that history, and continues appending to the same log.

```bash
iris -c
iris resume
iris resume <session-id>
```

Dangling trailing tool calls are repaired when rebuilding context so the next
provider request is valid. In-session `/resume` and `/new` swap the active
session at a safe turn boundary without restarting the process.

## Compaction

The harness tracks a context token budget. When context exceeds the budget at a
safe turn boundary, it can compact history into a summary and persist compaction
metadata. Manual compaction is exposed through `/compact`.

Provider-backed summarization is the default. A deterministic excerpt fallback
exists for bounded recovery, and `compactionSummarizer` can force excerpts.

Opt-in `microcompaction` writes `fold` entries for spent tool results, such as
superseded reads. Folds preserve provider tool-pair validity by replacing only
rebuilt result content with deterministic stubs.

## Output handles

Oversized successful tool outputs can be stored outside the inline transcript.
The sidecar directory is derived from the session path. Handles are content
addressed and validated on read.

The model-visible `read_output` tool pages stored output back into context. The
`recall` tool can recover compacted transcript detail from the current session
span.

## Permission policy

Per-project permission policy lives outside the repository by default. It stores
per-tool file grants and per-command bash allows. Destructive commands always
re-prompt and are not grantable.

The `/trust` and `/permissions` commands expose the policy in the TUI.

The store path is `~/.iris/trust.json` unless `IRIS_TRUST_PATH` overrides it.
Overrides must be absolute and outside the project directory; invalid stores
fail closed.

## Task checkpoints

Wayland tracks Iris-authored workspace changes during a task. `/checkpoint`
saves an explicit restore point and settles the current task state. `/rollback`
lists restore points; `/rollback <n>` restores Iris's own work at or after that
point while preserving user-owned paths. `/diff` shows the current task's net
diff. `/accept` accepts the current Iris changes and settles the task.

If a `verify` settings block is present, the harness runs the configured shell
command after a task's changes and can retry up to `verify.maxAttempts` (default
3, capped at 10). Verification runs through the normal bash approval gate.
