//! Fixture materialization: copy a committed fixture tree into a fresh temp
//! workspace, stripping the `.txt` suffix every committed fixture file carries
//! (so the copies never look like live sources to fmt/clippy/typos). The two
//! `build_*` functions synthesize trees too large to commit.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A temp workspace directory, removed on drop.
pub struct TempWorkspace {
    pub path: PathBuf,
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The committed fixtures root (`iris-bench/fixtures/`).
fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a unique, empty temp directory.
fn fresh_temp_dir() -> std::io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "iris-bench-{}-{}-{}",
        std::process::id(),
        nanos,
        seq
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Copy a committed fixture tree into a fresh temp workspace, stripping the
/// `.txt` suffix from every file.
pub fn materialize(fixture_id: &str) -> std::io::Result<TempWorkspace> {
    let src = fixtures_root().join(fixture_id);
    let dir = fresh_temp_dir()?;
    copy_stripping_txt(&src, &dir)?;
    Ok(TempWorkspace { path: dir })
}

fn copy_stripping_txt(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        if file_type.is_dir() {
            copy_stripping_txt(&entry.path(), &dst.join(name))?;
        } else {
            let name = name.to_string_lossy();
            let stripped = name.strip_suffix(".txt").unwrap_or(&name);
            std::fs::copy(entry.path(), dst.join(stripped))?;
        }
    }
    Ok(())
}

// --- Programmatic trees (too large to commit) ------------------------------
// Ported from iris-agent `bench_tokens/fixtures.rs`. Filled by the port.

/// Build a wide file tree that trips find's compaction rail: > `DEFAULT_FIND_LIMIT`
/// (1000) matches so the default limit omits some (needs_compact), with the
/// shown prefix small enough to fit the byte budget so grouping wins on bytes
/// (same paths, dir prefix shared once). A distinctive target path carries the
/// needle the paired live question asks for.
pub fn build_find_tree(root: &Path) {
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

/// Build decoy provider files around the chained PR-404-style workload. The
/// target bug is committed in `openai_codex_responses.rs`; these generated files
/// make broad `find`/`grep reasoning|summary` discovery noisy enough to exercise
/// output reduction without committing dozens of inert fixture files.
pub fn build_chained_provider_tree(root: &Path) {
    let dir = root.join("src/providers/generated");
    fs::create_dir_all(&dir).expect("create generated provider dir");
    for i in 0..72u32 {
        let body = format!(
            "//! Generated provider adapter decoy {i:02}.\n\
             //! Mentions reasoning summary routing, cache policy, and live rails,\n\
             //! but this file is not compiled into the fixture crate.\n\n\
             pub fn provider_{i:02}_label() -> &'static str {{\n\
                 \"provider-{i:02}\"\n\
             }}\n\n\
             pub fn reasoning_summary_supported_{i:02}() -> bool {{\n\
                 // Decoy: openai compatible summary support is negotiated elsewhere.\n\
                 {}\n\
             }}\n",
            i % 3 == 0
        );
        fs::write(dir.join(format!("provider_{i:02}.rs")), body).expect("write decoy provider");
    }
}
