# Iris — Naming Convention

**Last Updated:** 2026-06-17

How Iris names its tiers and packages, the meaning behind each name, and the one
deferred rename (Heimdall). For the dependency-direction split these names sit
on, see [`ARCHITECTURE.md`](ARCHITECTURE.md).

## The convention

Tiers and packages are named after **mythological figures whose role mirrors the
tier's relationship to the others** — not its contents. Pantheon is mixed
(Greek + Norse) on purpose; fit beats consistency. The product itself is **Iris**.

One name is deliberately *not* mythological: **Nexus** (the core), because the
core is a binding-point, not a character.

## Current naming scheme

| Name | Layer | Origin | Why it fits | Code today |
|---|---|---|---|---|
| **Iris** | Product + CLI tier (Tier 3) | Greek | Rainbow-messenger between gods and mortals; faces the user and paints the screen | `main.rs`, `cli.rs`, `ui/`, `tool_display.rs` |
| **Wayland** | Harness (Tier 2) | Germanic | Master smith who forges gear for heroes; equips the engine with sessions, config, execution env | `wayland/` |
| **Nexus** | Core (Tier 1) | Latin (non-myth, intentional) | "Binding"; the still center all dependencies point inward to | `nexus.rs` |
| **Mimir** | AI/provider package | Norse | Keeper of the well of wisdom Odin consults; the layer you query for answers (the pi-ai equivalent) | `mimir/` (`providers/`, `auth/`) |

The through-line: each name describes a *relationship* — Iris faces outward to
the user, Mimir is consulted for answers, Wayland equips the engine, Nexus is the
center everything binds to.

```
              user
               │
        ╭──────▼───────╮
        │ Iris  (CLI)  │  Greek rainbow-messenger
        ╰──────┬───────╯
        ╭──────▼───────╮
        │ Wayland      │  Germanic smith (harness)
        ╰──────┬───────╯
        ╭──────▼───────╮     ╭───────────────╮
        │ Nexus (core) │◀───▶│ Mimir         │  Norse well of wisdom
        ╰──────────────╯     │ (providers)   │  (implements Nexus's
          binding center     ╰───────┬───────╯   ChatProvider contract)
                                     ▼
                              external LLM providers
```

> The `ChatProvider` contract stays in **Nexus** (Tier 1); **Mimir** owns the
> concrete adapters + auth. Naming the provider package does not move the seam.

## Naming a new component

1. Identify the component's *relationship* to the rest of the system (faces the
   user? equips? is consulted? guards a boundary?).
2. Pick a mythological figure whose role matches that relationship. Either
   pantheon is fine; prefer Norse to sit alongside Wayland/Mimir when fit is equal.
3. Avoid duplicating a role already taken (e.g. two messengers).
4. Pure infrastructure with no character analogue may use a descriptive
   non-myth name, as Nexus does.

## Planned rebrand: Iris CLI tier to Heimdall

**Status: deferred — the product stays `Iris`.** Tracked in
[issue #35](https://github.com/5omeOtherGuy/iris-agent/issues/35).

**Heimdall** (Norse) is the proposed swap for the **CLI tier label only**:
watchman of Bifröst (the rainbow bridge) who sees and hears across all worlds and
guards the boundary — the layer the user meets first. It trades Iris's *rainbow
messenger* meaning for *boundary watchman* and keeps a Norse strand with
Wayland/Mimir. (Bifröst is the literal rainbow-bridge equivalent of Iris, but
Heimdall is the better *named-character* fit for a tier.)

### Blast radius

There is **no `iris` code symbol** (no `mod iris`, no type/struct). "Iris" lives
in three layers of very different cost:

- **Job A — CLI tier *label* (docs + ~6 source comments + one system-prompt
  string + its test): trivial.** Pure find/replace, no behavior, no path changes.
  This is all a tier-label rename needs.
- **Job B — product/crate/binary/env/paths named `iris`: breaking.**
  - Crate + binary `iris-agent`, repo dir, GitHub URL.
  - Env vars `IRIS_MODEL`, `IRIS_CODEX_BASE_URL`, `IRIS_CONFIG_PATH`,
    `IRIS_SESSION_DIR`, `IRIS_AUTH_PATH`, `CLAUDE_CONFIG_DIR`,
    `ANTIGRAVITY_CLIENT_SECRET`, `ANTIGRAVITY_PROJECT_ID` — **breaks existing
    users** without old-name fallback.
  - Data dirs `~/.iris/{settings,auth,sessions}` — **orphans user data** without
    migration/fallback.
  - Internal protocol strings `__IRIS_DONE_` / `__iris_rc`
    (`src/tools/bash/session.rs`, + tests).
  - ~150+ documentation matches.

### Decision criterion

Size is set by one question: **is "Iris" the product name or just the CLI tier
name?**

- Keep **Iris = product**, Heimdall = only the Tier-3 label → **Job A only**
  (env/paths stay `IRIS_*`; they belong to the product).
- Rebrand the **whole product** to Heimdall → **Job B**, which requires
  back-compat fallbacks (read old `IRIS_*` env and `~/.iris` dirs when new ones
  are absent) to avoid data loss.

**Recommendation:** keep the product **Iris**; if Heimdall is ever adopted, scope
it to the tier label (Job A). A full product rebrand (Job B) is a breaking change
and should not be undertaken without a migration plan.
