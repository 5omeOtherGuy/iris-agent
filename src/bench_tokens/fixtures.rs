//! Fixture materialization: copy a committed fixture tree into a fresh temp
//! workspace, stripping the `.txt` suffix every fixture file carries (so
//! fmt/clippy/typos never treat them as live sources).

use std::fs;
use std::path::{Path, PathBuf};

use crate::tools::test_support::{TestDir, temp_dir};

/// The committed fixtures root (`src/bench_fixtures/tokens_per_task/`).
fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bench_fixtures/tokens_per_task")
}

/// Copy a fixture tree into a fresh temp workspace, stripping the `.txt`
/// suffix every committed fixture file carries. Returns the temp dir
/// (auto-cleaned on drop).
pub(crate) fn materialize(fixture: &str) -> TestDir {
    let dir = temp_dir();
    copy_stripping_txt(&fixtures_root().join(fixture), &dir.path);
    dir
}

/// Build a wide file tree that trips find's compaction rail: > `DEFAULT_FIND_LIMIT`
/// (1000) matches so the default limit omits some (needs_compact), with the
/// shown prefix small enough to fit the byte budget so grouping wins on bytes
/// (same paths, dir prefix shared once). A distinctive target path carries the
/// needle the paired live question asks for. Shared by the find render probe
/// (`probes.rs`) and the find live workload (`workloads.rs`).
pub(crate) fn build_find_tree(root: &Path) {
    // 30 dirs x 45 files = 1350 matches > the 1000 default limit.
    for d in 0..30u32 {
        let dir = root.join(format!("services/svc{d:02}/gateway"));
        fs::create_dir_all(&dir).expect("create probe dir");
        for f in 0..45u32 {
            fs::write(
                dir.join(format!("handler_{d:02}_{f:02}.rs")),
                b"// probe file\n",
            )
            .expect("write probe file");
        }
    }
    // The target file last (newest mtime) in an alphabetically-first dir, so it
    // sorts into the shown prefix under either ordering (find sorts mtime-desc,
    // ties by name) -- the needle must survive reduction, not be truncated away.
    let target_dir = root.join("services/aaa_target/gateway");
    fs::create_dir_all(&target_dir).expect("create target dir");
    fs::write(
        target_dir.join("handler_zebra_target.rs"),
        b"// probe file\n",
    )
    .expect("write target file");
}

/// Add generic cargo/npm decoys around PR-seeded chained repair fixtures. The
/// decoys are not imported by the fixture programs, but they make broad
/// discovery (`find`, `grep auth`, `grep fold`, `grep package`) look like a real
/// repository instead of a tiny two-file puzzle.
pub(crate) fn build_repair_noise_tree(root: &Path) {
    if root.join("Cargo.toml").exists() {
        let dir = root.join("src/generated_decoys");
        fs::create_dir_all(&dir).expect("create cargo decoy dir");
        for i in 0..48u32 {
            fs::write(
                dir.join(format!("workflow_decoy_{i:02}.rs")),
                format!(
                    "//! Decoy module {i:02}: mentions recall spans, fold resume chains, \
                     provider summaries, and cargo test diagnostics.\n\
                     pub const DECOY_{i:02}: &str = \"recall fold summary package auth\";\n"
                ),
            )
            .expect("write cargo decoy");
        }
    } else if root.join("package.json").exists() {
        let dir = root.join("src/generated-decoys");
        fs::create_dir_all(&dir).expect("create npm decoy dir");
        for i in 0..48u32 {
            fs::write(
                dir.join(format!("extension-decoy-{i:02}.mjs")),
                format!(
                    "// Decoy module {i:02}: mentions GitHub auth, npm pack, \
                     docs/private, untracked files, and package release checks.\n\
                     export const decoy{i:02} = 'github token package private docs';\n"
                ),
            )
            .expect("write npm decoy");
        }
    }
}

fn copy_stripping_txt(src: &Path, dst: &Path) {
    for entry in fs::read_dir(src).expect("fixture dir readable") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().expect("file type").is_dir() {
            let sub = dst.join(&name);
            fs::create_dir_all(&sub).expect("create fixture subdir");
            copy_stripping_txt(&entry.path(), &sub);
        } else {
            let target = name.strip_suffix(".txt").unwrap_or(&name);
            let bytes = fs::read(entry.path()).expect("read fixture file");
            fs::write(dst.join(target), bytes).expect("write materialized fixture");
        }
    }
}
