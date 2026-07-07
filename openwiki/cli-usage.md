# CLI Usage

Iris is a terminal coding agent. The installed binary is `iris`; from a checkout
use `cargo run -- ...` for the same arguments.

## Install from source

```bash
cargo install --git https://github.com/5omeOtherGuy/iris-agent.git iris-agent --locked
```

From a checkout:

```bash
cargo run --
```

## Start a session

```bash
iris
iris --plain
iris --no-alt-screen
```

Interactive terminals use the TUI. Pipes, CI, `TERM=dumb`, `--plain`,
`IRIS_PLAIN=1`, `NO_COLOR`, or terminal startup failures use the plain text
fallback. `--no-alt-screen` or `IRIS_NO_ALT_SCREEN=1` keeps rendering inline
instead of using the pager alt screen. `IRIS_REDUCED_MOTION=1` freezes the
working indicator animation.

## Print mode

Run one non-interactive turn and print the final answer:

```bash
iris -p "summarize the build failure"
cat build.log | iris -p "explain this failure"
iris --print "apply the fix" --approve
```

Print mode exits after one turn. Mutating tools are denied by default;
`--approve` allows them without prompting. Piped stdin is appended to the prompt
after a blank line.

## Resume

```bash
iris -c
iris --continue
iris resume
iris resume <session-id>
iris resume --plain
iris resume <session-id> --plain
```

`iris -c` resumes the newest session for the current directory. `iris resume`
opens a picker in a rich TTY or lists sessions in plain text.

## Login

```bash
iris login openai-codex
iris login openai-codex --browser
iris login openai-codex --device-code
iris login openai
iris login openai-compatible
iris login anthropic
iris login anthropic --api-key
ANTIGRAVITY_CLIENT_SECRET=... iris login antigravity
```

Credentials are stored in the Iris auth store unless a provider adapter supports
bootstrapping from another tool.

## Update

```bash
iris update
```

Prebuilt release binaries self-replace from GitHub release assets. Source-built
binaries fall back to `cargo install`.

## Danger mode

```bash
iris --dangerously-skip-permissions
```

This session-only flag auto-approves every tool call, including destructive
commands, and records the mode in the transcript. It is not configurable from
settings, project files, the trust store, or environment variables.

## Slash commands

Backed interactive commands include:

- `/exit`, `/quit`
- `/model`, `/reasoning`
- `/resume`, `/new`
- `/session`, `/sessions`, `/tasks`, `/copy`
- `/compact`, `/context`
- `/debug`
- `/scoped-models`, `/settings`
- `/approval`
- `/trust`, `/permissions`
- `/login`, `/logout`
- `/find`, `/terminal-setup`, `/mouse`
- `/git`, `/tree`
- `/diff`, `/rollback`, `/accept`, `/checkpoint`

The text fallback supports command paths that do not require TUI selectors,
including `/model`, `/reasoning`, `/copy`, `/session`, `/compact`, `/approval`,
`/diff`, `/rollback`, `/accept`, and `/checkpoint`. Selector/modal commands
report that they are TUI-only.

## Exit codes

- `0`: success.
- `1`: general error.
- `2`: usage error, including unknown session ids or invalid commands.
- `3`: authentication error.
