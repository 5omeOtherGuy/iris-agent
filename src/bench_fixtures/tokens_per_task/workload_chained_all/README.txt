Combined chained-repair workload base.

The four single-bug subprojects (bytes/, clap/, nushell/, dayjs/) are assembled
into this workspace at run time by build_chained_all_tree, reusing the committed
single-bug fixtures rather than duplicating their files. See the
chained-all-four-fix workload in src/bench_tokens/workloads.rs.
