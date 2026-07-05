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
