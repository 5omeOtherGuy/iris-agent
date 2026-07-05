<!-- typos: this is a running lab notebook, not polished prose -->
# Lab Notebook: tokens-per-task benchmark (issue #210)

A live, running record of observations, decisions, deviations, and reactions
while building the Milestone-2 end-to-end tokens-per-task benchmark. Append-only
by convention: earlier entries are not rewritten, corrections are added as new
entries so the reasoning trail stays honest.

Format: timestamped entries (session-relative, not wall-clock), tagged
`OBSERVATION`, `DECISION`, `DEVIATION`, `QUESTION`, `RESULT`, `BLOCKER`.

---

## Entry 0 — Setup and context (session start)

**Task.** Implement issue #210: end-to-end tokens-per-COMPLETED-task benchmark.
Prove Iris efficiency features (bash output filtering, grep/find shaping, read
skim, handles) reduce tokens per completed task without reducing task success.

**DECISION — git loop.** Followed the repo loop: `worktree-preflight.sh` →
`PASS (primary main == origin/main)` → created worktree
`../iris-210-tokens-per-task` on branch `bench/tokens-per-task` off
`origin/main`. All edits happen only in that worktree. Do NOT merge (per task).

**OBSERVATION — pinned plan (issue #210, 2026-07-04 comment).** The design is
fixed; my job is to execute it, not redesign:
- **Arms:** (A) Iris defaults; (B) baseline with efficiency features disabled.
  B needs one benchmark-only global switch (env preferred) disabling bash output
  filtering + grep/find shaping. Per-call `raw:true` is explicitly NOT sufficient
  for an arm-level toggle. Switch is a measurement affordance, not a user feature.
- **Workloads (3), each with a mechanical success check:**
  1. fix-failing-test in a Rust fixture repo (success = gate/test green)
  2. multi-file search-and-edit (success = expected diff applied)
  3. investigate-large-log / test-output triage (success = correct answer keyed
     to a planted fact)
- **Metrics per arm×workload:** total prompt tokens (from provider usage
  records, NOT byte estimates), tokens-per-completed-task, success rate, turn
  count, wall cost. N>=3 real-provider runs per cell for headline; fake-provider
  / recorded replays for the re-runnable regression harness.
- **Honesty rules:** claims cite the committed report; if B ties or beats A, that
  IS the finding and the README claim does NOT ship. A success-rate regression in
  A vs B on any workload is stop-and-investigate, not a footnote.

**OBSERVATION — approvals (issue #210 addenda).** Both arms under the existing
ADR-0032 **auto preset**, identical config; no new bypass; safety floors stay
active. Workloads must avoid non-bypassable floors (no destructive bash).
Harness must assert **zero interactive approval prompts** per run — any prompt
observed invalidates the run. Replay path uses the existing test ApprovalGate
injection (as nexus_tests does), identical across arms.

**OBSERVATION — ADR-0036 rule 5.** "Reduction is measured." The per-tool corpora
(bash filter #336/ADR-0037, read skim #337, grep #338, find #340) already assert
per-result minimum bars via `src/tools/bench_support.rs`. This issue is the
end-to-end layer the skill explicitly defers: "does the model still complete the
task from reduced context." Distinct harness, must not duplicate the per-result
stack.

**OBSERVATION — token accounting rule.** SKILL/bench_support use a 4-bytes/token
*estimate* and only ratios are meaningful there. But #210 headline tokens must
come from **real provider usage records**, not estimates. So the end-to-end
harness needs a different token source than `bench_support::est_tokens`.

### Seam findings so far (read-only investigation)

**OBSERVATION — usage records.**
- `ProviderUsage` struct: `src/nexus.rs:2302`. Fields include `input_tokens`,
  `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`,
  `reasoning_output_tokens`, `total_tokens`, `provider`, `model`.
- `record_usage(&self, usage: &ProviderUsage)` implemented per provider:
  `openai_codex_responses.rs:232`, `openai_compatible_chat.rs:234`,
  `anthropic_messages.rs:392`. Today it only `tracing::info!`s the usage; it does
  not appear to accumulate into a session total. NEED TO CONFIRM where/if usage
  is aggregated per session (this is the headline token source).

**OBSERVATION — bash filter seam.**
- `src/tools/bash/mod.rs`: `filter_for_display(cmd, output, exit_ok, raw)` wraps
  `filter::filter_output(...)` (ADR-0037). Called at two sites (foreground +
  persistent shell). `raw:true` bypasses for one call. This is the arm-toggle
  point for bash.

### Open questions

- **QUESTION:** Where do per-session prompt-token totals get aggregated (if at
  all)? Headline metric depends on it. (Investigating `record_usage` callers /
  session accounting next.)
- **QUESTION:** What is the grep/find "shaping" seam the toggle must disable,
  and are read-skim + handles in scope for arm B? Plan names bash + grep/find
  explicitly; read skim/handles may be left on in both arms (documented).
- **QUESTION:** How does the fake provider drive tool-call loops, and how does
  nexus_tests inject an ApprovalGate? That defines the replay harness shape.

---

## Entry 1 — The auto-preset / bash constraint reshapes the workloads

**OBSERVATION (pivotal) — ADR-0032 auto v1 never auto-approves bash.** Read
`docs/adr/0032` + `src/nexus.rs` auto path (~L1719-1755) + the auto-mode tests
(`src/nexus_tests.rs` ~L6690-6900). Auto v1 auto-approves ONLY clean
in-workspace `edit`/`write`. `bash` always routes to the gate, even a clean
non-destructive `echo hi` (`auto_mode_does_not_auto_run_plain_bash`) — the safe
auto-bash path needs a proven sandbox preflight, which is deferred. `read`,
`grep`, `find`, `ls` are non-gated (no approval ever).

**Consequence for the plan:** the constraint "run under auto, assert ZERO
interactive prompts, redesign the step (never the floor) if it would prompt"
means **the agent may never call `bash`** in any workload. Under auto, an agent
bash call = a prompt = invalid run.

**DECISION — workloads: agent inspects/edits; harness runs the bash checks.**
Re-frame all three workloads so the *agent* only uses auto-safe tools
(read/grep/find/ls + clean in-workspace edit/write) and the *harness* performs
the mechanical success check outside the agent turn:
1. **fix-failing-test** (Rust fixture crate, one failing unit test). Agent:
   grep/read to locate the bug, `edit` the source (clean in-workspace edit,
   auto-approved). Harness: runs `cargo test` on the fixture AFTER the run;
   success = green. Agent never runs bash.
2. **multi-file search-and-edit** (symbol rename/fix across many files). Agent:
   grep/find to locate all sites, `edit` each. Harness: compares result files
   to the committed expected snapshot; success = expected diff applied.
3. **investigate-large-log** (a committed captured test/build log fixture with a
   planted fact). Agent: grep/read the committed log file, answer. Harness:
   checks the final answer contains the planted fact. Agent never runs bash.

**DECISION — which efficiency features the end-to-end benchmark measures.** With
no agent bash, the exercised default-on reductions are **grep shaping (#338)**
and **find compaction (#340)** (read-only, large-search-results / large-log
territory — exactly the ROADMAP acceptance signal). `read` skim (#337) is
*opt-in* (`skim:true` param, `src/tools/read.rs:84`), a model choice not a
default, so it is identical across arms and NOT part of the arm toggle. The
**bash filter (ADR-0037)** cannot be exercised end-to-end under auto (auto-bash
deferred); its proof stays at the per-result corpus level. This is an honest
scoping consequence of the approval constraint, to be stated plainly in the
plan + report. The Milestone-2 acceptance signal is satisfied by the
search/log workflows.

**DECISION — arm toggle scope.** The benchmark-only env switch disables the
default-on reductions: bash filter + grep shaping + find compaction. Bash is
included for completeness/arm-B cleanliness even though the auto workloads do
not trip it. Read skim is excluded (opt-in, not default). Default (unset) state
must be proven unchanged by existing tests + a new default-on test.

**OBSERVATION — token sources (two, honestly separated).**
- **Replay path (fake provider, CI):** FakeProvider records `seen` (the messages
  sent each turn). Prompt tokens = token estimate over `seen`. Differs between
  arms because tool OUTPUT size differs (grep/find shaping on vs off). Estimate
  only; asserts arm A < arm B deterministically. No cost.
- **Headline path (real provider, N>=3):** attach an observer capturing
  `AgentEvent::ProviderTurnCompleted.usage` (`ProviderUsage.input_tokens`) per
  turn = REAL provider usage records. Sum across turns = prompt tokens per run.

**OBSERVATION — driver seams.**
- `run_print_turn(harness, prompt, obs, gate)` (`src/cli.rs:777`) drives one
  headless turn-sequence (current-thread runtime + Ctrl-C watcher). Reusable.
- `build_provider(selection, system_prompt, session_id)` (`src/main.rs:681`)
  builds the real provider; `ModelSelection::resolve(&settings)`.
- `nexus_tests.rs` is wired as `#[cfg(test)] #[path="nexus_tests.rs"] mod`
  inside `nexus.rs` (L2726), so a sibling test module has crate-private access
  to `Agent`, `Harness`, `ApprovalMode`, `ProviderUsage`, the gate traits.
- `Agent::set_approval_mode(ApprovalMode::Auto)` (`src/nexus.rs:1040`) installs
  the preset; the test `run_preset_turn` + `FakeGuard` + `RecordingFrontend`
  scaffolding (`src/nexus_tests.rs` ~L6570-6673) already drive auto-mode turns
  with an injected gate + dirty guard. Reuse this shape for the replay harness.

**DECISION — harness placement (no new product surface).** Put the end-to-end
driver in a test-scoped module (like the existing corpora + nexus_tests), NOT a
new CLI subcommand (out-of-scope: no user features). Replay path = ordinary
`#[test]`s in CI. Real-provider headline path = an `#[ignore]`d test that runs
only when the operator opts in (env var + credentials), printing the usage
table with `-- --nocapture`. Auto mode + zero-prompt recording gate are
identical across arms and both providers; floors stay active.

**QUESTION (for oracle):** Is the in-process test-module driver the right home,
or should the real path reuse `iris -p` + RUST_LOG usage-trace parsing? Pressure
-test before building.

---

## Entry 2 — Oracle design review (verdict + the env-race correction)

**RESULT — oracle validated the direction** (overall 7.5/10, "good design with
two fix-before-build issues"):
- (A) auto+no-bash is the CORRECT reading; do NOT switch the headline to
  `--approve`+bash — that would measure a bypass mode, not the ADR-0032 auto
  preset. `--approve` bash could be a *separate, non-headline* exploratory run
  later, never mixed into the Milestone proof.
- (B) two-token-source split is honest IF labeled: replay = estimated message
  proxy (never call it exact tokens; assert A < B with a MARGIN, not a
  one-token inequality); headline = real `input_tokens`, and the headline path
  must FAIL if usage is missing, never silently fall back.
- (C) the `#[ignore]`d in-process test is the better home; `iris -p` + trace
  parsing is brittle and adds product surface.
- (D) not over-engineering; smallest shape = `Workload`, `Arm`, `RunMetrics`,
  one shared driver, replay provider, real-provider ignored test,
  harness-side success checks. No new CLI subcommand / framework / DB.
- (E) scripted replay is defensible IF worded honestly: it proves the tool-loop
  + transcript + success mechanics + token delta under a fixed SUCCESSFUL
  script; the tool OUTPUTS are real (real tools over committed fixtures). For
  the planted-fact workload, derive the answer from the actual tool result /
  a captured successful run, do not hardcode an answer from thin air.

**DECISION (fix-before-build #1) — arm toggle must NOT be a process-global env
var that tests mutate.** `std::env::set_var` is process-global and (Rust 2024)
unsafe under concurrent getenv; cargo runs `#[test]`s on parallel threads, so
two replay arms toggling a global env var race. Correction:
- The env var (`IRIS_BENCH_DISABLE_REDUCTIONS`, name TBD) remains the OPERATOR
  switch for real-provider runs — each `iris` invocation is its own process, no
  race.
- The reduction seam reads an explicit per-run flag carried in the tool
  execution context (ToolState/ToolEnv), NOT a global. Production seeds that
  flag from the env var ONCE at ToolState construction. The replay harness
  constructs TWO ToolStates (arm A reductions-on, arm B reductions-off), runs
  both, compares — zero env mutation, race-free, deterministic.
- Default (env unset) => reductions enabled => existing behavior unchanged
  (proven by a new default-on test + the fact that existing tool tests build a
  default ToolState and must stay green).

**DECISION (fix-before-build #2) — claim scope discipline.** State explicitly the
benchmark proves default *search-result* reductions (grep #338, find #340). It
does NOT cover ADR-0037 bash filtering (auto-bash deferred), read skim (opt-in),
or handles. Apply the same opt-in logic to grep `maxPerFile` (opt-in cap): the
arm toggle covers only default-on shaping, and the report says so.

**QUESTION:** Does the reduction seam (bash `filter_for_display`, grep/find
render) have access to `ToolEnv`/`ToolState` so the arm flag can be threaded
without an invasive refactor? Investigating next.

---

## Entry 4 — Toggle implemented (mechanism + one honest misstep)

**RESULT — toggle in place, default arm unchanged.** Implemented
`ToolState.reduce_output` (default true via `output_reductions_enabled_from_env`
reading `IRIS_BENCH_DISABLE_REDUCTIONS`), builder `with_reduce_output`, and the
pure `reductions_enabled_for(Option<&str>)` for race-free unit testing. Threaded
the flag: `registry::reduce_output(env)` -> `run_off_thread(reduce, body)` with
`grep::execute`/`find::execute`/`ls::execute` taking `reduce`; `BashTool` folds
it into the existing `raw` bypass (`let raw = raw || !reduce_output;`, one line,
no second code path). grep: `render_content` gained `group: bool` (flat =
`path:line:content`, no per-file header/markers). find: `render_compact` forces
`use_grouped=false` when `group` is false.
- `cargo check --bin iris`: clean. `cargo test tools::`: 308 pass.
- New `output_reductions_default_active_and_switch_is_explicit_truthy` passes;
  corpus tests (`corpus_grouping_is_parity`, `bench_concentrated_grouping`,
  `grouping_not_used_when_flat`) still green => default behavior byte-stable.

**DEVIATION / self-correction — blind scripting over-reached.** To update ~40
old-arity test call sites I ran a balanced-paren Python inserter for the call
heads `find(`, `grep(`, `execute(`, `render_results(`, `render_compact(`. The
`find(` head's `(?<![A-Za-z0-9_])` boundary also matched `.find(` METHOD calls
(`String::find`, `Iterator::find`), inserting a spurious `, true` into 4 method
calls, and it mangled 2 multiline `execute(` calls (`}),\n , true)`).
Compiler caught all of them (E0061 + parse errors). Fixed each by hand and
re-verified. Lesson logged: a call-head regex must exclude method-call
receivers (`.`), and machine edits to dense test code must be gated on a
re-compile before trusting them. The mechanical time saved was partly repaid in
the fix; net still positive but the boundary bug is the kind the notebook exists
to record.

**NEXT:** add arm-B *behavior* tests (reduce=false actually yields flat grep /
flat find / raw bash), then build the 3 fixtures + the replay harness.

---

## Entry 5 — Harness + fixture design (locked before building)

**OBSERVATION — driver seam confirmed.** `Agent::submit_turn(prompt, obs, gate,
env, cancel, steer)` (`src/nexus.rs:1137`) is driven directly with a
caller-built `ToolEnv` in the auto-mode tests (`run_preset_turn`,
`src/nexus_tests.rs:6626`). The replay harness copies that shape: build
`ToolEnv { workspace, state: &RefCell<ToolState::new().with_reduce_output(arm)>,
output_store: None, ... }`, `agent.set_approval_mode(Auto)`, and drive
`submit_turn` per arm. `output_store: None` keeps every tool output inline (no
handle offload) so the arm delta is the pure grep/find reduction, not offload.

**OBSERVATION — `edit` requires read-before-mutate.** `edit` ->
`observed.ensure_fresh` rejects an unread file (`stale-file`/`unread`,
`src/tools/observe.rs`). So every scripted `edit` is preceded by a scripted
`read` of that file — which is what a real agent does anyway.

**DECISION — token proxy (replay).** Sum of `bench_support::est_tokens` over the
messages the `ScriptedProvider` is sent each turn (`seen`), across all turns — a
honest cumulative-input proxy (each turn re-sends the growing transcript, as a
real provider bills it). Same estimator both arms; only the ratio is claimed;
never presented as exact tokens. Arm A < arm B by a margin is the assertion.

**DECISION — three fixtures (committed, mechanical checks, agent never runs
bash).** Stored under `src/bench_fixtures/tokens_per_task/<workload>/` with a
`.txt` suffix on every file (so fmt/clippy/typos never touch them); the harness
materializes them to a temp workspace stripping `.txt`.
1. **workload1_fix_test** — 4-file mini Rust lib; `parser.rs` has an off-by-one
   in `parse_len` (`count() - 1`); a `#[test]` in `lib.rs` fails; `parse_len`
   is referenced across all 4 files (multi-file grep lever). Script: grep
   `parse_len` -> read `parser.rs` -> edit (remove `- 1`) -> answer. Success:
   harness runs `rustc --test lib.rs` -> exit 0 (test goes green).
2. **workload2_rename** — 5 files in nested dirs each using `MAX_RETRIES`
   (large multi-file grep). Script: grep `MAX_RETRIES` -> per file read+edit
   (replace_all -> MAX_ATTEMPTS) -> answer. Success: harness asserts no file
   contains `MAX_RETRIES` and each source contains `MAX_ATTEMPTS` (expected diff
   applied).
3. **workload3_log_triage** — `logs/` with 4 realistic cargo-test-failure
   shards (built from the real captured `bash/filter/corpus/cargo-test-fail.txt`
   content) with `assertion` noise across all shards and ONE planted fact in
   shard-03 (`ceiling_is_exact`, left 8192 / right 8191). Script: grep
   `assertion` across `logs/` (multi-file grep lever) -> read shard-03 ->
   answer quoting the planted line. Success: final answer contains the planted
   numbers `8192`/`8191` (derived from the committed line, not invented).

**DECISION — harness placement + gating.** Module `src/bench_tokens_per_task.rs`
included via `#[cfg(test)] #[path] mod` in `nexus.rs`, sibling to the
`nexus_tests` include, for crate-private access. Its own tiny `ScriptedProvider`
/ `ZeroPromptGate` / `BenchObserver` (nexus_tests' doubles are module-private).
Replay tests are ordinary `#[test]`s (CI, fast; `rustc --test` is ~0.5s). The
real-provider headline is an `#[ignore]`d test gated on `IRIS_BENCH_REAL=1` +
credentials, printing the usage table with `--nocapture`. Zero-prompt gate +
auto mode identical across arms and both providers.

---

## Entry 6 — Replay path is GREEN (results + an honest caveat)

**RESULT — all 4 replay tests pass; the whole tools suite stays green.** The
harness materializes each fixture, drives `submit_turn` under auto + zero-prompt
gate for both arms, and asserts success + arm A < arm B + zero prompts.
`tokens_per_task_replay_report` (est-token proxy, 4 bytes/token; ratios only):

| workload | turns | arm B proxy | arm A proxy | reduction | both succeed |
|---|---|---|---|---|---|
| fix-failing-test | 4 | 2417 | 2336 | 3.4% | yes |
| multi-file-search-and-edit | 12 | 14730 | 13531 | 8.1% | yes |
| investigate-large-log | 3 | 1296 | 1178 | 9.1% | yes |

DoD item 4 is satisfied: re-runnable in CI-compatible time (< 0.5s), both arms,
success asserted, arm A < arm B by a margin, zero approval prompts.

**OBSERVATION — the reductions are modest, and that is honest.** 3.4-9.1% is a
grep/find *grouping*-only lever (the #338 grep-grouping lever is 3-27%; find
grouping helps most with many files in shared dirs, which these small fixtures
under-exercise). The larger levers (bash filter 68-89%, read skim 52-72%) are
out of scope end-to-end here (auto-bash deferred; skim opt-in). The report will
state the magnitude plainly; no rounding up.

**CAVEAT (must be loud in the report + ROADMAP) — what replay does NOT prove.**
The replay SCRIPTS the tool-call sequence, so both arms apply the identical
fix/answer and both succeed by construction. Replay therefore proves the token
PLUMBING (reduced tool outputs => smaller transcript => fewer input tokens) and
the success MECHANICS, but it does NOT prove the Milestone-2 acceptance signal's
hard part: that a real model still COMPLETES the task when it must reason from
the reduced context. That requires the real-provider run. So the ROADMAP gate is
NOT marked satisfied on replay alone, and the README claim does NOT ship on
replay alone.

**DECISION — real-provider cells need explicit spend authorization.** Auth exists
but: (1) real runs cost money (18+ multi-turn sessions); (2) GPT-5-Codex may
reach for `bash` (`cargo test`), which under auto = a prompt = invalid run,
likely needing prompt iteration; (3) spending the user's money is a
human-authorize action. Plan: wire the `#[ignore]`d real harness + exact repro +
cost estimate, ship the PR with the replay evidence, and surface the real-run
go/no-go to the operator rather than spending silently. If run and arm A wins
with no success regression, the README claim ships and the ROADMAP gate flips;
otherwise the honest finding ships and the claim does not.

---

## Entry 7 — Docs + gate + README decision

**RESULT — clippy + fmt clean** after gating `ToolState::with_reduce_output`
behind `#[cfg(test)]` (clippy `-D warnings` flagged it as dead code in the
non-test `--bin iris` build; it is only ever called by the harness + toggle
test). Real-provider harness (`run_real_arm` + `tokens_per_task_headline`,
`#[ignore]`d, `IRIS_BENCH_REAL=1`-gated) compiles under `--all-targets`.

**DECISION — report + ROADMAP + README.**
- `docs/benchmarks/tokens-per-task.md` committed: method, the replay table with
  the real measured numbers, repro commands, and a loud "what replay does /
  does not prove" + "headline pending operator run" section.
- ROADMAP "End-to-end measurement pending" note rewritten to cite the plan +
  report, credit the replay evidence, and keep the gate OPEN until the
  real-provider run lands. Not marked satisfied.
- README left UNCHANGED. It already says the end-to-end proof "follows #261" and
  makes no tokens-per-task claim; the honesty rule says the claim ships only
  when real numbers support it. They do not yet (no real run; replay lever is a
  modest 3.4-9.1%). The PR body states this explicitly.

**NEXT:** run `scripts/gate.sh` green; open the PR referencing #210 (no merge);
surface the real-provider spend go/no-go to the operator.

---

## Entry 8 — Independent review: two fixes applied

Ran the reviewer over the toggle + harness diff. Two medium findings, both
legitimate, both fixed:

**FIX 1 (reviewer) — removed the production env read (leak risk).** The reviewer
flagged that `ToolState::new()` reading `IRIS_BENCH_DISABLE_REDUCTIONS` lets an
ambient env var silently disable reductions in a NORMAL session — the
"benchmark-only, must not leak into normal runs" contract broken. Correct.
DECISION: drop the env var entirely. `ToolState::new()` is now pure
`reduce_output: true`; the baseline arm is reachable ONLY via the test-only
`with_reduce_output`. This supersedes Entry 3's env-seed design and the oracle's
Entry 2 env-race worry in one move — an in-process test-only flag cannot leak
OR race. The headline harness already runs in-process (uses `with_reduce_output`,
not a standalone `iris` process), so nothing needed the env var. Deviation from
the pinned "env var preferred" is documented in BENCHMARK_PLAN + report; the
binding requirement (arm-LEVEL, not per-call) is still met. Docs reconciled.

**FIX 2 (reviewer) — tied success to output fidelity (no vacuous pass).** The
reviewer noted the log-triage success check only inspected the SCRIPTED final
answer (hardcoded 8192/8191), so it could pass even if the reduced tool output
dropped the fact. Correct — that would let arm A "succeed" without proving the
reduction preserved the actionable content. FIX: added per-workload survival
`needles` and asserted they appear verbatim in the transcript the agent actually
saw, in BOTH arms (wl1: `parse_len`, `split_whitespace().count() - 1`; wl2:
`MAX_RETRIES`; wl3: `ceiling_is_exact`, `8192`, `8191`). This is the ADR-0036
rule-5 "verbatim survival" contract applied end-to-end — success is now tied to
the reduced output actually carrying the facts, not to a scripted answer.

All 4 replay tests + the toggle test stay green after both fixes.

---

## Entry 9 — Discussion, further-research, and an honest README update

**DECISION — wrote Discussion + Further-research sections** into
`tokens-per-task.md`. Discussion states the boundary plainly: the surviving
lever (grep/find grouping) is small on this deliberately-small corpus, the big
levers (bash filter, skim) are structurally out of the auto/zero-prompt
experiment, and replay is a plumbing proof not a capability proof. Further-
research is ordered by Milestone-2 leverage: (1) run the real headline (the
actual gate), (2) grow fixtures toward grouping's strength, (3) add a
bash-bearing arm under a non-auto preset, (4) swap the byte proxy for a real
tokenizer, (5) report turn-count variance not just mean tokens, (6) an
adversarial fixture that stresses success-rate regression.

**DECISION — README updated, honestly, no headline claim.** Changed three
spots: the Status clause, the Next bullet, and the Token-efficiency section's
end-to-end paragraph. All three now say the SAME true thing: the benchmark
(plan + replay harness + report) landed, replay shows the default arm winning
3.4-9.1% with equal success and zero prompts and verbatim needle survival, BUT
the real-provider confirmation is pending so the Milestone-2 gate stays open and
no headline efficiency number is claimed. This is a status update, not the
claim the DoD gates on real runs — the honesty line holds: replay evidence is
reportable, a proven tokens-per-completed-task win is not, and I did not write
one.

**NEXT:** gate green; update PR #391; still awaiting operator go/no-go on the
real-provider spend.

---

## Entry 10 — "Why only Codex, not Claude?" (good catch)

**QUESTION (reviewer):** why is the headline framed around OpenAI Codex and not
Anthropic/Claude?

**OBSERVATION — ground truth from code, not memory.** The real harness is
provider-AGNOSTIC: `run_real_arm` builds via `build_provider(&selection, ..)`
where `selection = ModelSelection::resolve(&Settings::load())`, so the cell runs
whichever provider the config selects. Anthropic is fully wired
(`ProviderId::Anthropic`, default `claude-sonnet-4-6`, its own `record_usage` in
`anthropic_messages.rs`, `api.anthropic.com`). Provider precedence:
`settings.default_provider` -> env fallback (`OPENAI_API_KEY` ->
`ANTHROPIC_API_KEY` -> ...) -> `ProviderId::DEFAULT` (= Codex). `IRIS_MODEL`
sets only the model string, NOT the provider.

**Why my docs read "Codex":** (1) Codex is `ProviderId::DEFAULT`; (2) local
`~/.iris/auth.json` has only `antigravity` + `openai-codex` (no `anthropic`), so
a Claude run needs auth added; (3) a STALE note in the primary AGENTS.md ("Iris's
only provider today is OpenAI Codex Responses") that the code contradicts. None
is a design limit.

**DEVIATION — corrected the over-narrowing.** Fixed the one report line that
said "the OpenAI Codex provider" (implying hardcoded) to state the harness is
provider-agnostic; added an accurate Claude repro (select Anthropic via
`default_provider`/`ANTHROPIC_API_KEY`, not a fabricated `IRIS_MODEL=provider:model`
syntax I first wrote and then verified against `selection.rs` was wrong); and
upgraded Further-research point 1 to require Codex AND Claude cells, since
reduction interacts with each model's tokenizer + compact-context tolerance.
Flagged the stale AGENTS.md note to the operator rather than editing it (out of
this PR's scope).

**NEXT:** gate green; push to PR #391.

---

## Entry 11 — First live smoke (real providers, low reasoning, N=1)

Ran `IRIS_BENCH_REAL=1 cargo test --bin iris tokens_per_task_smoke -- --ignored
--nocapture`. 8 real sessions (4 models x 1 read-only workload x 2 arms x N=1),
reasoning=low. 56.9s, rc=0, subscription/OAuth lanes so ~$0 marginal.

| model | arm | reachable | success | turns | usage in-tokens |
|---|---|---|---|---|---|
| openai-codex:gpt-5.4-mini | B | yes | false | 1 | 4216 |
| openai-codex:gpt-5.4-mini | A | yes | true | 4 | 18785 |
| openai-codex:gpt-5.3-codex-spark | B | yes | true | 4 | 18069 |
| openai-codex:gpt-5.3-codex-spark | A | yes | true | 4 | 17971 |
| anthropic:claude-haiku-4-5 | B | yes | true | 3 | 18299 |
| anthropic:claude-haiku-4-5 | A | yes | true | 3 | 19357 |
| antigravity:gemini-3.5-flash | B | yes | true | 4 | 0 |
| antigravity:gemini-3.5-flash | A | yes | true | 4 | 0 |

**RESULT — reachability + design both validated/challenged.**
- All 4 models reachable on EXISTING OAuth; zero approval prompts across all 8
  runs (auto/no-bash design holds live). Both Codex-lane models served by the
  openai-codex OAuth (operator was right; my api-lane doubt was wrong). Haiku
  4.5 ACCEPTS a `low` thinking level (operator's uncertainty resolved).

**BLOCKER — Antigravity/Gemini reports 0 usage tokens.** `antigravity.rs:463`
hardcodes `usage: None`; the provider never parses Gemini `usageMetadata`
(promptTokenCount/...). Gemini cannot be a tokens-per-task headline cell until
the Antigravity provider records usage. Real code gap, not noise. Options: drop
Gemini, or implement Antigravity usage first (separate task).

**DEVIATION — N=1 token deltas are dominated by variance, not the reduction
lever.** Live, each arm is a separate non-deterministic session, so turn-count +
success swamp a single-digit reduction: mini B=4216 only because it FAILED after
1 turn; haiku arm A (reduced) used MORE than arm B; spark ~tied. The replay's
clean 3-9% arm-A win came from SCRIPTING identical tool calls -- live that
control is gone, and the ~4x run-to-run spread here is far above a 5-9% effect.
Implication: **N=3 will not cleanly detect the effect.** The honest headline
may be "no significant per-task token difference on these small models," which
is itself a legitimate Milestone-2 result and consistent with the modest replay
numbers. Do NOT spend on the 3-workload x N=3 matrix as designed; take the
finding back to the operator for a design decision (raise N substantially +
treat turns/success as covariates; or reframe the workload/model to one where
the effect exceeds noise; or report the null honestly).

**NEXT:** report smoke findings + get operator direction on (a) Gemini usage,
(b) the variance/N/design question, before any further spend.

**DECISION (operator) — exclude Gemini for now.** Dropped
`antigravity:gemini-3.5-flash` from `DEFAULT_MODEL_SPECS`; the matrix is now the
three usage-reporting models (gpt-5.4-mini, gpt-5.3-codex-spark,
claude-haiku-4-5). Gemini stays re-addable via `IRIS_BENCH_MODELS` or by
restoring the const once the Antigravity adapter records `usageMetadata`. The
N=1 variance finding stands and is still open for the headline design.

---

## Entry 12 — First full headline run (3 models x 3 workloads x 2 arms x N=3)

Operator authorized ONE full run to surface design issues. Ran
`IRIS_BENCH_REAL=1 IRIS_BENCH_N=3 cargo test --bin iris tokens_per_task_headline
-- --ignored --nocapture`. 54 real sessions, 940.6s (~15.7 min), rc=0,
reasoning=low, subscription/OAuth lanes (~$0 marginal). Per-cell aggregation
(successful runs only for tokens; success rate separate):

| model | workload | B succ | A succ | B tok(ok) | A tok(ok) | A vs B | note |
|---|---|---|---|---|---|---|---|
| gpt-5.4-mini | fix-failing-test | 3/3 | 3/3 | 22141 | 20593 | +7.0% | A wins |
| gpt-5.4-mini | multi-file-edit | 3/3 | 3/3 | 32027 | 31563 | +1.4% | ~tie |
| gpt-5.4-mini | investigate-log | 1/3 | 2/3 | 18360 | 23092 | -25.8% | success rates differ; both had 1-turn early-quit fails |
| gpt-5.3-codex-spark | fix-failing-test | 3/3 | 3/3 | 35039 | 38381 | -9.5% | A loses (turn inflation 6-9) |
| gpt-5.3-codex-spark | multi-file-edit | 3/3 | 3/3 | 83756 | 82455 | +1.6% | ~tie; HUGE variance (turns 6-21, 40k-149k) |
| gpt-5.3-codex-spark | investigate-log | 3/3 | 3/3 | 19041 | 15404 | +19.1% | A wins, low variance |
| claude-haiku-4-5 | fix-failing-test | 3/3 | 3/3 | 25654 | 32218 | -25.6% | A loses (turn inflation 4->6) |
| claude-haiku-4-5 | multi-file-edit | 0/3 | 2/3 | - | 72306 | n/a | BASELINE failed all 3; A succeeded 2/3 |
| claude-haiku-4-5 | investigate-log | 3/3 | 3/3 | 16902 | 21580 | -27.7% | A loses (turn inflation) |

**RESULT — the design issues we came to find, all real:**

1. **POSITIVE: zero approval prompts across all 54 runs, including both EDIT
   workloads.** The auto/no-bash design holds live even when the agent edits
   files -- clean in-workspace edits auto-approve, no model reached for bash.
   The approval half of the design is sound.

2. **No consistent tokens-per-task reduction.** Arm A (reduced) wins ~4 cells
   and loses ~4. The SAME workload flips direction across models
   (investigate-log: spark +19%, but mini -26% and haiku -28%). So the effect
   is not a property of the workflow -- it is dominated by model/run behavior.

3. **Token use is driven by TURN COUNT, which the model chooses
   non-deterministically -- not by per-turn output size.** The A-loses cells are
   turn inflation (haiku fix-test A took 4/5/6 turns vs steady 4; spark fix-test
   A 6/9/7). One extra agentic turn swamps the small per-turn saving the
   reduction lever produces. The lever is real per-RESULT (proven separately);
   at whole-task scale it is second-order.

4. **Variance dwarfs the ~5-9% effect.** spark multi-file baseline spanned
   40k-149k tokens (turns 6-21) across 3 runs. N=3 cannot resolve a single-digit
   effect against that; the N needed is impractical.

5. **Success is not 100% and not arm-clean, breaking the metric.** mini
   investigate-log B=1/3 vs A=2/3; haiku multi-file B=0/3 vs A=2/3. Comparing
   mean tokens across arms with different success rates is apples-to-oranges,
   and averaging failed 1-turn early-quits (~4216 tok) into the mean skews it
   (fixed here by ok-only aggregation, but the underlying confound remains).
   Notably in haiku multi-file the REDUCED arm succeeded MORE (2/3 vs 0/3).

**HONEST CONCLUSION:** as designed, the end-to-end benchmark does NOT demonstrate
a tokens-per-completed-task win on these small models -- and likely no robust
whole-task win exists here, because task token use is governed by noisy turn
counts, not the per-result reduction lever. This is a legitimate Milestone-2
finding: per-result reductions are proven and real, but they do not translate
into a measurable whole-task token reduction on this matrix. => README claim
stays unshipped, ROADMAP gate stays OPEN. Honesty-first outcome, as committed.

**DESIGN FIXES the run implies (for operator decision):**
- Metric: count tokens only over SUCCESSFUL runs and report success rate
  separately; gate token comparison on comparable success. The harness should
  aggregate this, not dump raw rows.
- Either accept + report the null (my lean: it is the honest answer and the
  per-result benchmarks already carry the efficiency evidence), OR pivot the
  metric to tokens-per-TURN / a huge-single-output workload where the lever
  dominates (drifts from "realistic task"), OR big-N a single favorable cell
  (spark/investigate-log +19%) -- but that is cherry-picking unless framed as
  "at least one workflow," and even that workflow flips negative on other models.

**NEXT:** report to operator; do not spend further or change design without a
decision on metric + accept-null-vs-pivot.

---

## Entry 13 — Rich instrumentation + Sonnet 4.6 (the interpretation-cost test)

Operator raised the key hypothesis: the stripped-down (arm A) tool output may be
HARDER to interpret, making the model take MORE turns / tool calls -- so the
reduction is not free. Instrumented the harness to log everything as JSONL
(per-turn input/output tokens, per-tool call histogram, handle offloads,
tokens-per-turn vs turn count) and ran Sonnet 4.6 low, N=3, 3 workloads x 2 arms
(18 sessions, 400.8s, 18/18 success).

**H1 (capability -> steadier approach): confirmed on pure tasks, not a clean
law.** investigate-large-log arm A: Sonnet input tokens 19733/19738/19739 (CV
~0%), turns 3/3/3; small models CV 14-51% on the same cell. BUT gpt-5.4-mini was
CV 0% on multi-file (5/5/5) -- a small model can be consistently mediocre. Honest
read: Sonnet is both more successful AND far steadier on the reasoning-only
tasks, which is what finally makes the effect measurable; "capability lowers
variance" holds on the pure cells, not universally.

**H2 (reduction can COST turns/tool-calls): CONFIRMED on multi-file edit.**
Per-cell A-vs-B (ok-only, N=3), Sonnet:

| workload | dturns | dtool-calls | tok/turn A vs B | TOTAL input A vs B |
|---|---|---|---|---|
| fix-failing-test | -0.3 | +0.0 | +2.4% | +9.9% (A cheaper) |
| investigate-large-log | +0.0 | +0.0 | +2.3% | +2.3% (A cheaper) |
| multi-file-search-and-edit | +1.0 | +2.3 | -5.2% | **-20.3% (A COSTLIER)** |

On multi-file the "token-efficient" arm A used **20% MORE** input tokens: it took
+1 turn and +2.3 tool calls. Tool-mix breakdown pins it: grep=2.0 and read=5.0 in
BOTH arms, but edits went B=5.3 -> A=7.7 (+2.4 EDIT calls). So the compact
grep/find output did not cause re-searching -- it changed the model's EDITING
strategy (more, smaller edits), and the accumulating transcript from those extra
calls swamped the per-call reduction (A's tok/turn was actually 5% HIGHER). This
is exactly the hidden cost the operator predicted: on a multi-file edit flow the
reduction is net-negative, contradicting a blanket "efficient" claim.

**The clean wins are real but small.** investigate-large-log is the purest demo:
identical turns + identical tool mix in both arms, arm A just carries a smaller
per-turn context -> -2.3%, CV ~0%. fix-test -9.9% (mostly because arm B had one
5-turn run; arm A was steadier).

**SYNTHESIS:** even on a capable model the reduction is workload-dependent, not a
uniform win: -2.3% and -9.9% on the read/fix tasks, but **+20.3% on multi-file
edit**, driven by extra edit calls. => a blanket README token-efficiency claim is
NOT supported; the honest Milestone-2 result is "reduction helps on
search/read-heavy tasks and can hurt on multi-file edit flows."

**CAVEATS:** N=3; multi-file A magnitude is noisy (49k/75k/75k). The extra calls
are EDITs, so the mechanism is a strategy change, not "couldn't find it" -- to
explain WHY needs tool-ARGUMENT logging (not captured yet; args are deliberately
kept out of the lifecycle event). Good follow-up: log edit args + higher N on the
multi-file cell.

**NEXT:** report; propose (a) tool-arg logging + powered re-run of multi-file to
explain the edit-inflation, and (b) keep the claim unshipped / gate open.

---

## Entry 14 - Comprehensive suite design (oracle)

Operator asked to widen scope: PR #388 / ADR-0049 (`--dangerously-skip-
permissions`, `Agent::with_skip_permissions(bool)` + `ToolAutoApprovedDangerous`)
unlocks the bash tool, so we can now add real build+test end-to-end tasks AND
per-tool micro-tests that score model SUCCESS with the reduced output, not just
byte reduction. Consulted the oracle for a reusable/configurable harness design
(scored 4.25/5, conditioned on phasing + honest negatives).

Design of record: `docs/benchmarks/tool-efficiency-suite-design.md`. Key points:
data-driven `src/bench_tokens/*` modules (add a tool/workload = append a table
row); three comparison axes (reduce-toggle for grep/find/ls/bash; separate
read-skim and edit-result-class axes since the toggle does NOT cover read/edit);
real-usage vs 4-bytes proxy never mixed; skip-permissions workloads assert the
audit event + confinement; token-delta decomposed into per-turn saving vs
strategy change; Rust JSONL analyzer (repo is Rust-only). 7 phases, smallest-
correct-change first (merge #388 -> no-behavior refactor -> schema -> bash smoke
-> micro probes -> read/edit axes -> full matrix).

**NEXT:** operator sign-off on scope/phasing before the refactor + any live
spend; then Phase 1 (merge #388, prove harness+gate unchanged).

---

## Entry 3 — Toggle mechanism + reduction semantics + real-run feasibility

**OBSERVATION — the reduction seams and what "off" means.**
- `bash`: `filter_for_display(cmd, out, exit_ok, raw)` (`src/tools/bash/mod.rs`)
  already has a `raw` bypass. Arm B = force `raw`. Trivial. Bash `execute` gets
  `env`, so it can read the flag. (No agent-bash in workloads; wired for the
  toggle's completeness + a unit test.)
- `find`: `render_results` (`src/tools/find.rs:148`) already has a flat path
  (`if !needs_compact`) and grouping only happens inside `render_compact`
  (`use_grouped = grouped.shown > flat.shown || ...`). Arm B = force
  `use_grouped=false` (always flat), caps + omitted-summary preserved. Minimal.
- `grep`: grouping is baked into `render_content` (`src/tools/grep.rs:615`);
  `render_flat` exists only as a `#[cfg(test)]` benchmark baseline (no paging /
  notices / caps). Arm B needs a flat production render with the SAME paging /
  notices so the ONLY difference is grouping. DECISION: add `group: bool` to
  `render_content` — when false, prefix each line `path:sep:number:sep:content`
  and skip the per-file header + context markers; paging/caps/notices unchanged.

**DECISION — carrier: `ToolState.reduce_output: bool` (default true).** `ToolEnv`
carries `state: &RefCell<ToolState>` to every tool, so `ToolState` is the
race-free per-run carrier the oracle asked for.
- `ToolState::new()` keeps `reduce_output = true` (existing tests byte-identical;
  this is the "default => reductions active" contract).
- Add `ToolState::with_reduce_output(bool)` (builder) for the replay harness
  (arm B constructs `ToolState::new().with_reduce_output(false)`).
- Production entry points (`main.rs` run_print + interactive, `cli.rs` sites)
  seed it from the env var via a single helper `bench::reductions_enabled_env()`
  that reads `IRIS_BENCH_DISABLE_REDUCTIONS` with a getenv (never setenv). Tests
  NEVER set the env var, so getenv is race-safe.
- Registry threads the flag: `grep::execute`/`find::execute` gain a `reduce`
  param read from `env.state`; `run_off_thread` passes it; `ls::execute` ignores
  it (no ls reduction shipped). `BashTool::execute` OR-s it into `raw`.

**Default-unchanged proof:** new test asserts `ToolState::new().reduce_output`
is true and `reductions_enabled_env()` is true when the var is unset; the entire
existing tool + corpus test suite (which builds default `ToolState`) must stay
green. That is the "default config => filters active" contract.

**RESULT — real-provider feasibility.** `~/.iris/auth.json` has an `openai-codex`
entry (access/refresh/expires/type) — iris-agent's only provider (Codex
Responses) IS authenticated here, so real-provider runs are technically
possible. BUT: (1) real runs cost money (OpenAI Codex, reasoning model,
multi-turn, N=3 x 3 workloads x 2 arms = 18+ real sessions); (2) the OAuth token
may be expired; (3) spending money is an externally-visible, human-authorize
action per my operating guidelines.

**DECISION — sequencing to protect against wasted spend + keep honesty.** Build
and prove the ENTIRE deterministic stack first (toggle + fixtures + replay
harness + plan), which fully satisfies DoD 1-4 at zero cost. The real-provider
headline (DoD 5) is wired as an opt-in `#[ignore]`d harness with exact repro
commands + a cost estimate; actually spending money on >=3 cells is surfaced to
the operator for explicit go/no-go rather than run silently. If approved, run
the smallest workable set and record real usage-record numbers; if not, the
report ships with the replay evidence + a clearly-labeled "real-provider cells
pending operator run" section and the README claim does NOT ship. This is the
honest failure mode the issue demands, not a fabricated table.

## Entry 15 - Phase 5/5b: per-tool micro-probes + "log all results"

**Vertical slice (grep) then fan-out (find/bash), verified against the code, not
the design doc.** Each per-tool advantage now has two layers: a deterministic
RENDER probe (invoke the tool by name through the real dispatch, reduce on vs
off, assert a token-reduction bar AND that the exact fact the paired live
question asks for survives verbatim) and a live MODEL probe (a real model must
answer that exact question from the reduced output; scored mechanically; reuses
`run_real_cell` + the JSONL schema for behavior metrics).

**Measured render reductions (deterministic, proxy tokens, needles survive):**

| tool | probe | baseline B | reduced B | reduction | gate |
|---|---|---|---|---|---|
| grep | path grouping (issue-338) | 6421 | 4084 | 36.4% | fast (CI) |
| find | dir compaction (issue-340) | 40222 | 17798 | 55.7% | fast (CI) |
| bash | failing `cargo test` filter (ADR-0037) | 754 | 593 | 21.2% | slow (opt-in) |

**Key corrections to the design doc (code beats doc):**

- **`ls` is NOT arm-toggled.** `ls::execute` takes `_reduce` and ignores it
  (issue-339 not started), so reduce on/off is byte-identical -- ls has no A/B
  render probe and is deliberately absent from the probe table. Earlier drafts
  (and the compaction summary) called ls arm-toggled; that was wrong.
- **`find`'s advantage IS byte reduction, not just completeness.** Once matches
  exceed the 1000 default limit the listing compacts, and directory grouping
  renders the same shown paths in fewer bytes (dir prefix shared once). The
  probe builds a 1351-file tree at run time (too large to commit) via
  `fixtures::build_find_tree`; the target file is written last (newest mtime) in
  an alphabetically-first dir so find's mtime-desc sort keeps it in the shown
  prefix.
- **`bash`'s filter is command-specific.** A *failing* `cargo test` IS
  structured-filtered (chatter collapses, the failing test + `left/right`
  assertion values are preserved); pure-chatter cargo test passes through. The
  deterministic bash render corpus already lives in adr-0037, so the suite's
  bash render probe is opt-in (it compiles the fixture ~= a few seconds) and
  exists mainly to pair with the Phase 4 live bash workload on the same fixture.

**"Log all results" (operator directive).** JSONL schema bumped to v3: every
record now carries a `kind` discriminator -- `real_cell` (valid live run),
`real_cell_error` (unreachable/failed/select-rejected cell, `valid:false`, with
the backend message), and `render_probe` (deterministic per-tool measurement:
baseline/reduced bytes + proxy tokens + reduction% + needles_survived). All four
live tests (headline/smoke/bash-smoke/micro-probes) now log EVERY cell including
failures and unreachable models -- no silent drops. `tool_render_probe_log`
(opt-in) writes all render measurements to the same log so the Phase 7 analyzer
can correlate a tool's render reduction with its live outcome. The CI gate stays
side-effect-free (render probes only assert there).

**Honesty note.** No live model runs in this entry -- these are deterministic
render measurements and wiring. The live micro-probe/find/bash layers are wired
and opt-in; they cost provider calls and await operator go/no-go. No token or
success-rate claim is made from render bytes alone (proxy ratios, never mixed
with real usage records).

## Entry 16 - Backfill: Phases 3 + 4 (schema extend, skip-perms bash)

Retroactive entry (out of order): Phases 3 and 4 shipped in commits a5c4725 and
62d6238 with full commit messages + code doc comments, but were never given
notebook entries. Recording them here so the audit trail is complete.

**Phase 3 - JSONL schema v2 (commit a5c4725).** Extended `BenchObserver` to
capture, as metadata only (never raw tool output): `dangerous_approvals` (count
of `ToolAutoApprovedDangerous`), `tool_sequence` (ordered call names, every
attempt), `tool_errors` ((name, truncated message)), `tool_result_bytes`
(+by-tool -- the real-run analogue of the replay byte proxy), and
`bash_exit_codes`. Mirrored into `RealRunRecord` + the JSONL line; added
`schema_version`. A deterministic observer unit test drives synthetic
`ToolStarted/ToolResult/ToolError/ToolAutoApprovedDangerous` events and asserts
every field (no live provider). Edit outcome CLASSES (exact/tolerant/not-found/
not-unique/stale) were deliberately deferred to Phase 6's edit axis -- parsing
edit result content is coupled to edit's output format and belongs with that
probe.

**Phase 4 - skip-permissions bash (commit 62d6238).** Added
`ApprovalProfile {DenyGateNoPrompts, SkipPermissions}` on `Workload`; the three
existing workloads stay deny-gate. `run_real_cell` calls
`Agent::with_skip_permissions(true)` only for skip-perms workloads (ADR-0049) and
asserts the materialized workspace is a temp dir, never the repo tree
(confinement). Read-only diagnosis workload first (no mutation): a fixture crate
whose `ceiling_is_exact` test fails (`left: 8191, right: 8192`); the model runs
the tests and reports the values.

The gate-safe proof is `bash_wiring_skip_permissions` (deterministic, no
provider, ~0.4s): it scripts a bash call under skip-perms and proves, in BOTH
arms, that the deny gate is NEVER consulted, `ToolAutoApprovedDangerous` fires,
the non-zero exit code (3) is captured, and result bytes enter context -- the
CI-safe evidence that ADR-0049 genuinely unlocks bash in the harness. The live
`tokens_per_task_bash_smoke` (double-gated `IRIS_BENCH_REAL=1` +
`IRIS_BENCH_DANGEROUS_OK=1`) runs the real model over the matrix and asserts the
deny gate is never consulted. Later finding (Entry 15): the failing `cargo test`
this workload runs IS structured-filtered, so it also exercises the bash filter,
not just execution.

## Entry 17 - Phase 6: read-skim + edit-result-class (the always-on axes)

`read` and `edit` are NOT arm-toggled by `reduce_output`; each got its own probe
shape rather than being forced into the reduce template.

**read skim (issue-337) -- a second render-probe axis.** Generalized `ToolProbe`
with `ProbeAxis`: `ReduceToggle` (grep/find/bash, reduce off vs on) and a new
`ArgOverlay` (both arms at default reduce_output; the reduced arm merges a JSON
arg). read uses `ArgOverlay(skim:true)`: baseline `skim:false` vs reduced
`skim:true` on the SAME comment-heavy source. Measured **83.1%** (4306 -> 725 B)
with all four needles (`CHECKOUT_DEADLINE_MS`, `47231`, `settlement_id`,
`PendingCharge`) surviving verbatim -- skim strips whole-line comments + blank
lines but keeps every code signature. The fixture is deliberately
comment-dominated because read's never-worse guard falls back to a full read
(and 0% reduction) on a thin file, which would fail the bar. A live
`probe-read-skim-constant` workload asks a real model the exact constant from a
skimmed read (opt-in, reuses `run_real_cell`).

**edit result-class (issue-341) -- NOT a reduction ratio.** edit's advantage is
that it distinguishes five outcome classes and keeps the common case cheap, so
the probe asserts the CLASS + the on-disk effect, not bytes. `run_edit_case`
drives the real read+edit dispatch on ONE `ToolState` (so read-before-mutate
carries across the two calls), builds a tiny file, optionally pre-reads /
mutates out-of-band, runs one edit, and reports the class + whether disk
changed. All five hold deterministically:

| case | class token | ok | disk changed | trigger |
|---|---|---|---|---|
| exact | `exact` | yes | yes | unique byte match |
| tolerant | `tolerant-match-fired` | yes | yes | curly quotes in file vs ASCII in old_string; fuzzy folds Unicode |
| not-found | `not-found` | no | no | old_string absent |
| not-unique | `not-unique` | no | no | `= v;` twice, no replace_all |
| stale | `stale-file` | no | no | edit with no prior read (read-before-mutate) |

The class comes from the SUCCESS metadata `edit_outcome` and, on failure, from
`ClassifiedError::class()` (a new `#[cfg(test)]` read accessor on the existing
crate-internal error type -- the stable ADR-0040 token, so the probe never
matches on error prose). The probe also proves an exact success output stays
STRICTLY SHORTER than a tolerant success (the ADR-0038 conditional echo fires
only on a tolerant match). edit's LIVE behavior metrics need no new workload --
workload1 (fix-failing-test) and workload2 (multi-file rename) already drive
real edits, so tool_counts/outcomes ride the existing schema.

**Honesty note.** These are deterministic render + class measurements, not live
model runs. The read live workload is wired and opt-in. `restrictions_enabled()`
is `cfg!(test) || env`, so read-before-mutate and path confinement are genuinely
active in the probe -- the stale-file class is a real rejection, not a stub. No
token or success claim is made from these bytes (proxy ratios only, never mixed
with real usage records).

## Entry 18 - Live validation (Sonnet 4.6, low): the tool-choice gap

Ran two opt-in live sets on `anthropic:claude-sonnet-4-6`, reasoning `low`, N=1
(8 real sessions total). All cells succeeded (8/8), no approval prompts, real
usage records. N=1 -> descriptive only, no claim, ROADMAP gate stays open.

**Smoke (log-triage, grep+read):** B 23076 -> A 22505 input tok (-2.5%),
identical 3 turns / 2 tools; tool_result_bytes 5451 -> 4283 (-21%). Reduction
showed up in context, cost no turn, hurt no success. One data point.

**Micro-probes (per-tool):**

| probe | arm | in tok | turns | tool seq | result B |
|---|---|---|---|---|---|
| grep | B | 15031 | 2 | grep | 4354 |
| grep | A | 14493 | 2 | grep | 2588 |
| find | B | 13583 | 2 | find | 51 |
| find | A | 13583 | 2 | find | 51 |
| read | B | 21029 | 3 | grep,grep | 1025 |
| read | A | 14002 | 2 | find,grep | 903 |

**The key finding: a render-probe reduction is necessary but NOT sufficient --
the model must actually invoke the tool in the reduction-triggering way, and at
low effort Sonnet often does not.**

- **grep** behaved as designed: one grep both arms, A's tool output 40.6% smaller
  (4354 -> 2588 B), A input 3.6% lower, both correct. The render probe's 36.4%
  translated into a real (small) live delta.
- **find was byte-IDENTICAL across arms** (13583 == 13583, result 51 B). Asked to
  find the file whose name contains `zebra`, the model searched `*zebra*` -> ONE
  match -> no >1000-match listing -> compaction never fires. find's 55.7% render
  reduction only exists on a broad listing; a targeted search (the common real
  case) never trips the rail, so there is nothing to reduce. Honest: the feature
  helps a narrower slice of real usage than the render probe implies.
- **read skim was never exercised**: the model did NOT read the file at all -- it
  grepped for the constant (B: two greps + an extra turn; A: find+grep). So the
  83.1% skim reduction contributed zero here; the A - B delta (-33%, one fewer
  turn) is a grep-strategy difference at N=1, not skim. To exercise read skim the
  question must force a read (e.g. "summarize the module's exported API"), not a
  point-lookup a grep answers better.

**Consequence for the suite (Phase 7 follow-ups, not bugs):** the find and read
LIVE probe questions are mismatched to their tools -- they let a smart model
route around the reduction. Fixes: make the find question demand a broad listing
(e.g. count/enumerate) and the read question demand reading a file. The render
probes remain valid (they measure the tool directly); it is the live-question
design that must force the intended tool path. This tool-choice gap IS the
"connection between per-tool advantage and end-to-end outcome" the suite exists
to find, and it argues against any blanket "reductions save tokens per task"
claim: the saving is contingent on the model using the tool the reducing way.

## Entry 19 - Fixing the find/read live-question intent mismatch

Entry 18 found the find and read LIVE questions let a capable model route around
the very reduction they were meant to exercise. Fixed by redesigning the
questions to force the intended tool path (the render probes were already
correct; only the live-question design was wrong).

**find: `probe-find-target-path` -> `probe-find-odd-handler`.** The old question
("find the file whose name contains zebra") let the model glob `*zebra*` -> one
match -> compaction never fires. The new question asks for the ONE handler whose
name does not follow the `handler_NN_NN.rs` numeric pattern. You cannot glob for
"the odd one out", so the model must list broadly (`*.rs` / `handler_*.rs` ->
1351 matches -> compaction) and scan the reduced listing. The answer
(`handler_zebra_target.rs`) sorts into the shown prefix (newest mtime) and the
render probe already proves it survives, so a green render probe guarantees the
answer is present in both arms; only the token cost differs. Reuses
`check_probe_find_path`.

**read: `probe-read-skim-constant` -> `probe-read-sweep-local`.** The old
question asked for a top-level constant, which a single grep answers better than
a read -- so skim was never exercised. The new question asks for a body-level
local (`due_ids`) inside the `sweep` function: the model has to read the code (a
constant-grep cannot answer it), which is exactly what skim optimizes -- skim
keeps the function body verbatim while stripping the heavy comment narrative.
Renamed the fixture local `closed` -> `due_ids` (distinctive needle) and added
`due_ids` to the read render probe's needles, so the render probe now proves a
BODY-level identifier (not just top-level signatures) survives skim. Render
probe still 83.1%.

**Honest limitation (stated, not hidden):** question design can strongly NUDGE
but cannot GUARANTEE a specific tool for a capable model -- a determined model
could still `find handler_*.rs` then reason, or `grep 'fn sweep' -A` with wide
context. The point is not to trap the model but to remove the trivial route
around the reduction so the live probe actually stresses the feature more often
than not; a model that still routes around it is itself a measured outcome (the
schema logs the tool sequence). The deterministic render probes remain the
ground truth for "the reduction is real and lossless"; the live probes measure
whether it pays off end-to-end. No re-run yet -- these questions await the next
authorized live pass.

## Entry 20 - Phase 7: the deterministic analyzer (no live matrix)

Built `analysis.rs`: pure functions over the schema-v3 JSONL -> per-arm
aggregate -> paired A(defaults) vs B(baseline) -> honesty verdict. No provider;
the live matrix stays a separately-authorized opt-in. Gate-tested by
`analyzer_verdicts_hold`, which feeds synthetic logs covering every branch
(Supported / SuccessRegression / BaselineWins / Inconclusive) plus a render
probe, an error cell, a usage-None invalid cell, and a garbage line -- all
counted, none crash it.

**Verdict precedence (most-blocking wins; one regression fails the run):**
SuccessRegression > BaselineWins > Inconclusive > Incomplete > Supported.
A success drop is the headline regardless of N or tokens; baseline ties/wins ->
no claim; small N (< 5) or overlapping input spreads -> inconclusive.

**Two design choices that keep it honest:**
- **Never mix token sources.** Absolute deltas use ONLY `real_cell.input_tokens`
  (real usage). `render_probe` proxy tokens live in their own section as ratios.
  The "did the reduction actually fire in context" signal is the
  `tool_result_bytes` delta -- REAL bytes measured in BOTH arms, sharing units
  with neither proxy nor usage tokens, so nothing is mixed.
- **Token-delta decomposition.** delta = turns_a*(tpt_a - tpt_b) +
  (turns_a - turns_b)*tpt_b, splitting "each turn got cheaper" (the reduction
  working) from "the arm changed the turn count" (strategy variance).

**Validated on the real Sonnet 4.6 low micro-probe log (Entry 18 data):**

| workload | delta in | eff / turns | result-bytes delta | verdict |
|---|---|---|---|---|
| find | +0 (0%) | +0 / +0 | +0 | BASELINE WINS |
| grep | -538 (-3.6%) | -538 / +0 | -1766 | INCONCLUSIVE (N=1) |
| read | -7027 (-33.4%) | -17 / -7010 | -122 | INCONCLUSIVE (N=1) |

The analyzer mechanically reproduced the hand analysis: find's +0
`result-bytes` delta proves the reduction never fired (the model narrowed the
search), and read's decomposition attributes the -33% almost entirely to the
turn term (-7010), NOT cheaper output (-17) -- a strategy artifact, not skim.
Overall BASELINE WINS -> no claim, exactly right. This is the per-tool ->
end-to-end connection computed, not eyeballed.

**What remains (opt-in, authorization-gated):** run the live matrix against the
FIXED find/read questions (Entry 19), point `tokens_per_task_report` at that
log, and commit the sanitized JSONL + rendered report under docs/benchmarks/.
No claim ships until real N >= 5 clears the verdict gate without a regression.

## Entry 21 - Decomposition confound: the "-7010 turns" term was misleading

Operator flagged the read cell's `term_turns = -7010` as suspicious. It was
right to. The raw per-turn input (cumulative; each turn resends the transcript)
tells the real story:

    B (baseline): [6734, 6902, 7393]  sum 21029   grep, grep -> 3 turns
    A (defaults): [6734, 7268]         sum 14002   find, grep -> 2 turns

Exact per-turn delta: turn1 0, turn2 +366 (A's turn was BIGGER), turn3 -7393 (B
had one more turn). Total -7027. So A did not have cheaper turns -- A's
comparable turn was heavier -- it simply used one fewer. The whole -33% was the
eliminated third turn (mostly fixed system-prompt + tool-schema overhead), which
at N=1 is strategy noise, not a reduction effect.

The flaw: my decomposition used `tpt = cumulative_input / turns`, an AVERAGE over
a growing series. That smeared the truth into "efficiency -17 / turns -7010",
implying A's turns were slightly cheaper when per-turn they were slightly
DEARER. The arithmetic split is exact but only interpretable when both arms take
the same number of turns.

Fix (no verdict change -- it was already INCONCLUSIVE at N=1): the analyzer now
reports `turns a/b` and a `mechanism` label -- `per-turn (same turn count)` for a
genuine reduction effect, or `fewer/more turns (confounded w/ strategy)` when the
counts differ and the delta is dominated by whole turns of fixed overhead. The
report caveat says the eff/turns split is a clean reduction signal ONLY when turn
counts match. Re-rendered real log: grep/find show `per-turn` (2/2 turns), read
shows `fewer turns (confounded)` (2/3). The `result-bytes delta` cross-checks it:
read's real tool-output shrank only -122 B, consistent with the tiny -17
efficiency term -- the reduction's DIRECT effect here was negligible; the
apparent win was turn-count variance. Lesson reinforced (see Entry 12): at low
turn counts, turn-count variance dominates the token delta; only same-turn-count
cells or large N isolate the reduction.

## Entry 22 - Headline matrix, full 3-spec default, N=5 (authorized live run)

First full authorized headline matrix: 3 models (gpt-5.4-mini, gpt-5.3-codex-spark,
claude-haiku-4-5) x 3 workloads x 2 arms x N=5 = 90 real sessions, reasoning=low
(held identical across arms), real usage-record input tokens. Smoke-guard first
(all 3 reachable, all succeeded). Committed artifact:
docs/benchmarks/headline-matrix-2026-07-05.{md,jsonl}.

**Overall verdict: BASELINE WINS -- no tokens-per-task claim shipped.**

- **No success regression** (the stop-and-report trigger). A >= B success in
  every cell; gpt-5.4-mini/investigate improved it (A 5/5 vs B 4/5); two cells
  tied at 4/5 (haiku multi-file). Verified by direct count.
- **No consistent token win:** 3/9 cells defaults-cheaper, 6/9 baseline-cheaper.
- **Turn-count variance dominates (again, now at N=5).** Every big swing
  (-22% to +66%) is flagged `more/fewer turns (confounded w/ strategy)` by the
  Entry-21 mechanism label -- the arm changed turn count, and each turn is mostly
  fixed system-prompt + tool-schema overhead. E.g. codex-spark/investigate A
  +66.5% is entirely +11081 turn term (5 vs 3 turns), not per-turn cost.
- **The 4 clean same-turn-count cells** (`per-turn` mechanism) put the reduction
  inside noise: -4.1%, +0.1%, +0.5%, +2.2%. The lone negative
  (gpt-5.4-mini/multi-file, result-bytes -913, reduction fired) is INCONCLUSIVE
  by spread. So even where turn count is controlled, N=5 does not clear zero.

**Interpretation.** The per-tool render probes are real (grep 36%, find 56%,
read 83% raw-output reduction), but that shrink does NOT propagate to a
measurable per-completed-task token reduction here: reduced tool-result bytes are
a small slice of each turn's cumulative context, and turn-count strategy variance
swamps them. This is the benchmark doing its job -- it refuses to manufacture a
win. Milestone-2's tokens-per-completed-task claim is UNSUPPORTED by this run;
ROADMAP gate stays open; README carries no such claim.

**Caveats / next moves (not run):** low effort + these 3 tasks + N=5; a larger N
or an effort sweep (separate dimension) might narrow the clean-cell spreads, but
turn-count variance is the real obstacle and it is a property of model strategy,
not of the reduction. The micro-probe axis (single-tool tasks, more same-turn
cells) remains the cleaner place to isolate a per-tool token effect.

## Entry 23 - Stronger-model matrix: Sonnet 4.6 + GPT-5.4, low, N=5

Operator-requested second matrix: the two stronger models at low thinking, to
test whether a per-turn reduction signal is cleaner than on the default 3-spec
set. 2 models x 3 workloads x 2 arms x N=5 = 60 real sessions. Smoke first
(both reachable; `openai-codex:gpt-5.4` full model IS served, not just -mini).
Committed artifact: headline-matrix-sonnet46-gpt54-2026-07-05.{md,jsonl}.

**Verdict: BASELINE WINS (no claim) -- but directionally favorable, INCONCLUSIVE.**

- 100% success in BOTH arms of ALL 6 cells (no regression, no ties).
- Sign is now CONSISTENT: 5/6 cells defaults-cheaper (-1.5, -2.5, -36.9, -5.0,
  -1.0%); only 1 baseline-cheaper. Contrast the 3-spec run's mixed 3/9 vs 6/9.
- All 4 clean same-turn-count (`per-turn`) cells favor defaults (-1% to -5%),
  each with a matching negative result-bytes delta (-291/-1168/-1295/-539): the
  reduction fired and each turn was modestly cheaper. Yet every one is
  INCONCLUSIVE -- at N=5 the small delta does not clear the run-to-run spread.
- Overall BASELINE WINS is dragged by ONE confounded cell: gpt-5.4/investigate,
  A 4 turns vs B 2 (`more turns`, +94.7%, result-bytes +23 ~ nil) -- strategy
  variance, not the reduction losing.

**Cross-model observation.** The clean-cell reduction sign flipped from mixed
(3-spec: codex-spark/gpt-5.4-mini fix were per-turn BASELINE WINS at +0.1..+2.2%)
to uniformly favorable here (-1..-5%). Weak evidence that the per-turn benefit is
more consistent on stronger models, but N=5 and small magnitudes -- not a claim.

**Honesty notes.**
- Did NOT change the analyzer to soften the overall label after seeing the
  result. Overall BASELINE WINS stands as computed; no README claim; ROADMAP
  gate stays open.
- Logged a genuine analyzer asymmetry for SEPARATE consideration: a
  turn-count-`confounded` cell resolves INCONCLUSIVE when A is cheaper but
  BASELINE WINS when A is pricier. Both are "no claim", so the shipping decision
  is unchanged; a symmetric "confounded => inconclusive regardless of sign"
  would reclassify only this run's overall (3-spec stays BASELINE WINS because
  its BASELINE WINS cells are genuine per-turn, same-turn-count). Raise as its
  own change, not reactively.
- Path to significance: more runs on the clean same-turn-count cells (larger N
  on single-strategy tasks), not a wider model sweep. Turn-count variance
  remains the dominant obstacle and is a property of model strategy.

## Entry 24 - N=50 investigate cell: safety/non-regression read

Effort verified before the run (operator flagged it): Sonnet 4.6 is manual-budget
=> iris `low` = `thinking.budget_tokens: 4096`, NOT Anthropic's named "low"
effort (that scale is adaptive-tier only; there iris `minimal` -> Anthropic
"low"). GPT-5.4 `low` -> `reasoning.effort: "low"` direct. Added
`IRIS_BENCH_WORKLOAD` filter (6f31484) to target one cell without paying for the
other two. 200 sessions (2 models x investigate x 2 arms x N=50), low, held
across arms. Artifact: investigate-n50-sonnet46-gpt54-2026-07-05.{md,jsonl}.

Corrected interpretation: this run was never expected to show a large product
savings. `investigate-large-log` has a small reduced grep payload relative to the
full system prompt + schemas + transcript, so whole-run input-token savings must
be small by construction. The value of N=50 is **safety**: does compaction make
the output harder to interpret, lower success, or cause extra tool loops?

Safety result: no regression signal. Sonnet 4.6 was 100%/100% success, 3/3 median
turns, 2.0/2.0 median tool calls, 2/2 max tool calls, 0/0 tool errors. GPT-5.4
was 100%/100% success, 3/3 median turns, 3.0/3.0 median tool calls, 6/6 max tool
calls, 0/0 tool errors. That is exactly what this high-N run should prove:
reduced output did not cause wrong answers or a tool-call explosion on this task.

Token result is secondary: Sonnet showed a small statistically detectable
end-to-end input-token saving (~2.4%); GPT-5.4 was directionally cheaper but
within noise. This is useful mechanism evidence, but not a headline efficiency
claim because the eligible tool output was a minute part of the whole run.

Analyzer/report follow-up: added a `Safety / loop signals` section to the report
(success, turns, median/max tool calls, total tool errors) so future N-run tests
surface compaction regressions directly instead of only showing token deltas.

Workload follow-up: added `chained-openai-summary-fix`, seeded from real PR #404
(OpenAI request had `effort` but missed `summary: "auto"`). It hides the provider
bug among generated decoy provider files and scripts a real chain: find, multiple
greps, read failing test, `cargo test`, read provider, edit, and passing
`cargo test`. That is the right place to look for
material end-to-end token savings because bash/cargo-test output can dominate the
transcript while success still proves the model solved the task.

## Entry 25 - Parallel Sonnet 4.6 chained workload smoke (N=5 x 2 arms)

Operator authorized a small live run to validate the new `chained-openai-summary-fix`
workload and parallel-run shape: Sonnet 4.6, `low`, `IRIS_BENCH_N=5`, split into
two simultaneous processes with `IRIS_BENCH_ARM=baseline` and
`IRIS_BENCH_ARM=defaults`, separate logs, then merged for analysis. Runtime was
~134s; both cargo-test processes exited 0 and wrote 5 rows each.

Mechanical outcome under the then-current check: 10/10 rows succeeded, no approval
prompts, no tool errors. That proves the fixture is runnable in parallel and the
model can solve the hidden PR-404-style bug. Safety/loop metrics from the merged
log: defaults 100% / baseline 100%; turns 6/5; median tool calls 6.0/6.0; max
calls 7/6; tool errors 0/0. Not a massive loop, but defaults did take one extra
median turn and one higher max call. Token result was baseline-wins (+5.7% for
A/defaults) because the extra turn dominated; result bytes were slightly lower in
A (-509), so reduction fired but was swamped by strategy.

Important design finding: the workload prompt said to reproduce the failure first,
but the success check only required the final cargo tests to pass and
`summary:auto` to be present. Sonnet often read enough source/test context to edit
first and then only ran a passing test. Baseline did this in all 5 runs (`bash`
exits `[0]`), while defaults reproduced the failure in 2/5 runs (`[101, 0]`) and
therefore paid extra turns/tokens. That means the smoke validated the harness but
also proved the workload was not mechanically enforcing the intended chained
workflow.

Fix applied after the smoke: `IRIS_BENCH_ARM` now splits the bash matrix for safe
parallel arm runs, the prompt now says to run plain `cargo test` first (before
reading/editing), and `chained-openai-summary-fix` now treats runs without a
failing bash exit before the final passing bash exit as invalid/noncompliant
rather than valid task failures. Future token comparisons exclude those rows but
still report the invalid count.

Raw logs were left in `/tmp/chained_s46_low_{baseline,defaults}_n5.jsonl` and not
committed because their `success=true` values predate the invalid/noncompliant
classification.

## Entry 26 - Chained workload rerun with explicit plain `cargo test` first

Adjusted the workload after the previous smoke: prompt now says to run plain
`cargo test` before reading/editing; scripted replay uses `cargo test` (no
`--nocapture`); shortcut/noncompliant runs are marked invalid (excluded from token
comparison) rather than counted as task failures. Rationale: normal failing Rust
tests already report the failing test, panic site, and assertion left/right; the
fixture does not need artificial `--nocapture` noise.

Authorized rerun: Sonnet 4.6, low, `chained-openai-summary-fix`, N=5 per arm,
parallel arm processes (`IRIS_BENCH_ARM=baseline` and `defaults`). Artifact:
chained-openai-summary-fix-sonnet46-low-n5-2026-07-05.{md,jsonl}.

Result: 10/10 valid + successful. Every run reproduced a failing cargo test before
the final passing cargo test (exit code `101` before final `0`). Safety signals:
success 100%/100%, median turns 7/7, median tool calls 7.0/7.0, max calls 8/9
(A/B), tool errors 1/0 (A/B; one edit-before-read error in A run 5, recovered).
Token result: A/defaults median 97,690 vs B/baseline 101,987 input tokens, delta
-4,297 (-4.2%), same turn count; result-bytes delta -377. Welch CI crosses zero
at N=5, so descriptive only/no claim.

Conclusion: the workload design now works for the intended benchmark shape: it
forces the reproduce-failure -> fix -> verify chain, can run arms in parallel, and
surfaces both safety and token signals without relying on `--nocapture`.

## Entry 27 - Chained PR-seeded suite, Sonnet 4.6 low, N=50 per workload/arm

Ran the expanded chained repair suite at high N using the recommended sharding:
5 shards per arm, N=10 per shard, separate `IRIS_BENCH_LOG` files, merged after
completion. Added shard/run-offset metadata before launch (`IRIS_BENCH_SHARD`,
`IRIS_BENCH_RUN_OFFSET`) so merged rows remain debuggable and no process writes to
the same log file. Total: 6 workloads x 2 arms x 50 = 600 live sessions. All 10
processes exited 0 in ~1826s wall clock.

Artifacts: chained-suite-sonnet46-low-n50-2026-07-05.{md,jsonl}.

Summary: 600 valid rows, 0 invalid/noncompliant, 0 process errors, 590/600 task
success. The only failures were `chained-ampi-private-docs-fix`, split evenly
A 45/50 and B 45/50; those rows passed `npm test` but failed the mechanical source
check because the expected `docs/private/` exclusion was absent (anti-test-
weakening guard). No success regression: A matched B on every workload.

Safety/loop read: median turns matched in every workload; max tool calls stayed
bounded (7-9). Tool errors were low: 14 total, A=9/B=5, mostly edit-before-read or
old-string mismatch recoveries. No evidence of a compaction-induced massive loop.

Token read: overall BASELINE WINS/no blanket claim. Defaults cheaper on medians in
4/6 cells (`github-token` ~flat, `pack-untracked` ~flat, `recall` -0.8%,
`openai-summary` -8.6%), baseline cheaper in 2/6 (`private-docs` +0.1%,
`fold-resume` +3.4%). Only `openai-summary` cleared the Welch CI (+8980 mean
saving, CI [+3601,+14360]); `fold-resume` significantly favored baseline on the
mean (CI [-4407,-749]). The rest are small/noisy/inconclusive.

Conclusion: the sharded high-N harness works and gives the right safety evidence.
The product-level efficiency claim remains workload-dependent, not global.
