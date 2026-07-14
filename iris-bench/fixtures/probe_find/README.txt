`probe_find` fixture placeholder
================================

This directory is intentionally almost empty in Git. At benchmark run time,
`iris-bench/src/fixtures.rs::build_find_tree` materializes a deterministic tree of
1,350 Rust files (30 directories × 45 files) in the temporary workspace. That is
larger than `find`'s default 1,000-result rail and exercises grouped, bounded
output while preserving the distinctive `handler_zebra_target.rs` path required
by the paired workload.

Keep this file committed so fixture materialization has a source directory. Do
not commit the generated tree: doing so duplicates generated data, enlarges the
repository, and changes the benchmark setup. When changing the generated shape,
update the builder, workload/check, reduction expectations, and this note
together.
