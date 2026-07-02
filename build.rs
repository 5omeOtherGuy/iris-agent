// Expose the compile-time target triple to the crate as `IRIS_TARGET`. The
// self-update path uses it to pick the matching prebuilt release asset
// (`iris-agent-<target>.tar.gz`). Cargo sets `TARGET` for build scripts but not
// for the crate itself, so this is the canonical way to read it at runtime.
//
// Also emit the `iris_dist` cfg when built by the release pipeline, which sets
// `IRIS_DIST=1` (see .github/workflows/release.yml). `update_strategy()` gates
// the self-replace path on this marker so that a source build with
// `--all-features` (feature on, marker unset) still falls back to cargo.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=IRIS_TARGET={target}");

    println!("cargo::rustc-check-cfg=cfg(iris_dist)");
    println!("cargo::rerun-if-env-changed=IRIS_DIST");
    if std::env::var("IRIS_DIST").as_deref() == Ok("1") {
        println!("cargo::rustc-cfg=iris_dist");
    }
}
