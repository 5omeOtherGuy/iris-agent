# Iris

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/hero-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/assets/hero-light.svg">
  <img alt="Iris terminal banner. A user asks: What are you? The thinking indicator pulses, and the answer is: A precise, token-efficient coding agent for the terminal." src="docs/assets/hero-dark.svg" width="640">
</picture>

A fast coding agent for the terminal, built for token efficiency.

---

## Install

Install the latest version from the remote repository:

```bash
cargo install --git https://github.com/5omeOtherGuy/iris-agent.git --locked
```

Update an installed copy with:

```bash
iris update
```

Or run from a source checkout:

```bash
git clone https://github.com/5omeOtherGuy/iris-agent.git
cd iris-agent
cargo run
```

**Runtime dependencies: none beyond the binary.** The `grep` and `find` tools
search in-process via the ripgrep library crates (`grep`, `ignore`, `globset`),
so no `rg` or `fd` binary needs to be on `PATH`.

## Platforms

| Platform | Status | `bash` sandbox |
| --- | --- | --- |
| Linux | Supported | Kernel-enforced (Landlock LSM), opt-in via `IRIS_SECURITY_OPT_IN=1` |
| macOS | Supported | None yet — the shell runs **unconfined** |
| Windows | Unsupported | — |

macOS caveat: the `bash` sandbox is Linux-only. On macOS every shell command
runs without kernel confinement. Approval prompts appear only when
`IRIS_SECURITY_OPT_IN=1` enables them; a default run may auto-approve and show
no prompt at all. When a `bash` approval prompt is shown on macOS, it states
`unsandboxed` at the point you approve a command, so the posture is visible
where you decide, not buried in a startup line. macOS Seatbelt confinement is a
planned follow-up ([docs/ROADMAP.md](docs/ROADMAP.md)); until it lands, treat
macOS shell commands as unsandboxed whether or not a prompt is shown.

## Run

Create credentials for the provider you want, then start the REPL:

```bash
iris login openai-codex   # or: anthropic · antigravity
iris                      # /exit or /quit to leave
```

From a source checkout, replace `iris` with `cargo run --`.

At the prompt, `/model` views or switches provider/model and
`/reasoning off|minimal|low|medium|high|xhigh` changes thinking effort at a safe
turn boundary. `/settings`, `/scoped-models`, `/login`, and `/logout` open their
selectors.

<details>
<summary><b>Providers, settings &amp; environment</b></summary>

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
`anthropicContextManagement`, and `enabledModels`.

If unset, `promptCacheRetention` defaults to `short`; set it to `none` to omit
provider-native prompt-cache hints.

Project settings (`<cwd>/.iris/settings.json`) are deliberately limited to
`defaultModel`, `defaultReasoning`, and `contextTokenBudget`; a cloned repo
cannot choose your provider, scoped model cycle, provider-side cache retention,
Anthropic server-side context-management behavior, or redirect OAuth bearer
tokens with `baseUrl`.

### Environment variables

- `IRIS_AUTH_PATH` — auth-file path; defaults to `~/.iris/auth.json`.
- `IRIS_MODEL` — OpenAI Codex model override; defaults to `gpt-5.5`.
- `IRIS_CODEX_BASE_URL` — OpenAI Codex base URL; defaults to `https://chatgpt.com/backend-api`.
- `IRIS_CONFIG_PATH` — global settings-file path; defaults to `~/.iris/settings.json`.
- `IRIS_SESSION_DIR` — session transcript root; defaults to `~/.iris/sessions`.
- `CLAUDE_CONFIG_DIR` — Claude Code config directory override for Anthropic token bootstrap.
- `ANTIGRAVITY_CLIENT_SECRET` — Antigravity Google OAuth client secret, read at runtime or embedded when set while building Iris; required for `login antigravity` and refresh unless the binary was built with it.
- `ANTIGRAVITY_PROJECT_ID` — optional Antigravity project-id override; when set it wins over any persisted project id, otherwise Iris discovers/persists one from `loadCodeAssist` and errors if discovery fails.

</details>

## Status

As of 2026-06-26: Milestone 1, the async-hard runtime, and the Milestone 2
foundations are complete. The next milestone gate is proving the token-efficiency
thesis with benchmark evidence; efficiency claims wait on measurement.

Implemented:

- Interactive terminal TUI, with a plain-text fallback for pipes and CI.
- Tokio async runtime with turn-level cancellation.
- Multiple providers (OpenAI Codex, Anthropic, Antigravity) with runtime model/reasoning switching.
- Workspace-scoped tools: read, write, edit, bash, grep, find, ls.
- Approval gates with diff previews for mutating tools.
- JSONL transcript persistence and linear resume.
- Large-output handles and turn-boundary auto-compaction.

Next:

- Token-efficiency benchmark proof.
- Persistent approval policies, in-session resume picker, transcript branching/rollback, modes, and subagents.

## Testing

```bash
cargo test
```

## Documentation

- [Naming convention](docs/NAMING.md) — how the Iris/Wayland/Nexus/Mimir tiers are named.
- [Roadmap](docs/ROADMAP.md) — milestone sequencing and acceptance gates.
- [Feature list](docs/FEATURES.md) — implemented/planned capability inventory.
- [Product brief](PRODUCT.md) — target users, product purpose, voice, and product principles.
- [Design system summary](DESIGN.md) — concise visual-system summary for the Iris TUI.
- [Current codemap](docs/CODEMAPS/INDEX.md) — source-grounded map of the current codebase.
- [TUI design language](docs/TUI_DESIGN_LANGUAGE.md) — terminal layout, spacing, and menu rules.
- [TUI live testing](docs/TUI_LIVE_TESTING.md) — opt-in tmux harness for manual pane-rendering checks.
- [Architecture Decision Records](docs/adr/README.md) — accepted/proposed architecture decisions.
- [Competitor matrix](docs/COMPETITOR_MATRIX.md) — verified competitor feature matrix.
- [Competitor analysis](docs/COMPETITOR_ANALYSIS.md) — strategic competitor notes.

## License

[MIT](LICENSE).
