# Iris

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/hero-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/assets/hero-light.svg">
  <img alt="Iris terminal banner. A user asks: What are you? The thinking indicator pulses, and the answer is: A precise, token-efficient coding agent for the terminal." src="docs/assets/hero-dark.svg" width="640">
</picture>

A coding agent for the terminal, built around token efficiency.

Iris is a single native binary. It runs an interactive REPL in your terminal,
talks to multiple LLM providers, and drives a small set of workspace tools
(read, write, edit, bash, grep, find, ls) behind approval gates. What makes it
distinct is the context layer: Iris measures the token cost of every tool result
and every context rewrite, and spends that budget deliberately so long,
tool-heavy sessions stay readable and stay within their window.

---

## Highlights

- **Token-efficient tool output, measured, not asserted.** Every native tool
  returns the smallest result that preserves task success, and `bash` filters
  command output inside the runtime before it reaches the transcript. Reductions
  are pinned by tests against a corpus of real command output — for example
  ~98% on a passing `cargo build`, ~85–94% on `cargo test`, ~79% on
  `npm install`. Failure detail (failing-test names, panics, `file:line`,
  compiler diagnostics, diff hunks) is exempt and survives verbatim.

- **Non-blocking background compaction.** When context crosses the `start`
  threshold, Iris launches a summariser worker and immediately keeps working.
  The compacted context is computed off the main loop and swapped in at a later
  round-trip boundary, so a summary that finishes mid-loop applies as soon as
  it is ready instead of stalling the turn. Under hard pressure it waits only up
  to a bounded budget, then falls back deterministically.

- **Reversible microcompaction (opt-in).** Instead of deleting spent context,
  Iris folds stale tool results into deterministic, ID-tagged stubs. The
  original call/result stays in the session transcript, and the model can pull
  any of them back with the read-only `recall` tool. Folding is deterministic
  and loses nothing that is not recoverable.

- **Cache-aware fold-flush scheduling.** Folds carry a cost: flushing them
  rewrites the prompt prefix, which forces a cache write. Iris models this with a
  provider-neutral `CacheProfile` and prefers to release pending folds at moments
  the prefix cache breaks anyway — a compaction, a model or provider switch, a
  cold resume, or a manual `/compact` — where the flush is effectively free. A
  configurable token watermark remains as a pressure backstop. (Measured: a
  warm, badly-timed fold flush cost ~2,129 cache-write tokens on the live seed;
  the same flush at a compaction boundary is free by construction.)

- **Multiple providers, switchable at runtime.** OpenAI Codex, Anthropic (Claude
  Code OAuth lane), and Antigravity (Gemini). `/model` and `/reasoning` switch
  provider, model, and thinking effort at safe turn boundaries; `/scoped-models`
  defines the model cycle you page through. Switches are classified so a
  reasoning-only change never rewrites the prefix.

- **A terminal UI that behaves.** A ratatui TUI with a plain-text fallback for
  pipes and CI. It renders inline (no alternate screen, no mouse capture), so
  native scrollback, copy-mode, and text selection keep working, and
  detach/reattach under tmux just works. Diffs and failed shell results stay
  prominent; successful tools collapse into compact history.

- **No runtime dependencies.** `grep` and `find` run in-process via the ripgrep
  library crates, so no `rg` or `fd` binary needs to be on `PATH`. On Linux the
  `bash` tool can be confined by the kernel (Landlock LSM).

- **Opt-in web tools, off by default.** `web_search` and `read_web_page` are
  independently configurable in settings (`webSearchBackend`:
  off/native/brave/jina/searxng; `readWebPageBackend`: off/native/jina) and are
  not offered to the model until enabled. Native mode needs no API key; Brave
  and Jina keys are user-configured (settings or `BRAVE_API_KEY`/`JINA_API_KEY`);
  `searxng` targets a self-hosted instance via the trusted `searxngUrl`. Bounded
  by global-only dials: `searchTimeoutMs`/`readTimeoutMs` (default 30000),
  `maxSearchResults` (default and hard maximum 10), and
  `maxSearchResponseBytes`/`maxReadResponseBytes`/`maxReadOutputBytes` (default
  200 KiB); out-of-range values are rejected at load.
  Every call is approval-gated, private/localhost/internal targets are refused
  by an SSRF policy with connection pinning, and fetched content is framed as
  untrusted external data. These settings are global-only, so a cloned repo can
  never enable network egress or redirect where queries go (see ADR-0058).

## Install

Prebuilt binaries ship for Linux and macOS (x86_64 and aarch64) — no Rust
toolchain required. The installer downloads the latest release, verifies its
SHA-256 checksum, and installs `iris`:

```bash
curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/iris-agent/main/install.sh | sh
```

Override the install directory with `IRIS_INSTALL_DIR` or pin a version with
`IRIS_VERSION=vX.Y.Z`. Manual installs use the same archive plus checksum from
the [latest release](https://github.com/5omeOtherGuy/iris-agent/releases/latest).

With a Rust toolchain, install from
[crates.io](https://crates.io/crates/iris-agent) instead:

```bash
cargo install iris-agent --locked
```

Keep an installed copy current with `iris update`. It installs only stable
tagged releases — never `main`, never a prerelease — and never downgrades: a
prebuilt binary verifies the new checksum and atomically replaces itself, a
source build re-runs `cargo install` pinned to the latest release tag.

## Quickstart

Create credentials for a provider, then start the REPL:

```bash
iris login openai-codex   # or: anthropic · antigravity
iris                      # /exit or /quit to leave
```

From a source checkout, replace `iris` with `cargo run --`.

Useful commands at the prompt:

- `/model` — view or switch provider/model at a turn boundary.
- `/reasoning <level>` — change thinking effort; accepted levels and wire behavior are model-specific and shown by `/model`.
- `/settings` — compaction policy, thresholds, summariser, and worker input.
- `/context` — itemise the current window (system + tools, raw vs summarised
  conversation, folded-reclaimed tokens, pending folds).
- `/compact` — compact now.
- `/resume`, `/new` — swap the live session at a safe turn boundary.
- `$` or `/skills` — open the installed-skill picker.

### Headless print mode

Run one turn without the REPL and print just the final answer to stdout:

```bash
iris -p "summarize the build failure"           # --print is the long form
cat build.log | iris -p "explain this failure"  # piped stdin merges into the prompt
iris --print "apply the fix" --approve          # auto-approve gated tools
```

Print mode is non-interactive: it exits 0 on success and nonzero on failure, and
never prompts. Mutating tools (`bash`, `edit`, `write`) are denied by default;
pass `--approve` to auto-approve them.

### Resuming sessions

```bash
iris -c                    # --continue: resume the newest session for this directory
iris resume                # pick a session to resume (picker on a TTY; list otherwise)
iris resume <session-id>   # resume a specific session by id
```

## Supported providers

| Provider id | Auth | Notes |
| --- | --- | --- |
| `openai-codex` | OpenAI Codex OAuth (browser or `--device-code`) | Default provider when no setting is present. |
| `anthropic` | Claude Code OAuth (browser PKCE, manual-paste fallback) | Can bootstrap from an existing Claude Code token at `~/.claude/.credentials.json`. |
| `antigravity` | Google OAuth for Gemini Code Assist | Needs `ANTIGRAVITY_CLIENT_SECRET` at login/refresh time. |

### Native reasoning capabilities

Reasoning support is resolved per provider **and model**, not by provider-wide
assumption. The model picker shows only supported provider-native labels;
`/model` reports the current level, supported levels, and active wire behavior.
On a model switch Iris preserves the level when supported, otherwise clamps it
and reports the fallback. Unsupported fields are omitted from provider requests.

| Route/model family | Selectable levels | Provider request behavior |
| --- | --- | --- |
| OpenAI Codex Responses, GPT-5.6 `sol`/`terra`/`luna` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | `reasoning.effort` (`minimal` maps to `low`) plus `summary: auto`; `off` omits the object. |
| Other OpenAI Codex Responses models | `off` through `xhigh` | Same mapping; `max` is unsupported. |
| Anthropic manual models (Haiku 4.5, Sonnet 4.6, Opus 4.6) | `off`, 1,024 / 4,096 / 10,240 / 20,480 / 32,768 tokens | `thinking.type=enabled` with the selected `budget_tokens`; `off` omits thinking. |
| Anthropic adaptive models (Sonnet 5, Opus 4.7/4.8, Fable 5) | `off`, `low`, `medium`, `high`, `xhigh`, `max` | Adaptive thinking plus `output_config.effort`; `off` omits thinking. |
| Older/unknown Anthropic ids | `off` through 20,480 tokens | Conservative manual-budget fallback; no 32,768-token tier. |
| Antigravity Gemini Flash | `off`, `minimal`, `low`, `medium`, `high` | `generationConfig.thinkingConfig` with matching `thinkingLevel`. |
| Antigravity Gemini Pro | `off`, `minimal`, `low`, `medium`, `high` | Gemini wire levels collapse to `low` (`minimal`/`low`) or `high` (`medium`/`high`). |
| OpenAI Chat Completions | Built-in non-reasoning models: `off`; allowlisted reasoning models: `off`, `low`, `medium`, `high` | `reasoning_effort` only for allowlisted models. |
| Custom OpenAI-compatible Chat Completions | `off`, `low`, `medium`, `high` when `openAiCompatible.reasoning=true`; otherwise `off` | `reasoning_effort` only when explicitly enabled. |

The typed source of truth is `src/mimir/model_capabilities.rs`; provider adapters,
validation, switching, the model picker, and CLI status all consume that map.

Choose the default with `defaultProvider`/`defaultModel` in
`~/.iris/settings.json`, or switch live with `/model`. The full set of provider
credentials, settings keys, project-permission (`/trust`) rules, skills
discovery, and environment variables is documented below.

<details>
<summary><b>Providers, settings, permissions &amp; environment</b></summary>

### Credentials and provider selection

Iris stores OAuth credentials in an Iris auth file. By default it reads
`~/.iris/auth.json`. Create or refresh credentials:

```bash
iris login openai-codex
iris login openai-codex --device-code
iris login anthropic
ANTIGRAVITY_CLIENT_SECRET=... iris login antigravity
```

- `openai-codex` uses OpenAI Codex OAuth (browser or device-code) and is the
  default provider if no setting is present.
- `anthropic` uses the Claude Code OAuth lane. `iris login anthropic` runs a
  browser PKCE login with a manual paste fallback; Iris can also bootstrap from
  Claude Code's token at `~/.claude/.credentials.json` (or
  `CLAUDE_CONFIG_DIR/.credentials.json`) when Anthropic credentials are not
  already in the Iris auth store.
- `antigravity` uses Google OAuth for Gemini Code Assist. Its installed-app
  client ID is public and decoded at runtime; the client secret is not committed
  to source and must be supplied via `ANTIGRAVITY_CLIENT_SECRET` at runtime or
  when building Iris.

Override the auth-file path with `IRIS_AUTH_PATH=/path/to/auth.json iris`.

### Settings

Choose the provider for a run with `defaultProvider` in the global JSON settings
file (`~/.iris/settings.json`, or `IRIS_CONFIG_PATH`):

```json
{
  "defaultProvider": "antigravity",
  "defaultModel": "gemini-3.5-flash"
}
```

Supported provider ids are `openai-codex`, `anthropic`, and `antigravity`.
Recognized settings keys are `defaultProvider`, `defaultModel`, `baseUrl`,
`contextTokenBudget`, `defaultReasoning`, `promptCacheRetention`,
`anthropicContextManagement`, `compaction`, `toolResultCompaction`, and
`enabledModels`.

If unset, `promptCacheRetention` defaults to `short`; set it to `none` to omit
provider-native prompt-cache hints.

Automatic compaction uses the selected model's effective context window. An
explicit `contextTokenBudget` remains an absolute upper bound. Active-provider
portable summaries are the default; unsupported routes fall back through the
portable summarizer. OpenAI native compaction is an explicit global opt-in:
set `compaction.providerNative` to `auto` (the default is `off`). Native mode
stores an opaque encrypted continuation block that only the same OpenAI model
can reuse. After a model switch Iris uses the separately generated portable
text summary; differences between the two may change subsequent behavior.
Microcompaction and tool-result compaction remain disabled unless enabled
explicitly. Tune or disable the trigger ladder with:

```json
{
  "compaction": {
    "enabled": true,
    "thresholds": { "warn": 0.60, "start": 0.72, "hard": 0.90 },
    "keepRecentTokens": 8000,
    "hardWaitMs": 120000,
    "maxConsecutiveFailures": 3
  }
}
```

`compaction.enabled=false` disables automatic rewrites; manual `/compact` and
tool-result folds keep their own behavior. A legacy budget below 8,192 tokens is
invalid because it cannot reserve space for a summary.

Tool-result compaction is opt-in. This example enables stale-read dedupe and
older replayable-result clearing locally:

```json
{
  "toolResultCompaction": {
    "enabled": true,
    "aggressiveness": "custom",
    "cacheTiming": "cacheAware",
    "triggerTokens": 64000,
    "semanticDedupe": {
      "enabled": true,
      "retainPerPath": 1,
      "protectRecentToolResults": 4,
      "protectRecentTokens": 2000
    },
    "toolClearing": {
      "enabled": true,
      "backend": "local",
      "mode": "replayable",
      "keepRecentToolUses": 8,
      "clearAtLeastTokens": 1000,
      "eligibleTools": [],
      "excludedTools": ["edit", "write", "recall", "read_output"],
      "includeFailures": false,
      "clearToolInputs": false
    }
  }
}
```

`aggressiveness` accepts `conservative`, `balanced`, `aggressive`, or `custom`.
`cacheTiming` accepts `breakOnly`, `cacheAware`, `pressureOnly`, or `immediate`.
`backend` accepts `local`, `anthropicNative`, or `auto`. Anthropic-native
clearing is global-only and must be disjoint from local reducers. The legacy
`microcompaction=true` setting remains a conservative alias with the independent
`microcompactionWatermark` default of 64,000 tokens. Folded originals remain in
the local session transcript and can be retrieved with
`recall(tool_call_id="...")`.

Project settings (`<cwd>/.iris/settings.json`) are deliberately limited to
local model/runtime preferences, including local tool-result reducers and cache
timing. A cloned repo cannot choose your provider, scoped model cycle,
provider-side cache retention, Anthropic server-side context-management
behavior, select a native compaction backend, or redirect OAuth bearer tokens
with `baseUrl`.

### Project permissions (`/trust`)

The fragment portion of the system prompt is assembled entirely from fragments
built into the binary. No `.md` fragment files are read from
`~/.iris/fragments` or a repo's `.iris/fragments`, so a cloned repo cannot
inject through the old fragment surface (ADR-0026). Project docs
(`AGENTS.md`/`CLAUDE.md`) remain the intentional repo/user steering channel;
review them like any other project instruction file.

Per-project permissions persist in `~/.iris/trust.json`, keyed by the canonical
(symlink-resolved) working directory (ADR-0027):

- At an approval prompt, `[p]` ("always for this project") persists a grant:
  the tool name for `write`/`edit`, the exact command for `bash`. Granted
  tools/commands auto-approve in this directory from then on, across sessions.
- Destructive commands (`rm`, `dd`, `mkfs`, ...) always re-prompt and can never
  be granted — no `[p]` is offered for them.
- `/trust` (alias: `/permissions`) opens the rich-TUI project-permissions
  editor. The text fallback has no editor, but its approval prompt still
  supports `[p]` grants.
- The store is HOME-owned; a repo-committed file can never grant permissions.
  `IRIS_TRUST_PATH` may override the store only with an absolute path outside
  the project directory.

### Skills

Iris loads Codex-compatible filesystem skills. A skill is a directory containing
`SKILL.md` with YAML `name` and `description` fields:

```markdown
---
name: review-patch
description: Review a patch for correctness, safety, and missing tests.
---

Follow the review workflow here.
```

Discovery matches Codex's local layout: `.agents/skills` from the repo root down
to the cwd, `<repo>/.codex/skills`, `~/.agents/skills`, `$CODEX_HOME/skills`
(default `~/.codex/skills`) and its bundled `.system` root, `~/.iris/skills`, and
`/etc/codex/skills` + `/etc/iris/skills` for admin-installed skills.

Type `$` or run `/skills` to search and insert a path-qualified mention. Only
skill metadata enters the initial context (capped at 2% of the configured
budget); the full `SKILL.md` loads when selected. Iris also advertises skill
names and descriptions to the model so it can select one implicitly; set
`policy.allow_implicit_invocation: false` in `agents/openai.yaml` to require an
explicit mention.

### Environment variables

- `IRIS_AUTH_PATH` — auth-file path; defaults to `~/.iris/auth.json`.
- `IRIS_MODEL` — OpenAI Codex model override; defaults to `gpt-5.5`.
- `IRIS_CODEX_BASE_URL` — OpenAI Codex base URL; defaults to `https://chatgpt.com/backend-api`.
- `IRIS_CONFIG_PATH` — global settings-file path; defaults to `~/.iris/settings.json`.
- `IRIS_TRUST_PATH` — project-permission policy store path; defaults to `~/.iris/trust.json`; overrides must be absolute and outside the project directory.
- `IRIS_SESSION_DIR` — session transcript root; defaults to `~/.iris/sessions`.
- `IRIS_SECURITY_OPT_IN` — set to `1` to enable Linux workspace-path and Landlock enforcement.
- `CODEX_HOME` — optional existing Codex home; Iris reads its `skills` directory and skill settings in `config.toml`.
- `CLAUDE_CONFIG_DIR` — Claude Code config directory override for Anthropic token bootstrap.
- `ANTIGRAVITY_CLIENT_SECRET` — Antigravity Google OAuth client secret, read at runtime or embedded when set while building; required for `login antigravity` and refresh unless the binary was built with it.
- `ANTIGRAVITY_PROJECT_ID` — optional Antigravity project-id override.

</details>

## How context compaction works

Iris keeps a long session inside its window with a few layered mechanisms, each
measured before it is claimed:

1. **Per-result reduction (on by default).** Native tools return bounded
   windows, oversized outputs move behind session handles, and `bash` filters
   captured command output inside the runtime before it enters the transcript.
   Semantics never change: any filter error or unparsable output returns the raw
   output, exit codes are never altered, and `raw: true` bypasses filtering.
   ([ADR-0036](docs/adr/0036-tools-are-token-efficient-by-design.md),
   [ADR-0037](docs/adr/0037-native-output-filtering-for-bash-pass-through.md))

2. **Background compaction (on by default).** A context governor runs at
   round-trip boundaries. At the `start` threshold it launches one summariser
   worker and returns immediately; the rewrite is installed at a later boundary
   when it is ready. At `hard` it waits only up to `hardWaitMs`, then falls back
   through a provider-native rung to deterministic excerpts. A portable summary
   is persisted beside any provider-native block so a session survives a model
   switch or resume.
   ([ADR-0055](docs/adr/0055-govern-context-between-provider-round-trips.md),
   [ADR-0057](docs/adr/0057-cover-the-current-turn-under-hard-pressure-and-escalate-fallback.md))

3. **Microcompaction (opt-in).** Spent tool results — a superseded read, a
   retired failure — are folded into deterministic stubs rather than dropped.
   The original stays durable in the JSONL transcript, and `recall` returns it
   on demand, with an optional filter pattern to search a folded range.
   ([ADR-0048](docs/adr/0048-fold-spent-tool-results-behind-handles.md),
   [ADR-0046](docs/adr/0046-recall-compacted-originals-mid-session.md))

4. **Cache-aware flush timing.** Fold *detection* runs every boundary; fold
   *flushing* waits for a moment the prefix cache breaks anyway, priced against a
   provider-neutral `CacheProfile`. The `cacheTiming` policy selects which
   triggers release pending folds.
   ([ADR-0051](docs/adr/0051-cache-aware-fold-flush-scheduling.md))

## Token efficiency

Every tool result carries the fewest tokens that preserve full task success.
Measured on a committed corpus of captured real command outputs; the numbers are
asserted as minimum bars by tests, not just reported:

| command class | token reduction |
|---|---|
| cargo build (pass) | 98% |
| cargo test (pass) | 85–94% |
| npm install | 79% |
| npm test / vitest (pass) | 68% / 70% |
| git log | 62% |
| git diff (lockfile churn) | 58% |
| git status | 50% |

Reduction never changes semantics: failure detail survives verbatim, filter
errors return raw output, exit codes are untouched, `raw: true` bypasses
filtering, and filter overhead is under 10 ms per call (all test-asserted). The
opt-in `read` skim mode strips comments, docstrings, and blank lines for
exploration reads (50–72% reduction on comment-heavy source; data formats pass
through byte-identical; a skim read does not satisfy read-before-edit).

The opt-in web tools reduce at the same measured seams: `read_web_page`'s
HTML→Markdown extraction (~74% on a real article) and objective excerpting
(~73%), and `web_search`'s raw-response→compact-list render (~91% on a captured
DuckDuckGo page). `web_search` returns a snippet-rich ranked list, not a
server-composed summary the model cannot verify — same token cost, all the
evidence at the model's fingertips
([ADR-0059](docs/adr/0059-web-search-returns-a-snippet-rich-list-not-a-server-summary.md)).
The untrusted-content framing that marks web output as external data survives
reduction (test-asserted).

Full tables and regeneration commands:
[bash filter benchmark](docs/benchmarks/adr-0037-bash-filter-tokens.md),
[read skim benchmark](docs/benchmarks/issue-337-read-skim-tokens.md),
[web tools benchmark](docs/benchmarks/web-tools-token-efficiency.md).

**End-to-end cost is not yet a headline claim.** The
[tokens-per-task benchmark](docs/BENCHMARK_PLAN.md) (issue #210)
replays three workloads deterministically and shows the default arm spending
fewer prompt tokens than a reductions-off baseline (3.4–9.1% on the current
fixtures) with identical success and every task-critical fact preserved. But
replay proves the plumbing, not that a model still completes the task from
reduced context: the real-provider confirmation is pending, so no headline
efficiency number is claimed yet.

## Status

Pre-1.0 and under active development — expect rough edges and breaking changes.

- **Platforms:** Linux and macOS are supported; Windows is not. On Linux the
  `bash` tool can be kernel-confined (Landlock LSM, opt-in via
  `IRIS_SECURITY_OPT_IN=1`). **On macOS the shell runs unconfined** — there is no
  sandbox yet, so treat macOS shell commands as unsandboxed. Iris still asks
  before running mutating tools on every platform, and states `unsandboxed` at
  the point you approve a `bash` command on macOS.
- **Implemented:** interactive TUI with a plain-text fallback; Tokio async
  runtime with turn-level cancellation; three providers with runtime model and
  reasoning switching; workspace-scoped tools with approval gates and diff
  previews; JSONL transcript persistence with continue/resume; background
  compaction and opt-in microcompaction with `recall`; Codex-compatible skills.
- **In progress:** a git-centered workflow slice (dirty-tree safety, task
  checkpoint/rollback, final diff summary, verification loop; ADR-0028), the
  real-provider tokens-per-task confirmation (#210), macOS Seatbelt confinement,
  and a general-purpose subagent surface (a read-only backend contract exists;
  spawning subagents as tools is on the roadmap). See
  [docs/ROADMAP.md](docs/ROADMAP.md) and
  [docs/FEATURES.md](docs/FEATURES.md).

## Documentation

- [Roadmap](docs/ROADMAP.md) — milestone sequencing and acceptance gates.
- [Feature list](docs/FEATURES.md) — implemented/planned capability inventory.
- [Architecture Decision Records](docs/adr/README.md) — accepted/proposed decisions.
- [Product brief](PRODUCT.md) — target users, purpose, and product principles.
- [Naming convention](docs/NAMING.md) — how the Iris/Wayland/Nexus/Mimir tiers are named.
- [Releasing](docs/RELEASING.md) — operator runbook for cutting a release.
- OpenWiki-generated agent docs live in `openwiki/` (see [docs/OPENWIKI.md](docs/OPENWIKI.md)).

## Testing

```bash
cargo test
```

## License

[MIT](LICENSE). Some files are derived from
[OpenAI Codex](https://github.com/openai/codex) and are distributed under the
Apache License 2.0; each carries an SPDX header, and [NOTICE](NOTICE) lists them.
