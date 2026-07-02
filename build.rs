// Expose the compile-time target triple to the crate as `IRIS_TARGET`. The
// self-update path uses it to pick the matching prebuilt release asset
// (`iris-<target>.tar.gz`). Cargo sets `TARGET` for build scripts but not for
// the crate itself, so this is the canonical way to read it at runtime.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=IRIS_TARGET={target}");
}
