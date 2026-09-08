# Tools

Nexus owns tool contracts and approval enforcement. Iris owns the concrete
built-in tool implementations. Wayland owns workspace path safety and execution
state.

## Built-ins

| Tool | Purpose | Approval |
| --- | --- | --- |
| `read` | Read text files with truncation and binary/invalid UTF-8 rejection. | No |
| `write` | Create or overwrite files atomically. | Yes |
| `edit` | Replace exact strings with optional `replace_all`. | Yes |
| `bash` | Run shell commands, persistent sessions, and background jobs. | Yes |
| `grep` | Search workspace content in process. | No |
| `find` | Find workspace files in process. | No |
| `ls` | List directory entries or recursive trees. | No |
| `read_output` | Read oversized tool output stored behind a session handle. | No |
| `recall` | Recall compacted transcript detail from the session store. | No |
| `AskUserQuestion` | Collect structured answers from the operator. | Required interaction, even in skip mode |
| `get_goal`, `create_goal`, `update_goal` | Restricted session-goal controls. | Harness-owned goal lifecycle |
| `web_search`, `read_web_page` | Bounded search/page extraction. | Opt-in and approval-gated |
| `request_compaction` | Schedule one safe-boundary compaction. | Opt-in; no direct context mutation |
| `spawn_subagent` and lifecycle/apply tools | Manifest-driven delegation and reviewed apply. | Spawn/apply gates, tool ceilings and isolated mutation |

`bashToolMode` keeps `bash`, `edit`, `AskUserQuestion`, goal tools, `read_output`
and `recall`, plus configured web/compaction/delegation tools. It removes ordinary
file/search tools; the model uses shell commands for those operations.

## Path safety

Tools resolve requested paths against the workspace root. Runtime refusal of
workspace escapes is currently opt-in with `IRIS_SECURITY_OPT_IN=1`; by default
tools resolve paths but do not confine them to the workspace. Some derived
surfaces are always stricter: auto-approval classification and compacted path
carry fail closed for paths that do not resolve inside the workspace.

## Mutating tools

`write`, `edit`, and `bash` are approval-gated. File mutations use trusted diff
previews before the approval decision. Denied calls are recorded as denied tool
results instead of disappearing from the transcript.

Approval modes are `strict`, `auto`, `never`, and
`dangerously-skip-permissions`. Auto only approves file mutations it proves safe;
normal modes retain destructive-action floors. Dangerous-skip bypasses those
floors and persists globally when enabled through its CLI flag. It is not a
sandbox and cannot answer required human questions. Narrow project grants are
separate from blanket session allow. See the
[approval contract](../README.md#approval-modes).

## Bash

`bash` runs in the workspace. With `IRIS_SECURITY_OPT_IN=1` on Linux, the shell
sandbox uses Landlock where available. The policy grants writes to the workspace,
temp directories, and `/dev/null`; reads and execution are unrestricted; TCP
network access is denied when the kernel supports the required Landlock ABI.
Without the opt-in, or on non-Linux platforms, shell commands run unconfined and
the posture is surfaced at approval/output time.

One-shot commands, persistent sessions, and background jobs are supported. Child
processes are managed through process groups so cancellation and cleanup can
target the right work.

## Scheduling

Tools are exclusive by default. Concrete tools can mark calls as
concurrency-safe. Today `grep`, `find`, `ls`, and `read_output` can join safe
parallel batches. Mutating tools, shell commands, `read`, and `recall` stay
exclusive.

## Output handling

Large successful tool outputs can be folded behind session-scoped output handles.
The transcript keeps a compact preview plus handle metadata while the full output
is stored in a sidecar directory.

Default reductions compact supported noisy outputs. `bash` accepts `raw:true`
to bypass filtering for a call; read skim and some search guards are opt-in.
Benchmark arms can disable reductions without changing normal defaults.
Per-result reduction is not proof of lower completed-task cost.
