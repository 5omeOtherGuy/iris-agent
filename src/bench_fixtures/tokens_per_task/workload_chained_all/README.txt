`workload_chained_all` fixture placeholder
==========================================

This directory is the committed base for the `chained-all-four-fix` benchmark.
At run time, `src/bench_tokens/fixtures.rs::build_chained_all_tree` assembles four
existing single-bug fixtures under:

- `bytes/`
- `clap/`
- `nushell/`
- `dayjs/`

Each remains an independent Cargo or npm project; there is no root workspace
manifest. One session must repair all four in the order defined by
`src/bench_tokens/workloads.rs`.

Keep this file committed so `materialize` has a source directory. Do not duplicate
the four fixture trees here: the builder intentionally reuses their canonical
committed copies. When adding, removing, reordering, or changing a subproject,
update the builder, `chained-all-four-fix` workload and checker, fixture needles,
and this note together.
