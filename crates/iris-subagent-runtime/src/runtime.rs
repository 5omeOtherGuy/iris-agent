use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle as LocalJoinHandle;
use tokio_util::sync::CancellationToken;

use crate::persistence::{Journal, JournalRecord, recovered_status};
use crate::{
    ArtifactStore, ExecutorError, ExecutorFactory, ExecutorOutput, FilesystemArtifactStore,
    GroupId, GroupResult, GroupSnapshot, RuntimeConfig, RuntimeError, SCHEMA_VERSION, Usage,
    WorkerContext, WorkerEvent, WorkerEventKind, WorkerFilter, WorkerId, WorkerPriority,
    WorkerRequest, WorkerResult, WorkerSnapshot, WorkerStatus,
};

#[derive(Debug)]
struct WorkerState {
    request: WorkerRequest,
    group_id: Option<GroupId>,
    status: WorkerStatus,
    usage: Usage,
    result: Option<WorkerResult>,
    events: Vec<WorkerEvent>,
}

#[derive(Debug)]
struct WorkerCell {
    state: Mutex<WorkerState>,
    changed: Condvar,
    notify: tokio::sync::Notify,
    token: CancellationToken,
}

impl WorkerCell {
    fn snapshot(&self) -> WorkerSnapshot {
        let state = lock(&self.state);
        snapshot(&state)
    }
}

struct Shared {
    config: RuntimeConfig,
    workers: Mutex<BTreeMap<WorkerId, Arc<WorkerCell>>>,
    groups: Mutex<BTreeMap<GroupId, Vec<WorkerId>>>,
    journal: Journal,
    events: broadcast::Sender<WorkerEvent>,
    artifacts: Arc<dyn ArtifactStore>,
    shutdown: AtomicBool,
    thread: Mutex<Option<JoinHandle<Result<(), RuntimeError>>>>,
}

impl Shared {
    fn cell(&self, id: &WorkerId) -> Result<Arc<WorkerCell>, RuntimeError> {
        lock(&self.workers)
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::NotFound {
                kind: "worker",
                id: id.to_string(),
            })
    }

    fn emit(&self, id: &WorkerId, kind: WorkerEventKind) -> Result<WorkerEvent, RuntimeError> {
        let cell = self.cell(id)?;
        let mut state = lock(&cell.state);
        if state.status.is_terminal() {
            return state
                .events
                .last()
                .cloned()
                .ok_or_else(|| RuntimeError::NotFound {
                    kind: "worker event",
                    id: id.to_string(),
                });
        }
        if state.status == WorkerStatus::WaitingForApproval
            && !matches!(kind, WorkerEventKind::ApprovalWait { .. })
        {
            self.append_event_locked(
                id,
                &mut state,
                WorkerEventKind::Status(WorkerStatus::Running),
            )?;
            state.status = WorkerStatus::Running;
        }
        match &kind {
            WorkerEventKind::Status(status) => state.status = *status,
            WorkerEventKind::ApprovalWait { .. } => state.status = WorkerStatus::WaitingForApproval,
            WorkerEventKind::Usage(usage) => {
                state.usage = usage.clone();
                if exceeds_usage(&state.request, usage) {
                    cell.token.cancel();
                }
            }
            _ => {}
        }
        let event = self.append_event_locked(id, &mut state, kind)?;
        drop(state);
        cell.changed.notify_all();
        cell.notify.notify_waiters();
        Ok(event)
    }

    fn append_event_locked(
        &self,
        id: &WorkerId,
        state: &mut WorkerState,
        kind: WorkerEventKind,
    ) -> Result<WorkerEvent, RuntimeError> {
        let event = WorkerEvent {
            schema_version: SCHEMA_VERSION,
            worker_id: id.clone(),
            sequence: state.events.last().map_or(1, |event| event.sequence + 1),
            timestamp_ms: now_ms(),
            kind,
        };
        self.journal.append(&JournalRecord::Event {
            schema_version: SCHEMA_VERSION,
            event: event.clone(),
        })?;
        state.events.push(event.clone());
        let _ = self.events.send(event.clone());
        Ok(event)
    }

    fn finish(
        &self,
        id: &WorkerId,
        outcome: Result<ExecutorOutput, ExecutorError>,
        forced_message: Option<String>,
    ) -> Result<WorkerResult, RuntimeError> {
        let cell = self.cell(id)?;
        let mut state = lock(&cell.state);
        if let Some(result) = &state.result {
            return Ok(result.clone());
        }
        let cancelled = cell.token.is_cancelled();
        let (status, output, message) = if let Some(message) = forced_message {
            (WorkerStatus::Cancelled, None, Some(message))
        } else {
            match outcome {
                Ok(output) if cancelled => (
                    WorkerStatus::Cancelled,
                    Some(output),
                    Some("cancelled".to_string()),
                ),
                Ok(output) => (WorkerStatus::Completed, Some(output), None),
                Err(error) if error.cancelled || cancelled => {
                    (WorkerStatus::Cancelled, None, Some(error.message))
                }
                Err(error) => (WorkerStatus::Failed, None, Some(error.message)),
            }
        };
        let mut result = shape_result(
            id,
            status,
            output,
            message,
            &state.request,
            self.config.default_inline_output_bytes,
            self.artifacts.as_ref(),
        );
        if let Err(error) = result {
            result = Ok(WorkerResult {
                schema_version: SCHEMA_VERSION,
                worker_id: id.clone(),
                status: WorkerStatus::Failed,
                summary: String::new(),
                inline_output: None,
                artifacts: Vec::new(),
                usage: state.usage.clone(),
                changed_paths: Vec::new(),
                worktree: None,
                apply_plan_id: None,
                host: Default::default(),
                message: Some(error.to_string()),
            });
        }
        let mut result = result.expect("fallback result is infallible");
        if result.usage == Usage::default() {
            result.usage = state.usage.clone();
        }
        let sequence = state.events.last().map_or(1, |event| event.sequence + 1);
        let terminal_events = [
            WorkerEvent {
                schema_version: SCHEMA_VERSION,
                worker_id: id.clone(),
                sequence,
                timestamp_ms: now_ms(),
                kind: WorkerEventKind::Status(result.status),
            },
            WorkerEvent {
                schema_version: SCHEMA_VERSION,
                worker_id: id.clone(),
                sequence: sequence + 1,
                timestamp_ms: now_ms(),
                kind: WorkerEventKind::Completed,
            },
        ];
        self.journal.finish(&result, &terminal_events)?;
        state.status = result.status;
        state.result = Some(result.clone());
        for event in terminal_events {
            state.events.push(event.clone());
            let _ = self.events.send(event);
        }
        drop(state);
        cell.changed.notify_all();
        cell.notify.notify_waiters();
        Ok(result)
    }
}

/// Cloneable, thread-safe handle to the backend-owned scheduler.
#[derive(Clone)]
pub struct RuntimeHandle {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
}

impl RuntimeHandle {
    /// Starts a dedicated scheduler thread and recovers durable records.
    pub fn start(
        config: RuntimeConfig,
        factory: Arc<dyn ExecutorFactory>,
        artifacts: Option<Arc<dyn ArtifactStore>>,
    ) -> Result<Self, RuntimeError> {
        validate_config(&config)?;
        let journal = Journal::open(&config.state_dir)?;
        let artifacts = match artifacts {
            Some(store) => store,
            None => Arc::new(FilesystemArtifactStore::new(
                config.state_dir.join("artifacts"),
            )?),
        };
        let recovered = journal.recover()?;
        let (event_tx, _) = broadcast::channel(config.event_channel_capacity);
        let shared = Arc::new(Shared {
            config: config.clone(),
            workers: Mutex::new(BTreeMap::new()),
            groups: Mutex::new(recovered.groups),
            journal,
            events: event_tx,
            artifacts,
            shutdown: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        recover_workers(&shared, recovered.workers)?;
        let (commands, receiver) = mpsc::channel(config.command_capacity);
        let scheduler_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("iris-subagent-scheduler".to_string())
            .spawn(move || scheduler_thread(scheduler_shared, factory, receiver))
            .map_err(|error| RuntimeError::Thread(error.to_string()))?;
        *lock(&shared.thread) = Some(thread);
        Ok(Self { shared, commands })
    }

    /// Durably accepts and independently queues one worker.
    pub fn spawn(&self, request: WorkerRequest) -> Result<WorkerId, RuntimeError> {
        request.validate()?;
        self.ensure_running()?;
        let permit = self
            .commands
            .try_reserve()
            .map_err(|_| self.command_backpressure())?;
        self.ensure_worker_capacity(1)?;
        let id = WorkerId::new();
        let cell = self.accept_worker(&id, None, request)?;
        lock(&self.shared.workers).insert(id.clone(), cell);
        permit.send(Command::StartBatch(vec![id.clone()]));
        Ok(id)
    }

    /// Durably accepts a group and queues every member as one scheduler command.
    pub fn spawn_group(&self, requests: Vec<WorkerRequest>) -> Result<GroupId, RuntimeError> {
        if requests.is_empty() {
            return Err(RuntimeError::InvalidRequest(
                "worker group must contain at least one request".to_string(),
            ));
        }
        for request in &requests {
            request.validate()?;
        }
        self.ensure_running()?;
        let permit = self
            .commands
            .try_reserve()
            .map_err(|_| self.command_backpressure())?;
        self.ensure_worker_capacity(requests.len())?;
        let group_id = GroupId::new();
        let ids = requests.iter().map(|_| WorkerId::new()).collect::<Vec<_>>();
        self.shared.journal.append(&JournalRecord::Group {
            schema_version: SCHEMA_VERSION,
            group_id: group_id.clone(),
            workers: ids.clone(),
        })?;
        let mut accepted = Vec::with_capacity(ids.len());
        for (id, request) in ids.iter().zip(requests) {
            accepted.push((
                id.clone(),
                self.accept_worker(id, Some(&group_id), request)?,
            ));
        }
        lock(&self.shared.groups).insert(group_id.clone(), ids.clone());
        lock(&self.shared.workers).extend(accepted);
        permit.send(Command::StartBatch(ids));
        Ok(group_id)
    }

    fn accept_worker(
        &self,
        id: &WorkerId,
        group_id: Option<&GroupId>,
        request: WorkerRequest,
    ) -> Result<Arc<WorkerCell>, RuntimeError> {
        let event = WorkerEvent {
            schema_version: SCHEMA_VERSION,
            worker_id: id.clone(),
            sequence: 1,
            timestamp_ms: now_ms(),
            kind: WorkerEventKind::Status(WorkerStatus::Queued),
        };
        self.shared.journal.accept(id, group_id, &request, &event)?;
        let _ = self.shared.events.send(event.clone());
        Ok(Arc::new(WorkerCell {
            state: Mutex::new(WorkerState {
                request,
                group_id: group_id.cloned(),
                status: WorkerStatus::Queued,
                usage: Usage::default(),
                result: None,
                events: vec![event],
            }),
            changed: Condvar::new(),
            notify: tokio::sync::Notify::new(),
            token: CancellationToken::new(),
        }))
    }

    /// Reads one content-addressed artifact by handle.
    pub fn read_artifact(&self, id: &crate::ArtifactId) -> Result<Vec<u8>, RuntimeError> {
        self.shared.artifacts.get(id)
    }

    /// Returns a non-consuming snapshot.
    pub fn poll(&self, id: &WorkerId) -> Result<WorkerSnapshot, RuntimeError> {
        Ok(self.shared.cell(id)?.snapshot())
    }

    /// Lists non-consuming snapshots matching a filter.
    pub fn list(&self, filter: &WorkerFilter) -> Vec<WorkerSnapshot> {
        lock(&self.shared.workers)
            .values()
            .map(|cell| cell.snapshot())
            .filter(|snapshot| matches_filter(snapshot, filter))
            .collect()
    }

    /// Replays events after `sequence` (exclusive).
    pub fn replay_events(
        &self,
        id: &WorkerId,
        sequence: u64,
    ) -> Result<Vec<WorkerEvent>, RuntimeError> {
        let cell = self.shared.cell(id)?;
        let events = lock(&cell.state)
            .events
            .iter()
            .filter(|event| event.sequence > sequence)
            .cloned()
            .collect();
        Ok(events)
    }

    /// Subscribes to replay followed by live runtime events.
    pub fn subscribe(
        &self,
        id: &WorkerId,
        after_sequence: u64,
    ) -> Result<EventSubscription, RuntimeError> {
        let replay = self.replay_events(id, after_sequence)?.into();
        Ok(EventSubscription {
            worker_id: id.clone(),
            replay,
            receiver: self.shared.events.subscribe(),
        })
    }

    /// Waits asynchronously without consuming the terminal result.
    pub async fn wait(&self, id: &WorkerId) -> Result<WorkerResult, RuntimeError> {
        let cell = self.shared.cell(id)?;
        loop {
            let notified = cell.notify.notified();
            if let Some(result) = lock(&cell.state).result.clone() {
                return Ok(result);
            }
            notified.await;
        }
    }

    /// Waits synchronously without consuming the terminal result.
    pub fn wait_blocking(&self, id: &WorkerId) -> Result<WorkerResult, RuntimeError> {
        self.wait_blocking_timeout(id, None)
    }

    /// Waits synchronously with an optional caller deadline.
    pub fn wait_blocking_timeout(
        &self,
        id: &WorkerId,
        timeout: Option<Duration>,
    ) -> Result<WorkerResult, RuntimeError> {
        let cell = self.shared.cell(id)?;
        let mut state = lock(&cell.state);
        let deadline = timeout.map(|duration| Instant::now() + duration);
        loop {
            if let Some(result) = &state.result {
                return Ok(result.clone());
            }
            state = if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(RuntimeError::WaitTimeout);
                }
                let (guard, wait) = cell
                    .changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|poison| poison.into_inner());
                if wait.timed_out() && guard.result.is_none() {
                    return Err(RuntimeError::WaitTimeout);
                }
                guard
            } else {
                cell.changed
                    .wait(state)
                    .unwrap_or_else(|poison| poison.into_inner())
            };
        }
    }

    /// Requests cooperative cancellation and bounded hard abort.
    pub fn cancel(&self, id: &WorkerId) -> Result<WorkerSnapshot, RuntimeError> {
        let cell = self.shared.cell(id)?;
        let status = lock(&cell.state).status;
        if status.is_terminal() {
            return Ok(cell.snapshot());
        }
        if status == WorkerStatus::Queued {
            cell.token.cancel();
            self.shared.finish(
                id,
                Err(ExecutorError::cancelled("cancelled while queued")),
                Some("cancelled while queued".to_string()),
            )?;
            return Ok(cell.snapshot());
        }
        let permit = self
            .commands
            .try_reserve()
            .map_err(|_| self.command_backpressure())?;
        cell.token.cancel();
        permit.send(Command::Cancel(id.clone()));
        Ok(cell.snapshot())
    }

    /// Returns a non-consuming group snapshot.
    pub fn poll_group(&self, id: &GroupId) -> Result<GroupSnapshot, RuntimeError> {
        let workers =
            lock(&self.shared.groups)
                .get(id)
                .cloned()
                .ok_or_else(|| RuntimeError::NotFound {
                    kind: "group",
                    id: id.to_string(),
                })?;
        let snapshots = workers
            .iter()
            .map(|worker| self.poll(worker))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(GroupSnapshot {
            group_id: id.clone(),
            workers,
            snapshots,
        })
    }

    /// Waits for every group member, tolerating individual failures as results.
    pub async fn wait_group(&self, id: &GroupId) -> Result<GroupResult, RuntimeError> {
        let workers = self.poll_group(id)?.workers;
        let mut results = Vec::with_capacity(workers.len());
        for worker in workers {
            results.push(self.wait(&worker).await?);
        }
        Ok(GroupResult {
            group_id: id.clone(),
            results,
        })
    }

    /// Cancels every non-terminal member of a group.
    pub fn cancel_group(&self, id: &GroupId) -> Result<GroupSnapshot, RuntimeError> {
        let workers = self.poll_group(id)?.workers;
        for worker in workers {
            let _ = self.cancel(&worker)?;
        }
        self.poll_group(id)
    }

    /// Returns records recovered from a previous runtime instance.
    pub fn recover(&self) -> Vec<WorkerSnapshot> {
        self.list(&WorkerFilter {
            status: None,
            group_id: None,
            session_id: None,
            include_internal: true,
        })
        .into_iter()
        .filter(|snapshot| {
            matches!(
                snapshot.status,
                WorkerStatus::Interrupted | WorkerStatus::Adoptable
            )
        })
        .collect()
    }

    /// Cancels, joins, and stops all scheduler-owned work.
    pub fn shutdown(&self) -> Result<(), RuntimeError> {
        if !self.shared.shutdown.swap(true, Ordering::SeqCst) {
            loop {
                match self.commands.try_send(Command::Shutdown) {
                    Ok(()) => break,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        }
        let thread = lock(&self.shared.thread).take();
        if let Some(thread) = thread {
            thread
                .join()
                .map_err(|_| RuntimeError::Thread("scheduler panicked".to_string()))??;
        }
        Ok(())
    }

    fn ensure_running(&self) -> Result<(), RuntimeError> {
        if self.shared.shutdown.load(Ordering::SeqCst) || self.commands.is_closed() {
            Err(RuntimeError::Shutdown)
        } else {
            Ok(())
        }
    }

    fn ensure_worker_capacity(&self, additional: usize) -> Result<(), RuntimeError> {
        let active = lock(&self.shared.workers)
            .values()
            .filter(|cell| !lock(&cell.state).status.is_terminal())
            .count();
        if active.saturating_add(additional) > self.shared.config.queue_capacity {
            Err(RuntimeError::Backpressure {
                queue: "worker",
                capacity: self.shared.config.queue_capacity,
            })
        } else {
            Ok(())
        }
    }

    fn command_backpressure(&self) -> RuntimeError {
        if self.shared.shutdown.load(Ordering::SeqCst) || self.commands.is_closed() {
            RuntimeError::Shutdown
        } else {
            RuntimeError::Backpressure {
                queue: "command",
                capacity: self.shared.config.command_capacity,
            }
        }
    }
}

/// Replay-plus-live worker event subscription.
pub struct EventSubscription {
    worker_id: WorkerId,
    replay: VecDeque<WorkerEvent>,
    receiver: broadcast::Receiver<WorkerEvent>,
}

impl EventSubscription {
    /// Receives the next event for the subscribed worker.
    pub async fn recv(&mut self) -> Result<WorkerEvent, RuntimeError> {
        if let Some(event) = self.replay.pop_front() {
            return Ok(event);
        }
        loop {
            match self.receiver.recv().await {
                Ok(event) if event.worker_id == self.worker_id => return Ok(event),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Err(RuntimeError::Shutdown),
            }
        }
    }

    /// Blocking variant of [`recv`](Self::recv).
    pub fn recv_blocking(&mut self) -> Result<WorkerEvent, RuntimeError> {
        if let Some(event) = self.replay.pop_front() {
            return Ok(event);
        }
        loop {
            match self.receiver.blocking_recv() {
                Ok(event) if event.worker_id == self.worker_id => return Ok(event),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Err(RuntimeError::Shutdown),
            }
        }
    }
}

enum Command {
    StartBatch(Vec<WorkerId>),
    Cancel(WorkerId),
    Shutdown,
}

struct RunningTask {
    handle: LocalJoinHandle<()>,
    cancel_deadline: Option<Instant>,
    group_id: Option<GroupId>,
}

fn scheduler_thread(
    shared: Arc<Shared>,
    factory: Arc<dyn ExecutorFactory>,
    receiver: mpsc::Receiver<Command>,
) -> Result<(), RuntimeError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| RuntimeError::Thread(error.to_string()))?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, scheduler_loop(shared, factory, receiver))
}

async fn scheduler_loop(
    shared: Arc<Shared>,
    factory: Arc<dyn ExecutorFactory>,
    mut commands: mpsc::Receiver<Command>,
) -> Result<(), RuntimeError> {
    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();
    let mut normal = VecDeque::new();
    let mut urgent = VecDeque::new();
    let mut running: BTreeMap<WorkerId, RunningTask> = BTreeMap::new();
    let mut group_running: BTreeMap<GroupId, usize> = BTreeMap::new();
    let mut urgent_streak = 0usize;
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    let mut stopping = false;

    loop {
        launch_ready(
            &shared,
            &factory,
            &completed_tx,
            &mut normal,
            &mut urgent,
            &mut running,
            &mut group_running,
            &mut urgent_streak,
        )?;
        if stopping && running.is_empty() {
            break;
        }
        tokio::select! {
            command = commands.recv(), if !stopping => match command {
                Some(Command::StartBatch(ids)) => {
                    for id in ids {
                        let priority = shared.cell(&id)?.snapshot().request.priority;
                        match priority {
                            WorkerPriority::InternalUrgent => urgent.push_back(id),
                            WorkerPriority::Normal => normal.push_back(id),
                        }
                    }
                }
                Some(Command::Cancel(id)) => {
                    if let Some(task) = running.get_mut(&id) {
                        task.cancel_deadline = Some(Instant::now() + shared.config.cancellation_grace);
                    }
                }
                Some(Command::Shutdown) | None => {
                    stopping = true;
                    normal.clear();
                    urgent.clear();
                    for (id, task) in &mut running {
                        shared.cell(id)?.token.cancel();
                        task.cancel_deadline = Some(Instant::now() + shared.config.cancellation_grace);
                    }
                    for (id, cell) in lock(&shared.workers).iter() {
                        if lock(&cell.state).status == WorkerStatus::Queued {
                            let _ = shared.finish(id, Err(ExecutorError::cancelled("runtime shutdown")), Some("runtime shutdown".to_string()));
                        }
                    }
                }
            },
            Some((id, outcome)) = completed_rx.recv() => {
                if let Some(task) = running.remove(&id) {
                    decrement_group(&mut group_running, task.group_id.as_ref());
                    let _ = shared.finish(&id, outcome, None)?;
                }
            }
            _ = interval.tick() => {
                let now = Instant::now();
                let ids = running.keys().cloned().collect::<Vec<_>>();
                for id in ids {
                    let token_cancelled = shared.cell(&id)?.token.is_cancelled();
                    let task = running.get_mut(&id).expect("running id exists");
                    if token_cancelled && task.cancel_deadline.is_none() {
                        task.cancel_deadline = Some(now + shared.config.cancellation_grace);
                    }
                    if task.cancel_deadline.is_some_and(|deadline| deadline <= now) {
                        let task = running.remove(&id).expect("running id exists");
                        task.handle.abort();
                        decrement_group(&mut group_running, task.group_id.as_ref());
                        let _ = shared.finish(&id, Err(ExecutorError::cancelled("cancellation grace elapsed")), Some("cancelled after grace period".to_string()))?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn launch_ready(
    shared: &Arc<Shared>,
    factory: &Arc<dyn ExecutorFactory>,
    completed: &mpsc::UnboundedSender<(WorkerId, Result<ExecutorOutput, ExecutorError>)>,
    normal: &mut VecDeque<WorkerId>,
    urgent: &mut VecDeque<WorkerId>,
    running: &mut BTreeMap<WorkerId, RunningTask>,
    group_running: &mut BTreeMap<GroupId, usize>,
    urgent_streak: &mut usize,
) -> Result<(), RuntimeError> {
    while running.len() < shared.config.global_concurrency {
        let Some(id) = next_eligible(shared, normal, urgent, group_running, urgent_streak)? else {
            break;
        };
        let cell = shared.cell(&id)?;
        if lock(&cell.state).status.is_terminal() {
            continue;
        }
        shared.emit(&id, WorkerEventKind::Status(WorkerStatus::Initializing))?;
        let request = cell.snapshot().request;
        let mut executor = match factory.create(&request) {
            Ok(executor) => executor,
            Err(error) => {
                shared.finish(&id, Err(ExecutorError::failed(error.to_string())), None)?;
                continue;
            }
        };
        shared.emit(&id, WorkerEventKind::Status(WorkerStatus::Running))?;
        let group_id = cell.snapshot().group_id;
        if let Some(group_id) = &group_id {
            *group_running.entry(group_id.clone()).or_default() += 1;
        }
        let task_shared = shared.clone();
        let task_id = id.clone();
        let task_cell = cell.clone();
        let task_completed = completed.clone();
        let wall_clock = request.budgets.wall_clock_ms.map(Duration::from_millis);
        let emit_shared = shared.clone();
        let emit_id = id.clone();
        let emit = std::rc::Rc::new(move |kind| {
            let _ = emit_shared.emit(&emit_id, kind);
        });
        let context = WorkerContext::new(
            id.clone(),
            group_id.clone(),
            request,
            task_cell.token.clone(),
            emit,
            task_shared.artifacts.clone(),
        );
        let handle = tokio::task::spawn_local(async move {
            let future = executor.execute(context);
            let outcome = if let Some(limit) = wall_clock {
                match tokio::time::timeout(limit, future).await {
                    Ok(outcome) => outcome,
                    Err(_) => Err(ExecutorError::failed(format!(
                        "wall-clock limit exceeded after {} ms",
                        limit.as_millis()
                    ))),
                }
            } else {
                future.await
            };
            let _ = task_completed.send((task_id, outcome));
        });
        running.insert(
            id,
            RunningTask {
                handle,
                cancel_deadline: None,
                group_id,
            },
        );
    }
    Ok(())
}

fn next_eligible(
    shared: &Shared,
    normal: &mut VecDeque<WorkerId>,
    urgent: &mut VecDeque<WorkerId>,
    group_running: &BTreeMap<GroupId, usize>,
    urgent_streak: &mut usize,
) -> Result<Option<WorkerId>, RuntimeError> {
    let prefer_urgent = !urgent.is_empty()
        && (normal.is_empty() || *urgent_streak < shared.config.max_urgent_streak);
    let first_urgent = prefer_urgent;
    for use_urgent in [first_urgent, !first_urgent] {
        let queue = if use_urgent {
            &mut *urgent
        } else {
            &mut *normal
        };
        let len = queue.len();
        for _ in 0..len {
            let id = queue.pop_front().expect("queue length checked");
            let group = shared.cell(&id)?.snapshot().group_id;
            let eligible = group.as_ref().is_none_or(|group| {
                group_running.get(group).copied().unwrap_or(0) < shared.config.per_group_concurrency
            });
            if eligible {
                if use_urgent {
                    *urgent_streak += 1;
                } else {
                    *urgent_streak = 0;
                }
                return Ok(Some(id));
            }
            queue.push_back(id);
        }
    }
    Ok(None)
}

fn decrement_group(counts: &mut BTreeMap<GroupId, usize>, group: Option<&GroupId>) {
    if let Some(group) = group
        && let Some(count) = counts.get_mut(group)
    {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counts.remove(group);
        }
    }
}

fn shape_result(
    id: &WorkerId,
    status: WorkerStatus,
    output: Option<ExecutorOutput>,
    message: Option<String>,
    request: &WorkerRequest,
    default_inline: usize,
    artifacts: &dyn ArtifactStore,
) -> Result<WorkerResult, RuntimeError> {
    let Some(output) = output else {
        return Ok(WorkerResult {
            schema_version: SCHEMA_VERSION,
            worker_id: id.clone(),
            status,
            summary: String::new(),
            inline_output: None,
            artifacts: Vec::new(),
            usage: Usage::default(),
            changed_paths: Vec::new(),
            worktree: None,
            apply_plan_id: None,
            host: Default::default(),
            message,
        });
    };
    let inline_limit = request
        .budgets
        .max_inline_output_bytes
        .unwrap_or(default_inline);
    let max_output = request.budgets.max_output_bytes.unwrap_or(usize::MAX);
    let mut stored = output.artifacts;
    let inline_output = if output.output.len() <= inline_limit {
        Some(String::from_utf8_lossy(&output.output).into_owned())
    } else {
        stored.push(artifacts.put(&output.output, Some("text/plain; charset=utf-8"))?);
        None
    };
    let artifact_limit = request.budgets.max_artifacts.unwrap_or(usize::MAX);
    let over_budget = output.output.len() > max_output
        || stored.len() > artifact_limit
        || exceeds_usage(request, &output.usage);
    let final_status = if over_budget {
        WorkerStatus::Failed
    } else {
        status
    };
    Ok(WorkerResult {
        schema_version: SCHEMA_VERSION,
        worker_id: id.clone(),
        status: final_status,
        summary: truncate_utf8(output.summary, inline_limit),
        inline_output,
        artifacts: stored,
        usage: output.usage,
        changed_paths: output.changed_paths,
        worktree: output.worktree,
        apply_plan_id: None,
        host: output.host,
        message: if over_budget {
            Some("worker resource budget exceeded".to_string())
        } else {
            message
        },
    })
}

fn truncate_utf8(mut value: String, max: usize) -> String {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn exceeds_usage(request: &WorkerRequest, usage: &Usage) -> bool {
    request
        .budgets
        .max_provider_rounds
        .is_some_and(|limit| usage.provider_rounds > limit)
}

fn snapshot(state: &WorkerState) -> WorkerSnapshot {
    WorkerSnapshot {
        request: state.request.clone(),
        worker_id: state
            .events
            .first()
            .expect("accepted worker has event")
            .worker_id
            .clone(),
        status: state.status,
        group_id: state.group_id.clone(),
        usage: state.usage.clone(),
        result: state.result.clone(),
        last_event_sequence: state.events.last().map_or(0, |event| event.sequence),
    }
}

fn matches_filter(snapshot: &WorkerSnapshot, filter: &WorkerFilter) -> bool {
    filter.status.is_none_or(|status| status == snapshot.status)
        && filter
            .group_id
            .as_ref()
            .is_none_or(|group| snapshot.group_id.as_ref() == Some(group))
        && filter
            .session_id
            .as_ref()
            .is_none_or(|session| snapshot.request.session_id.as_ref() == Some(session))
        && (filter.include_internal
            || !matches!(snapshot.request.priority, WorkerPriority::InternalUrgent))
}

fn recover_workers(
    shared: &Arc<Shared>,
    recovered: BTreeMap<WorkerId, crate::persistence::RecoveredWorker>,
) -> Result<(), RuntimeError> {
    let mut workers = lock(&shared.workers);
    for (id, worker) in recovered {
        let mut events = worker.events;
        let (status, result) = if let Some(result) = worker.result {
            (result.status, Some(result))
        } else {
            let status = recovered_status(
                worker.request.recovery,
                worker.request.policy.isolation == crate::IsolationMode::Worktree,
            );
            let result = WorkerResult {
                schema_version: SCHEMA_VERSION,
                worker_id: id.clone(),
                status,
                summary: String::new(),
                inline_output: None,
                artifacts: Vec::new(),
                usage: Usage::default(),
                changed_paths: Vec::new(),
                worktree: None,
                apply_plan_id: None,
                host: Default::default(),
                message: Some("worker interrupted by runtime restart".to_string()),
            };
            let event = WorkerEvent {
                schema_version: SCHEMA_VERSION,
                worker_id: id.clone(),
                sequence: events.last().map_or(1, |event| event.sequence + 1),
                timestamp_ms: now_ms(),
                kind: WorkerEventKind::Status(status),
            };
            shared
                .journal
                .finish(&result, std::slice::from_ref(&event))?;
            events.push(event);
            (status, Some(result))
        };
        workers.insert(
            id,
            Arc::new(WorkerCell {
                state: Mutex::new(WorkerState {
                    request: worker.request,
                    group_id: worker.group_id,
                    status,
                    usage: result
                        .as_ref()
                        .map(|result| result.usage.clone())
                        .unwrap_or_default(),
                    result,
                    events,
                }),
                changed: Condvar::new(),
                notify: tokio::sync::Notify::new(),
                token: CancellationToken::new(),
            }),
        );
    }
    Ok(())
}

fn validate_config(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    if config.command_capacity == 0
        || config.queue_capacity == 0
        || config.global_concurrency == 0
        || config.per_group_concurrency == 0
        || config.max_urgent_streak == 0
        || config.event_channel_capacity == 0
    {
        return Err(RuntimeError::InvalidRequest(
            "runtime capacities and concurrency limits must be non-zero".to_string(),
        ));
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}
