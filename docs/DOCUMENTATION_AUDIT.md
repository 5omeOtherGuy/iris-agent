# Documentation audit — 2026-09-08

Baseline: `b420985581f5b3c75b4f09991e343b674c77b45e` (main, 2026-08-03).
Latest release checked through GitHub: `v0.3.7` (2026-07-14).

This was a repository-wide documentation inventory and claim audit, not an
exhaustive proof of every sentence or platform/provider qualification. Source-backed
corrections were applied to current manuals, capability status and navigation.
Historical decisions and measurements retain their original context.
The [v1.0 roadmap](ROADMAP.md) defines the remaining implementation gates.

## Method and coverage

The baseline contains **164 tracked Markdown files**: **161 documentation files**
and three corpus/vendor texts. The inventory excluded `.txt` workload fixtures,
raw benchmark data from rewriting, ignored local instructions, credentials and
private sessions. This audit adds one documentation file.

Six headless Pi jobs used provider `opencode`, model
`muse-spark-1.3-contributor-free`, thinking `high`. Three initial audits divided
all 161 docs into runtime/design (18), operator/product (35), and evidence/ADRs
(108). Three follow-ups checked operator safety/defaults, ADR/benchmark claims,
and the rewritten roadmap/codemap. All jobs completed and reported zero model
cost. They had only `read`, `grep`, `find`, and `ls`; context-file/extension/skill
discovery was disabled. The lead alone edited documentation.

Workers returned candidate findings with source references. The lead checked
material claims against code and read-only GitHub issue/release data. Reports
were not accepted wholesale: several inferred missing features from stale prose,
confused dormant Iris controls with implemented public crate APIs, or treated
parser tests as live-provider evidence. Those claims were rejected.

### Coverage ledger

Depth terms:

- **Source checks:** operator contracts, settings, ownership or command claims
  checked against targeted implementation/test slices. Not every line audited.
- **Historical index:** date/status/links/verdicts reviewed; no re-execution or
  complete re-analysis of measurements or old implementation checklists.
- **Guidance:** policy/template/reference checked for scope and usable repository
  pointers, not a test of every generic language example.
- **Static scan:** ordinary local Markdown links, simple heading anchors and
  literal source-path references scanned across the documentation inventory.

All rows below received the static scan where applicable. Unchanged files were
not assumed correct merely because a worker reported no finding.

| Documentation scope | Depth and disposition |
| --- | --- |
| `README.md`, `docs/FEATURES.md`, `docs/ROADMAP.md` | Source checks and synthesis. Corrected current capability/default claims; replaced the old mixed history/backlog roadmap with open v1.0 gates. |
| `docs/ARCHITECTURE.md`, `docs/CODEMAPS/INDEX.md`, `crates/iris-subagent-runtime/README.md` | Source checks. Corrected workspace packaging, entrypoint ownership, concrete core dependencies, implemented APIs and public-crate vs model-surface boundaries. |
| All nine `openwiki/*.md` pages | Source checks, especially commands, flags, security, sessions and installation. Corrected persistence/settlement, tool/config lists and worktree/release workflow. Rendering details remain sampled, not visually revalidated. |
| `CONTRIBUTING.md`, `.github/pull_request_template.md`, `SECURITY.md`, `AGENTS.md`, `CLAUDE.md` | Guidance and source checks of gates/worktree commands. Updated contributor/PR checks and removed hook-bypass advice; agent instructions and security-reporting policy unchanged. |
| `PRODUCT.md` | Product intent separated from implementation. Planner/ledger are goals, checkpoints already exist, and the missing design-summary link was removed. |
| `BUGS.md`, `DESIGN_NOTES.md`, `TUI_POLISH_SHOWCASE_PROMPT.md` | Historical task records/prompt. Added history labels to the bug ledger and review notes; did not claim old gate counts apply to main. |
| `CHANGELOG.md`, `iris-bench/CHANGELOG.md` | Historical release records, retained. Release chronology is not rewritten into a fabricated unreleased history. |
| `CODE_OF_CONDUCT.md`, `showcase/README.md` | Guidance/collateral, retained; not runtime specifications. |
| `docs/NAMING.md`, `docs/MODULARIZATION.md` | Naming/proposal status checked against current workspace. Further tier-per-crate split remains proposed; target paths are labeled as future files. |
| `docs/TUI_DESIGN_LANGUAGE.md`, `docs/TUI_LIVE_TESTING.md` | Source checks of command/rendering seams and test instructions. Removed unsupported `/undo`/`/clear` commands. No live TTY run or exhaustive pane conformance check. |
| All nine `docs/specs/*.md` files | Source/status checks of questions, console drawers, streaming, settings hatches, flow meter, sticky prompt, reasoning, density and review posture. Added implementation/history notes; retired caret mandates stay visible as superseded history. Original acceptance lists were not rerun. |
| `docs/OPENWIKI.md`, `docs/RELEASING.md` | Source/runbook checks of scripts, distribution and release workflow. Added dated release status and corrected the claim that main can never reach users. No artifact build or publication. |
| `docs/COMPETITOR_ANALYSIS.md`, `docs/COMPETITOR_MATRIX.md` | Historical research. Marked unavailable local-only evidence instead of publishing broken private-reference links. External competitor claims were not re-researched. |
| All 68 files under `docs/adr/` | Historical status/amendment/index scan, with targeted source checks for live contracts. Added implementation notes to 0026/0027/0042/0060, an open-follow-up note to 0055, and public provenance notes to 0035. Original formal statuses retained; no new architecture decision or retroactive approval. |
| `docs/BENCHMARK_PLAN.md`, `docs/benchmarks/HARNESS.md`, `docs/benchmarks/tool-efficiency-suite-design.md` | Source checks of module wiring, runner/config/env names, migration and reporting. Distinguished parser support from live cache-write evidence; corrected current vs historical commands. |
| `docs/auto-compaction-implementation-notebook.md`, `docs/benchmarks/auto-compaction-live-loop.md`, `docs/benchmarks/auto-compaction-v2-tuning.md` | Historical protocol/closeout/tuning evidence, with current compaction source checks. Added current-runner/roadmap navigation; old live results are not apply-on-ready validation. |
| `docs/benchmarks/adr-0037-bash-filter-tokens.md`, `issue-337-read-skim-tokens.md`, `issue-338-grep-output-tokens.md`, `issue-339-ls-tokens.md`, `issue-340-find-compaction.md`, `issue-341-edit-result-classes.md`, `web-tools-token-efficiency.md` (all under `docs/benchmarks/`) | Historical per-result reports; source/command checks sampled. Corrected `ls`/`find` library test targets. Kept measured tables unchanged. |
| `docs/benchmarks/issue-372-compaction-retention.md`, `issue-372-compaction-retention-slice-b.md`, `issue-378-residual-tool-mass.md`, `issue-400-fold-flush-cost.md` (same directory) | Historical retention/fold evidence; source/registration checks for reproduction commands. Corrected library targets, not measurements. Full arithmetic/quality was not rerun. |
| All Markdown under `docs/benchmarks/campaigns/` | Historical index/verdict checks: legacy headline/chained/tokens-per-task, pilot-a, pilot-b, pilot-b-s1-tuned and probe reports. Annotated the superseded pending headline paragraph. Raw JSONL/manifest data and result tables unchanged. |
| All nine `.agents/skills/*/SKILL.md` files | Guidance and source-pointer spot checks. No instruction/skill rewrites; projection/metadata integrity checked separately. Generic Rust/style examples are not Iris feature claims. |
| `src/tools/bash/filter/data/NOTICE.md`, `src/tools/web/corpus/reader-gnu-free-sw.jina.md`, `src/tools/web/testdata/excerpts_doc.md` | Excluded from product-doc reconciliation: attribution/corpus/test input. Preserved unchanged. |

## Material corrections and evidence

| Correction | Decisive implementation/evidence |
| --- | --- |
| Compaction is implemented, but ready work is held until hard/manual consumption | `src/wayland/compaction_background.rs::poll_background_ready`; governed pressure in `compaction_governor.rs`; blocking turn-edge `wait_blocking_timeout` in `src/wayland/mod.rs`. #658–#662 remain open. |
| Dangerous-skip is global/persistent, not session-only | `src/lib.rs::persist_cli_skip_permissions`, `src/config.rs::save_default_approval`; required questions still use the separate interaction seam. |
| `/checkpoint` does not settle a task; tasks are opt-in | `src/wayland/git_safety/settlement.rs::checkpoint_now`, `src/config.rs::tasks`; normal mutation safety is a separate default-on control. |
| Five provider routes, output dereference, terminal doctor, resume picker and worker routing already exist | `src/mimir/selection.rs`, `src/tools/registry.rs`, `src/ui/slash.rs`, `src/ui/terminal_doctor.rs`, `src/wayland/subagents.rs`. |
| Public worker groups are real APIs, not types-only; Iris best-of-N tools are dormant | `crates/iris-subagent-runtime/src/runtime.rs::spawn_group`, group tests, `src/wayland/worker_runtime.rs`, ADR-0065 and registry exclusion tests. |
| Compiled fragments, grants, named themes and harness actor exist despite old proposed ADR labels | `src/wayland/system_prompt/`, `src/wayland/trust.rs`, `src/ui/theme.rs`, `src/ui/harness_actor.rs`, `src/ui/tui_loop.rs`. |
| Workspace split exists, but core layering is not completely clean | `Cargo.toml`, thin `src/main.rs`, `src/lib.rs`; `src/nexus.rs` still references concrete tool state and path/display helpers. |
| Syntax highlighting and safe OSC-8 links are implemented | `src/ui/markdown.rs`, `src/ui/highlight.rs`, `src/ui/hyperlink.rs`, transcript setup and terminal serialization. Old codemap said they were only seams. |
| Real task-efficiency measurement ran; no universal savings result | [90-session headline report](benchmarks/campaigns/legacy-headline-matrix/2026-07-05/headline-matrix-2026-07-05.md): no success regression; baseline cheaper in six of nine cells; [#210](https://github.com/5omeOtherGuy/iris-agent/issues/210) closed as measurement work. |
| Codex cache-write parsing is not proof of live reports | `src/live_harness/runner.rs` preserves nonzero writes and flags zero/unreported values. Sampled `probe-sol-s1` (2 rows) and `probe-terra-32k` (4 rows) each have zero nonzero-write rows and all rows marked write-unreported. |
| Current benchmark test targets are library-based | `src/nexus.rs` registers benchmark modules under tests; `src/main.rs` is a shim. Four per-result/retention reproduction commands now use `-p iris-agent --lib`; historical command captures remain labeled. |
| Docs-only gate does not run Rust or link validation | `scripts/gate.sh`, `scripts/change-scope.sh`. Manuals now state the fast-path boundary. |

## Verification

Checks below ran on the documentation patch. No source, dependency, test
fixture, benchmark result table or raw campaign data was changed. There are
49 modified tracked Markdown files plus this new audit file.

- Static scan: 162 documentation files after adding this audit, zero broken
  ordinary local links and zero unresolved simple heading anchors. Literal
  source-path exceptions were reviewed: external Pi/Claude reference paths,
  future modularization paths, and synthetic retention-fixture paths; none is
  presented as a missing implemented module.
- The local-file-link reproduction below passed. `git diff --check` passed.
- `bash scripts/check-repo-guidance.sh`: PASS, nine canonical skills and valid
  projections. `typos --force-exclude` over all changed Markdown: exit 0.
- `bash scripts/gate.sh --verbose`: PASS — format, Clippy with warnings denied,
  tests and maintenance scripts. Library: 2,816 passed, zero failed, 26 ignored;
  integration: one `ok` result; binary/doc-test targets: zero tests. The Btrfs
  integration check permits an honest platform skip and is not Btrfs qualification.
  The worker-crate suite was not run separately; this is not a workspace-wide count.
- The nested worker-crate README triggers the conservative full-code gate even
  though every changed file is Markdown. An initial 30-second command timeout
  was followed by the passing shared-cache run (`CARGO_BUILD_JOBS=2`). No check
  was disabled. The first attempt's task-local target was removed; shared build
  artifacts were preserved.
- Six changed historical-report table sets were compared with the baseline and
  remain identical. Source/dependency/runtime-test paths have no diff.
- No live provider campaign, real TTY, distribution build, paid Iris benchmark
  or external competitor validation was run for this documentation task.
- The inspected scheduled dependency audit on baseline main failed
  ([run](https://github.com/5omeOtherGuy/iris-agent/actions/runs/34112493767)). Its
  cause is untriaged here; no vulnerability or waiver is inferred.

### Recheck ordinary local links

This stdlib check resolves ordinary inline/reference Markdown file destinations.
It does not fetch external URLs, render Markdown, or prove example behavior.
Heading anchors and quoted source paths need separate review.

```sh
python3 - <<'PY'
from pathlib import Path
from urllib.parse import unquote, urlsplit
import re
import subprocess

files = subprocess.check_output([
    'git', 'ls-files', '--cached', '--others', '--exclude-standard', '*.md'
], text=True).splitlines()
errors = []
for name in sorted(set(files)):
    doc = Path(name)
    fenced = False
    for number, line in enumerate(doc.read_text().splitlines(), 1):
        if re.match(r'^\s*(```|~~~)', line):
            fenced = not fenced
            continue
        if fenced:
            continue
        line = re.sub(r'`+[^`]*`+', '', line)
        targets = re.findall(r'\]\(([^\s)]+)(?:\s+"[^"]*")?\)', line)
        ref = re.match(r'^\s{0,3}\[[^\]]+\]:\s+(\S+)', line)
        if ref:
            targets.append(ref.group(1))
        for target in targets:
            url = urlsplit(target.strip('<>'))
            if not url.scheme and not url.netloc and url.path:
                if not (doc.parent / unquote(url.path)).exists():
                    errors.append(f'{name}:{number}: {target}')
print('\n'.join(errors) if errors else 'PASS: local file destinations resolve')
raise SystemExit(bool(errors))
PY
```

## Remaining limits

- Historical ADR status is not implementation status. The added notes cover
  verified drift, not a new ratification of every older decision.
- Historical tests/campaigns remain evidence at their recorded revisions. The
  long-session freshness/economics overhaul still needs its own deterministic and
  operator-approved live validation.
- Compiler/platform/provider/authentication changes can invalidate an old pass.
  M0–M5 stay open until their actual release-candidate evidence exists.
- Static scans cannot prove all semantic claims. Full renderer conformance,
  protocol correctness, benchmark arithmetic and safety fault matrices are
  implementation/release work, not certified by this documentation patch.
