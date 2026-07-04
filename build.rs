// Expose the compile-time target triple to the crate as `IRIS_TARGET`. The
// self-update path uses it to pick the matching prebuilt release asset
// (`iris-agent-<target>.tar.gz`). Cargo sets `TARGET` for build scripts but not
// for the crate itself, so this is the canonical way to read it at runtime.
//
// Also emit the `iris_dist` cfg when built by the release pipeline, which sets
// `IRIS_DIST=1` (see .github/workflows/release.yml). `update_strategy()` gates
// the self-replace path on this marker so that a source build with
// `--all-features` (feature on, marker unset) still falls back to cargo.
//
// Also concatenate the bash output-filter data files (ADR-0037) into one TOML
// blob under OUT_DIR so the filter engine can embed it with `include_str!`.
// Files are concatenated in sorted order for deterministic filter precedence;
// validation (parse, per-filter compile, inline tests) lives in unit tests in
// `src/tools/bash/filter/`, not here.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=IRIS_TARGET={target}");

    embed_bash_filters();

    println!("cargo::rustc-check-cfg=cfg(iris_dist)");
    println!("cargo::rerun-if-env-changed=IRIS_DIST");
    if std::env::var("IRIS_DIST").as_deref() == Ok("1") {
        println!("cargo::rustc-cfg=iris_dist");
    }
}

fn embed_bash_filters() {
    let data_dir = std::path::Path::new("src/tools/bash/filter/data");
    println!("cargo:rerun-if-changed=src/tools/bash/filter/data");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR must be set by Cargo");
    let dest = std::path::Path::new(&out_dir).join("bash_builtin_filters.toml");

    let mut files: Vec<_> = std::fs::read_dir(data_dir)
        .expect("src/tools/bash/filter/data must exist")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();

    let mut combined = String::from("schema_version = 1\n\n");
    for path in &files {
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        combined.push_str(&format!(
            "# --- {} ---\n",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        combined.push_str(&content);
        combined.push_str("\n\n");
    }
    std::fs::write(&dest, combined).expect("failed to write bash_builtin_filters.toml");
}
