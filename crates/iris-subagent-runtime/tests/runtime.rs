use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iris_subagent_runtime::{
    ExecutorError, ExecutorFactory, ExecutorOutput, LocalExecutorFuture, RuntimeConfig,
    RuntimeError, RuntimeHandle, Usage, WorkerContext, WorkerExecutor, WorkerPriority,
    WorkerRequest, WorkerStatus,
};
use rand::random;

struct TestDir(std::path::PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("iris-subagent-{label}-{:032x}", random::<u128>()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct Stats {
    running: Arc<AtomicUsize>,
    max_running: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
    order: Arc<Mutex<Vec<String>>>,
}

impl Stats {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicUsize::new(0)),
            max_running: Arc::new(AtomicUsize::new(0)),
            completed: Arc::new(AtomicUsize::new(0)),
            order: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

struct FakeExecutor {
    prompt: String,
    stats: Stats,
    local: Rc<Cell<usize>>,
}

impl WorkerExecutor for FakeExecutor {
    fn execute<'a>(&'a mut self, context: WorkerContext) -> LocalExecutorFuture<'a> {
        Box::pin(async move {
            self.local.set(self.local.get() + 1);
            let running = self.stats.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.stats.max_running.fetch_max(running, Ordering::SeqCst);
            self.stats.order.lock().unwrap().push(self.prompt.clone());
            context.progress("started");
            if self.prompt == "pending" {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.stats.running.fetch_sub(1, Ordering::SeqCst);
            self.stats.completed.fetch_add(1, Ordering::SeqCst);
            let mut output = ExecutorOutput::text(
                format!("done {}", self.prompt),
                format!("full output for {}", self.prompt).into_bytes(),
            );
            output.usage = Usage::new(2, 3, 1, 0);
            Ok(output)
        })
    }
}

fn factory(stats: Stats) -> Arc<dyn ExecutorFactory> {
    Arc::new(move |request: &WorkerRequest| {
        Ok(Box::new(FakeExecutor {
            prompt: request.prompt.clone(),
            stats: stats.clone(),
            local: Rc::new(Cell::new(0)),
        }) as Box<dyn WorkerExecutor>)
    })
}

fn runtime(root: &TestDir, stats: Stats, concurrency: usize) -> RuntimeHandle {
    let mut config = RuntimeConfig::new(&root.0);
    config.global_concurrency = concurrency;
    config.per_group_concurrency = concurrency;
    config.cancellation_grace = Duration::from_millis(30);
    RuntimeHandle::start(config, factory(stats), None).unwrap()
}

#[test]
fn spawn_completes_without_poll_or_wait_driving_execution() {
    let root = TestDir::new("background");
    let stats = Stats::new();
    let runtime = runtime(&root, stats.clone(), 1);
    let id = runtime
        .spawn(WorkerRequest::read_only("independent"))
        .unwrap();

    std::thread::sleep(Duration::from_millis(100));

    assert_eq!(stats.completed.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.poll(&id).unwrap().status, WorkerStatus::Completed);
    runtime.shutdown().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_waiters_observe_the_same_terminal_result() {
    let root = TestDir::new("waiters");
    let runtime = runtime(&root, Stats::new(), 1);
    let id = runtime.spawn(WorkerRequest::read_only("same")).unwrap();

    let (one, two) = tokio::join!(runtime.wait(&id), runtime.wait(&id));
    let one = one.unwrap();
    let two = two.unwrap();

    assert_eq!(one, two);
    assert_eq!(runtime.wait(&id).await.unwrap(), one);
    runtime.shutdown().unwrap();
}

#[test]
fn bounded_queue_returns_typed_backpressure() {
    let root = TestDir::new("backpressure");
    let stats = Stats::new();
    let mut config = RuntimeConfig::new(&root.0);
    config.global_concurrency = 1;
    config.queue_capacity = 1;
    let runtime = RuntimeHandle::start(config, factory(stats), None).unwrap();
    runtime.spawn(WorkerRequest::read_only("pending")).unwrap();

    let error = runtime
        .spawn(WorkerRequest::read_only("rejected"))
        .unwrap_err();

    assert!(matches!(
        error,
        RuntimeError::Backpressure {
            queue: "worker",
            capacity: 1
        }
    ));
    runtime.shutdown().unwrap();
}

#[test]
fn scheduler_enforces_global_and_group_concurrency() {
    let root = TestDir::new("concurrency");
    let stats = Stats::new();
    let runtime = runtime(&root, stats.clone(), 2);
    let group = runtime
        .spawn_group(vec![
            WorkerRequest::read_only("one"),
            WorkerRequest::read_only("two"),
            WorkerRequest::read_only("three"),
        ])
        .unwrap();

    let result = futures::executor::block_on(runtime.wait_group(&group)).unwrap();

    assert_eq!(result.results.len(), 3);
    assert_eq!(stats.max_running.load(Ordering::SeqCst), 2);
    runtime.shutdown().unwrap();
}

#[test]
fn group_cancel_uses_the_same_terminal_runtime_path() {
    let root = TestDir::new("group-cancel");
    let runtime = runtime(&root, Stats::new(), 2);
    let group = runtime
        .spawn_group(vec![
            WorkerRequest::read_only("pending"),
            WorkerRequest::read_only("pending"),
        ])
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));

    runtime.cancel_group(&group).unwrap();
    let result = futures::executor::block_on(runtime.wait_group(&group)).unwrap();

    assert!(
        result
            .results
            .iter()
            .all(|result| result.status == WorkerStatus::Cancelled)
    );
    runtime.shutdown().unwrap();
}

#[test]
fn cancellation_hard_aborts_an_uncooperative_executor() {
    let root = TestDir::new("hard-abort");
    let runtime = runtime(&root, Stats::new(), 1);
    let id = runtime.spawn(WorkerRequest::read_only("pending")).unwrap();
    std::thread::sleep(Duration::from_millis(20));

    runtime.cancel(&id).unwrap();
    let result = runtime
        .wait_blocking_timeout(&id, Some(Duration::from_secs(1)))
        .unwrap();

    assert_eq!(result.status, WorkerStatus::Cancelled);
    assert!(result.message.unwrap().contains("grace"));
    runtime.shutdown().unwrap();
}

#[test]
fn oversized_output_is_preserved_behind_an_artifact() {
    let root = TestDir::new("artifact");
    let runtime = runtime(&root, Stats::new(), 1);
    let mut request = WorkerRequest::read_only("artifact");
    request.budgets.max_inline_output_bytes = Some(4);
    let id = runtime.spawn(request).unwrap();

    let result = runtime.wait_blocking(&id).unwrap();

    assert!(result.inline_output.is_none());
    assert_eq!(result.artifacts.len(), 1);
    assert!(result.artifacts[0].bytes > 4);
    assert_eq!(
        runtime.read_artifact(&result.artifacts[0].id).unwrap(),
        b"full output for artifact"
    );
    runtime.shutdown().unwrap();
}

#[test]
fn provider_round_budget_fails_the_result_without_losing_output() {
    let root = TestDir::new("budget");
    let runtime = runtime(&root, Stats::new(), 1);
    let mut request = WorkerRequest::read_only("budget");
    request.budgets.max_provider_rounds = Some(0);
    request.budgets.max_inline_output_bytes = Some(1);
    let id = runtime.spawn(request).unwrap();

    let result = runtime.wait_blocking(&id).unwrap();

    assert_eq!(result.status, WorkerStatus::Failed);
    assert_eq!(result.artifacts.len(), 1);
    assert!(result.message.unwrap().contains("budget"));
    runtime.shutdown().unwrap();
}

#[test]
fn events_replay_in_sequence() {
    let root = TestDir::new("events");
    let runtime = runtime(&root, Stats::new(), 1);
    let id = runtime.spawn(WorkerRequest::read_only("events")).unwrap();
    runtime.wait_blocking(&id).unwrap();

    let events = runtime.replay_events(&id, 0).unwrap();

    assert!(events.len() >= 5);
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    runtime.shutdown().unwrap();
}

#[test]
fn urgent_work_has_bounded_priority_over_normal_work() {
    let root = TestDir::new("fairness");
    let stats = Stats::new();
    let mut config = RuntimeConfig::new(&root.0);
    config.global_concurrency = 1;
    config.max_urgent_streak = 1;
    let runtime = RuntimeHandle::start(config, factory(stats.clone()), None).unwrap();
    let blocker = runtime.spawn(WorkerRequest::read_only("blocker")).unwrap();
    let normal = runtime.spawn(WorkerRequest::read_only("normal")).unwrap();
    let mut urgent_one = WorkerRequest::read_only("urgent-one");
    urgent_one.priority = WorkerPriority::InternalUrgent;
    let urgent_one = runtime.spawn(urgent_one).unwrap();
    let mut urgent_two = WorkerRequest::read_only("urgent-two");
    urgent_two.priority = WorkerPriority::InternalUrgent;
    let urgent_two = runtime.spawn(urgent_two).unwrap();

    for id in [&blocker, &normal, &urgent_one, &urgent_two] {
        runtime.wait_blocking(id).unwrap();
    }
    let order = stats.order.lock().unwrap().clone();

    assert_eq!(order[0], "blocker");
    assert_eq!(order[1], "urgent-one");
    assert_eq!(order[2], "normal");
    assert_eq!(order[3], "urgent-two");
    runtime.shutdown().unwrap();
}

#[test]
fn startup_recovery_marks_incomplete_jobs_interrupted_without_rerunning_them() {
    let root = TestDir::new("recovery");
    let id = iris_subagent_runtime::WorkerId::new();
    let request = WorkerRequest::read_only("must not rerun");
    let accepted = serde_json::json!({
        "record": "accepted",
        "schema_version": iris_subagent_runtime::SCHEMA_VERSION,
        "worker_id": id,
        "group_id": null,
        "request": request,
    });
    let event = serde_json::json!({
        "record": "event",
        "schema_version": iris_subagent_runtime::SCHEMA_VERSION,
        "event": {
            "schema_version": iris_subagent_runtime::SCHEMA_VERSION,
            "worker_id": id,
            "sequence": 1,
            "timestamp_ms": 1,
            "kind": {"type": "status", "data": "queued"}
        }
    });
    std::fs::write(
        root.0.join("runtime.jsonl"),
        format!("{}\n{}\n", accepted, event),
    )
    .unwrap();
    let stats = Stats::new();

    let runtime = runtime(&root, stats.clone(), 1);
    let recovered = runtime.recover();

    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].status, WorkerStatus::Interrupted);
    assert_eq!(stats.completed.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.wait_blocking(&id).unwrap().status,
        WorkerStatus::Interrupted
    );
    runtime.shutdown().unwrap();
}

struct FailingExecutor;

impl WorkerExecutor for FailingExecutor {
    fn execute<'a>(&'a mut self, _context: WorkerContext) -> LocalExecutorFuture<'a> {
        Box::pin(async { Err(ExecutorError::failed("candidate failed")) })
    }
}

#[test]
fn group_tolerates_individual_failure_results() {
    let root = TestDir::new("group-failure");
    let factory: Arc<dyn ExecutorFactory> = Arc::new(|request: &WorkerRequest| {
        if request.prompt == "fail" {
            Ok(Box::new(FailingExecutor) as Box<dyn WorkerExecutor>)
        } else {
            Ok(Box::new(FakeExecutor {
                prompt: request.prompt.clone(),
                stats: Stats::new(),
                local: Rc::new(Cell::new(0)),
            }) as Box<dyn WorkerExecutor>)
        }
    });
    let runtime = RuntimeHandle::start(RuntimeConfig::new(&root.0), factory, None).unwrap();
    let group = runtime
        .spawn_group(vec![
            WorkerRequest::read_only("ok"),
            WorkerRequest::read_only("fail"),
        ])
        .unwrap();

    let result = futures::executor::block_on(runtime.wait_group(&group)).unwrap();

    assert_eq!(result.results[0].status, WorkerStatus::Completed);
    assert_eq!(result.results[1].status, WorkerStatus::Failed);
    runtime.shutdown().unwrap();
}
