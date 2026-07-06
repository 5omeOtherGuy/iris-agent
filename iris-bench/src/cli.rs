//! Command-line interface: `run`, `report`, `list`. Hand-rolled flag parsing
//! (the surface is small); no argument-parser dependency.

use std::path::PathBuf;
use std::process::ExitCode;

use iris_agent::harness::Arm;

use crate::spec::RunSpec;
use crate::{analysis, engine, report, tui, workloads};

/// Parse args and dispatch.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("report") => cmd_report(&args[1..]),
        Some("list") | Some("--list-workloads") => cmd_list(),
        Some("-h") | Some("--help") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("iris-bench: unknown command '{other}'\n");
            print_help();
            ExitCode::FAILURE
        }
    }
}

/// Minimal flag scanner. `--key value` for valued flags; presence for bools.
struct Flags {
    map: std::collections::HashMap<String, String>,
    bools: std::collections::HashSet<String>,
}

impl Flags {
    fn parse(args: &[String], valued: &[&str]) -> Result<Self, String> {
        let mut map = std::collections::HashMap::new();
        let mut bools = std::collections::HashSet::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            let Some(key) = a.strip_prefix("--").or_else(|| a.strip_prefix('-')) else {
                return Err(format!("unexpected argument '{a}'"));
            };
            let key = normalize_key(key);
            if valued.contains(&key.as_str()) {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| format!("flag --{key} needs a value"))?;
                map.insert(key, val.clone());
                i += 2;
            } else {
                bools.insert(key);
                i += 1;
            }
        }
        Ok(Flags { map, bools })
    }
    fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }
    fn has(&self, key: &str) -> bool {
        self.bools.contains(key)
    }
}

fn normalize_key(k: &str) -> String {
    match k {
        "n" => "runs".to_string(),
        "c" => "concurrency".to_string(),
        "no-tui" => "headless".to_string(),
        other => other.to_string(),
    }
}

fn parse_arms(raw: &str) -> Result<Vec<Arm>, String> {
    let mut arms = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let arm = match part.to_ascii_lowercase().as_str() {
            "b" | "baseline" => Arm::Baseline,
            "a" | "default" | "defaults" => Arm::Defaults,
            other => return Err(format!("unknown arm '{other}' (use baseline/defaults)")),
        };
        if !arms.contains(&arm) {
            arms.push(arm);
        }
    }
    if arms.is_empty() {
        return Err("no arms selected".to_string());
    }
    Ok(arms)
}

fn cmd_run(args: &[String]) -> ExitCode {
    let valued = [
        "models",
        "reasoning",
        "workloads",
        "arms",
        "runs",
        "concurrency",
        "log",
    ];
    let flags = match Flags::parse(args, &valued) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("iris-bench run: {e}");
            return ExitCode::FAILURE;
        }
    };

    let models: Vec<String> = match flags.get("models") {
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        None => {
            eprintln!(
                "iris-bench run: --models is required (e.g. --models anthropic:claude-haiku-4-5)"
            );
            return ExitCode::FAILURE;
        }
    };
    if models.is_empty() {
        eprintln!("iris-bench run: --models is empty");
        return ExitCode::FAILURE;
    }

    let reasoning = match flags.get("reasoning").unwrap_or("low") {
        "none" => None,
        other => Some(other.to_string()),
    };

    let catalog = workloads::catalog();
    let all_names: Vec<String> = catalog.iter().map(|w| w.name.to_string()).collect();
    let selected: Vec<String> = if flags.has("all") {
        all_names.clone()
    } else if let Some(v) = flags.get("workloads") {
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    } else {
        // Default: all non-bash (deny-gate) workloads, so a plain run is safe.
        catalog
            .iter()
            .filter(|w| !w.skip_permissions)
            .map(|w| w.name.to_string())
            .collect()
    };

    // Validate workload names.
    for name in &selected {
        if !all_names.contains(name) {
            eprintln!("iris-bench run: unknown workload '{name}'. Try `iris-bench list`.");
            return ExitCode::FAILURE;
        }
    }

    let arms = match parse_arms(flags.get("arms").unwrap_or("baseline,defaults")) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("iris-bench run: {e}");
            return ExitCode::FAILURE;
        }
    };

    let runs: usize = flags.get("runs").unwrap_or("1").parse().unwrap_or(0);
    if runs == 0 {
        eprintln!("iris-bench run: --runs must be >= 1");
        return ExitCode::FAILURE;
    }
    let concurrency: usize = flags.get("concurrency").unwrap_or("1").parse().unwrap_or(1);
    let log_path = PathBuf::from(flags.get("log").unwrap_or("target/iris-bench-runs.jsonl"));
    let allow_skip = flags.has("skip-permissions");

    // Safety: refuse bash workloads unless explicitly acknowledged.
    let wants_bash: Vec<&str> = catalog
        .iter()
        .filter(|w| w.skip_permissions && selected.iter().any(|n| n == w.name))
        .map(|w| w.name)
        .collect();
    if !wants_bash.is_empty() && !allow_skip {
        eprintln!(
            "iris-bench run: workloads {wants_bash:?} execute bash under skip-permissions.\n\
             Re-run with --skip-permissions to acknowledge (runs real shell in a temp workspace)."
        );
        return ExitCode::FAILURE;
    }

    let spec = RunSpec {
        models,
        reasoning,
        workloads: selected,
        arms,
        runs,
        concurrency,
        log_path: log_path.clone(),
        allow_skip_permissions: allow_skip,
    };

    // Cost preview + confirmation gate (real provider calls cost money).
    let cells = spec.cell_count();
    eprintln!(
        "Plan: {cells} cells  ({} models x {} workloads x {} arms x {} runs)  -> {}",
        spec.models.len(),
        spec.workloads.len(),
        spec.arms.len(),
        spec.runs,
        log_path.display(),
    );
    if !flags.has("yes") {
        eprintln!(
            "This runs REAL provider calls (and bash, if --skip-permissions). \
             Re-run with --yes to proceed."
        );
        return ExitCode::SUCCESS;
    }

    // Pre-flight model reachability (warn only; per-cell errors are logged).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for model in &spec.models {
        if let Err(e) = iris_agent::harness::validate_model(&cwd, model, spec.reasoning.as_deref())
        {
            eprintln!("warning: model '{model}' may be unreachable: {e}");
        }
    }

    let headless = flags.has("headless");
    let summary = if headless {
        run_headless(&spec, &catalog)
    } else {
        tui::run_live(&spec, &catalog).map_err(|e| e.to_string())
    };

    match summary {
        Ok(s) => {
            println!(
                "done: {} completed, {} succeeded, {} failed, {} skipped (of {})",
                s.completed, s.succeeded, s.failed, s.skipped, s.total
            );
            println!("log: {}", log_path.display());
            println!("report: iris-bench report --log {}", log_path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("iris-bench run: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_headless(
    spec: &RunSpec,
    catalog: &[workloads::WorkloadSpec],
) -> Result<engine::Summary, String> {
    use crate::event::CellEvent;
    let cancel = tokio_util::sync::CancellationToken::new();
    let total = spec.cell_count();
    engine::run(spec, catalog, &cancel, |ev| match ev {
        CellEvent::Started { index, cell } => {
            eprintln!(
                "[{:>4}/{total}] start  {} | {} | {} #{}",
                index + 1,
                cell.model,
                cell.workload,
                cell.arm,
                cell.run
            );
        }
        CellEvent::Finished { index, record } => {
            eprintln!(
                "[{:>4}/{total}] {}  {} | {} | {} #{} | turns {} | in {} tok",
                index + 1,
                if record.success { "ok  " } else { "fail" },
                record.model,
                record.workload,
                record.arm,
                record.run,
                record.turns,
                record.input_tokens
            );
        }
        CellEvent::Failed {
            index,
            cell,
            reason,
        } => {
            eprintln!(
                "[{:>4}/{total}] err   {} | {} | {} #{} | {}",
                index + 1,
                cell.model,
                cell.workload,
                cell.arm,
                cell.run,
                reason
            );
        }
    })
    .map_err(|e| e.to_string())
}

fn cmd_report(args: &[String]) -> ExitCode {
    let flags = match Flags::parse(args, &["log", "html"]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("iris-bench report: {e}");
            return ExitCode::FAILURE;
        }
    };
    let log = flags.get("log").unwrap_or("target/iris-bench-runs.jsonl");
    let body = match std::fs::read_to_string(log) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("iris-bench report: cannot read {log}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let analysis = analysis::analyze_jsonl(&body);
    print!("{}", analysis::format_report(&analysis));

    if let Some(out) = flags.get("html") {
        let html = report::html_report(&analysis);
        if let Err(e) = std::fs::write(out, html) {
            eprintln!("iris-bench report: cannot write {out}: {e}");
            return ExitCode::FAILURE;
        }
        println!("wrote HTML report: {out}");
    }
    ExitCode::SUCCESS
}

fn cmd_list() -> ExitCode {
    println!("Available workloads:");
    for w in workloads::catalog() {
        let kind = if w.skip_permissions {
            "bash (needs --skip-permissions)"
        } else {
            "deny-gate"
        };
        println!("  {:<28} {kind}", w.name);
    }
    ExitCode::SUCCESS
}

fn print_help() {
    println!(
        "iris-bench - token-per-task benchmark control + analysis\n\
         \n\
         USAGE:\n\
         \x20 iris-bench run [flags]        run the matrix (real provider)\n\
         \x20 iris-bench report [flags]     analyze a run log\n\
         \x20 iris-bench list               list available workloads\n\
         \n\
         RUN FLAGS:\n\
         \x20 --models a,b        provider:model specs (required)\n\
         \x20 --reasoning low     reasoning effort, or 'none' (default low)\n\
         \x20 --workloads a,b     workload names (default: all deny-gate)\n\
         \x20 --all               include bash workloads too\n\
         \x20 --arms b,defaults   arms to compare (default baseline,defaults)\n\
         \x20 -n, --runs N        repetitions per cell (default 1)\n\
         \x20 -c, --concurrency N parallel cells (default 1)\n\
         \x20 --log PATH          JSONL log path\n\
         \x20 --skip-permissions  acknowledge bash workloads\n\
         \x20 --headless          print progress instead of the TUI\n\
         \x20 --yes               confirm real spend and start\n\
         \n\
         REPORT FLAGS:\n\
         \x20 --log PATH          run log to analyze\n\
         \x20 --html OUT          also write a self-contained HTML report\n"
    );
}
