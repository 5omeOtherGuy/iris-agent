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

The harness resolves model-aware warn/start/hard pressure and checks it at turn
edges and pair-closed provider boundaries. Defaults are 60%/72%/90%, subject to
model reserves; the recent-tail default is 8,000 tokens. A worker prepares in the
background at start pressure. Ready summaries remain held until hard pressure or
manual `/compact`; they do not currently apply as soon as ready. The turn-edge
hard wait remains blocking. The [v1.0 roadmap](../docs/ROADMAP.md) tracks the
apply-on-ready, wait and safe-settings overhaul.

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
per-tool file grants and per-command bash allows. Destructive commands re-prompt
and are not grantable in normal modes; explicit dangerous-skip bypasses the gate
and its floors.

The `/trust` and `/permissions` commands expose the policy in the TUI.

The store path is `~/.iris/trust.json` unless `IRIS_TRUST_PATH` overrides it.
Overrides must be absolute and outside the project directory; invalid stores
fail closed.

## Task checkpoints

Mutation safety is default-on; durable task records and checkpoint/settlement
commands require `tasks=true`. `/checkpoint` saves a restore point without
settling the task. `/rollback` lists/restores task checkpoints while protecting
user-owned changes. `/diff` shows the task's Iris-attributed net diff; `/accept`
accepts those changes and settles the task. This file rollback is not
conversation branching, which remains planned.

If a `verify` settings block is present, the harness runs the configured shell
command after a task's changes and can retry up to `verify.maxAttempts` (default
3, capped at 10). Verification runs through the normal bash approval gate.
