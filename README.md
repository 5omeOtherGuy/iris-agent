# Iris

A precise, token-efficient coding agent for the terminal.

This is the owner's manual for humans and AI agents evaluating, operating, or
maintaining Iris. It describes shipped behavior first, names the safety
boundaries, and separates planned work from working code.

> **Status vocabulary**
>
> - **Implemented** means the behavior exists in this repository and is covered by
>   code or tests.
> - **Opt-in** means implemented but disabled until the operator enables it.
> - **Constrained** means implemented with a stated boundary or known gap.
> - **Planned** and **research** do not describe usable product behavior.
>
> Trust code over prose when they disagree. Use
> [the codemap](docs/CODEMAPS/INDEX.md) for the implementation map,
> [the feature inventory](docs/FEATURES.md) for breadth, and
> [the roadmap](docs/ROADMAP.md) for sequencing. Do not infer that an accepted ADR
> has shipped unless code or the current implementation snapshot says so.

---

## What makes Iris different

Iris is not a general agent framework or an autonomous project manager. It is a
single Rust binary for making precise, reviewable changes in a real repository.
Its distinct capabilities are the controls around model context, tool output,
terminal interaction, and recovery:

1. **Tool output is reduced before it enters context.** Native tools return
   bounded, task-preserving output. `bash` recognizes noisy build, test, package,
   lint, and Git output and keeps summaries plus failure detail. `raw: true`
   bypasses filtering for one call.
2. **Large output remains retrievable.** Successful results over 50 KiB move into
   a session-scoped content-addressed store. The model receives a compact preview
   and an `outputHandle`, then pages the original with `read_output` if needed.
3. **Compaction does not freeze the main loop.** A background worker summarizes an
   older closed range while the session continues. Wayland applies the result at
   a safe provider-round-trip boundary; hard pressure has a bounded wait and a
   deterministic fallback.
4. **Spent context can be folded without being destroyed.** Opt-in tool-result
   compaction replaces stale results with deterministic stubs. Originals remain
   in JSONL and are recoverable through `recall` by tool-call id or compaction
   handle.
5. **Prompt-cache cost affects scheduling.** Iris distinguishes a context fold
   from the prefix-cache write it may cause. Cache-aware policies prefer to flush
   folds at compaction, model-switch, or cold-resume boundaries where the prefix
   is already changing.
6. **The terminal remains an instrument, not a chat dashboard.** The default rich
   UI is an alternate-screen pager with an Iris-owned transcript, docked controls,
   live input during model work, and a one-command focus mode. Inline and plain
   renderers preserve operation in multiplexers, pipes, CI, and minimal terminals.
7. **Recovery is explicit and replay-safe.** Sessions, compactions, task
   checkpoints, provider transport fallbacks, and dangerous permission mode are
   durable or auditable. Recovery never silently replays a Codex transport after
   visible output.
8. **Claims have measurement seams.** Provider-reported token flows, cache reads
   and writes, context levels, tool timing, compaction generations, and benchmark
   records have typed homes. Iris does not turn replay-only savings into an
   end-to-end marketing number.

## Install

Prebuilt archives are published for Linux and macOS on x86_64 and aarch64. The
installer downloads the latest stable release, verifies its SHA-256 checksum,
and installs `iris`:

```bash
curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/iris-agent/main/install.sh | sh
```

Set `IRIS_INSTALL_DIR` to choose the destination or `IRIS_VERSION=vX.Y.Z` to pin
a release. Manual installs use the archive and checksum from the
[latest release](https://github.com/5omeOtherGuy/iris-agent/releases/latest).

With Rust installed:

```bash
cargo install iris-agent --locked
```

From a checkout:

```bash
cargo build --release
./target/release/iris --version
```

`iris update` installs only a newer stable tag. Release binaries verify the
archive checksum and replace themselves atomically; source installs rerun
`cargo install` at the selected release tag.

## Start a session

Authenticate, enter a repository, and launch Iris:

```bash
iris login openai-codex
cd my-repository
iris
```

OpenAI Codex login offers browser and device-code flows. Other supported login
commands are listed under [Providers](#providers-auth-and-model-switching).

### Invocation modes

| Command | Behavior |
| --- | --- |
| `iris` | Start an interactive session. A capable TTY uses the rich pager; unsupported terminals degrade safely. |
| `iris --plain` | Force the ANSI-free text REPL. `IRIS_PLAIN=1` and `NO_COLOR` do the same. |
| `iris -c`, `iris --continue` | Resume the newest session for the current directory. |
| `iris resume` | Open the rich resume picker; on a plain/non-TTY path, print resumable sessions. |
| `iris resume <session-id>` | Rebuild context and continue the same JSONL transcript. |
| `iris -p "task"` | Run one headless turn sequence, print only the final answer, then exit. |
| `cat log | iris -p "diagnose"` | Append piped stdin to the headless prompt. |
| `iris -p "apply the fix" --approve` | Auto-approve gated tools for this non-interactive run. Without `--approve`, gated tools are denied rather than prompting. |
| `iris --no-alt-screen` | Use the inline rich renderer. `IRIS_NO_ALT_SCREEN=1` is equivalent. |
| `iris --dangerously-skip-permissions` | Auto-approve every gated call, including destructive calls and safety floors, and save that mode as the global default. Use only inside a trusted external sandbox. |

Print mode uses the same provider loop, tools, settings, persistence, compaction,
and usage events as the interactive path. `IRIS_USAGE_JSON=/path/report.json`
adds an opt-in machine-readable run report without contaminating stdout.

## How a turn works

A normal turn follows one provider-neutral loop:

1. Wayland assembles the system prompt from in-binary Iris instructions, bounded
   user and root-to-working-directory project instruction layers, current date
   and cwd, skill metadata, and the live tool registry.
2. Mimir translates that conversation and tool surface to the selected provider.
3. Nexus consumes streamed text, reasoning summaries, tool-input deltas, activity,
   usage, and completion events. Provider-specific payloads do not leak into the
   core loop.
4. Tool calls are validated, approved when required, executed, persisted, and
   returned as structured results. Safe read-only calls may run in parallel;
   everything else remains sequential.
5. Wayland checks context pressure only after complete tool-call/result pairs.
   It may apply a ready fold or compaction before the next provider request.
6. The loop continues until the model returns without another tool call. There is
   no default round-trip cap; `maxToolRoundtrips` adds a graceful local cap.
7. The completed round trip flushes to JSONL before another provider request.

The rich TUI owns a live input actor beside this loop. A running model does not
freeze keyboard input, settings, cancellation, approvals, or queued messages.

---

## Terminal owner’s manual

### Three render paths

| Path | Selection | What it preserves |
| --- | --- | --- |
| **Pager** | Default when `tui.altScreen=auto` and the terminal supports it | Full-frame alternate screen, pinned session/composer chrome, Iris-owned scrollback, mouse hit-testing, transcript search, sticky prompts, hyperlinks, and viewport-windowed rendering. |
| **Inline** | `--no-alt-screen`, `tui.altScreen=never`, tmux control mode, Zellij, or a failed capability check | Rich transcript and composer on native terminal scrollback; no alternate screen or mouse capture. |
| **Plain text** | `--plain`, `IRIS_PLAIN=1`, `NO_COLOR`, pipes/CI, non-TTY stdio, or rich-TUI startup failure | ANSI-free streamed text, approvals, core slash commands, and structured-question fallback. |

`auto` fails toward inline, never toward a broken pager. `always` overrides
multiplexer heuristics but still cannot force a pager on non-TTY or `TERM=dumb`.
Pager entry and exit are panic-safe; a repeat Ctrl-C restores terminal modes
before force-quit.

### Pane anatomy

The rich UI is one transcript column:

- The **session bar** shows cwd, Git branch/task state, and measured context
  occupancy. It also owns two mutually exclusive dropdowns: directory tree and
  Git console.
- The **transcript** renders user turns, streamed Markdown answers, reasoning
  rails, exploration groups, shell cells, edit diffs, approvals, failures, and
  measured turn receipts.
- The **composer** is multiline and remains editable while a turn runs. Its
  statusline reports model, effort, approval posture, work phase, queue state,
  and context activity without adding a separate dashboard.
- The **start page** appears only for a fresh interactive launch and exposes the
  new-session, resume/task, and settings entry points.

Assistant Markdown supports headings, emphasis, strikethrough, inline and fenced
code, nested bullet/ordered/task lists, quotes, tables, syntax-highlighted code,
and sanitized clickable links. Unicode width and ZWJ shaping are probed and
handled at terminal-cell boundaries. Successful tool groups settle compactly;
errors and diffs remain prominent. Live reasoning uses a bounded tail and commits
to a foldable rail; provider-redacted reasoning is never reconstructed.

### Focus mode

`/focus` is a session-local distraction-free layout, not model input.

- `/focus on` removes the top session bar and collapses an empty composer to one
  bottom metadata row.
- Typing expands the composer; session metadata moves into its top edge.
- `/focus off` returns to automatic behavior.
- Panes 12 rows high or shorter enter the same compact posture automatically.
- The start page and an explicitly opened tree/Git dropdown retain full chrome.

### Input, queueing, and cancellation

| Input | Effect |
| --- | --- |
| `Enter` while idle | Submit the prompt. |
| `Enter` while a turn runs | Queue a steering message for the next safe injection point. |
| `Alt+Enter` while a turn runs | Queue a follow-up after the active turn. |
| `Shift+Enter`, `Ctrl+Enter`, or `Ctrl+J` | Insert a newline. `Ctrl+J` is the reliable fallback when a terminal cannot distinguish Shift+Enter. |
| `Ctrl+C` during work | Cancel the active provider/tool/approval operation and keep a valid transcript. A second Ctrl-C force-quits. |
| `Ctrl+C` with editor text | Clear the editor. With an empty idle editor, exit. |
| `Ctrl+D` with an empty editor | Exit. |
| Up/Down in an empty or single-line editor | Recall submitted prompt history. |

A cancelled tool call receives a real or synthetic cancelled result so the next
provider request never contains a dangling call. The plain fallback still uses a
blocking terminal approval read; its first Ctrl-C cannot preempt that read until
input returns.

### Navigation and controls

| Control | Effect |
| --- | --- |
| `Ctrl+,` | Open the settings faceplate. |
| `Ctrl+L` | Open settings at the model/engine hatch. |
| `Ctrl+P` / `Shift+Ctrl+P` | Cycle forward/backward through the scoped model list. |
| `Shift+Tab` | Cycle reasoning effort supported by the active model. |
| `Ctrl+O` | Expand/collapse transcript panels, including the live reasoning tail. |
| `Ctrl+G` | Toggle the Git console. |
| `@` as the first composer character | Open the directory tree directly in fuzzy-filter mode; selecting a file inserts `@path `. |
| `$` | Open the skill picker and insert an exact, path-qualified skill mention. |
| `PageUp` / `PageDown` | Page pager scrollback. |
| `Alt+Up` / `Alt+Down` | Scroll one line in pager mode. |
| `Home` / `End` with an empty composer | Jump to the start / resume following the live tail. |
| `Tab` in pager scrollback | Move focus between composer and transcript. |
| `n` / `N` after `/find` | Move between transcript matches. |
| `Ctrl+T` or `/mouse` | Toggle pager mouse capture. Turning it off restores terminal-native selection/copy. |

The editor also supports shell-style `Ctrl+A/E/B/F`, `Alt+B/F`, `Ctrl+U/K/W`,
`Alt+D`, `Ctrl+Y`, undo/redo, word-arrow movement, bracketed paste, and ordinary
Home/End/Delete/Backspace behavior.

`/terminal-setup` reports pager mode, multiplexer state, Kitty keyboard protocol,
Shift+Enter support, OSC 52 clipboard routing, and concrete tmux fixes. `/copy`
uses `pbcopy`, `wl-copy`, `xclip`, `xsel`, or Termux tools when available, then
falls back to OSC 52 for remote sessions.

### Directory tree and Git console

The tree uses `git ls-files --cached --others --exclude-standard` inside a Git
repository, so ignored files stay out. Outside Git it uses a bounded directory
walk and skips hidden entries. Directories expand lazily; `/` filters; Enter on a
file inserts a cwd-relative reference.

The Git console is a top-chrome control surface, not a free-running Git agent. It
shows current status, recent branches, linked worktrees, and active task
settlement. It can switch branches, create a branch, create a linked worktree at
`worktreeRoot`, open another worktree as a session, accept a task, or choose a
rollback point. Unmerged paths disable switching. Dirty and unsettled states
require an explicit carry, stash, accept, or rollback decision.

---

## Tools

The standard tool surface is small on purpose. Tool schemas are generated from
the live registry; disabled tools contribute no prompt cost.

| Tool | Implemented behavior | Approval / safety behavior |
| --- | --- | --- |
| `read` | UTF-8 text reads with `offset`/`limit`, byte and line caps, binary/NUL rejection, and optional `skim` that strips comments, docstrings, and blank lines for exploration. | Read-only. A skim does not satisfy read-before-edit. Loaded skills may grant read-only access to their own resource directory. |
| `write` | Create or replace a file, create parents, preserve Unix mode on overwrite, and use same-directory atomic replacement. | Gated; new-file or unified-diff preview. Existing files must have been observed and must still match the last read. |
| `edit` | Unique exact-string replacement, `replace_all`, whitespace/Unicode-normalized fallback matching, and atomic replacement. | Gated; exact diff preview; same observation/freshness rule as `write`. |
| `bash` | One-shot commands; optional timeout; persistent named shells; background jobs with start/poll/finalize/list/cancel; process-group cancellation; bounded capture; exit/duration metadata; native output filters. | Gated every call. Destructive commands always re-prompt in normal modes. Linux Landlock is an explicit opt-in; see [Confinement reality](#confinement-reality). |
| `grep` | In-process ripgrep search with files/content/count modes, context lines, case and literal controls, bounded result sets, and exact per-file omission counts. | Read-only and safe-parallel. No `rg` binary required. |
| `find` | In-process ignore/glob walk, sorted results, exact truncation totals, top omitted directories, and grouping only when smaller. | Read-only and safe-parallel. No `fd` binary required. |
| `ls` | Directories-first listing, recursive tree depth, scan cap, and optional type/size metadata. | Read-only and safe-parallel. |
| `AskUserQuestion` | One to four single- or multi-select questions with previews, recommendations, `Other`, review, cancellation, and bounded “Chat about this” feedback. | Always requires human interaction; approval presets cannot answer it. |
| `read_output` | Page a session-scoped oversized-output handle by line offset and limit. An oversized dereference is re-offloaded instead of flooding context. | Read-only; handle ids are validated and cannot become filesystem paths. |
| `recall` | Recover original turns behind a compaction handle, a transcript range, or one folded tool-call id; supports bounded windows and search. | Read-only over the current session transcript. It cannot read arbitrary files. |
| `web_search` | Return a ranked, snippet-rich result list through native DuckDuckGo HTML, Brave, Jina, or a trusted SearXNG instance. | **Opt-in, off by default, approval-gated.** Global settings control egress and bounds. |
| `read_web_page` | Fetch public HTTP(S), extract readable Markdown, and optionally return objective-focused excerpts through native or Jina backends. | **Opt-in, off by default, approval-gated.** Private, loopback, link-local, and internal targets are refused and connections are pinned against DNS rebinding. |
| `request_compaction` | Let the model schedule one compaction at the next pair-closed boundary. It accepts no authority-bearing arguments. | **Opt-in** through `compaction.modelTool`; it only sets a one-shot flag. |

`bashToolMode=true` narrows the visible surface to `bash`, `edit`,
`AskUserQuestion`, `read_output`, and `recall` (plus enabled web/compaction tools).
It is a prompt/tool-shape preference, not a permission bypass: shell calls still
use the normal gate.

### Native output reduction

`bash` filters captured output after command completion and before the final
50-KiB / 2,000-line tail bound. Structured reducers cover Cargo build/check/test
and Clippy, Git status/log/diff, and npm/pnpm test output; declarative filters
cover dozens of other noisy commands.

The invariants are stricter than “short output”:

- nonzero exits retain failure diagnostics;
- panic lines, failing-test names, compiler locations, and diff hunks survive;
- exit codes and command semantics never change;
- a filter parse/compile error returns raw output rather than inventing a summary;
- `raw: true` bypasses the reducer;
- the full captured result can be retained behind a session handle.

Committed corpus tests currently pin minimum reductions around 98% for a passing
Cargo build, 85–94% for passing Cargo tests, 79% for npm install, 68–70% for
passing npm/vitest, 62% for Git log, 58% for lockfile-heavy Git diff, and 50% for
Git status. These are per-result render measurements, not an end-to-end task-cost
claim.

---

## Context, handles, compaction, and recall

### Layer 1: bounded results and output handles

Every tool returns a bounded inline representation. Results larger than 50 KiB
are written beside the session transcript under a truncated SHA-256 id. The
provider sees a head/tail preview, byte/line metadata, and an `outputHandle`.
`read_output` retrieves only the requested window.

This happens before the provider-visible message enters context, so resume never
re-inlines the payload.

### Layer 2: automatic background compaction

Iris resolves a model-aware effective context window and optionally clamps it
with `contextTokenBudget`. The default pressure ladder is:

| Tier | Default | Action |
| --- | ---: | --- |
| warn | 60% | Surface pressure; do not rewrite context. |
| start | 72% | Start one background summarizer and keep the turn moving. |
| hard | 90% | Wait up to `hardWaitMs` for safe relief, then use finite fallbacks. |

The worker covers only closed provider round trips, preserves complete tool
call/result pairs, and retains a recent tail (8,000 tokens by default). A ready
summary applies at the next safe boundary. Under hard pressure Iris can shrink
the worker range, use provider-native compaction when explicitly enabled and
compatible, fall back to deterministic excerpts, and perform a final deep cut.
The parent process alone validates, persists, and applies the result.

Each durable compaction records its generation, covered entry range, original and
summary token estimates, structured carry paths, instructions/focus, origin,
worker usage, and a recall handle. Resume rebuilds through the summary while the
original JSONL rows remain intact.

Useful controls:

```text
/context                 live context composition, headroom, folds, and worker
/compact                  compact now
/compact focus on tests   compact with a bounded handoff focus
/compaction               inspect the latest durable generation
/compaction 3             inspect generation 3
```

`compaction.reactive=true` (the default) also handles a provider-classified
context overflow before visible output. Iris applies deterministic relief and
retries once; a second overflow reports measured context and recovery commands.

### Layer 3: recoverable tool-result compaction

`toolResultCompaction` is implemented and default-off. It can combine:

- **semantic dedupe**: keep the newest N results per file path and fold superseded
  reads;
- **tool clearing**: fold older eligible results after count/token guards;
- **local or Anthropic-native backends** with overlap rejection;
- **replayable/all-recoverable modes**, explicit exclusions, failure policy, and
  optional input clearing;
- **cache timing**: `breakOnly`, `cacheAware`, `pressureOnly`, or `immediate`.

Every fold is a durable entry. The provider-visible stub identifies the original
call and the exact `recall(tool_call_id="...")` retrieval route. Recent results,
active compaction ranges, mutation tools, `recall`, and `read_output` are
protected by default.

The legacy `microcompaction=true` plus `microcompactionWatermark` remains a
conservative alias; its default watermark is 64,000 tokens.

### Cache-aware prompt behavior

`promptCacheRetention` is global-only: `none`, `short` (default), or `long`.
Mimir translates it only where a provider has a public control. Iris keeps stable
prompt/tool prefixes, classifies model transitions, records provider-reported
cache reads/writes when available, and warns on a proven stable-prefix break
rather than treating an ordinary cold cache as an error.

A reasoning-only switch keeps the prefix warm. A model/provider switch starts a
new cache lane and, for a large context, advises manual compaction first. Codex
prompt-cache identity remains session-scoped; Iris does not merge transport
sessions to chase unproven cross-session cache reuse.

### What is measured, and what is not

Iris has one arithmetic path for provider turns, input/output tokens, prompt-cache
reads/writes, hidden reasoning tokens, latest context level, generation timing,
and output rate. The exit receipt shows only fields actually reported.

The deterministic tokens-per-task replay currently shows lower prompt input on
its fixtures with identical mechanical success and retained task facts. Real
provider confirmation remains the gate for an end-to-end headline. Iris therefore
claims measured render reductions, not a universal “X% cheaper per task.”

---

## Sessions and recovery

### Durable transcript

Each session is a versioned JSONL file under:

```text
${IRIS_SESSION_DIR:-~/.iris/sessions}/<cwd-slug>/<timestamp>_<session-id>.jsonl
```

The header has a stable session id. Entries have stable ids and `parentId` links,
provider-turn ids, token estimates, model-selection audits, dangerous-mode
audits, transport fallback records, folds, compactions, and task linkage. Each
append flushes, so a crash leaves a readable prefix. Incomplete trailing JSON is
ignored during recovery; dangling tool calls are repaired before reuse.

Implemented session operations:

- resume newest with `-c`;
- list/pick/resume by id;
- `/resume` another session at an idle boundary;
- `/new` without restarting Iris;
- `/session` for id, path, message count, context estimate, and active model;
- `/copy last|all` for assistant output;
- `/debug` for a sanitized screen plus provider-visible-context snapshot at
  `~/.iris/iris-debug.log`;
- prompt history and current-directory filtering in the rich picker.

Conversation entry ids are tree-ready, but transcript branching/fork navigation
is **planned**, not implemented.

### Provider waits and transport recovery

All providers emit neutral activity while bytes arrive, including reasoning and
tool-input frames that do not yet produce visible text. Anthropic, Antigravity,
OpenAI API, and OpenAI-compatible streams use a 90-second translated-event idle
guard plus a 30-minute whole-request backstop.

OpenAI Codex has a provider-specific policy because a generic 90-second event
guard is too short for its interactive transport:

- `codexStreamIdleTimeoutMs` is global-only and defaults to 300,000 ms; `0`
  disables only raw-read idleness.
- The sliding timeout applies to both WebSocket frames and HTTPS/SSE reads.
- Before visible output, WebSocket setup/read failures consume the shared retry
  budget with cancellation-aware backoff. Reconnect classification, count, and
  delay are shown without leaking provider payloads.
- After retries are exhausted, Iris switches once to sticky HTTPS/SSE for that
  session and persists one allow-listed fallback record.
- After text, reasoning summary, or tool-input output becomes visible, transport
  failure is fatal. Iris will not silently replay a partial response and risk
  duplicate text or tool execution.
- Default retry policy is three transient retries, 2-second exponential base,
  60-second ceiling, jitter, and bounded `Retry-After` handling. `retry` is
  global-only.

### Task and mutation recovery

Mutation safety is on by default. At the first mutating call, Iris snapshots the
repository's existing dirty/untracked state and index, then protects any path the
agent would touch. Dirty-file approval is per path and per task; a model cannot
silently overwrite a user's uncommitted bytes.

The durable task workflow (`tasks=true`) is opt-in. When enabled it adds:

- opaque task ids linked to every participating session;
- a per-task process lease and repository mutation lock;
- Git checkpoint commits under `refs/iris/checkpoints/<task-id>/`, built with a
  temporary index so HEAD, the user's index, stash, branches, and tags are not
  moved;
- a checkpoint after each attributed mutation and `/checkpoint` on demand;
- `/diff` over Iris-attributed paths only;
- `/rollback` to pre-task or intermediate state, preserving any path the user
  changed after Iris's last write;
- `/accept` settlement and task-scoped checkpoint cleanup;
- crash recovery, orphan discovery/adoption, session lookup, expiry, and notices
  when external Git activity settles or diverges the task.

A non-Git workspace uses content snapshots for rollback where possible and
surfaces degraded guarantees rather than pretending Git semantics. A jj workspace
requires explicit native-jj consent; without it, mutation safety reports a
file-only degraded mode. Linked-worktree creation is implemented in the Git
console, but a complete isolated-worktree-per-task service and apply/settlement
boundary remain planned.

`verify` can run a configured project command after a turn changes files. It is a
normal gated shell call, never auto-detected. Failure output returns to the model
for another fix only after the model makes a change, up to `maxAttempts` (default
3, hard cap 10). A failed check leaves the task unsettled and rollbackable.

---

## Safety and permissions

### Approval modes

`/approval` and `defaultApproval` select one of four postures:

| Mode | Behavior |
| --- | --- |
| `strict` | Default. Prompt for each gated call not covered by a narrower grant. |
| `auto` | Auto-run only calls Iris proves safe: currently clean, in-workspace `edit`/`write`. Prompt for everything else. Safety floors still win. |
| `never` | Never prompt. Calls requiring a prompt are denied and returned to the model. Existing non-floor grants still apply. |
| `dangerously-skip-permissions` | Bypass the gate and all floors. Loud, audited, global-only, and persistent until changed. |

Normal approval choices are allow once, allow for the session where the tool
permits it, persist for this project where permitted, or deny. `bash`, destructive
calls, and other arbitrary-effect paths opt out of blanket session allow. A
project grant stores a non-shell tool name or an exact/token-boundary shell
command in `~/.iris/trust.json`, keyed by canonical cwd. `/trust` and
`/permissions` inspect, add, toggle, or revoke those grants.

The trust store is HOME-owned. A cloned repository cannot grant itself permission,
change the default approval mode, enable dangerous skip, or redirect the trust
store into the project. Destructive commands always re-prompt unless the operator
has deliberately enabled dangerous skip.

### File integrity

- Existing files must be read before `write` or `edit` mutates them.
- Iris records mtime plus content hash; a real content change since the read is a
  conflict. Benign timestamp-only changes refresh safely.
- Writes use same-directory temp files, fsync, rename, cleanup on failure, and Unix
  mode preservation.
- Mutating previews come from the requested operation, not an unrelated whole-tree
  `git diff`.
- Dirty-tree attribution distinguishes user bytes from Iris bytes for previews,
  rollback, and final diff.

### Confinement reality

**Workspace path confinement and the Linux shell sandbox are currently explicit
development opt-ins, not default enforcement.** Set:

```bash
IRIS_SECURITY_OPT_IN=1 iris
```

With the opt-in, file tools reject absolute/traversal/symlink escapes. On Linux,
Landlock allows writes only to the workspace, temp directories, and `/dev/null`,
and denies TCP bind/connect when the kernel supports the required ABI. Reads and
program execution remain unrestricted; UDP, raw sockets, and already-bound Unix
sockets are outside this Landlock policy. Older kernels report filesystem-only or
unconfined fallback rather than hiding it.

Without the opt-in, path tools resolve outside-workspace paths and shell commands
run unconfined. The `auto` approval preset still refuses to silently approve an
outside-workspace mutation. macOS has no shell sandbox backend and always reports
unconfined shell execution. Do not treat an approval prompt as a sandbox.

### Network tools

Web tools form a separate, off-by-default egress class. Their backend, endpoint,
timeouts, result count, and byte caps are global-only so project config cannot
turn on egress or choose where queries go. Every call is approval-gated. Fetched
content is marked as untrusted external data before it reaches the model, and the
marker survives excerpting/reduction.

---

## Providers, auth, and model switching

Iris supports five provider routes. Provider adapters own credentials, endpoints,
wire formats, cache controls, reasoning replay, retries, and stream parsing;
Nexus sees one neutral contract.

| Provider id | Route and auth | Notes |
| --- | --- | --- |
| `openai-codex` | ChatGPT Codex Responses over OAuth; browser or device code | Default when no provider/key setting selects another route. WebSocket-first with session-sticky SSE recovery. |
| `openai` | OpenAI Chat Completions with API key | `iris login openai` or `OPENAI_API_KEY`. |
| `anthropic` | Anthropic Messages through Claude Code OAuth or API key | Browser PKCE with manual paste fallback, existing Claude Code credential/keychain reuse, or `iris login anthropic --api-key` / `ANTHROPIC_API_KEY`. |
| `antigravity` | Gemini Code Assist through Google OAuth | Requires `ANTIGRAVITY_CLIENT_SECRET` at login/refresh unless embedded by the builder; persists project discovery and Gemini thought signatures. |
| `openai-compatible` | Configurable Chat Completions endpoint | Defaults to local `http://localhost:11434/v1`; can run without auth or use a dedicated stored/API env key. It never reuses `OPENAI_API_KEY`. |

Credentials live in `~/.iris/auth.json` unless `IRIS_AUTH_PATH` overrides it.
Writes are atomic and use restricted Unix permissions. Stored API keys win over
environment variables. Refreshable OAuth tokens are rotated back to their source
without dropping sibling credential fields.

`/model` and `/reasoning` use one typed per-model capability map shared by startup
validation, request construction, settings, selectors, and transitions. The UI
shows only supported effort labels. Unsupported request fields are omitted; a
model switch preserves a supported level or reports the clamp. Provider-origin
reasoning is replayed only to the same compatible origin, so switching providers
does not leak or send an invalid reasoning block.

`/scoped-models` controls the ordered `Ctrl+P` cycle. Changes apply to the current
session immediately; `Ctrl+S` persists them globally. Runtime provider/model/
effort changes apply at safe boundaries and append an audit entry.

---

## Skills and instructions

Iris loads Codex-compatible filesystem skills. A skill is a directory containing
`SKILL.md` with YAML `name` and `description`; optional `agents/openai.yaml`
metadata can provide display text, dependencies, products, and implicit-invocation
policy.

Discovery order covers:

- `.agents/skills` from repository root down to cwd;
- legacy `<repo>/.codex/skills`;
- `~/.agents/skills`;
- `$CODEX_HOME/skills` and `skills/.system`;
- `~/.iris/skills`;
- `/etc/codex/skills` and `/etc/iris/skills`.

Iris deduplicates canonical paths, bounds scan depth and count, honors Codex
`skills.include_instructions` plus ordered enable rules, and treats malformed
optional metadata as non-fatal. Only name, description, and source path enter the
initial catalog, capped at 2% of the context budget. The full body enters a
lower-authority contextual message only after selection.

Invoke explicitly with `$name`, `$`, or `/skills`. Duplicate names use a
path-qualified `skill://` mention. The model may invoke from the catalog unless
`allow_implicit_invocation: false`. A selected skill may expose its own directory
to `read`; it does not gain mutation access outside the workspace.

System-prompt fragments themselves are compiled into Iris. Files under old
`.iris/fragments` locations are not loaded. The tracked root `AGENTS.md` is the
public repository guide; root `CLAUDE.md` imports it. Canonical repository skills
live under `.agents/skills`, with relative `.claude/skills` projections for
Claude Code and no duplicate `.pi/skills` tree.

User instructions load from `~/.agents/AGENTS.md`, then
`~/.iris/AGENTS.md`. Project instructions load root-to-leaf. Each directory
selects the first non-empty regular base from `AGENTS.override.md`, `AGENTS.md`,
then `CLAUDE.md`, followed by the first non-empty local candidate from
`AGENTS.local.md`, then `CLAUDE.local.md`. Each selected document is capped at
32 KiB. User-level paths may be symlinks to regular files; project candidates
refuse symlinks and non-regular files and emit deduplicated warnings.

Ignored local instruction files use harness-native semantics. Claude Code loads
`CLAUDE.local.md`; trusted Pi projects may append `.pi/APPEND_SYSTEM.md`.
`.worktreeinclude` supports harness-managed copies, while
`scripts/worktree-create.sh` supplies the same regular-file-only local layer to
repository-created plain Git worktrees.

---

## Settings and customization

### Files and precedence

```text
~/.iris/settings.json          global, operator-owned
<cwd>/.iris/settings.json      project, restricted to project-safe fields
```

`IRIS_CONFIG_PATH` replaces the global path. A malformed settings file fails
startup; unknown keys are ignored so older binaries tolerate newer files.
Project-safe values override global values. Security-, credential-, provider-,
and egress-bearing values remain global-only even if a project file contains
them.

Start with:

```json
{
  "defaultProvider": "openai-codex",
  "defaultModel": "gpt-5.6-sol",
  "defaultReasoning": "high",
  "tui": {
    "altScreen": "auto",
    "scrollSpeed": 3,
    "reducedMotion": false,
    "theme": "terminal"
  },
  "compaction": {
    "enabled": true,
    "thresholds": { "warn": 0.60, "start": 0.72, "hard": 0.90 },
    "keepRecentTokens": 8000,
    "hardWaitMs": 120000,
    "reactive": true
  }
}
```

`/settings` is the preferred interactive editor. It exposes engine/provider
controls, approval posture, permissions, compaction, tool-result compaction, web
backends and bounds, verification, themes, screen behavior, mutation safety,
native jj consent, and worktree location. Changes that cannot safely alter an
active operation are queued and applied at the next boundary.

### Setting reference

| Key | Scope | Implemented behavior |
| --- | --- | --- |
| `defaultProvider` | global | `openai-codex`, `openai`, `anthropic`, `antigravity`, or `openai-compatible`. |
| `defaultModel` | project-safe | Startup model id. `IRIS_MODEL` has higher precedence. |
| `baseUrl` | global | Endpoint for the initially selected provider; never accepted from project config. |
| `defaultReasoning` | project-safe | Normalized effort/budget label, validated against the selected model. |
| `enabledModels` | global | Ordered qualified ids for model cycling. |
| `openAiCompatible` | global | `contextWindow`, `reasoning`, and `apiKeyRequired` metadata for a custom endpoint. |
| `promptCacheRetention` | global | `none`, `short` (default), or `long`; sent only where supported. |
| `anthropicContextManagement` | global | Explicit public clear-tool-use / clear-thinking edits. |
| `retry` | global | `maxRetries`, `baseDelayMs`, `maxDelayMs`. |
| `codexTransport` | global | `auto` (WebSocket then sticky SSE recovery) or `sse`. |
| `codexStreamIdleTimeoutMs` | global | Sliding raw-read timeout; default 300,000; `0` disables this detector. |
| `contextTokenBudget` | project-safe | Absolute clamp on the model-aware effective context window; minimum 8,192. |
| `maxToolRoundtrips` | project-safe | Optional graceful provider/tool-loop cap; absent means no fixed cap. |
| `bashToolMode` | project-safe | Replace the ordinary file/search surface with the shell-centered surface. |
| `mutationSafety` | global | Dirty-tree guard and snapshot master; default true. |
| `tasks` | project-safe | Durable task/checkpoint/recovery UI; default false and requires mutation safety. |
| `worktreeRoot` | project-safe | New linked-worktree directory; default `../wt` beside the main worktree. |
| `defaultApproval` | global | `strict`, `auto`, `never`, or `dangerously-skip-permissions`. |
| `verify` | project-safe | `command` and `maxAttempts` (default 3, cap 10). Runs under the normal shell gate. |
| `tui` | project-safe | `altScreen`, `scrollSpeed` (1–100), `reducedMotion`, and `theme`. |
| `compactionSummarizer` | project-safe | `provider` (default), `subagent`, or `excerpts`. |
| `compaction` | mixed | Project-safe: `enabled`, thresholds, `keepRecentTokens`, `hardWaitMs`, `maxConsecutiveFailures`, `reactive`, `instructions`, `modelTool`, worker input/timeout/roundtrips. Global-only: `providerNative`, worker model. |
| `microcompaction` | project-safe | Legacy conservative tool-result folding alias; default false. |
| `microcompactionWatermark` | project-safe | Legacy/cache-pressure fold trigger; default 64,000 tokens. |
| `toolResultCompaction` | mixed | Local policy is project-safe; provider-native backend controls remain global-only. |
| `webSearchBackend` | global | `off`, `native`, `brave`, `jina`, or `searxng`. |
| `readWebPageBackend` | global | `off`, `native`, or `jina`. |
| `searxngUrl` | global | Trusted absolute HTTP(S) endpoint required by the SearXNG backend. |
| `searchTimeoutMs`, `readTimeoutMs` | global | Per-call 1–120 second deadlines; default 30,000 ms. |
| `maxSearchResults` | global | 1–10; default and hard maximum 10. |
| `maxSearchResponseBytes`, `maxReadResponseBytes`, `maxReadOutputBytes` | global | 1 KiB–10 MiB; default 200 KiB each. |

`toolResultCompaction` exposes `enabled`, `aggressiveness`, `cacheTiming`,
`triggerTokens`, `semanticDedupe` (`enabled`, `retainPerPath`, recent result/token
guards), and `toolClearing` (`enabled`, backend, mode, recent count, minimum token
reclamation, eligible/excluded tools, failure inclusion, and input clearing).
Invalid overlap between local and provider-native reducers fails startup.

### Themes

`terminal` is the adaptive default and uses the terminal's ANSI roles, including
light themes and `NO_COLOR`. Fixed palettes are opt-in:

```text
gruvbox
catppuccin-latte
catppuccin-frappe
catppuccin-macchiato
catppuccin-mocha
nord
tokyo-night
dracula
rose-pine
solarized
everforest
```

An unknown id warns and falls back to `terminal`. `IRIS_REDUCED_MOTION=1` overrides
the setting and freezes working indicators, pacing, and detent flashes.

### Environment reference

| Variable | Purpose |
| --- | --- |
| `IRIS_AUTH_PATH` | Auth store; default `~/.iris/auth.json`. |
| `IRIS_CONFIG_PATH` | Global settings file; default `~/.iris/settings.json`. |
| `IRIS_TRUST_PATH` | Project-grant store; override must be absolute and outside the project. |
| `IRIS_SESSION_DIR` | Session/output root; default `~/.iris/sessions`. |
| `IRIS_MODEL` | Highest-precedence startup model override. |
| `IRIS_CODEX_BASE_URL` | Codex endpoint override. |
| `OPENAI_API_KEY` | OpenAI API credential and implicit provider selection when no provider is configured. |
| `ANTHROPIC_API_KEY` | Anthropic API credential and implicit provider selection when no provider is configured. |
| `OPENAI_COMPATIBLE_API_KEY`, `IRIS_OPENAI_COMPATIBLE_API_KEY` | Dedicated custom-endpoint key. Never inferred from `OPENAI_API_KEY`. |
| `CLAUDE_CONFIG_DIR` | Claude Code credential/config root used for Anthropic reuse. |
| `ANTIGRAVITY_CLIENT_SECRET` | Google OAuth secret required for Antigravity login/refresh unless built in. |
| `ANTIGRAVITY_PROJECT_ID` | Optional project-id override. |
| `BRAVE_API_KEY`, `JINA_API_KEY` | Credentials for enabled web backends. |
| `CODEX_HOME` | Codex config/skill root. |
| `IRIS_SECURITY_OPT_IN` | Enable workspace path enforcement and Linux Landlock policy. |
| `IRIS_PLAIN`, `NO_COLOR` | Force plain rendering. |
| `IRIS_NO_ALT_SCREEN` | Force inline rich rendering. |
| `IRIS_REDUCED_MOTION` | Disable animation/pacing. |
| `IRIS_USAGE_JSON` | Headless usage-report destination. |
| `RUST_LOG` | Structured diagnostic logging to stderr. |

---

## Slash command reference

Typing `/` opens a filtered command palette. A multiline paste beginning with `/`
is treated as ordinary prompt text, not hijacked as a command.

### Session and context

| Command | Action |
| --- | --- |
| `/new` | Start a fresh transcript at an idle boundary. |
| `/resume` | Pick and resume a prior session for this directory. |
| `/session` | Show id, transcript path, message count, context estimate, and model. |
| `/copy [last|all]` | Copy assistant output. |
| `/context` | Show system/tools, raw/summarized conversation, folds, worker state, and headroom. |
| `/compact [focus]` | Run manual compaction with optional focus. |
| `/compaction [generation]` | Inspect a durable compaction entry. |
| `/debug` | Write a sanitized screen/context snapshot. `/dbug` is an unlisted alias. |

### Engine, auth, and policy

| Command | Action |
| --- | --- |
| `/model [qualified-id]` | Open engine settings or switch model/provider. |
| `/reasoning [level]` | Open engine settings or change effort. |
| `/scoped-models` | Edit the ordered model cycle. |
| `/settings` | Open the settings faceplate. |
| `/approval <mode>` | Set approval posture. |
| `/trust`, `/permissions` | Edit this canonical cwd's persistent grants. |
| `/login`, `/logout` | Open provider credential controls. |
| `/skills` | Browse and mention an installed skill. |

### Terminal and repository

| Command | Action |
| --- | --- |
| `/find [query]` | Search pager transcript; `n`/`N` move, bare `/find` clears. |
| `/focus [on|off]` | Toggle focus layout. |
| `/mouse` | Toggle pager mouse capture. |
| `/terminal-setup` | Diagnose terminal/multiplexer/key/clipboard capabilities. |
| `/tree` | Open the directory tree. |
| `/git` | Open branch/worktree/task console. |

### Tasks

| Command | Action |
| --- | --- |
| `/tasks` | Review active or recoverable durable tasks. |
| `/task` | Show task workflow help. |
| `/sessions` | List sessions linked to a task id. |
| `/diff` | Show Iris's net task diff. |
| `/checkpoint` | Save a rollback point without settling. |
| `/rollback` | List or restore task checkpoints. |
| `/accept` | Accept Iris-attributed changes and settle the task. |

`/exit` and `/quit` end the session.

The text fallback implements exit, model/reasoning, copy/session, compact/context,
and structured approvals directly. Rich-only menus degrade to notices rather than
pretending an interactive surface exists.

---

## Architecture for maintainers and agents

Iris ships as one crate and one product binary, with inward-pointing module
boundaries:

```text
╭────────────────────────────────────────────────────────────────╮
│ Iris CLI / adapters                                             │
│ terminal, concrete tools, approval UX, Mimir provider/auth      │
╰──────────────────────────────┬─────────────────────────────────╯
                               ▼
╭────────────────────────────────────────────────────────────────╮
│ Wayland harness                                                 │
│ sessions, settings, skills, context, handles, mutation safety   │
╰──────────────────────────────┬─────────────────────────────────╯
                               ▼
╭────────────────────────────────────────────────────────────────╮
│ Nexus core                                                      │
│ provider-neutral loop, events, tool and approval contracts      │
╰────────────────────────────────────────────────────────────────╯
```

Nexus imports no terminal, concrete provider, session store, or concrete tool.
Wayland owns the execution environment and durable context. Mimir owns provider
names, credentials, endpoints, transport policy, and wire translation. The UI
renders typed events and never decides authorization.

Blocking HTTP, filesystem walks, Git scans, and tool bodies are kept off the UI
actor where required. Provider reads, tools, approvals, compaction workers, and
turn cancellation meet through explicit async contracts rather than one giant
agent function.

Read [Architecture](docs/ARCHITECTURE.md), [Naming](docs/NAMING.md), and the
[current codemap](docs/CODEMAPS/INDEX.md) before changing a tier boundary.

---

## Capability status

### Implemented now

- Interactive pager, inline rich UI, and ANSI-free text fallback.
- Always-live composer with steering/follow-up queues and turn cancellation.
- Streamed Markdown, syntax highlighting, links, reasoning summaries, live shell
  cells, diffs, fold controls, transcript search, focus mode, terminal doctor,
  directory tree, Git console, themes, reduced motion, and clipboard ladder.
- Five provider routes with typed model capabilities, OAuth/API-key auth,
  runtime switching, retries, idle detection, and provider-safe reasoning replay.
- Native `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, structured user
  questions, output dereference, and compaction recall.
- Opt-in web search/page read with SSRF protection and bounded egress.
- Structured tool results, native noisy-output reduction, output handles, token
  estimates/usage, model-aware context pressure, background/manual/reactive
  compaction, and opt-in recoverable tool-result compaction.
- JSONL sessions with continue/resume/new, compaction-aware rebuild, audit rows,
  and crash-prefix recovery.
- Approval presets, HOME-owned project grants, file freshness checks, atomic
  mutation, dirty-tree protection, diff previews, and opt-in durable task
  checkpoints/rollback/verification.
- Codex-compatible skills with progressive disclosure and turn-boundary refresh.
- Prebuilt install/update flow and a separate `iris-bench` executable for
  real-provider, replay, and report workflows.

### Implemented with constraints

- Workspace path and Landlock confinement exist but require
  `IRIS_SECURITY_OPT_IN=1`; macOS shell execution is unconfined.
- Durable task workflow exists but is default-off. Mutation safety itself is
  default-on in Git workspaces.
- Provider-native compaction exists behind explicit capability/setting gates.
  Unsupported/rejected routes fall back to portable summaries.
- Anthropic context management and native tool clearing are opt-in and validated
  against local reducers.
- Session ids are tree-ready, but the product exposes linear resume rather than
  conversation branching.
- End-to-end token savings have deterministic replay evidence; real-provider
  campaign confirmation is not yet a headline claim.

### Planned — not implemented

The following are roadmap targets, not commands or guarantees:

- named mode profiles that change prompt/tool/compaction policy;
- general subagents as model-facing tools, background worker fleets, per-worker
  routing/budgets, and mutable worker isolation;
- a complete per-task linked-worktree service with explicit apply/settlement,
  pooling, adoption, and remote restore;
- conversation branching/fork navigation and richer session search;
- a full token-budget planner and context ledger with reason-based eviction,
  diff-aware file context, handle indexing/search, lifecycle management, and a
  handle browser;
- multimodal image, PDF, and notebook reads;
- content-hash-anchored edit syntax and provider-native patch surfaces;
- per-hunk staging, pre-commit review, approved auto-commit, and automated PR
  construction;
- GitHub issue/PR/review/CI/stacked-PR workflows;
- a ranked tree-sitter repository map;
- macOS Seatbelt confinement and a stronger cross-platform network sandbox.

### Research or explicitly uncommitted

A third-party plugin system is exploratory. No WASM or subprocess plugin runtime,
manifest contract, identity-based plugin approval, or extension marketplace is
implemented or scheduled. Iris is a product, not an SDK surface for embedding a
runtime into other agents.

---

## Verification and development

Repository work uses task-specific Git worktrees. From a clean primary checkout:

```bash
bash scripts/worktree-create.sh ../iris-my-task feat/my-task
```

The wrapper runs primary freshness preflight, creates from `origin/main`, and
copies only the ignored regular instruction files listed in `.worktreeinclude`.
A direct `git worktree add` receives tracked guidance and skills but not ignored
local layers.

Run the full CI-equivalent gate in the task worktree:

```bash
bash scripts/gate.sh
```

The gate runs formatting, Clippy, and tests. Focused development can use:

```bash
cargo test
cargo test <name>
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Real-provider and live benchmark tests are ignored and additionally opt-in; an
ordinary gate does not spend provider credits. See
[Benchmark plan](docs/BENCHMARK_PLAN.md) and the `iris-bench` help output before
running a paid campaign.

## Documentation map

- [Public agent guide](AGENTS.md) — repository commands, boundaries, checks, and worktree policy.
- [Current codemap](docs/CODEMAPS/INDEX.md) — implemented modules and entry points.
- [Feature inventory](docs/FEATURES.md) — status-tagged breadth; verify stale tags
  against code.
- [Roadmap](docs/ROADMAP.md) — sequence, acceptance gates, and deferred work.
- [Architecture](docs/ARCHITECTURE.md) — tier ownership and dependency direction.
- [ADR index](docs/adr/README.md) — decisions, status, amendments, and tradeoffs.
- [TUI design language](docs/TUI_DESIGN_LANGUAGE.md) — canonical terminal grammar.
- [OpenWiki manual](openwiki/README.md) — offline subsystem guides.
- [Release runbook](docs/RELEASING.md) — operator-only release procedure.

## Platform status

Linux and macOS are supported on x86_64 and aarch64. Windows is not supported.
Iris is pre-1.0: session formats are versioned and read compatibly, but commands,
settings, and UI details may still change.

## License

[MIT](LICENSE). Files derived from [OpenAI Codex](https://github.com/openai/codex)
carry SPDX headers and remain under Apache License 2.0; [NOTICE](NOTICE) identifies
them.
