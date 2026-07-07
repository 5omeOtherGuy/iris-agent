# OpenWiki Docs

OpenWiki owns generated agent documentation for Iris. Keep that content in this
repository so it versions with the codebase and can be bundled into offline
release artifacts later. Keep the public docs website in a separate repository.

## Local setup

Initialize OpenWiki from the repository root:

```bash
bash scripts/openwiki-init.sh
```

The first run configures the model provider, API key, and optional LangSmith
tracing. OpenWiki stores those local secrets in `~/.openwiki/.env`; do not commit
that file or copy its contents into this repository.

## Writing loop

Generate or refresh the wiki:

```bash
bash scripts/openwiki-update.sh
```

OpenWiki writes generated documentation under `openwiki/`. Review the diff like
normal source changes. Commit `openwiki/` content when it reflects the current
Iris behavior.

## Codex subscription path

OpenWiki itself uses provider API keys. To generate the same `openwiki/` content
with an existing Codex subscription instead, run:

```bash
bash scripts/openwiki-codex-update.sh
```

This runs `codex exec` against the Iris checkout and asks Codex to create or
refresh Markdown files under `openwiki/`. It does not use OpenWiki's LangChain
agent or provider setup, but it produces the same repository-local wiki content
that the offline bundle and separate website consume.

Pass a focused request when updating a specific area:

```bash
bash scripts/openwiki-codex-update.sh "Refresh provider authentication docs"
```

Use the raw CLI wrapper for one-off prompts:

```bash
bash scripts/openwiki-run.sh -p "Refresh docs for provider authentication"
```

## Offline bundle boundary

The offline docs source of truth is `openwiki/` in this repository. The website
repo should import that directory from a checkout, release archive, or CI
artifact. Do not add a frontend build stack to this Rust repo just to present the
wiki.

Future offline packaging should embed or copy generated `openwiki/` content into
Iris release artifacts without depending on the website implementation.

## Website boundary

Use a separate repo, `iris-wiki-site`, for the website shell, theme, search UI,
hosting config, analytics, and frontend CI. The site may consume Iris docs by
copying `../iris-agent/openwiki/` into its own `content/openwiki/` directory
during development or CI.
