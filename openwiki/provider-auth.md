# Provider Authentication

Mimir owns provider adapters and auth. Nexus only sees the provider-neutral
`ChatProvider` contract.

## Providers

| Provider id | Command | Credential path |
| --- | --- | --- |
| `openai-codex` | `iris login openai-codex` | Iris auth store |
| `openai` | `iris login openai` | Iris auth store API key |
| `openai-compatible` | `iris login openai-compatible` | Iris auth store API key when required |
| `anthropic` | `iris login anthropic` or `iris login anthropic --api-key` | Iris auth store or Claude Code credentials |
| `antigravity` | `iris login antigravity` | Iris auth store |

The default provider is `openai-codex` when no setting overrides it.

## OpenAI Codex

OpenAI Codex auth supports browser OAuth and device-code login:

```bash
iris login openai-codex
iris login openai-codex --browser
iris login openai-codex --device-code
```

The provider adapter lives in `src/mimir/providers/openai_codex_responses.rs`.
It sends provider-specific Responses requests, maps Iris tools into provider
tool declarations, applies reasoning settings where supported, and handles
streamed events.

## OpenAI API

```bash
iris login openai
```

This stores an API key for the `openai` provider. Stored credentials win over
`OPENAI_API_KEY`. The built-in default model is `gpt-4.1`; select another model
with settings or `/model`.

## OpenAI-Compatible

```bash
iris login openai-compatible
```

The `openai-compatible` provider targets an OpenAI-style chat endpoint. The
built-in base URL is `http://localhost:11434/v1` and the default model is
`llama3.1`. Capability metadata lives under the global `openAiCompatible`
settings block; base URL and provider selection are global-only. It uses only a
stored provider-specific key, `OPENAI_COMPATIBLE_API_KEY`, or
`IRIS_OPENAI_COMPATIBLE_API_KEY`; it does not reuse `OPENAI_API_KEY`.

## Anthropic

Anthropic auth uses the Claude Code OAuth lane:

```bash
iris login anthropic
```

Iris can also bootstrap from Claude Code credentials when they are present. The
provider adapter preserves same-origin reasoning continuity and maps normalized
reasoning settings into Anthropic thinking controls where supported.
Stored API-key credentials win over `ANTHROPIC_API_KEY`.

## Antigravity

Antigravity uses Google OAuth for Gemini Code Assist:

```bash
ANTIGRAVITY_CLIENT_SECRET=... iris login antigravity
```

`ANTIGRAVITY_PROJECT_ID` can override the project id. Without an injected client
secret, login cannot complete.

## Settings

Global settings live at `~/.iris/settings.json` unless `IRIS_CONFIG_PATH`
overrides the path. Project settings may override only project-safe values:

- `defaultModel`
- `defaultReasoning`
- `contextTokenBudget`
- `compactionSummarizer`
- `microcompaction`
- `bashToolMode`
- `maxToolRoundtrips`
- `verify`
- `tui`
- `worktreeRoot`

Global-only values include provider selection, base URLs, prompt-cache controls,
Anthropic context-management controls, scoped model lists, retry settings,
OpenAI-compatible capability metadata, and startup approval posture.

Common global keys include `defaultProvider`, `defaultModel`, `baseUrl`,
`defaultReasoning`, `promptCacheRetention`, `anthropicContextManagement`,
`enabledModels`, `retry`, `openAiCompatible`, and `defaultApproval`.

## Environment

Important environment variables:

- `IRIS_AUTH_PATH`: auth store override.
- `IRIS_CONFIG_PATH`: global settings override.
- `IRIS_SESSION_DIR`: transcript root override.
- `IRIS_TRUST_PATH`: permission policy store override; must be absolute and
  outside the project directory.
- `IRIS_MODEL`: OpenAI Codex model override.
- `IRIS_CODEX_BASE_URL`: OpenAI Codex base URL override.
- `OPENAI_API_KEY`: API-key fallback for the `openai` provider.
- `ANTHROPIC_API_KEY`: API-key fallback for the `anthropic` provider.
- `OPENAI_COMPATIBLE_API_KEY`: API-key fallback for `openai-compatible`.
- `IRIS_OPENAI_COMPATIBLE_API_KEY`: alternate API-key fallback for
  `openai-compatible`.
- `IRIS_PLAIN`: force the plain text UI when truthy.
- `IRIS_NO_ALT_SCREEN`: force inline rendering when truthy.
- `IRIS_REDUCED_MOTION`: freeze TUI animation when truthy.
- `IRIS_SECURITY_OPT_IN`: enable workspace path confinement and the Linux bash
  Landlock sandbox when truthy.
- `CLAUDE_CONFIG_DIR`: Claude Code credential bootstrap directory.
- `ANTIGRAVITY_CLIENT_SECRET`: Antigravity OAuth secret.
- `ANTIGRAVITY_PROJECT_ID`: Antigravity project override.

Repository settings cannot choose a provider or redirect credentials.
