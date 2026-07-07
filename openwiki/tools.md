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

When `bashToolMode` is enabled, Iris registers only `bash`, `edit`,
`read_output`, and `recall`. The model then uses shell commands for file
inspection, listing, search, and creation.

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

Approval modes are `strict`, `auto`, and `never`. The `auto` mode can silently
approve in-workspace file targets, but destructive bash commands always prompt.
`bash`, `write`, and `edit` do not support blanket allow-always grants.

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

Default-on output reductions compact noisy bash, grep, find, and ls output for
model context. The benchmark harness can disable reductions for measurement, but
normal CLI sessions always use the reduced form.
