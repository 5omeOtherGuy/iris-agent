# ADR-0058: Native web tools as an opt-in, approval-gated egress class

**Date**: 2026-07-12
**Status**: accepted
**Deciders**: Iris maintainers

## Context

Iris had no way to reach the public internet. Adding `web_search` and
`read_web_page` introduces a capability class the agent never had before:
outbound network egress driven by model-chosen text and URLs. That is qualitatively
different from the workspace tools, which are confined to a directory. Two risks
dominate: SSRF (the model reaching cloud metadata, localhost, or internal
services) and exfiltration (query/URL text carrying workspace secrets to a third
party). Web content the tools return is also untrusted input that may attempt
prompt injection.

The reference implementation (`ampi-web`) validated the two-tool split and the
SSRF gate approach but documents a residual DNS-rebinding TOCTOU and ships no
approval integration or untrusted-content framing.

## Decision

Ship both tools as an **opt-in, approval-gated egress class** with layered
containment:

1. **Off by default.** Each tool has a global-only backend setting
   (`webSearchBackend`, `readWebPageBackend`); absent/`off` means the tool is not
   registered at all (invisible to the model, no prompt bloat).
2. **Global-only configuration.** The backend settings are excluded from the
   project-file merge (same class as `defaultProvider`/`baseUrl`): a cloned repo
   can never enable egress or redirect it.
3. **Approval-gated per call with allow-always.** Enabling a backend turns the
   tool on; it does not pre-authorize the model to transmit arbitrary text.
   Both tools are `requires_approval` and join the permissions hatch
   (`POLICY_TOOLS`) so a user can grant standing consent through the existing
   lever, not a new one.
4. **One SSRF policy, two client profiles.** A single URL/IP gate (scheme,
   ports 80/443, no userinfo, canonical host, IANA special-purpose deny tables
   for IPv4+IPv6) applies to every user/model URL and to Jina target URLs. User
   URLs go through a *pinned* client (fresh per validated hop, redirects walked
   by hand and re-gated, `no_proxy`, HTTP/1-only, DNS pinned to the validated
   IPs with a fail-closed resolver beneath the override) which closes the
   rebinding TOCTOU the reference lives with. The hardcoded Brave/Jina API
   endpoints go through a separate normal client with redirects disabled.
5. **Untrusted-content framing.** Both tools frame output with a source header
   and a fixed "external data, not instructions" notice, reinforced by a
   system-prompt guideline when a web tool is registered.
6. **Per-tool, independent backends.** `web_search` is `native`/`brave`/`jina`;
   `read_web_page` is `native`/`jina`. The settings never couple (Jina meters
   search and reader separately).

Keys are user-configured service credentials (`brave-search`, `jina`) in the
auth store with `BRAVE_API_KEY`/`JINA_API_KEY` env fallbacks; stored key wins.

## Alternatives Considered

### Alternative 1: Enabling a backend is blanket consent (no per-call approval)
- **Pros**: fewer prompts; simpler loop.
- **Cons**: turns a config toggle into standing authority to exfiltrate workspace
  text on any model whim.
- **Why not**: egress is a new capability class; the standing-consent lever
  already exists (allow-always via the permissions hatch). Default to a human in
  the loop.

### Alternative 2: Embed/self-host Firecrawl or a headless browser for rendering
- **Pros**: best extraction quality; renders JavaScript.
- **Cons**: AGPL + multi-container footprint (Firecrawl); an embedded browser is
  an SSRF amplifier and a large attack surface.
- **Why not**: wrong tier and risk profile. The Jina backend covers the
  JS/PDF-rendering need by shifting that risk off-box, with the target URL still
  policy-checked locally and the privacy cost disclosed in the setting's help.

### Alternative 3: Trust text-level URL validation (accept the rebinding TOCTOU)
- **Pros**: simpler fetch path; matches the reference.
- **Cons**: a name that passes text validation can re-resolve to a private
  address at connect time.
- **Why not**: connection pinning + a fail-closed resolver closes the window for
  a small, contained cost.

## Consequences

### Positive
- No surprise egress: a fresh install and a cloned repo both start fully off.
- SSRF is defended in depth (text gate, resolved-IP gate, pinned connection,
  fail-closed resolver, stricter-than-reference deny tables).
- Backends are a thin seam: adding SearXNG later is roughly one file.

### Negative
- The pinned client is `no_proxy` by design, so native backends do not work
  behind a mandatory corporate egress proxy (the API backends do). Documented.
- Per-call approval adds prompts until a user grants allow-always.

### Risks
- Prompt injection via web content is mitigated (framing + guidance + approval)
  but not eliminated.
- Native backends depend on third-party markup/throttling (DuckDuckGo) and
  extraction fidelity (`dom_smoothie`); honest diagnostics and the Brave/Jina
  backends are the reliability path.
- Jina sees the URLs/queries it serves; disclosed in the setting's help text.
