# pi-mono TUI model, provider, thinking, and login behavior

Source baseline: `~/vendor/pi-mono` at `0ab2aa86`.

This spec describes the pi-mono user-facing behavior Iris should match for model selection, scoped model configuration, thinking/effort selection, and provider authentication through slash commands. It is behavioral on purpose: Iris does not need pi-mono's exact component structure, only the visible flow and state changes.

## Source map

- Slash command registration: `packages/coding-agent/src/core/slash-commands.ts`
- Interactive command dispatch and picker wiring: `packages/coding-agent/src/modes/interactive/interactive-mode.ts`
- Model picker: `packages/coding-agent/src/modes/interactive/components/model-selector.ts`
- Scoped model picker: `packages/coding-agent/src/modes/interactive/components/scoped-models-selector.ts`
- Settings thinking picker: `packages/coding-agent/src/modes/interactive/components/settings-selector.ts`
- Legacy exported thinking picker: `packages/coding-agent/src/modes/interactive/components/thinking-selector.ts`
- Provider auth picker: `packages/coding-agent/src/modes/interactive/components/oauth-selector.ts`
- Login dialog: `packages/coding-agent/src/modes/interactive/components/login-dialog.ts`
- Model/auth state: `packages/coding-agent/src/core/agent-session.ts`, `model-registry.ts`, `auth-storage.ts`, `model-resolver.ts`

## Shared interaction model

- Pickers replace the editor area while open and take focus.
- Closing a picker restores the normal editor and focus.
- Pickers are framed by a top and bottom dynamic border.
- Selection rows use `→ ` for the selected item.
- Generic selection keys are:
  - Up/down: move selection.
  - Enter: confirm.
  - Escape or Ctrl+C: cancel, except where a component gives Ctrl+C a local search-clearing behavior.
- `/model`, `/scoped-models`, `/settings`, `/login`, and `/logout` clear the editor text after the command is accepted.
- `/login` and `/logout` accept no arguments. `/login foo` and `/logout foo` are not recognized as these commands.

## Slash command autocomplete

Builtin slash commands include:

- `/model` - `Select model (opens selector UI)`
- `/scoped-models` - `Enable/disable models for Ctrl+P cycling`
- `/settings` - `Open settings menu`
- `/login` - `Configure provider authentication`
- `/logout` - `Remove provider authentication`

`/model` has argument completions:

- Candidates are session scoped models if a scope is active, otherwise all available authenticated models.
- Completion value is `provider/modelId`.
- Completion display label is the model id, with provider as the description.
- Filtering is fuzzy over `modelId provider`.
- No completions are returned when no candidate models are available or no filtered item matches.

## Model picker

### Entry points

- Press Ctrl+L (`app.model.select`) to open the picker.
- Submit `/model` to open the picker.
- Submit `/model <search>`:
  - First tries an exact model match without opening the picker.
  - If an exact match is found, selects that model immediately.
  - If no exact match is found, opens the picker with `<search>` pre-filled as the search input.

### Exact `/model <search>` matching

- Candidate models are current scoped models if any scope is active; otherwise authenticated available models.
- Matching is case-insensitive.
- `provider/modelId` matches canonically.
- Bare `modelId` matches only when exactly one candidate has that id.
- Ambiguous matches do not select; they fall back to opening the picker with the search term.

### Candidate set

- The picker refreshes the model registry before loading models.
- Only authenticated/configured models are shown.
- If a scoped model set exists, the picker starts in `scoped` scope and can toggle between `scoped` and `all`.
- If no scoped model set exists, only `all` exists and the picker shows:
  - `Only showing models from configured providers. Use /login to add providers.`
- If `models.json` has a load error, the picker shows the error text but still attempts to show available built-in models.

### Layout

- Header area:
  - With scoped models: `Scope: all | scoped`, with the active scope accented and the inactive scope muted.
  - Scope hint: Tab switches scope.
  - Without scoped models: warning text about configured providers and `/login`.
- Search input appears above the list.
- List shows at most 10 rows.
- Each row shows `modelId [provider]`.
- Current selected row is accented and prefixed with `→ `.
- The current model gets a trailing `✓`.
- If the list is scrolled, a muted `(selectedIndex/filteredCount)` indicator appears.
- If no rows match, show `No matching models`.
- Otherwise show `Model Name: <model.name>` below the list.

### Ordering

- In `all` scope:
  - Current model sorts first.
  - Remaining models sort by provider name.
  - Models from the same provider keep registry order.
- In `scoped` scope:
  - The configured scoped model order is preserved.
  - The current model is selected but not moved to the top.

### Search and keys

- Search filters fuzzily over `id provider provider/id provider id`.
- Up/down wraps around at list boundaries.
- Enter selects the highlighted model.
- Escape or Ctrl+C cancels and restores the editor.
- Tab toggles `all`/`scoped` only when scoped models exist.
- Any other input edits the search field and re-filters.

### Selection side effects

On successful selection:

- Save the selected provider/model as the default model in settings.
- Set the session model.
- Validate auth first; if auth is missing, show `No API key for <provider>/<modelId>` and do not switch.
- Append a model-change entry to the session log.
- Clamp/reapply the current thinking level for the new model.
- Emit model-select events for extensions.
- Invalidate the footer and update the editor border color.
- Show status `Model: <modelId>`.
- If selecting Anthropic with subscription auth, show the subscription extra-usage warning once.

## Scoped model picker

This picker configures which models Ctrl+P/Shift+Ctrl+P cycle through.

### Entry point

- Submit `/scoped-models`.
- If no authenticated models are available, show status `No models available` and do not open a picker.

### Initial enabled set

- If the session already has scoped models, use that ordered list.
- Otherwise, if settings have `enabledModels`, resolve those patterns into an ordered model list.
- Otherwise, use `null`, meaning all models enabled and no scoped filter.

### Layout

- Title: `Model Configuration`.
- Subtitle: `Session-only. Ctrl+S to save to settings.`
- Search input above the list.
- List shows at most 8 rows.
- Each row shows `modelId [provider]`.
- When enabled set is explicit, enabled rows show `✓`; disabled rows show `✗`.
- When enabled set is `null` (all enabled), no per-row checkmark is shown.
- Empty search result shows `No matching models`.
- If scrolled, show `(selectedIndex/filteredCount)`.
- Below the list, show `Model Name: <model.name>` for the selected row.
- Footer shows:
  - `Enter toggle`
  - `Ctrl+A all`
  - `Ctrl+X clear`
  - `Ctrl+P provider`
  - `Alt+Up/Alt+Down reorder`
  - `Ctrl+S save`
  - either `all enabled` or `<enabled>/<total> enabled`
  - `(unsaved)` while dirty

### Search and keys

- Search filters fuzzily over `model.id model.provider`.
- Up/down wraps around at list boundaries.
- Enter toggles the highlighted model.
- Ctrl+A enables all matching rows when search is non-empty, otherwise all rows.
- Ctrl+X clears all matching rows when search is non-empty, otherwise all rows.
- Ctrl+P toggles all models from the highlighted row's provider.
- Alt+Up/Alt+Down reorder enabled models, but only when an explicit enabled list exists and the selected row is enabled.
- Ctrl+S saves the current model set to settings and leaves the picker open.
- Ctrl+C clears search if search is non-empty; otherwise cancels.
- Escape cancels.

### Enabled-set semantics

- `null` means all enabled and no scoped filter.
- Toggling one model while enabled set is `null` creates an explicit one-item list containing only that model.
- Toggling an enabled model in an explicit list removes it.
- Toggling a disabled model appends it to the explicit list.
- Enabling all rows converts the state back to `null` when every model is enabled.
- Clearing all rows produces an empty explicit list.
- In session state, `null`, all models enabled, and zero models enabled all clear the scoped-model filter. pi-mono does not represent a cycle-through-no-models state.
- Provider toggle disables that provider if all provider models are currently enabled; otherwise it enables every model from that provider.
- Reordering swaps adjacent enabled ids and moves the highlight with the swapped item.

### Session and persistence side effects

Every unsaved change updates the current session immediately:

- If enabled ids are non-empty and fewer than all models, session scoped models become that resolved ordered list.
- Otherwise session scoped models are cleared.
- Provider count in the footer is recomputed.
- The UI rerenders.

Ctrl+S persistence:

- If all models are enabled or state is `null`, clear `enabledModels` from settings.
- Otherwise save the ordered `provider/modelId` ids to settings.
- Show status `Model selection saved to settings`.
- The picker remains open and dirty state clears.

## Model cycling

- Ctrl+P (`app.model.cycleForward`) cycles forward.
- Shift+Ctrl+P (`app.model.cycleBackward`) cycles backward.
- If scoped models exist, cycling uses only scoped models that currently have configured auth.
- If no scoped models exist, cycling uses all authenticated available models.
- Cycling wraps around.
- If there is only one cycle candidate, show:
  - `Only one model in scope` when scoped models exist.
  - `Only one model available` otherwise.
- On successful cycle:
  - Set the session model.
  - Save provider/model as default in settings.
  - Append a model-change entry.
  - Reapply or clamp thinking level.
  - Emit model-select event with source `cycle`.
  - Invalidate footer and update editor border color.
  - Show `Switched to <model.name-or-id>`.
  - If the new model supports reasoning and thinking is not `off`, append ` (thinking: <level>)`.

Scoped model thinking override:

- A scoped model can carry an explicit thinking level from a `model:level` pattern.
- When cycling to that scoped model, the explicit level overrides current session thinking.
- Without an explicit scoped level, cycling inherits the current session thinking preference and clamps it to the new model.

## Thinking/effort behavior

pi-mono calls this `thinking level`; Iris may label it `effort` if the product language requires it, but behavior should map to the same values.

### Levels

Supported level names and descriptions:

- `off` - No reasoning
- `minimal` - Very brief reasoning (~1k tokens)
- `low` - Light reasoning (~2k tokens)
- `medium` - Moderate reasoning (~8k tokens)
- `high` - Deep reasoning (~16k tokens)
- `xhigh` - Maximum reasoning (~32k tokens)

Default thinking level is `medium`.

### Available levels

- A non-reasoning model exposes only `off`.
- A reasoning model exposes `off`, `minimal`, `low`, `medium`, and `high` by default.
- `xhigh` is exposed only when the model has an explicit `thinkingLevelMap.xhigh` value.
- Any level mapped to `null` by the model is hidden.
- If no model is selected, pi-mono uses `off`, `minimal`, `low`, `medium`, and `high`.

### Clamping

When a requested level is not supported by the selected model:

- If the requested level is available, use it.
- Otherwise choose the first supported level at or above the requested level.
- If none exists above it, choose the nearest supported level below it.
- If no supported levels exist, use `off`.

### Cycle shortcut

- Shift+Tab (`app.thinking.cycle`) cycles through available thinking levels for the current model.
- If the model does not support thinking, show `Current model does not support thinking`.
- Otherwise:
  - Advance to the next available level with wraparound.
  - Save a session thinking-level change only if the effective level changed.
  - Save the default thinking level in settings when the model supports thinking, or when the effective level is not `off`.
  - Emit thinking-level events.
  - Invalidate footer and update editor border color.
  - Show status `Thinking level: <level>`.

### Settings picker

Entry:

- Submit `/settings`, then select the `Thinking level` setting.

Settings row:

- Label: `Thinking level`.
- Description: `Reasoning depth for thinking-capable models`.
- Current value: current session thinking level.

Submenu:

- Title: `Thinking Level`.
- Description: `Select reasoning depth for thinking-capable models`.
- Options are the current model's available thinking levels, with the descriptions above.
- The current level is preselected.
- Enter selects and closes the submenu.
- Escape returns to the settings menu.
- Selection updates session thinking, footer, and editor border color.

## Provider authentication picker

### `/login` flow overview

Submitting `/login` opens a two-step flow:

1. Auth method selector.
2. Provider selector for the chosen auth method.

The auth method selector title is `Select authentication method:` and has exactly two options:

- `Use a subscription`
- `Use an API key`

Auth method selector keys:

- Up/down or `j`/`k` move selection without wrap.
- Enter selects.
- Escape or Ctrl+C cancels and restores the editor.

Choosing `Use a subscription` opens the provider selector with OAuth-capable providers.
Choosing `Use an API key` opens the provider selector with API-key providers.

### Login provider list

OAuth/subscription providers:

- From registered OAuth providers.
- Display name is the OAuth provider name.
- Auth type is `oauth`.

API-key providers:

- Include any provider with a built-in display name, even if that provider also supports OAuth. This is why Anthropic appears in both subscription and API-key flows.
- Exclude built-in providers without a display name if they are OAuth-only.
- Include custom model providers that are not OAuth providers.
- Display name comes from dynamic registered provider name, dynamic OAuth name, built-in provider display name, or provider id fallback.
- Auth type is `api_key`.

Both provider lists are sorted by display name.

If no providers exist for a method:

- OAuth: show `No subscription providers available.`
- API key: show `No API key providers available.`

### Provider selector layout

- Login title: `Select provider to configure:`.
- Logout title: `Select provider to logout:`.
- Search input above the list.
- List shows at most 8 providers.
- Selected row is accented and prefixed `→ `.
- If scrolled, show `(selectedIndex/filteredCount)`.
- Empty result says:
  - `No providers available` when login provider list is empty.
  - `No providers logged in. Use /login first.` when logout provider list is empty.
  - `No matching providers` when search filters everything out.

### Provider selector search and keys

- Search filters fuzzily over `provider.name provider.id provider.authType`.
- Up/down do not wrap; they clamp at first/last row.
- Enter selects highlighted provider.
- Escape or Ctrl+C cancels.
- In login provider selection, cancel returns to the auth method selector, not directly to the editor.
- In logout provider selection, cancel restores the editor.

### Provider status indicators

Rows append a status indicator:

- Stored credential of the same auth type: `✓ configured`.
- Stored OAuth credential shown in API-key flow: `subscription configured`.
- Stored API-key credential shown in OAuth flow: `API key configured`.
- OAuth provider without stored credential: `unconfigured`.
- API-key provider with environment key: `✓ env: <ENV_VAR>`.
- API-key provider with runtime `--api-key`: `✓ runtime API key`.
- API-key provider with fallback/custom config: `✓ custom API key`.
- API-key provider with literal key in `models.json`: `✓ key in models.json`.
- API-key provider with command-backed key in `models.json`: `✓ command in models.json`.
- Otherwise: `unconfigured`.

Status lookup must not expose secret values and must not execute command-backed config just to draw the selector.

## API-key login dialog

Triggered by selecting an API-key provider, except Amazon Bedrock.

Dialog behavior:

- Replaces the editor.
- Title: `Login to <providerName>`.
- Prompt: `Enter API key:`.
- Enter submits.
- Escape or Ctrl+C cancels.
- The entered value is trimmed.
- Empty value fails with `API key cannot be empty.`.
- On cancel, restore editor and show no error.
- On non-cancel error, restore editor and show `Failed to save API key for <providerName>: <error>`.

Persistence:

- Store under the provider id in `auth.json` as `{ "type": "api_key", "key": <value> }`.
- `auth.json` is created with parent directory mode `0700` and file mode `0600` in pi-mono.
- Storage updates are lock-protected.

Security note for Iris:

- pi-mono's dialog uses the normal text input component; submitted prompt values are rendered back as `> <value>`. If Iris masks API-key input, that is a deliberate security improvement and a visible deviation from pi-mono.

## Amazon Bedrock API-key flow

Selecting Amazon Bedrock in the API-key provider selector does not ask for a single key.

Dialog:

- Title: `Amazon Bedrock setup`.
- Body:
  - `Amazon Bedrock uses AWS credentials instead of a single API key.`
  - `Configure an AWS profile, IAM keys, bearer token, or role-based credentials.`
  - `See:` followed by the pi providers docs path.
- Escape or Ctrl+C closes back to the editor.
- No credential is saved by this dialog.

## OAuth/subscription login dialog

Triggered by selecting an OAuth provider.

Dialog behavior:

- Replaces the editor.
- Title: `Login to <providerName>`.
- The dialog owns an abort signal. Cancel aborts in-flight login and resolves as `Login cancelled`.
- Cancel restores the editor and shows no error.
- Non-cancel errors restore the editor and show `Failed to login to <providerName>: <error>`.

OAuth callbacks must update the dialog as follows:

### Auth URL

On auth URL callback:

- Clear prior content.
- Show the URL as an OSC-8 terminal hyperlink.
- Show a second hyperlink with click hint:
  - macOS: `Cmd+click to open`
  - other platforms: `Ctrl+click to open`
- Show provider instructions if supplied.
- Open the browser to the URL.
- Request render.

For callback-server providers, also append a manual input prompt:

- `Paste redirect URL below, or complete login in browser:`
- The manual input races the callback server result.

### Device code

On device-code callback:

- Clear prior content.
- Show verification URI as a hyperlink.
- Show click hint as above.
- Show `Enter code: <userCode>`.
- Open the browser to the verification URI.
- Show `Waiting for authentication...`.

### Provider prompts

On prompt callback:

- Append the prompt without clearing prior auth URL/instructions.
- Show placeholder as `e.g., <placeholder>` when supplied.
- Add the input component.
- Show cancel/submit hint.
- Enter submits and replaces the input line with `> <submitted value>`.
- Multiple prompts preserve already-submitted values while the next prompt is active.

### Provider selection during OAuth

If the OAuth provider asks the user to choose from options:

- Temporarily replace the login dialog with a generic selector using the provider's prompt message.
- Options are displayed by label.
- Selecting returns the option id to the OAuth provider and restores the login dialog.
- Cancelling returns `undefined` and restores the login dialog.

### Progress

Progress callbacks append dim text lines and request render.

### Success

On successful OAuth login:

- Store provider credentials in `auth.json` as `{ "type": "oauth", ...credentials }`.
- Restore editor.
- Run the shared provider-authentication completion behavior below.

## Shared provider-authentication completion

After API-key save or OAuth success:

- Refresh the model registry.
- Recompute available provider count for the footer.
- Invalidate the footer.
- Update editor border color.
- Status action label is:
  - OAuth: `Logged in to <providerName>`.
  - API key: `Saved API key for <providerName>`.

If the previous model is the special unknown placeholder model:

- Try to select the provider's default model.
- If the provider has no configured default, show an error:
  - `<actionLabel>, but no default model is configured for provider "<providerId>". Use /model to select a model.`
- If no models are available for that provider, show:
  - `<actionLabel>, but no models are available for that provider. Use /model to select a model.`
- If the configured default model is unavailable, show:
  - `<actionLabel>, but its default model "<defaultModelId>" is not available. Use /model to select a model.`
- If selecting the default model fails, show:
  - `<actionLabel>, but selecting its default model failed: <error>. Use /model to select a model.`
- If default selection succeeds, status is:
  - `<actionLabel>. Selected <modelId>. Credentials saved to <authPath>`

Otherwise, do not auto-switch models. Status is:

- `<actionLabel>. Credentials saved to <authPath>`

Anthropic warning:

- If Anthropic OAuth credentials are active, or the resolved Anthropic API key starts with `sk-ant-oat`, show the warning once unless disabled in settings:
  - `Anthropic subscription auth is active. Third-party harness usage draws from extra usage and is billed per token, not your Claude plan limits. Manage extra usage at https://claude.ai/settings/usage.`

## `/logout` flow

Submitting `/logout` opens a provider selector containing only providers with stored credentials in `auth.json`.

- It does not list environment variables, runtime `--api-key`, fallback resolvers, or `models.json` credentials.
- If no stored credentials exist, show:
  - `No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.`

On provider selection:

- Remove that provider from `auth.json`.
- Refresh the model registry.
- Recompute available provider count.
- For OAuth credentials, show `Logged out of <providerName>`.
- For API-key credentials, show `Removed stored API key for <providerName>. Environment variables and models.json config are unchanged.`
- On failure, show `Logout failed: <error>`.

## Auth resolution behavior that affects UI

A model is available when its provider has configured auth by one of these means:

1. Runtime override from CLI `--api-key`.
2. Stored API key in `auth.json`.
3. Stored OAuth credential in `auth.json`.
4. Environment variable for the provider.
5. Fallback/custom provider resolver.
6. Provider request API key configured in `models.json`.

When `AuthStorage.getApiKey()` is called directly with fallback enabled, priority is:

1. Runtime override.
2. Stored API key.
3. Stored OAuth token, refreshing with a lock when expired.
4. Environment variable.
5. Fallback resolver unless explicitly disabled by caller.

When `ModelRegistry.getApiKeyAndHeaders()` resolves request auth, it disables the auth-storage fallback and then uses the provider request API key from `models.json` if auth storage did not produce a key. That gives request auth this effective priority:

1. Runtime override.
2. Stored API key.
3. Stored OAuth token, refreshing with a lock when expired.
4. Environment variable.
5. Provider `models.json` API key.

OAuth refresh failures preserve credentials for retry and can cause model discovery to skip the provider until the user logs in again.

## Minimum Iris acceptance checks

Implementing this in Iris is done when these user-visible checks pass:

1. `/model` opens a searchable model picker with current-model checkmark, provider badges, no unauthenticated models, and exact `/model provider/id` bypasses the picker.
2. Ctrl+L opens the same model picker.
3. `/model bad-prefix` opens the picker with `bad-prefix` in search and shows `No matching models` if none match.
4. Scoped model order set by `/scoped-models` is preserved in the model picker's scoped tab and in Ctrl+P cycling.
5. `/scoped-models` changes cycle scope immediately, but only Ctrl+S persists it to settings.
6. Shift+Tab cycles thinking levels only for thinking-capable models and shows the unsupported-model status otherwise.
7. `/settings` thinking picker shows only levels supported by the current model and applies clamping through session state.
8. `/login` first asks subscription vs API key, then shows the provider selector for that method with correct status badges.
9. API-key login trims input, rejects empty keys, stores non-empty keys, refreshes available models, and does not remove env/models.json credentials.
10. OAuth login shows auth URL/device-code/prompt/progress states, supports cancel, stores credentials on success, and refreshes available models.
11. `/logout` lists only stored `/login` credentials and removes only those credentials.
12. Anthropic subscription auth warning appears once when Anthropic subscription credentials become active.
