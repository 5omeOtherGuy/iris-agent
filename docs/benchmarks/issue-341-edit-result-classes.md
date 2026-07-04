# issue #341 edit result classes — token cost per class

Model-facing payload cost for each `edit` outcome class (ADR-0036 token
efficiency, ADR-0038 conditional echo). Tokens estimated at 4 bytes/token, as in
`adr-0037-bash-filter-tokens.md`; only the relative sizes matter. Sizes are for
representative payloads (a `src/config.rs` target, a one-line change with two
context lines each side); real values scale with path length and the size of the
echoed/candidate region, which is bounded to a few lines.

| class | outcome | model-facing payload | approx bytes | approx tokens |
|---|---|---|---|---|
| `exact` | success | `Successfully replaced N occurrence(s) in <path>.` | 52 | ~13 |
| `tolerant-match-fired` | success | terse line + `Applied region` snippet (change ± 2 lines) | 171 | ~43 |
| not-found | error | actionable message + closest-candidate region (± 2 lines) | 444 | ~111 |
| not-unique | error | occurrence count + disambiguation hint | 175 | ~44 |
| stale-file | error | read-before-mutate rejection | 79 | ~20 |

Deltas that this change introduces:

- Exact success is unchanged in content; it gains only the `edit_outcome`
  metadata field (`"exact"`, ~6 tokens), stripped of any file content.
- The tolerant echo adds ~30 tokens over the terse line, spent only where a
  fuzzy match was applied — nowhere else (ADR-0038: "spend a few dozen tokens
  exactly where ambiguity was introduced").
- The not-found closest-candidate region adds ~36 tokens over the base message
  (300 bytes / ~75 tokens); it is a bounded few-line region, never the file.

## Telemetry class

Each edit attempt records its outcome class so a transcript-level, per-model
join can measure how often each recovery path fires (ADR-0038; the active model
is a transcript fact, never plumbed into the tool):

- Success outcomes carry the class in `ToolOutput` metadata as
  `edit_outcome` = `exact` | `tolerant-match-fired` (ADR-0021 per-tool metadata).
- Failure outcomes carry the class in their actionable error text
  (not-found / not-unique / stale-file), which is the transcript record for a
  failed call — the ADR-0021 error envelope (`{ ok: false, error }`) has no
  metadata channel.
