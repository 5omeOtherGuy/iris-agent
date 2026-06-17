# ADR-0008: Harden bash with process groups, persistent sessions, and Landlock

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

The `bash` tool is powerful enough to mutate files, start long-running processes, and access the network. Iris needs useful shell behavior for coding tasks while keeping cancellation, cleanup, and sandbox policy explicit. Prior hardening work added persistent sessions, background jobs, process-group cleanup, and Linux Landlock confinement where available.

## Decision

`bash` is a first-class native tool with approval gating, process-group ownership, persistent sessions, background jobs, timeout/cancellation handling, and Linux Landlock sandboxing where supported. macOS Seatbelt/PTY support and stronger network namespace isolation are deferred.

## Alternatives Considered

### One-shot shell commands only
- **Pros**: Simpler implementation.
- **Cons**: Cannot preserve `cd`/environment state or manage long-running jobs well.
- **Why not**: Coding agents need persistent sessions and background process workflows.

### No kernel sandbox
- **Pros**: Simpler and more portable.
- **Cons**: Shell commands can write or connect more broadly than intended.
- **Why not**: Shell is the highest-risk built-in tool; kernel confinement is worth using where available.

### Full namespace/container sandbox now
- **Pros**: Stronger isolation, including broader network controls.
- **Cons**: Much larger implementation and portability burden.
- **Why not**: Landlock plus approval and process groups is the smallest useful hardening layer for now.

### PTY-based shell sessions now
- **Pros**: Better compatibility with interactive commands.
- **Cons**: More terminal control complexity and UI coupling.
- **Why not**: Pipe-based sessions cover the current non-interactive coding workflow.

## Consequences

### Positive
- Long-running shell work can be started, polled, finalized, or cancelled.
- Ctrl-C/force-quit cleanup has one process-group ownership model.
- Linux users get kernel-enforced filesystem/network restrictions.

### Negative
- macOS currently lacks equivalent sandbox enforcement.
- Pipe-based sessions are not a full terminal emulator.

### Risks
- Child processes can escape some cleanup/sandbox assumptions; mitigate with clear surfaced notices, process-group tests, and future sandbox work only when needed.
