//! Thin binary shim. All CLI logic lives in the `iris_agent` library
//! (`src/lib.rs`) so the benchmark harness and the `iris-bench` workspace crate
//! can reuse it without duplicating module wiring.

fn main() -> std::process::ExitCode {
    iris_agent::run_cli()
}
