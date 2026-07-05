#!/usr/bin/env python3
"""Measure residual tool-result token mass over real Iris session transcripts.

Task 0 of #378 / ADR-0048: before building the fold engine, establish how much
tool-result token mass accumulates per session and how much of it is provably
"spent" (superseded reads + retired command output). Read-only: this script
opens session JSONLs, never writes or mutates anything.

Method
------
Each session JSONL (see `src/session.rs`) carries one `message` entry per line
with a persisted per-entry `tokenEstimate`. This script counts only
`role == "tool"` results, keyed by the `toolName` the result carries and paired
to its call args via `toolCallId` (from the matching `assistant_tool_call`).
Token mass is read straight from `tokenEstimate` -- never re-estimated.

"Spent" classes (ADR-0048 v1 scope), detected per session:
  - superseded reads   -- a `read`/`ls` result whose path is later read, edited,
                          or written again (latest-read-wins); the earlier copy
                          is stale by construction.
  - retired command    -- a `grep`/`find`/`bash` result whose exact call args are
                          re-run later in the same session; the earlier copy is
                          superseded. This counts every identical earlier copy
                          regardless of whether it failed, so it is an
                          identical-rerun command-output proxy -- an *upper
                          bound* for retired failure output, not a failure-output
                          floor. Bash results do not persist an exit code (only
                          an `ok` execution flag), so true exit-status failures
                          are not measurable from current transcripts.

Usage
-----
  python3 scripts/measure-residual-tool-mass.py [SESSION_ROOT]

SESSION_ROOT defaults to $IRIS_SESSION_DIR, else ~/.iris/sessions. All
`**/*.jsonl` transcripts under it are scanned. Output is a plain-text report to
stdout.
"""

from __future__ import annotations

import collections
import json
import os
import sys

REPRODUCIBLE_CMD = ("grep", "find", "bash")
PATH_KEYED = ("read", "ls", "edit", "write")


def norm_path(p):
    return os.path.normpath(p) if p else None


def session_root(argv):
    if len(argv) > 1:
        return argv[1]
    env = os.environ.get("IRIS_SESSION_DIR")
    if env:
        return env
    return os.path.join(os.path.expanduser("~"), ".iris", "sessions")


def load_entries(path):
    out = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                # A crash can leave a truncated final line; skip it.
                continue
    return out


def tool_results(entries):
    """Return ordered [idx, tool, token_estimate, args_str, path] for tool results."""
    call_args = {}
    for e in entries:
        if e.get("type") != "message":
            continue
        m = e["message"]
        if m.get("role") == "assistant_tool_call":
            call_args[m.get("toolCallId")] = m.get("content")

    results = []
    for idx, e in enumerate(entries):
        if e.get("type") != "message":
            continue
        m = e["message"]
        if m.get("role") != "tool":
            continue
        tool = m.get("toolName") or "unknown"
        te = e.get("tokenEstimate") or 0
        args_str = call_args.get(m.get("toolCallId"))
        args = {}
        if args_str:
            try:
                args = json.loads(args_str)
            except json.JSONDecodeError:
                args = {}
        path = None
        if tool in PATH_KEYED:
            path = norm_path(args.get("path") or args.get("file_path"))
        results.append([idx, tool, te, args_str, path])
    return results


def foldable_indices(results):
    """Indices of tool results that are provably spent under ADR-0048 v1 scope."""
    fold = set()
    # Superseded reads: a read/ls whose path is touched (read/edit/write) later.
    last_touch = collections.defaultdict(list)
    for r in results:
        if r[4]:
            last_touch[r[4]].append(r[0])
    for r in results:
        idx, tool, _te, _args, path = r
        if tool in ("read", "ls") and path and any(x > idx for x in last_touch[path]):
            fold.add(idx)
    # Retired command output: identical (tool, args) re-run later.
    groups = collections.defaultdict(list)
    for r in results:
        if r[1] in REPRODUCIBLE_CMD and r[3]:
            groups[(r[1], r[3])].append(r)
    for rs in groups.values():
        if len(rs) < 2:
            continue
        latest = max(x[0] for x in rs)
        for r in rs:
            if r[0] < latest:
                fold.add(r[0])
    return fold


def iter_session_files(root):
    """Yield resolved `*.jsonl` paths confined to the canonical session root.

    Read-only containment guard: resolve the root, walk it without following
    symlinks, and require each resolved file path to stay under the resolved
    root before it is opened. This prevents reading `*.jsonl` outside the
    session store via a symlinked subdirectory or an accidentally broad root.
    """
    root_real = os.path.realpath(root)
    if not os.path.isdir(root_real):
        return
    prefix = root_real + os.sep
    for dirpath, _dirnames, filenames in os.walk(root_real, followlinks=False):
        for name in filenames:
            if not name.endswith(".jsonl"):
                continue
            real = os.path.realpath(os.path.join(dirpath, name))
            # Confine to the canonical root: reject symlink escapes.
            if real != root_real and not real.startswith(prefix):
                continue
            yield real


def main(argv):
    root = os.path.realpath(session_root(argv))
    files = sorted(iter_session_files(root))
    if not files:
        print(f"no session transcripts under {root}", file=sys.stderr)
        return 1

    class_mass = collections.Counter()
    class_count = collections.Counter()
    class_okfalse = collections.Counter()
    decile_mass = [0] * 10

    grand_total = 0
    grand_fold = 0
    superseded_read_mass = 0
    retired_cmd_mass = 0
    residual_total = 0  # older 75% of each session
    residual_fold = 0
    long_sessions = 0
    long_total = 0
    long_fold = 0
    sessions_with_tools = 0

    for f in files:
        entries = load_entries(f)
        results = tool_results(entries)
        if not results:
            continue
        sessions_with_tools += 1
        fold = foldable_indices(results)

        for idx, tool, te, _args, _path in results:
            class_mass[tool] += te
            class_count[tool] += 1
            grand_total += te

        # ok:false accounting per class (execution-level failures).
        for e in entries:
            m = e.get("message", {})
            if m.get("role") != "tool":
                continue
            try:
                if json.loads(m.get("content")).get("ok") is False:
                    class_okfalse[m.get("toolName") or "unknown"] += 1
            except (json.JSONDecodeError, TypeError, AttributeError):
                pass

        # Split foldable mass by class of the folded result.
        for idx, tool, te, _args, _path in results:
            if idx in fold:
                grand_fold += te
                if tool in ("read", "ls"):
                    superseded_read_mass += te
                else:
                    retired_cmd_mass += te

        ordered = sorted(results, key=lambda x: x[0])
        n = len(ordered)
        for rank, r in enumerate(ordered):
            decile_mass[min(9, rank * 10 // n)] += r[2]

        cut = int(n * 0.75)  # retained-tail model: newest 25% never folded
        for rank, r in enumerate(ordered):
            if rank < cut:
                residual_total += r[2]
                if r[0] in fold:
                    residual_fold += r[2]

        if n >= 40:  # long-session subset: where residency accumulates
            long_sessions += 1
            sess_total = sum(r[2] for r in results)
            long_total += sess_total
            long_fold += sum(r[2] for r in results if r[0] in fold)

    def pct(part, whole):
        return f"{100 * part / whole:.1f}%" if whole else "n/a"

    print("# Residual tool-result mass (ADR-0048 task 0, #378)")
    print()
    print(f"corpus root: {root}")
    print(f"transcripts scanned: {len(files)}")
    print(f"sessions with tool results: {sessions_with_tools}")
    print(f"total tool-result token mass: {grand_total}")
    print()
    print("## By tool class")
    print(f"{'class':<8} {'mass':>10} {'share':>7} {'count':>7} {'okFalse':>8}")
    for tool, mass in class_mass.most_common():
        print(
            f"{tool:<8} {mass:>10} {pct(mass, grand_total):>7} "
            f"{class_count[tool]:>7} {class_okfalse[tool]:>8}"
        )
    print()
    print("## By age (tool-result deciles per session, oldest -> newest)")
    dtot = sum(decile_mass)
    for i, mm in enumerate(decile_mass):
        print(f"  d{i} (oldest+{i}0%): {pct(mm, dtot):>6}")
    print()
    print("## Spent mass (ADR-0048 v1 scope)")
    print(
        f"  superseded reads (read/ls, latest-wins): {superseded_read_mass} "
        f"({pct(superseded_read_mass, grand_total)})"
    )
    print(
        f"  retired command output (grep/find/bash, identical re-run -- "
        f"identical-rerun proxy / upper bound for failure output, incl. "
        f"successful reruns): {retired_cmd_mass} "
        f"({pct(retired_cmd_mass, grand_total)})"
    )
    print(f"  combined foldable share of all tool mass: {pct(grand_fold, grand_total)}")
    print(
        f"  combined foldable share of residual (older 75%): "
        f"{pct(residual_fold, residual_total)}"
    )
    print(
        f"  long sessions (>=40 tool results): {long_sessions}; "
        f"foldable share {pct(long_fold, long_total)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
