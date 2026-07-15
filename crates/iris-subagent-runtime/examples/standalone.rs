use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use iris_subagent_runtime::{
    ExecutorFactory, ExecutorOutput, LocalExecutorFuture, RuntimeConfig, RuntimeError,
    RuntimeHandle, WorkerContext, WorkerExecutor, WorkerRequest,
};

struct FakeExecutor {
    local_runs: Rc<Cell<u32>>,
}

impl WorkerExecutor for FakeExecutor {
    fn execute<'a>(&'a mut self, context: WorkerContext) -> LocalExecutorFuture<'a> {
        Box::pin(async move {
            self.local_runs.set(self.local_runs.get() + 1);
            context.progress("fake executor running");
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok(ExecutorOutput::text(
                "standalone complete",
                b"complete standalone output".to_vec(),
            ))
        })
    }
}

fn main() -> Result<(), RuntimeError> {
    let state = std::env::temp_dir().join(format!(
        "iris-subagent-standalone-{:032x}",
        rand::random::<u128>()
    ));
    let factory: Arc<dyn ExecutorFactory> = Arc::new(|_request: &WorkerRequest| {
        Ok(Box::new(FakeExecutor {
            local_runs: Rc::new(Cell::new(0)),
        }) as Box<dyn WorkerExecutor>)
    });
    let runtime = RuntimeHandle::start(RuntimeConfig::new(&state), factory, None)?;
    let worker = runtime.spawn(WorkerRequest::read_only("standalone fake work"))?;

    // No poll or wait drives execution. The dedicated scheduler progresses it.
    std::thread::sleep(Duration::from_millis(100));
    let snapshot = runtime.poll(&worker)?;
    assert!(snapshot.status.is_terminal());

    let first = runtime.wait_blocking(&worker)?;
    let second = runtime.wait_blocking(&worker)?;
    assert_eq!(first, second);
    println!("{}: {}", worker, first.summary);

    runtime.shutdown()?;
    std::fs::remove_dir_all(state).map_err(|error| RuntimeError::Thread(error.to_string()))?;
    Ok(())
}
