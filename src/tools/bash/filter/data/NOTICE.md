# Third-party notice: RTK filter definitions

Most `.toml` files in this directory are vendored from RTK
(<https://github.com/rtk-ai/rtk>), licensed under the Apache License 2.0
(see `LICENSE-APACHE-2.0` in this directory).

- Upstream: rtk-ai/rtk, commit `31f9d43d81f90d29e89142f3306473e786e59f6c`
  (2026-07-03), path `src/filters/*.toml`.
- Iris modifications are marked with a `# modified from RTK upstream: ...`
  comment at the top of the affected file. The systematic change: the Iris
  engine requires an `unless` error-guard on every `match_output`
  short-circuit rule (ADR-0037), so guards were added where upstream had none.
- Files with an `# iris-authored` header comment are original to Iris and are
  not from RTK.
- Inline `[[tests.<name>]]` sections are the upstream test cases, ported
  verbatim unless the file is marked modified; they run in
  `src/tools/bash/filter/mod.rs` unit tests.

Re-syncing with upstream: diff this directory against `src/filters/` at the
upstream HEAD, re-apply the marked Iris modifications, and run `cargo test`
(the inline tests and the corpus quality suite are the acceptance gate).
