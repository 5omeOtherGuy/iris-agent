//! Thin CLI shim. All logic lives in the `iris_bench` library.

fn main() -> std::process::ExitCode {
    iris_bench::cli::main()
}
