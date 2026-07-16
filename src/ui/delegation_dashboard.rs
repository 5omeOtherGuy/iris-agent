//! Live delegation dashboard state, rendering, and typed backend requests.
//!
//! The dashboard is presentation-only. It owns immutable display snapshots and
//! turns keys into requests; backend work runs through the harness actor or an
//! idle `spawn_blocking` lane.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iris_subagent_runtime::worktree::{
    ApplyChangeKind, ApplyDisposition, ApplyFileKind, ApplyOptions, ApplyPlan, ApplyResult,
    CreationMode, GcReport, RemoveOutcome, WorktreeKind, WorktreeRecord, WorktreeStatus,
};
use iris_subagent_runtime::{
    ApplyPlanId, ArtifactId, GroupId, IsolationMode, WorkerEvent, WorkerId, WorkerSnapshot,
    WorkerStatus, WorktreeId,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::modal::{ModalAction, ModalKey, ModalOutcome};
use crate::ui::{palette, symbols, textengine};
use crate::wayland::subagents::SubagentBackend;

const WORKER_REFRESH: Duration = Duration::from_millis(250);
const WORKTREE_REFRESH: Duration = Duration::from_secs(1);
const ARTIFACT_DISPLAY_LIMIT: usize = 50 * 1024;
const EVENT_TAIL: usize = 8;
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DelegationScope {
    Workers,
    Worktrees,
}

impl DelegationScope {
    fn other(self) -> Self {
        match self {
            Self::Workers => Self::Worktrees,
            Self::Worktrees => Self::Workers,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Workers => "WORKERS",
            Self::Worktrees => "WORKTREES",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DelegationSnapshot {
    pub(crate) workers: Vec<WorkerSnapshot>,
    pub(crate) worktrees: Option<Vec<WorktreeRecord>>,
    pub(crate) events: BTreeMap<WorkerId, Vec<WorkerEvent>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ArtifactContent {
    pub(crate) id: ArtifactId,
    pub(crate) total_bytes: usize,
    pub(crate) text: Option<String>,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DelegationRequestKind {
    Snapshot {
        include_worktrees: bool,
    },
    CancelWorker(WorkerId),
    CancelGroup(GroupId),
    ReadArtifact(ArtifactId),
    SelectCandidate(WorktreeId),
    PlanApply(WorkerId),
    Apply {
        plan_id: ApplyPlanId,
        digest: String,
        approved_overwrites: BTreeSet<PathBuf>,
        approved_escaping_symlinks: BTreeSet<PathBuf>,
        skipped_paths: BTreeSet<PathBuf>,
    },
    AdoptWorktree(WorktreeId),
    IgnoreWorktree(WorktreeId),
    RemoveWorktree {
        id: WorktreeId,
        force: bool,
    },
    GcWorktrees,
    RebuildWorktrees,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationRequest {
    pub(crate) request_id: u64,
    pub(crate) kind: DelegationRequestKind,
}

#[derive(Debug, Clone)]
pub(crate) enum DelegationPayload {
    Snapshot(DelegationSnapshot),
    Artifact(ArtifactContent),
    Plan(ApplyPlan),
    Apply(ApplyResult),
    Worker,
    Group,
    Worktree,
    Removed(RemoveOutcome),
    Gc(GcReport),
    Rebuilt(Vec<WorktreeRecord>),
}

#[derive(Debug, Clone)]
pub(crate) struct DelegationResponse {
    pub(crate) request_id: u64,
    pub(crate) result: Result<DelegationPayload, String>,
}

/// Execute one already-authorized dashboard request. Call only from a blocking
/// task: worktree registry and Git operations may block.
pub(crate) fn execute_request(
    backend: Option<Arc<SubagentBackend>>,
    request: DelegationRequest,
) -> DelegationResponse {
    let request_id = request.request_id;
    let result = backend
        .ok_or_else(|| "subagents are not configured for this session".to_string())
        .and_then(|backend| {
            execute_with_backend(&backend, request.kind).map_err(|e| format!("{e:#}"))
        });
    DelegationResponse { request_id, result }
}

fn artifact_content(id: ArtifactId, bytes: Vec<u8>) -> ArtifactContent {
    let total_bytes = bytes.len();
    let truncated = total_bytes > ARTIFACT_DISPLAY_LIMIT;
    let text = std::str::from_utf8(&bytes).ok().map(|text| {
        let mut end = total_bytes.min(ARTIFACT_DISPLAY_LIMIT);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        textengine::clean_text(&text[..end])
    });
    ArtifactContent {
        id,
        total_bytes,
        text,
        truncated,
    }
}

fn execute_with_backend(
    backend: &SubagentBackend,
    kind: DelegationRequestKind,
) -> anyhow::Result<DelegationPayload> {
    Ok(match kind {
        DelegationRequestKind::Snapshot { include_worktrees } => {
            let workers = backend
                .runtime()
                .handle()
                .list(&iris_subagent_runtime::WorkerFilter::default());
            let mut events = BTreeMap::new();
            for worker in &workers {
                let mut tail = backend
                    .runtime()
                    .handle()
                    .replay_events(&worker.worker_id, 0)?;
                if tail.len() > EVENT_TAIL {
                    let drain_end = tail.len() - (EVENT_TAIL - 1);
                    tail.drain(1..drain_end);
                }
                events.insert(worker.worker_id.clone(), tail);
            }
            let worktrees = include_worktrees
                .then(|| backend.list_worktrees())
                .transpose()?;
            DelegationPayload::Snapshot(DelegationSnapshot {
                workers,
                worktrees,
                events,
            })
        }
        DelegationRequestKind::CancelWorker(id) => {
            backend.cancel(&id)?;
            DelegationPayload::Worker
        }
        DelegationRequestKind::CancelGroup(id) => {
            backend.cancel_group(&id)?;
            DelegationPayload::Group
        }
        DelegationRequestKind::ReadArtifact(id) => {
            let bytes = backend.read_artifact(&id)?;
            DelegationPayload::Artifact(artifact_content(id, bytes))
        }
        DelegationRequestKind::SelectCandidate(id) => {
            backend.select_worktree_candidate(&id)?;
            DelegationPayload::Worktree
        }
        DelegationRequestKind::PlanApply(id) => DelegationPayload::Plan(backend.plan_apply(&id)?),
        DelegationRequestKind::Apply {
            plan_id,
            digest,
            approved_overwrites,
            approved_escaping_symlinks,
            skipped_paths,
        } => {
            let plan = backend.load_apply_plan(&plan_id)?;
            anyhow::ensure!(
                plan.digest == digest,
                "apply plan digest changed; reload the preview before applying"
            );
            let mut options = ApplyOptions::new();
            options.approved_overwrites = approved_overwrites;
            options.approved_escaping_symlinks = approved_escaping_symlinks;
            options.skipped_paths = skipped_paths;
            DelegationPayload::Apply(backend.apply(&plan, &options)?)
        }
        DelegationRequestKind::AdoptWorktree(id) => {
            backend.adopt_worktree(&id)?;
            DelegationPayload::Worktree
        }
        DelegationRequestKind::IgnoreWorktree(id) => {
            backend.ignore_worktree(&id)?;
            DelegationPayload::Worktree
        }
        DelegationRequestKind::RemoveWorktree { id, force } => {
            DelegationPayload::Removed(backend.remove_worktree(&id, force)?)
        }
        DelegationRequestKind::GcWorktrees => DelegationPayload::Gc(backend.gc_worktrees()?),
        DelegationRequestKind::RebuildWorktrees => {
            DelegationPayload::Rebuilt(backend.rebuild_worktree_registry()?)
        }
    })
}

#[derive(Debug, Clone)]
enum Detail {
    Worker(WorkerId),
    Worktree(WorktreeId),
    Artifact(ArtifactContent),
    Apply(ApplyPlan),
    Report(Vec<String>),
}

#[derive(Debug, Clone)]
struct Confirmation {
    prompt: String,
    request: DelegationRequestKind,
    force_stage: bool,
}

#[derive(Debug, Clone, Default)]
struct ScopeState {
    filter: String,
    filtering: bool,
    selected_id: Option<String>,
    selected_index: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct DelegationDashboard {
    scope: DelegationScope,
    workers_state: ScopeState,
    worktrees_state: ScopeState,
    workers: Vec<WorkerSnapshot>,
    worktrees: Vec<WorktreeRecord>,
    events: BTreeMap<WorkerId, Vec<WorkerEvent>>,
    detail: Option<Detail>,
    detail_scroll: usize,
    artifact_index: usize,
    confirmation: Option<Confirmation>,
    latest_requested: u64,
    pending_request: Option<u64>,
    pending_refresh: bool,
    pending_detail: bool,
    in_flight: Option<String>,
    last_worker_refresh: Option<Instant>,
    last_worktree_refresh: Option<Instant>,
    loading: bool,
    stale: bool,
    message: Option<String>,
}

impl DelegationDashboard {
    pub(crate) fn new(scope: DelegationScope) -> Self {
        Self {
            scope,
            workers_state: ScopeState::default(),
            worktrees_state: ScopeState::default(),
            workers: Vec::new(),
            worktrees: Vec::new(),
            events: BTreeMap::new(),
            detail: None,
            detail_scroll: 0,
            artifact_index: 0,
            confirmation: None,
            latest_requested: 0,
            pending_request: None,
            pending_refresh: false,
            pending_detail: false,
            in_flight: None,
            last_worker_refresh: None,
            last_worktree_refresh: None,
            loading: true,
            stale: false,
            message: None,
        }
    }

    pub(crate) fn request_initial_if_needed(&mut self) -> Option<DelegationRequest> {
        if self.loading {
            self.request_initial()
        } else {
            None
        }
    }

    pub(crate) fn request_initial(&mut self) -> Option<DelegationRequest> {
        self.make_request(DelegationRequestKind::Snapshot {
            include_worktrees: true,
        })
    }

    pub(crate) fn request_refresh(&mut self, now: Instant) -> Option<DelegationRequest> {
        if self.pending_request.is_some() {
            return None;
        }
        let workers_due = self
            .last_worker_refresh
            .is_none_or(|last| now.duration_since(last) >= WORKER_REFRESH);
        let worktrees_due = self
            .last_worktree_refresh
            .is_none_or(|last| now.duration_since(last) >= WORKTREE_REFRESH);
        if !workers_due && !worktrees_due {
            return None;
        }
        self.last_worker_refresh = Some(now);
        if worktrees_due {
            self.last_worktree_refresh = Some(now);
        }
        self.make_request(DelegationRequestKind::Snapshot {
            include_worktrees: worktrees_due,
        })
    }

    fn make_request(&mut self, kind: DelegationRequestKind) -> Option<DelegationRequest> {
        if self.pending_request.is_some() {
            return None;
        }
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let label = request_label(&kind);
        self.latest_requested = request_id;
        self.pending_request = Some(request_id);
        self.pending_refresh = matches!(&kind, DelegationRequestKind::Snapshot { .. });
        self.pending_detail = matches!(
            &kind,
            DelegationRequestKind::ReadArtifact(_) | DelegationRequestKind::PlanApply(_)
        );
        self.in_flight = Some(label);
        Some(DelegationRequest { request_id, kind })
    }

    pub(crate) fn apply_response(&mut self, response: DelegationResponse) -> bool {
        if response.request_id < self.latest_requested
            || self.pending_request != Some(response.request_id)
        {
            return false;
        }
        let was_refresh = self.pending_refresh;
        self.pending_request = None;
        self.pending_refresh = false;
        self.pending_detail = false;
        self.in_flight = None;
        match response.result {
            Ok(DelegationPayload::Snapshot(snapshot)) => {
                self.apply_snapshot(snapshot);
                self.loading = false;
                self.stale = false;
                self.message = None;
            }
            Ok(DelegationPayload::Artifact(content)) => {
                self.detail = Some(Detail::Artifact(content));
                self.detail_scroll = 0;
                self.message = None;
            }
            Ok(DelegationPayload::Plan(plan)) => {
                self.detail = Some(Detail::Apply(plan));
                self.detail_scroll = 0;
                self.message = None;
            }
            Ok(DelegationPayload::Apply(result)) => {
                self.detail = Some(Detail::Report(apply_result_lines(&result)));
                self.message = Some(match result.disposition {
                    ApplyDisposition::Complete | ApplyDisposition::AlreadyApplied => {
                        "Apply completed. Refreshing delegation state.".to_string()
                    }
                    ApplyDisposition::Partial => {
                        "Apply was partial; the candidate remains reviewable.".to_string()
                    }
                    _ => "Apply result received.".to_string(),
                });
                self.last_worker_refresh = None;
                self.last_worktree_refresh = None;
            }
            Ok(DelegationPayload::Gc(report)) => {
                self.detail = Some(Detail::Report(gc_report_lines(&report)));
                self.last_worktree_refresh = None;
            }
            Ok(DelegationPayload::Removed(outcome)) => {
                self.message = Some(format!("Worktree removal: {outcome:?}."));
                self.detail = None;
                self.last_worktree_refresh = None;
            }
            Ok(DelegationPayload::Rebuilt(records)) => {
                self.message = Some(format!(
                    "Registry rebuilt from {} record(s).",
                    records.len()
                ));
                self.worktrees = records;
                self.detail = None;
                self.last_worktree_refresh = None;
            }
            Ok(
                DelegationPayload::Worker | DelegationPayload::Group | DelegationPayload::Worktree,
            ) => {
                self.message = Some("Action completed. Refreshing delegation state.".to_string());
                self.last_worker_refresh = None;
                self.last_worktree_refresh = None;
            }
            Err(error) => {
                if was_refresh {
                    self.stale = !self.workers.is_empty() || !self.worktrees.is_empty();
                    self.loading = false;
                }
                self.message = Some(format!("{} ERROR  {error}", symbols::ERROR));
            }
        }
        true
    }

    fn apply_snapshot(&mut self, snapshot: DelegationSnapshot) {
        let worker_id = self.workers_state.selected_id.clone();
        let worktree_id = self.worktrees_state.selected_id.clone();
        self.workers = snapshot.workers;
        if let Some(worktrees) = snapshot.worktrees {
            self.worktrees = worktrees;
        }
        self.events = snapshot.events;
        self.restore_worker_cursor(worker_id.as_deref());
        self.restore_worktree_cursor(worktree_id.as_deref());
        if let Some(detail) = &self.detail {
            let present = match detail {
                Detail::Worker(id) => self.workers.iter().any(|worker| &worker.worker_id == id),
                Detail::Worktree(id) => self.worktrees.iter().any(|worktree| &worktree.id == id),
                Detail::Artifact(_) | Detail::Apply(_) | Detail::Report(_) => true,
            };
            if !present {
                self.detail = None;
                self.message = Some("Selected item no longer exists.".to_string());
            }
        }
    }

    fn state(&self) -> &ScopeState {
        match self.scope {
            DelegationScope::Workers => &self.workers_state,
            DelegationScope::Worktrees => &self.worktrees_state,
        }
    }

    fn state_mut(&mut self) -> &mut ScopeState {
        match self.scope {
            DelegationScope::Workers => &mut self.workers_state,
            DelegationScope::Worktrees => &mut self.worktrees_state,
        }
    }

    pub(crate) fn paste_text(&mut self, text: &str) -> ModalOutcome {
        if !self.state().filtering {
            return ModalOutcome::Ignore;
        }
        self.state_mut().filter.push_str(text);
        self.restore_active_cursor(None);
        ModalOutcome::Redraw
    }

    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        if let Some(confirmation) = self.confirmation.clone() {
            return self.handle_confirmation(key, confirmation);
        }
        if self.state().filtering {
            return self.handle_filter_key(key);
        }
        if self.detail.is_some() {
            return self.handle_detail_key(key);
        }
        match key {
            ModalKey::Up => {
                self.move_selection(-1);
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.move_selection(1);
                ModalOutcome::Redraw
            }
            ModalKey::Enter | ModalKey::Right => {
                self.open_selected();
                ModalOutcome::Redraw
            }
            ModalKey::Tab | ModalKey::BackTab => {
                self.scope = self.scope.other();
                ModalOutcome::Redraw
            }
            ModalKey::Char('/') => {
                self.state_mut().filtering = true;
                ModalOutcome::Redraw
            }
            ModalKey::Char('r') | ModalKey::Char('R') => self
                .make_request(DelegationRequestKind::Snapshot {
                    include_worktrees: true,
                })
                .map_or(ModalOutcome::Ignore, |request| {
                    ModalOutcome::Emit(ModalAction::Delegation(request))
                }),
            ModalKey::Char('g') | ModalKey::Char('G')
                if self.scope == DelegationScope::Worktrees =>
            {
                self.emit(DelegationRequestKind::GcWorktrees)
            }
            ModalKey::Char('b') | ModalKey::Char('B')
                if self.scope == DelegationScope::Worktrees =>
            {
                self.confirmation = Some(Confirmation {
                    prompt: "Rebuild the managed worktree registry?".to_string(),
                    request: DelegationRequestKind::RebuildWorktrees,
                    force_stage: false,
                });
                ModalOutcome::Redraw
            }
            ModalKey::Esc | ModalKey::Char('q') | ModalKey::Char('Q') => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn handle_filter_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Char(c) => {
                self.state_mut().filter.push(c);
                self.restore_active_cursor(None);
                ModalOutcome::Redraw
            }
            ModalKey::Backspace => {
                self.state_mut().filter.pop();
                self.restore_active_cursor(None);
                ModalOutcome::Redraw
            }
            ModalKey::Enter => {
                self.state_mut().filtering = false;
                ModalOutcome::Redraw
            }
            ModalKey::Esc => {
                self.state_mut().filtering = false;
                ModalOutcome::Redraw
            }
            _ => ModalOutcome::Ignore,
        }
    }

    fn handle_confirmation(&mut self, key: ModalKey, confirmation: Confirmation) -> ModalOutcome {
        match key {
            ModalKey::Esc | ModalKey::Char('n') | ModalKey::Char('N') => {
                self.confirmation = None;
                ModalOutcome::Redraw
            }
            ModalKey::Char('x') | ModalKey::Char('X') if confirmation.force_stage => {
                self.confirmation = Some(Confirmation {
                    prompt: "Force removal can discard a live worktree. Press y to authorize."
                        .to_string(),
                    request: confirmation.request,
                    force_stage: false,
                });
                ModalOutcome::Redraw
            }
            ModalKey::Char('y') | ModalKey::Char('Y') if !confirmation.force_stage => {
                self.confirmation = None;
                self.emit(confirmation.request)
            }
            _ => ModalOutcome::Ignore,
        }
    }

    fn invalidate_pending_detail(&mut self) {
        if self.pending_detail {
            self.pending_request = None;
            self.pending_detail = false;
            self.in_flight = None;
        }
    }

    fn handle_detail_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Left => {
                self.invalidate_pending_detail();
                self.detail = None;
                self.detail_scroll = 0;
                ModalOutcome::Redraw
            }
            ModalKey::Esc => {
                self.invalidate_pending_detail();
                self.detail = None;
                self.detail_scroll = 0;
                ModalOutcome::Redraw
            }
            ModalKey::Tab | ModalKey::BackTab => {
                self.invalidate_pending_detail();
                self.detail = None;
                self.scope = self.scope.other();
                ModalOutcome::Redraw
            }
            ModalKey::AltUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
                ModalOutcome::Redraw
            }
            ModalKey::AltDown => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
                ModalOutcome::Redraw
            }
            ModalKey::Up => {
                if self.selected_worker_artifacts().is_empty() {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                } else {
                    let count = self.selected_worker_artifacts().len();
                    self.artifact_index = (self.artifact_index + count - 1) % count;
                }
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                if self.selected_worker_artifacts().is_empty() {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                } else {
                    self.artifact_index =
                        (self.artifact_index + 1) % self.selected_worker_artifacts().len();
                }
                ModalOutcome::Redraw
            }
            ModalKey::Enter | ModalKey::Right => {
                if let Some(artifact) = self
                    .selected_worker_artifacts()
                    .get(self.artifact_index)
                    .cloned()
                {
                    return self.emit(DelegationRequestKind::ReadArtifact(artifact.id));
                }
                ModalOutcome::Ignore
            }
            ModalKey::Char('c') | ModalKey::Char('C') => self.confirm_cancel(),
            ModalKey::Char('s') | ModalKey::Char('S') => self.select_candidate(),
            ModalKey::Char('p') | ModalKey::Char('P') => self.plan_apply(),
            ModalKey::Char('o') | ModalKey::Char('O') => {
                self.open_linked();
                ModalOutcome::Redraw
            }
            ModalKey::Char('a') | ModalKey::Char('A') => self.apply_or_adopt(),
            ModalKey::Char('i') | ModalKey::Char('I') => self.confirm_ignore(),
            ModalKey::Char('x') | ModalKey::Char('X') => self.confirm_remove(),
            _ => ModalOutcome::Ignore,
        }
    }

    fn emit(&mut self, kind: DelegationRequestKind) -> ModalOutcome {
        self.make_request(kind)
            .map_or(ModalOutcome::Ignore, |request| {
                ModalOutcome::Emit(ModalAction::Delegation(request))
            })
    }

    fn open_selected(&mut self) {
        match self.scope {
            DelegationScope::Workers => {
                if let Some(worker) = self.selected_worker() {
                    self.detail = Some(Detail::Worker(worker.worker_id.clone()));
                }
            }
            DelegationScope::Worktrees => {
                if let Some(worktree) = self.selected_worktree() {
                    self.detail = Some(Detail::Worktree(worktree.id.clone()));
                }
            }
        }
        self.detail_scroll = 0;
        self.artifact_index = 0;
    }

    fn open_linked(&mut self) {
        match self.detail.clone() {
            Some(Detail::Worker(id)) => {
                let worktree_id = self
                    .worktrees
                    .iter()
                    .find(|record| {
                        record.worker_id.as_ref() == Some(&id)
                            || self
                                .workers
                                .iter()
                                .find(|worker| worker.worker_id == id)
                                .and_then(|worker| worker.result.as_ref())
                                .and_then(|result| result.worktree.as_ref())
                                .is_some_and(|linked| linked.id == record.id)
                    })
                    .map(|worktree| worktree.id.clone());
                if let Some(worktree_id) = worktree_id {
                    self.invalidate_pending_detail();
                    self.restore_worktree_cursor(Some(worktree_id.as_str()));
                    self.scope = DelegationScope::Worktrees;
                    self.detail = Some(Detail::Worktree(worktree_id));
                }
            }
            Some(Detail::Worktree(id)) => {
                let worker_id = self
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.id == id)
                    .and_then(|worktree| worktree.worker_id.clone());
                if let Some(worker_id) = worker_id {
                    self.invalidate_pending_detail();
                    self.restore_worker_cursor(Some(worker_id.as_str()));
                    self.scope = DelegationScope::Workers;
                    self.detail = Some(Detail::Worker(worker_id));
                }
            }
            _ => {}
        }
    }

    fn confirm_cancel(&mut self) -> ModalOutcome {
        let Some(worker) = self.detail_worker().cloned() else {
            return ModalOutcome::Ignore;
        };
        if worker.status.is_terminal() {
            self.message = Some("Terminal workers cannot be cancelled.".to_string());
            return ModalOutcome::Redraw;
        }
        let (prompt, request) = if let Some(group) = worker.group_id {
            (
                format!(
                    "Cancel all non-terminal workers in group {}?",
                    short_id(&group.to_string())
                ),
                DelegationRequestKind::CancelGroup(group),
            )
        } else {
            (
                format!("Cancel worker {}?", short_id(&worker.worker_id.to_string())),
                DelegationRequestKind::CancelWorker(worker.worker_id),
            )
        };
        self.confirmation = Some(Confirmation {
            prompt,
            request,
            force_stage: false,
        });
        ModalOutcome::Redraw
    }

    fn select_candidate(&mut self) -> ModalOutcome {
        let worktree_id = match self.detail.clone() {
            Some(Detail::Worker(id)) => {
                let Some(worker) = self.workers.iter().find(|worker| worker.worker_id == id) else {
                    return ModalOutcome::Ignore;
                };
                if !self.worker_selectable(worker) {
                    return ModalOutcome::Ignore;
                }
                worker
                    .result
                    .as_ref()
                    .and_then(|result| result.worktree.as_ref())
                    .map(|worktree| worktree.id.clone())
            }
            Some(Detail::Worktree(id)) => self
                .worktrees
                .iter()
                .find(|worktree| worktree.id == id)
                .filter(|worktree| {
                    worktree.status == WorktreeStatus::Alive
                        && !worktree.applied_to_parent
                        && worktree.worker_id.as_ref().is_some_and(|worker_id| {
                            self.workers
                                .iter()
                                .find(|worker| &worker.worker_id == worker_id)
                                .is_some_and(|worker| self.worker_selectable(worker))
                        })
                })
                .map(|worktree| worktree.id.clone()),
            _ => None,
        };
        worktree_id.map_or(ModalOutcome::Ignore, |id| {
            self.emit(DelegationRequestKind::SelectCandidate(id))
        })
    }

    fn plan_apply(&mut self) -> ModalOutcome {
        let worker_id = match self.detail.clone() {
            Some(Detail::Worker(id)) => Some(id),
            Some(Detail::Worktree(id)) => self
                .worktrees
                .iter()
                .find(|worktree| worktree.id == id)
                .and_then(|worktree| worktree.worker_id.clone()),
            _ => None,
        };
        let Some(worker_id) = worker_id else {
            return ModalOutcome::Ignore;
        };
        let Some(worker) = self
            .workers
            .iter()
            .find(|worker| worker.worker_id == worker_id)
        else {
            return ModalOutcome::Ignore;
        };
        if !worker_plan_eligible(worker, &self.worktrees) {
            return ModalOutcome::Ignore;
        }
        self.emit(DelegationRequestKind::PlanApply(worker_id))
    }

    fn apply_or_adopt(&mut self) -> ModalOutcome {
        match self.detail.clone() {
            Some(Detail::Apply(plan)) => {
                let mut options = ApplyOptions::new();
                for operation in &plan.operations {
                    if operation.dirty_parent || operation.base_drift {
                        options.approved_overwrites.insert(operation.path.clone());
                    }
                    if operation.escaping_symlink {
                        options
                            .approved_escaping_symlinks
                            .insert(operation.path.clone());
                    }
                }
                self.confirmation = Some(Confirmation {
                    prompt: format!(
                        "Apply exact plan {} ({}) to the parent workspace?",
                        short_id(&plan.id.to_string()),
                        textengine::ellipsize_to_width(&plan.digest, 12)
                    ),
                    request: DelegationRequestKind::Apply {
                        plan_id: plan.id,
                        digest: plan.digest,
                        approved_overwrites: options.approved_overwrites,
                        approved_escaping_symlinks: options.approved_escaping_symlinks,
                        skipped_paths: options.skipped_paths,
                    },
                    force_stage: false,
                });
                ModalOutcome::Redraw
            }
            Some(Detail::Worktree(id)) => {
                let legal = self
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.id == id)
                    .is_some_and(|worktree| worktree.status == WorktreeStatus::Adoptable);
                if !legal {
                    return ModalOutcome::Ignore;
                }
                self.confirmation = Some(Confirmation {
                    prompt: format!("Adopt worktree {}?", short_id(&id.to_string())),
                    request: DelegationRequestKind::AdoptWorktree(id),
                    force_stage: false,
                });
                ModalOutcome::Redraw
            }
            _ => ModalOutcome::Ignore,
        }
    }

    fn confirm_ignore(&mut self) -> ModalOutcome {
        let Some(Detail::Worktree(id)) = self.detail.clone() else {
            return ModalOutcome::Ignore;
        };
        let legal = self
            .worktrees
            .iter()
            .find(|worktree| worktree.id == id)
            .is_some_and(|worktree| worktree.status == WorktreeStatus::Adoptable);
        if !legal {
            return ModalOutcome::Ignore;
        }
        self.confirmation = Some(Confirmation {
            prompt: format!("Ignore adoptable worktree {}?", short_id(&id.to_string())),
            request: DelegationRequestKind::IgnoreWorktree(id),
            force_stage: false,
        });
        ModalOutcome::Redraw
    }

    fn confirm_remove(&mut self) -> ModalOutcome {
        let Some(Detail::Worktree(id)) = self.detail.clone() else {
            return ModalOutcome::Ignore;
        };
        let Some(worktree) = self.worktrees.iter().find(|worktree| worktree.id == id) else {
            return ModalOutcome::Ignore;
        };
        if matches!(
            worktree.status,
            WorktreeStatus::Corrupt | WorktreeStatus::Removed
        ) {
            return ModalOutcome::Ignore;
        }
        let force = worktree.status == WorktreeStatus::Alive;
        self.confirmation = Some(Confirmation {
            prompt: if force {
                format!(
                    "Worktree {} may have a live owner. Press x to enter force-removal review.",
                    short_id(&id.to_string())
                )
            } else {
                format!("Remove managed worktree {}?", short_id(&id.to_string()))
            },
            request: DelegationRequestKind::RemoveWorktree { id, force },
            force_stage: force,
        });
        ModalOutcome::Redraw
    }

    fn selected_worker_artifacts(&self) -> Vec<iris_subagent_runtime::ArtifactRef> {
        self.detail_worker()
            .and_then(|worker| worker.result.as_ref())
            .map(|result| result.artifacts.clone())
            .unwrap_or_default()
    }

    fn detail_worker(&self) -> Option<&WorkerSnapshot> {
        let Detail::Worker(id) = self.detail.as_ref()? else {
            return None;
        };
        self.workers.iter().find(|worker| &worker.worker_id == id)
    }

    fn worker_selectable(&self, worker: &WorkerSnapshot) -> bool {
        let Some(group_id) = worker.group_id.as_ref() else {
            return false;
        };
        let linked = worker
            .result
            .as_ref()
            .and_then(|result| result.worktree.as_ref());
        worker.status == WorkerStatus::Completed
            && self
                .workers
                .iter()
                .filter(|candidate| candidate.group_id.as_ref() == Some(group_id))
                .all(|candidate| candidate.status.is_terminal())
            && linked.is_some_and(|linked| {
                self.worktrees.iter().any(|worktree| {
                    worktree.id == linked.id
                        && worktree.status == WorktreeStatus::Alive
                        && !worktree.applied_to_parent
                })
            })
            && !self.worktrees.iter().any(|worktree| {
                worktree.group_id.as_ref() == Some(group_id) && worktree.applied_to_parent
            })
    }

    fn visible_worker_indices(&self) -> Vec<usize> {
        let filter = self.workers_state.filter.to_ascii_lowercase();
        let mut indices = (0..self.workers.len())
            .filter(|index| worker_matches(&self.workers[*index], &filter))
            .collect::<Vec<_>>();
        indices.sort_by(|a, b| {
            let left = &self.workers[*a];
            let right = &self.workers[*b];
            let lg = left.group_id.as_ref().map(ToString::to_string);
            let rg = right.group_id.as_ref().map(ToString::to_string);
            match (lg, rg) {
                (Some(lg), Some(rg)) if lg == rg => {
                    creation_ms(right, &self.events).cmp(&creation_ms(left, &self.events))
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(lg), Some(rg)) => group_creation_ms(&rg, &self.workers, &self.events)
                    .cmp(&group_creation_ms(&lg, &self.workers, &self.events))
                    .then_with(|| lg.cmp(&rg)),
                (None, None) => {
                    creation_ms(right, &self.events).cmp(&creation_ms(left, &self.events))
                }
            }
        });
        indices
    }

    fn visible_worktree_indices(&self) -> Vec<usize> {
        let filter = self.worktrees_state.filter.to_ascii_lowercase();
        let mut indices = (0..self.worktrees.len())
            .filter(|index| worktree_matches(&self.worktrees[*index], &filter))
            .collect::<Vec<_>>();
        indices.sort_by(|a, b| {
            let left = &self.worktrees[*a];
            let right = &self.worktrees[*b];
            worktree_class(left)
                .cmp(&worktree_class(right))
                .then_with(|| worktree_time(right).cmp(&worktree_time(left)))
        });
        indices
    }

    fn selected_worker(&self) -> Option<&WorkerSnapshot> {
        let visible = self.visible_worker_indices();
        visible
            .get(self.workers_state.selected_index)
            .map(|index| &self.workers[*index])
    }

    fn selected_worktree(&self) -> Option<&WorktreeRecord> {
        let visible = self.visible_worktree_indices();
        visible
            .get(self.worktrees_state.selected_index)
            .map(|index| &self.worktrees[*index])
    }

    fn move_selection(&mut self, delta: isize) {
        let len = match self.scope {
            DelegationScope::Workers => self.visible_worker_indices().len(),
            DelegationScope::Worktrees => self.visible_worktree_indices().len(),
        };
        if len == 0 {
            return;
        }
        let state = self.state_mut();
        state.selected_index = if delta < 0 {
            (state.selected_index + len - 1) % len
        } else {
            (state.selected_index + 1) % len
        };
        self.capture_selected_id();
    }

    fn capture_selected_id(&mut self) {
        let id = match self.scope {
            DelegationScope::Workers => self
                .selected_worker()
                .map(|worker| worker.worker_id.to_string()),
            DelegationScope::Worktrees => self
                .selected_worktree()
                .map(|worktree| worktree.id.to_string()),
        };
        self.state_mut().selected_id = id;
    }

    fn restore_active_cursor(&mut self, preferred: Option<&str>) {
        match self.scope {
            DelegationScope::Workers => self.restore_worker_cursor(preferred),
            DelegationScope::Worktrees => self.restore_worktree_cursor(preferred),
        }
    }

    fn restore_worker_cursor(&mut self, preferred: Option<&str>) {
        let visible = self.visible_worker_indices();
        let old = self.workers_state.selected_index;
        let wanted = preferred.or(self.workers_state.selected_id.as_deref());
        self.workers_state.selected_index = wanted
            .and_then(|id| {
                visible
                    .iter()
                    .position(|index| self.workers[*index].worker_id.as_str() == id)
            })
            .unwrap_or_else(|| old.min(visible.len().saturating_sub(1)));
        self.workers_state.selected_id = visible
            .get(self.workers_state.selected_index)
            .map(|index| self.workers[*index].worker_id.to_string());
    }

    fn restore_worktree_cursor(&mut self, preferred: Option<&str>) {
        let visible = self.visible_worktree_indices();
        let old = self.worktrees_state.selected_index;
        let wanted = preferred.or(self.worktrees_state.selected_id.as_deref());
        self.worktrees_state.selected_index = wanted
            .and_then(|id| {
                visible
                    .iter()
                    .position(|index| self.worktrees[*index].id.as_str() == id)
            })
            .unwrap_or_else(|| old.min(visible.len().saturating_sub(1)));
        self.worktrees_state.selected_id = visible
            .get(self.worktrees_state.selected_index)
            .map(|index| self.worktrees[*index].id.to_string());
    }

    pub(crate) fn render_budgeted(&self, width: usize, budget: usize) -> Vec<Line<'static>> {
        let title = format!("DELEGATION · {}", self.scope.label());
        let mut rows = if let Some(confirmation) = &self.confirmation {
            vec![
                (
                    Line::from(Span::styled(confirmation.prompt.clone(), review_style())),
                    false,
                ),
                (
                    Line::from(Span::styled(
                        if confirmation.force_stage {
                            "x force-removal review · esc cancel"
                        } else {
                            "y authorize · n deny · esc cancel"
                        },
                        dim_style(),
                    )),
                    false,
                ),
            ]
        } else if let Some(detail) = &self.detail {
            self.detail_rows(detail, width)
        } else {
            self.list_rows(width)
        };
        if let Some(in_flight) = &self.in_flight {
            rows.insert(
                0,
                (
                    Line::from(Span::styled(
                        format!("{} RUNNING  {in_flight}", symbols::RUNNING),
                        review_style(),
                    )),
                    false,
                ),
            );
        }
        if let Some(message) = &self.message {
            rows.insert(
                0,
                (
                    Line::from(Span::styled(message.clone(), dim_style())),
                    false,
                ),
            );
        }
        if self.stale {
            rows.insert(
                0,
                (
                    Line::from(Span::styled(
                        format!("{} STALE  Last good snapshot retained.", symbols::REVIEW),
                        review_style(),
                    )),
                    false,
                ),
            );
        }
        let body_budget = budget.saturating_sub(3).max(1);
        rows = window_rows(rows, self.window_anchor(), body_budget);
        crate::ui::tui::overlay_menu(Some(&title), rows, Some(&self.footer()), width)
    }

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        self.render_budgeted(usize::from(width), 14)
    }

    fn list_rows(&self, width: usize) -> Vec<(Line<'static>, bool)> {
        let state = self.state();
        let mut rows = Vec::new();
        if state.filtering || !state.filter.is_empty() {
            rows.push((
                Line::from(vec![
                    Span::styled("/ ", dim_style()),
                    Span::raw(state.filter.clone()),
                    Span::styled(
                        if state.filtering { symbols::CARET } else { "" },
                        review_style(),
                    ),
                ]),
                false,
            ));
        }
        if self.loading {
            rows.push((
                Line::from(Span::styled(
                    format!("{} LOADING", symbols::EMPTY),
                    dim_style(),
                )),
                false,
            ));
            return rows;
        }
        match self.scope {
            DelegationScope::Workers => {
                let visible = self.visible_worker_indices();
                if visible.is_empty() {
                    rows.push((
                        Line::from(Span::styled(
                            if self.workers_state.filter.is_empty() {
                                "No delegated workers in this session."
                            } else {
                                "No workers match the filter."
                            },
                            dim_style(),
                        )),
                        false,
                    ));
                    return rows;
                }
                let mut last_group: Option<GroupId> = None;
                for (position, index) in visible.iter().enumerate() {
                    let worker = &self.workers[*index];
                    if worker.group_id != last_group {
                        if let Some(group) = &worker.group_id {
                            let members = self
                                .workers
                                .iter()
                                .filter(|candidate| candidate.group_id.as_ref() == Some(group))
                                .count();
                            rows.push((
                                Line::from(Span::styled(
                                    format!(
                                        "GROUP {} · {members} candidates",
                                        short_id(&group.to_string())
                                    ),
                                    dim_style().add_modifier(Modifier::BOLD),
                                )),
                                false,
                            ));
                        }
                        last_group = worker.group_id.clone();
                    }
                    rows.extend(worker_rows(
                        worker,
                        &self.worktrees,
                        width,
                        position == self.workers_state.selected_index,
                    ));
                }
            }
            DelegationScope::Worktrees => {
                let visible = self.visible_worktree_indices();
                if visible.is_empty() {
                    rows.push((
                        Line::from(Span::styled(
                            if self.worktrees_state.filter.is_empty() {
                                "No managed worktrees."
                            } else {
                                "No worktrees match the filter."
                            },
                            dim_style(),
                        )),
                        false,
                    ));
                    return rows;
                }
                for (position, index) in visible.iter().enumerate() {
                    rows.extend(worktree_rows(
                        &self.worktrees[*index],
                        width,
                        position == self.worktrees_state.selected_index,
                    ));
                }
            }
        }
        rows
    }

    fn detail_rows(&self, detail: &Detail, width: usize) -> Vec<(Line<'static>, bool)> {
        let lines = match detail {
            Detail::Worker(id) => self
                .workers
                .iter()
                .find(|worker| &worker.worker_id == id)
                .map(|worker| {
                    worker_detail_lines(
                        worker,
                        &self.workers,
                        &self.worktrees,
                        &self.events,
                        self.artifact_index,
                    )
                })
                .unwrap_or_else(|| {
                    vec![format!(
                        "{} ERROR  Worker no longer exists.",
                        symbols::ERROR
                    )]
                }),
            Detail::Worktree(id) => self
                .worktrees
                .iter()
                .find(|worktree| &worktree.id == id)
                .map(|worktree| {
                    let worker = worktree.worker_id.as_ref().and_then(|worker_id| {
                        self.workers
                            .iter()
                            .find(|worker| &worker.worker_id == worker_id)
                    });
                    let selectable = worker.is_some_and(|worker| self.worker_selectable(worker));
                    worktree_detail_lines(worktree, selectable, worker)
                })
                .unwrap_or_else(|| {
                    vec![format!(
                        "{} ERROR  Worktree no longer exists.",
                        symbols::ERROR
                    )]
                }),
            Detail::Artifact(content) => artifact_lines(content),
            Detail::Apply(plan) => apply_plan_lines(plan),
            Detail::Report(lines) => lines.clone(),
        };
        let inner = width.saturating_sub(2).max(1);
        lines
            .into_iter()
            .flat_map(|line| textengine::wrap_to_width(&line, inner))
            .map(|line| {
                let selected = matches!(detail, Detail::Worker(_))
                    && line
                        .trim_start()
                        .starts_with(&format!("{} artifact", symbols::ACTIVE));
                (Line::from(line), selected)
            })
            .collect()
    }

    fn window_anchor(&self) -> usize {
        if self.detail.is_some() {
            self.detail_scroll
        } else {
            self.state().selected_index
        }
    }

    fn footer(&self) -> String {
        if let Some(confirmation) = &self.confirmation {
            return if confirmation.force_stage {
                "x continue · esc cancel".to_string()
            } else {
                "y authorize · n deny · esc cancel".to_string()
            };
        }
        if let Some(detail) = &self.detail {
            let mut fields = vec!["← back".to_string(), "tab scope".to_string()];
            match detail {
                Detail::Worker(_) => {
                    if let Some(worker) = self.detail_worker() {
                        if !worker.status.is_terminal() {
                            fields.push("c cancel".to_string());
                        }
                        if self.worker_selectable(worker) {
                            fields.push("s select".to_string());
                        }
                        if worker_plan_eligible(worker, &self.worktrees) {
                            fields.push("p plan".to_string());
                        }
                        if worker
                            .result
                            .as_ref()
                            .and_then(|r| r.worktree.as_ref())
                            .is_some()
                        {
                            fields.push("o worktree".to_string());
                        }
                        if self.selected_worker_artifacts().is_empty() {
                            fields.push("↑↓ scroll".to_string());
                        } else {
                            fields.push("↑↓ artifact · ↵ open · alt-↑↓ scroll".to_string());
                        }
                    }
                }
                Detail::Worktree(id) => {
                    fields.push("↑↓ scroll".to_string());
                    if let Some(worktree) = self.worktrees.iter().find(|record| &record.id == id) {
                        if worktree.worker_id.is_some() {
                            fields.push("o worker".to_string());
                        }
                        if worktree.worker_id.as_ref().is_some_and(|worker_id| {
                            self.workers
                                .iter()
                                .find(|worker| &worker.worker_id == worker_id)
                                .is_some_and(|worker| self.worker_selectable(worker))
                        }) {
                            fields.push("s select".to_string());
                        }
                        if worktree.worker_id.as_ref().is_some_and(|worker_id| {
                            self.workers
                                .iter()
                                .find(|worker| &worker.worker_id == worker_id)
                                .is_some_and(|worker| worker_plan_eligible(worker, &self.worktrees))
                        }) {
                            fields.push("p plan".to_string());
                        }
                        if worktree.status == WorktreeStatus::Adoptable {
                            fields.push("a adopt".to_string());
                            fields.push("i ignore".to_string());
                        }
                        if !matches!(
                            worktree.status,
                            WorktreeStatus::Corrupt | WorktreeStatus::Removed
                        ) {
                            fields.push("x remove".to_string());
                        }
                    }
                }
                Detail::Apply(_) => {
                    fields.push("↑↓ scroll".to_string());
                    fields.push("a apply".to_string());
                }
                Detail::Artifact(_) | Detail::Report(_) => fields.push("↑↓ scroll".to_string()),
            }
            fields.push("esc back".to_string());
            return fields.join(" · ");
        }
        let mut fields = vec![
            "↑↓ move".to_string(),
            "↵ detail".to_string(),
            "tab scope".to_string(),
            "/ filter".to_string(),
            "r refresh".to_string(),
        ];
        if self.scope == DelegationScope::Worktrees {
            fields.push("g gc".to_string());
            fields.push("b rebuild".to_string());
        }
        fields.push("q close".to_string());
        fields.join(" · ")
    }

    #[cfg(test)]
    fn worker_state(status: WorkerStatus) -> (&'static str, &'static str) {
        worker_state(status)
    }
}

fn request_label(kind: &DelegationRequestKind) -> String {
    match kind {
        DelegationRequestKind::Snapshot { .. } => "refreshing dashboard".to_string(),
        DelegationRequestKind::CancelWorker(id) => {
            format!("cancelling worker {}", short_id(&id.to_string()))
        }
        DelegationRequestKind::CancelGroup(id) => {
            format!("cancelling group {}", short_id(&id.to_string()))
        }
        DelegationRequestKind::ReadArtifact(id) => {
            format!("loading artifact {}", short_id(&id.to_string()))
        }
        DelegationRequestKind::SelectCandidate(id) => {
            format!("selecting candidate {}", short_id(&id.to_string()))
        }
        DelegationRequestKind::PlanApply(id) => {
            format!("planning apply for {}", short_id(&id.to_string()))
        }
        DelegationRequestKind::Apply { plan_id, .. } => {
            format!("applying plan {}", short_id(&plan_id.to_string()))
        }
        DelegationRequestKind::AdoptWorktree(id) => {
            format!("adopting worktree {}", short_id(&id.to_string()))
        }
        DelegationRequestKind::IgnoreWorktree(id) => {
            format!("ignoring worktree {}", short_id(&id.to_string()))
        }
        DelegationRequestKind::RemoveWorktree { id, force } => format!(
            "{} worktree {}",
            if *force { "force-removing" } else { "removing" },
            short_id(&id.to_string())
        ),
        DelegationRequestKind::GcWorktrees => "running conservative worktree GC".to_string(),
        DelegationRequestKind::RebuildWorktrees => "rebuilding worktree registry".to_string(),
    }
}

fn worker_state(status: WorkerStatus) -> (&'static str, &'static str) {
    match status {
        WorkerStatus::Queued => (symbols::EMPTY, "QUEUED"),
        WorkerStatus::Initializing => (symbols::RUNNING, "STARTING"),
        WorkerStatus::Running => (symbols::RUNNING, "RUNNING"),
        WorkerStatus::WaitingForApproval => (symbols::REVIEW, "REVIEW"),
        WorkerStatus::Completed => (symbols::DONE, "DONE"),
        WorkerStatus::Failed => (symbols::ERROR, "ERROR"),
        WorkerStatus::Cancelled => (symbols::CANCELLED, "CANCELLED"),
        WorkerStatus::Interrupted => (symbols::REVIEW, "INTERRUPTED"),
        WorkerStatus::Adoptable => (symbols::REVIEW, "ADOPTABLE"),
        _ => (symbols::ERROR, "UNKNOWN"),
    }
}

fn worktree_state(status: WorktreeStatus) -> (&'static str, &'static str) {
    match status {
        WorktreeStatus::Alive => (symbols::RUNNING, "ALIVE"),
        WorktreeStatus::Adoptable => (symbols::REVIEW, "ADOPTABLE"),
        WorktreeStatus::Ignored => (symbols::CANCELLED, "IGNORED"),
        WorktreeStatus::Applied => (symbols::DONE, "APPLIED"),
        WorktreeStatus::Removed => (symbols::CANCELLED, "REMOVED"),
        WorktreeStatus::Corrupt => (symbols::ERROR, "CORRUPT"),
        _ => (symbols::ERROR, "UNKNOWN"),
    }
}

fn status_style(status: WorkerStatus) -> Style {
    match status {
        WorkerStatus::Completed => success_style(),
        WorkerStatus::Failed => error_style(),
        WorkerStatus::WaitingForApproval | WorkerStatus::Interrupted | WorkerStatus::Adoptable => {
            review_style()
        }
        WorkerStatus::Running | WorkerStatus::Initializing => review_style(),
        _ => dim_style(),
    }
}

fn worker_rows(
    worker: &WorkerSnapshot,
    worktrees: &[WorktreeRecord],
    width: usize,
    selected: bool,
) -> Vec<(Line<'static>, bool)> {
    let (glyph, label) = worker_state(worker.status);
    let id = short_id(&worker.worker_id.to_string());
    let description = worker_description(worker);
    let linked = worker
        .result
        .as_ref()
        .and_then(|result| result.worktree.as_ref());
    let selected_candidate = worktrees.iter().any(|worktree| {
        worktree.worker_id.as_ref() == Some(&worker.worker_id) && worktree.selected
    });
    if width < 65 {
        let mut first = vec![
            Span::styled(format!("{glyph} {label}"), status_style(worker.status)),
            Span::raw("  "),
            Span::styled(id, Style::default().add_modifier(Modifier::BOLD)),
        ];
        if selected_candidate {
            first.push(Span::styled(
                format!("  {} selected", symbols::ACTIVE),
                review_style(),
            ));
        }
        return vec![
            (Line::from(first), selected),
            (
                Line::from(Span::styled(
                    format!(
                        "  {}",
                        textengine::ellipsize_to_width(&description, width.saturating_sub(2))
                    ),
                    dim_style(),
                )),
                selected,
            ),
        ];
    }
    let mut meta = Vec::new();
    if let Some(group) = &worker.group_id {
        let marker = if selected_candidate {
            symbols::ACTIVE
        } else {
            symbols::EMPTY
        };
        meta.push(format!("{marker} {}", short_id(&group.to_string())));
    }
    meta.push(match worker.request.policy.isolation {
        IsolationMode::Worktree => "worktree".to_string(),
        _ => "read-only".to_string(),
    });
    if width >= 100 && worker.usage.total_tokens() > 0 {
        meta.push(format!(
            "↑{} ↓{}",
            compact_count(worker.usage.input_tokens),
            compact_count(worker.usage.output_tokens)
        ));
    }
    if let Some(result) = &worker.result
        && !result.changed_paths.is_empty()
    {
        meta.push(format!("{} changed", result.changed_paths.len()));
    }
    if width >= 100 && linked.is_some() {
        meta.push("linked".to_string());
    }
    vec![(
        Line::from(vec![
            Span::styled(format!("{glyph} {label}"), status_style(worker.status)),
            Span::raw("  "),
            Span::styled(id, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::raw(textengine::ellipsize_to_width(
                &description,
                width.saturating_sub(35),
            )),
            Span::raw("  "),
            Span::styled(meta.join(" · "), dim_style()),
        ]),
        selected,
    )]
}

fn worktree_rows(
    worktree: &WorktreeRecord,
    width: usize,
    selected: bool,
) -> Vec<(Line<'static>, bool)> {
    let (glyph, label) = worktree_state(worktree.status);
    let id = short_id(&worktree.id.to_string());
    if width < 65 {
        return vec![
            (
                Line::from(vec![
                    Span::styled(
                        format!("{glyph} {label}"),
                        match worktree.status {
                            WorktreeStatus::Applied => success_style(),
                            WorktreeStatus::Corrupt => error_style(),
                            WorktreeStatus::Adoptable => review_style(),
                            _ => dim_style(),
                        },
                    ),
                    Span::raw("  "),
                    Span::styled(id, Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::raw(worktree.repo_name.clone()),
                ]),
                selected,
            ),
            (
                Line::from(Span::styled(
                    format!(
                        "  {} · {}",
                        worktree_kind(worktree.kind),
                        textengine::ellipsize_middle_to_width(
                            &worktree.path.display().to_string(),
                            width.saturating_sub(12)
                        )
                    ),
                    dim_style(),
                )),
                selected,
            ),
        ];
    }
    let mut meta = vec![format!(
        "{} · {}",
        worktree_kind(worktree.kind),
        creation_mode(worktree.creation_mode)
    )];
    if let Some(worker) = &worktree.worker_id {
        meta.push(format!("worker {}", short_id(&worker.to_string())));
    }
    if worktree.selected {
        meta.push(format!("{} selected", symbols::ACTIVE));
    }
    if worktree.applied_to_parent {
        meta.push("applied".to_string());
    }
    if width >= 100 {
        meta.push(textengine::ellipsize_middle_to_width(
            &worktree.path.display().to_string(),
            28,
        ));
    }
    vec![(
        Line::from(vec![
            Span::styled(format!("{glyph} {label}"), dim_style()),
            Span::raw("  "),
            Span::styled(id, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::raw(worktree.repo_name.clone()),
            Span::raw("  "),
            Span::styled(meta.join(" · "), dim_style()),
        ]),
        selected,
    )]
}

fn worker_detail_lines(
    worker: &WorkerSnapshot,
    workers: &[WorkerSnapshot],
    worktrees: &[WorktreeRecord],
    events: &BTreeMap<WorkerId, Vec<WorkerEvent>>,
    artifact_index: usize,
) -> Vec<String> {
    let (glyph, label) = worker_state(worker.status);
    let request = &worker.request;
    let mut lines = vec![
        format!("{glyph} {label}  {}", worker.worker_id),
        format!("description  {}", worker_description(worker)),
        format!(
            "kind         {:?} · priority {:?}",
            request.kind, request.priority
        ),
        format!(
            "policy       capability {:?} · isolation {:?} · outside reads {}",
            request.policy.capability,
            request.policy.isolation,
            if request.policy.allow_outside_workspace {
                "allowed"
            } else {
                "confined"
            }
        ),
    ];
    push_optional(
        &mut lines,
        "group",
        worker.group_id.as_ref().map(ToString::to_string),
    );
    if let Some(group_id) = worker.group_id.as_ref() {
        lines.push(format!(
            "members      {}",
            workers
                .iter()
                .filter(|candidate| candidate.group_id.as_ref() == Some(group_id))
                .map(|candidate| candidate.worker_id.to_string())
                .collect::<Vec<_>>()
                .join(" · ")
        ));
        lines.push(format!(
            "candidate    {}",
            worktrees
                .iter()
                .find(|worktree| {
                    worktree.group_id.as_ref() == Some(group_id) && worktree.selected
                })
                .and_then(|worktree| worktree.worker_id.as_ref())
                .map(ToString::to_string)
                .unwrap_or_else(|| "none selected".to_string())
        ));
    }
    push_optional(
        &mut lines,
        "parent",
        request.parent_worker_id.as_ref().map(ToString::to_string),
    );
    push_optional(&mut lines, "session", request.session_id.clone());
    let route = [request.route_id.as_deref(), request.profile_id.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    if !route.is_empty() {
        lines.push(format!("route        {route}"));
    }
    let budgets = budget_line(&request.budgets);
    if !budgets.is_empty() {
        lines.push(format!("budgets      {budgets}"));
    }
    lines.push(format!(
        "usage        ↑{} ↓{} · {} provider rounds · {} tool rounds",
        worker.usage.input_tokens,
        worker.usage.output_tokens,
        worker.usage.provider_rounds,
        worker.usage.tool_rounds
    ));
    if let Some(result) = &worker.result {
        if !result.summary.is_empty() {
            lines.push(format!("summary      {}", result.summary));
        }
        push_optional(&mut lines, "message", result.message.clone());
        if !result.changed_paths.is_empty() {
            lines.push(format!(
                "changed      {}",
                result
                    .changed_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" · ")
            ));
        }
        push_optional(
            &mut lines,
            "apply plan",
            result.apply_plan_id.as_ref().map(ToString::to_string),
        );
        if let Some(worktree) = &result.worktree {
            lines.push(format!(
                "worktree     {} · {} · base {} · {}",
                worktree.id,
                worktree.path.display(),
                worktree.base_commit,
                worktree.creation_mode
            ));
        }
        for (index, artifact) in result.artifacts.iter().enumerate() {
            lines.push(format!(
                "{} artifact {} · {} bytes · {}",
                if index == artifact_index {
                    symbols::ACTIVE
                } else {
                    symbols::EMPTY
                },
                artifact.id,
                artifact.bytes,
                artifact.media_type.as_deref().unwrap_or("unknown type")
            ));
        }
    }
    if let Some(record) = worktrees
        .iter()
        .find(|record| record.worker_id.as_ref() == Some(&worker.worker_id))
    {
        lines.push(format!(
            "managed      {} · {:?} · selected {} · applied {}",
            record.id, record.status, record.selected, record.applied_to_parent
        ));
    }
    if let Some(tail) = events.get(&worker.worker_id)
        && !tail.is_empty()
    {
        lines.push("EVENT TAIL".to_string());
        for event in tail {
            lines.push(format!(
                "#{:<4} {}  {:?}",
                event.sequence, event.timestamp_ms, event.kind
            ));
        }
    }
    lines
}

fn worktree_detail_lines(
    worktree: &WorktreeRecord,
    selectable: bool,
    worker: Option<&WorkerSnapshot>,
) -> Vec<String> {
    let (glyph, label) = worktree_state(worktree.status);
    let mut lines = vec![
        format!("{glyph} {label}  {}", worktree.id),
        format!("path         {}", worktree.path.display()),
        format!(
            "repository   {} ({})",
            worktree.repo_name,
            worktree.source_repo.display()
        ),
        format!("base         {}", worktree.base_commit),
        format!(
            "kind         {} · {}",
            worktree_kind(worktree.kind),
            creation_mode(worktree.creation_mode)
        ),
        format!(
            "owner        {} · pid {}",
            worktree.owner_instance_id, worktree.owner_pid
        ),
        format!("created      {}", age(worktree.created_at_ms)),
        format!(
            "accessed     {}",
            worktree
                .last_accessed_at_ms
                .map(age)
                .unwrap_or_else(|| "not recorded".to_string())
        ),
        format!(
            "selected     {} · applied {}",
            worktree.selected, worktree.applied_to_parent
        ),
    ];
    push_optional(&mut lines, "git ref", worktree.git_ref.clone());
    push_optional(&mut lines, "session", worktree.session_id.clone());
    push_optional(
        &mut lines,
        "worker",
        worktree.worker_id.as_ref().map(ToString::to_string),
    );
    push_optional(
        &mut lines,
        "parent",
        worktree.parent_worker_id.as_ref().map(ToString::to_string),
    );
    push_optional(
        &mut lines,
        "group",
        worktree.group_id.as_ref().map(ToString::to_string),
    );
    if let Some(result) = worker.and_then(|worker| worker.result.as_ref()) {
        lines.push(format!(
            "changed      {} path(s)",
            result.changed_paths.len()
        ));
        push_optional(
            &mut lines,
            "apply plan",
            result.apply_plan_id.as_ref().map(ToString::to_string),
        );
    }
    let actions = legal_worktree_actions(worktree, selectable);
    lines.push(format!(
        "legal        {}",
        if actions.is_empty() {
            "inspect only".to_string()
        } else {
            actions.join(" · ")
        }
    ));
    lines
}

fn artifact_lines(content: &ArtifactContent) -> Vec<String> {
    let mut lines = vec![format!(
        "ARTIFACT {} · {} bytes{}",
        content.id,
        content.total_bytes,
        if content.truncated {
            " · bounded preview"
        } else {
            ""
        }
    )];
    match &content.text {
        Some(text) if text.is_empty() => lines.push("Empty UTF-8 artifact.".to_string()),
        Some(text) => lines.extend(text.lines().map(str::to_string)),
        None => lines.push("Binary or non-UTF-8 artifact; content is not rendered.".to_string()),
    }
    lines
}

fn apply_plan_lines(plan: &ApplyPlan) -> Vec<String> {
    let mut creates = 0usize;
    let mut updates = 0usize;
    let mut deletes = 0usize;
    let mut binary = 0usize;
    let mut modes = 0usize;
    let mut symlinks = 0usize;
    let mut unresolved = 0usize;
    for operation in &plan.operations {
        match operation.change {
            ApplyChangeKind::Create => creates += 1,
            ApplyChangeKind::Update => updates += 1,
            ApplyChangeKind::Delete => deletes += 1,
            _ => {}
        }
        if operation.child.kind == ApplyFileKind::Regular
            && std::str::from_utf8(&operation.child.content).is_err()
        {
            binary += 1;
        }
        if operation.child.mode != operation.base.mode {
            modes += 1;
        }
        if operation.child.kind == ApplyFileKind::Symlink {
            symlinks += 1;
        }
        if operation.child.kind == ApplyFileKind::Gitlink {
            unresolved += 1;
        }
    }
    let mut lines = vec![
        format!("{} PREVIEW  {}", symbols::PREVIEW, plan.id),
        format!("digest       {}", plan.digest),
        format!(
            "source       {} · base {}",
            plan.child_root.display(),
            plan.base_commit
        ),
        format!(
            "target       {} · head {}",
            plan.parent_root.display(),
            plan.parent_head
        ),
        format!(
            "changes      {}{creates} · {}{updates} · {}{deletes} · rename 0 · binary {binary} · mode {modes} · symlink {symlinks} · unresolved {unresolved}",
            symbols::ADDED,
            symbols::DIRTY,
            symbols::REMOVED,
        ),
    ];
    for operation in &plan.operations {
        let marker = match operation.change {
            ApplyChangeKind::Create => symbols::ADDED,
            ApplyChangeKind::Update => symbols::DIRTY,
            ApplyChangeKind::Delete => symbols::REMOVED,
            _ => "?",
        };
        let mut warnings = Vec::new();
        if operation.dirty_parent {
            warnings.push("dirty parent");
        }
        if operation.base_drift {
            warnings.push("base drift");
        }
        if operation.escaping_symlink {
            warnings.push("escaping symlink");
        }
        if operation.child.kind == ApplyFileKind::Gitlink {
            warnings.push("unresolved gitlink");
        }
        lines.push(format!(
            "{marker} {}{}",
            operation.path.display(),
            if warnings.is_empty() {
                String::new()
            } else {
                format!("  {} {}", symbols::REVIEW, warnings.join(" · "))
            }
        ));
        if let (Ok(old), Ok(new)) = (
            std::str::from_utf8(&operation.parent.content),
            std::str::from_utf8(&operation.child.content),
        ) {
            let old = textengine::clean_text(old);
            let new = textengine::clean_text(new);
            if old != new
                && let Some(diff) = crate::tools::render_preview(crate::tools::Preview::Available {
                    path: operation.path.display().to_string(),
                    old,
                    new,
                })
            {
                lines.extend(textengine::clean_text(&diff).lines().map(str::to_string));
            }
        }
    }
    lines
}

fn apply_result_lines(result: &ApplyResult) -> Vec<String> {
    let (glyph, label) = match result.disposition {
        ApplyDisposition::Complete | ApplyDisposition::AlreadyApplied => (symbols::DONE, "DONE"),
        ApplyDisposition::Partial => (symbols::REVIEW, "REVIEW"),
        _ => (symbols::ERROR, "ERROR"),
    };
    let mut lines = vec![
        format!("{glyph} {label}  plan {}", result.plan_id),
        format!("applied      {} path(s)", result.applied.len()),
    ];
    for path in &result.applied {
        lines.push(format!("{} {}", symbols::ADDED, path.display()));
    }
    for conflict in &result.conflicts {
        lines.push(format!(
            "{} {} · {:?}",
            symbols::REVIEW,
            conflict.path.display(),
            conflict.kind
        ));
    }
    lines
}

fn gc_report_lines(report: &GcReport) -> Vec<String> {
    vec![
        "WORKTREE GC".to_string(),
        format!("removed      {}", report.removed.len()),
        format!("live skipped {}", report.skipped_live.len()),
        format!("adoptable    {}", report.adoptable.len()),
        format!("corrupt      {}", report.corrupt.len()),
        format!(
            "prune        {}",
            if report.prune_suppressed {
                "suppressed"
            } else {
                "completed"
            }
        ),
    ]
}

fn worker_plan_eligible(worker: &WorkerSnapshot, worktrees: &[WorktreeRecord]) -> bool {
    if worker.status != WorkerStatus::Completed {
        return false;
    }
    let Some(result) = worker.result.as_ref() else {
        return false;
    };
    let Some(linked) = result.worktree.as_ref() else {
        return false;
    };
    if result.changed_paths.is_empty() {
        return false;
    }
    match worker.group_id.as_ref() {
        Some(group) => worktrees.iter().any(|worktree| {
            worktree.id == linked.id
                && worktree.group_id.as_ref() == Some(group)
                && worktree.status == WorktreeStatus::Alive
                && worktree.selected
                && !worktree.applied_to_parent
        }),
        None => worktrees.iter().any(|worktree| {
            worktree.id == linked.id
                && worktree.status == WorktreeStatus::Alive
                && !worktree.applied_to_parent
        }),
    }
}

fn worker_matches(worker: &WorkerSnapshot, filter: &str) -> bool {
    filter.is_empty()
        || worker
            .worker_id
            .as_str()
            .to_ascii_lowercase()
            .contains(filter)
        || worker_description(worker)
            .to_ascii_lowercase()
            .contains(filter)
        || format!("{:?}", worker.status)
            .to_ascii_lowercase()
            .contains(filter)
        || worker
            .group_id
            .as_ref()
            .is_some_and(|group| group.as_str().to_ascii_lowercase().contains(filter))
}

fn worktree_matches(worktree: &WorktreeRecord, filter: &str) -> bool {
    filter.is_empty()
        || worktree.id.as_str().to_ascii_lowercase().contains(filter)
        || worktree.repo_name.to_ascii_lowercase().contains(filter)
        || worktree
            .path
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains(filter)
        || format!("{:?}", worktree.status)
            .to_ascii_lowercase()
            .contains(filter)
}

fn creation_ms(worker: &WorkerSnapshot, events: &BTreeMap<WorkerId, Vec<WorkerEvent>>) -> u64 {
    events
        .get(&worker.worker_id)
        .and_then(|events| events.first())
        .map_or(0, |event| event.timestamp_ms)
}

fn group_creation_ms(
    group: &str,
    workers: &[WorkerSnapshot],
    events: &BTreeMap<WorkerId, Vec<WorkerEvent>>,
) -> u64 {
    workers
        .iter()
        .filter(|worker| {
            worker
                .group_id
                .as_ref()
                .is_some_and(|id| id.as_str() == group)
        })
        .map(|worker| creation_ms(worker, events))
        .max()
        .unwrap_or(0)
}

fn worktree_class(worktree: &WorktreeRecord) -> u8 {
    match (worktree.status, worktree.kind) {
        (WorktreeStatus::Alive, WorktreeKind::Pool) => 3,
        (WorktreeStatus::Alive, _) => 0,
        (WorktreeStatus::Adoptable | WorktreeStatus::Corrupt, _) => 1,
        (WorktreeStatus::Applied | WorktreeStatus::Ignored, _) => 2,
        (WorktreeStatus::Removed, _) => 4,
        _ => 4,
    }
}

fn worktree_time(worktree: &WorktreeRecord) -> u64 {
    worktree
        .last_accessed_at_ms
        .unwrap_or(worktree.created_at_ms)
}

fn worker_description(worker: &WorkerSnapshot) -> String {
    if !worker.request.description.trim().is_empty() {
        return worker.request.description.trim().to_string();
    }
    let prompt = worker
        .request
        .prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if prompt.is_empty() {
        format!("{:?}", worker.request.kind).to_ascii_lowercase()
    } else {
        prompt
    }
}

fn budget_line(budgets: &iris_subagent_runtime::WorkerBudgets) -> String {
    let mut fields = Vec::new();
    if let Some(ms) = budgets.wall_clock_ms {
        fields.push(format!("{}s", ms / 1000));
    }
    if let Some(rounds) = budgets.max_provider_rounds {
        fields.push(format!("{rounds} provider rounds"));
    }
    if let Some(rounds) = budgets.max_tool_rounds {
        fields.push(format!("{rounds} tool rounds"));
    }
    if let Some(tokens) = budgets.max_tokens {
        fields.push(format!("{} tokens", compact_count(tokens)));
    }
    fields.join(" · ")
}

fn legal_worktree_actions(worktree: &WorktreeRecord, selectable: bool) -> Vec<&'static str> {
    let mut actions = Vec::new();
    if worktree.worker_id.is_some() {
        actions.push("open worker");
    }
    if selectable {
        actions.push("select");
    }
    if worktree.status == WorktreeStatus::Adoptable {
        actions.extend(["adopt", "ignore"]);
    }
    if !matches!(
        worktree.status,
        WorktreeStatus::Corrupt | WorktreeStatus::Removed
    ) {
        actions.push(if worktree.status == WorktreeStatus::Alive {
            "force-remove review"
        } else {
            "remove"
        });
    }
    actions
}

fn push_optional(lines: &mut Vec<String>, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        lines.push(format!("{label:<12} {value}"));
    }
}

fn worktree_kind(kind: WorktreeKind) -> &'static str {
    match kind {
        WorktreeKind::Worker => "worker",
        WorktreeKind::Restore => "restore",
        WorktreeKind::Pool => "pool",
        WorktreeKind::Session => "session",
        _ => "unknown",
    }
}

fn creation_mode(mode: CreationMode) -> &'static str {
    match mode {
        CreationMode::Linked => "linked",
        CreationMode::BtrfsSnapshot => "btrfs",
        _ => "unknown",
    }
}

fn short_id(id: &str) -> String {
    id.split_once('_')
        .map(|(prefix, suffix)| format!("{prefix}_{}", &suffix[..suffix.len().min(8)]))
        .unwrap_or_else(|| textengine::ellipsize_to_width(id, 12))
}

fn compact_count(value: u64) -> String {
    crate::ui::tui::compact_count(value)
}

fn age(timestamp_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64);
    let seconds = now.saturating_sub(timestamp_ms) / 1000;
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn window_rows(
    rows: Vec<(Line<'static>, bool)>,
    anchor: usize,
    budget: usize,
) -> Vec<(Line<'static>, bool)> {
    if rows.len() <= budget {
        return rows;
    }
    let anchor = rows
        .iter()
        .position(|(_, selected)| *selected)
        .unwrap_or(anchor);
    let offset = anchor.saturating_sub(budget / 2).min(rows.len() - budget);
    let total = rows.len();
    let mut visible = rows
        .into_iter()
        .skip(offset)
        .take(budget.saturating_sub(1))
        .collect::<Vec<_>>();
    visible.push((
        Line::from(Span::styled(
            format!("({}/{total})", (anchor + 1).min(total)),
            dim_style(),
        )),
        false,
    ));
    visible
}

fn dim_style() -> Style {
    Style::default().fg(palette::muted())
}

fn success_style() -> Style {
    Style::default().fg(palette::green())
}

fn error_style() -> Style {
    Style::default()
        .fg(palette::red())
        .add_modifier(Modifier::BOLD)
}

fn review_style() -> Style {
    Style::default().fg(palette::orange())
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_subagent_runtime::InstanceId;
    use serde_json::json;

    fn worker(id: &str, status: &str, description: &str, group: Option<&str>) -> WorkerSnapshot {
        serde_json::from_value(json!({
            "request": {
                "schema_version": 1,
                "kind": {"type": "general"},
                "prompt": description,
                "description": description,
                "priority": "normal",
                "policy": {
                    "capability": "read_only",
                    "parent_capability": "all",
                    "isolation": "none",
                    "cwd": null,
                    "tool_allowlist": [],
                    "allow_outside_workspace": false,
                    "nesting_depth": 0,
                    "max_nesting_depth": 2
                },
                "budgets": {},
                "recovery": "adoptable",
                "parent_worker_id": null,
                "session_id": "session-1",
                "route_id": null,
                "profile_id": null,
                "resume_from": null,
                "host": {"schema_version": 1, "kind": "none", "value": null}
            },
            "worker_id": id,
            "status": status,
            "group_id": group,
            "usage": {},
            "result": null,
            "last_event_sequence": 1
        }))
        .unwrap()
    }

    fn event(worker_id: &str, timestamp_ms: u64) -> WorkerEvent {
        serde_json::from_value(json!({
            "schema_version": 1,
            "worker_id": worker_id,
            "sequence": 1,
            "timestamp_ms": timestamp_ms,
            "kind": {"type": "status", "data": "queued"}
        }))
        .unwrap()
    }

    fn response(id: u64, workers: Vec<WorkerSnapshot>) -> DelegationResponse {
        let events = workers
            .iter()
            .enumerate()
            .map(|(index, worker)| {
                (
                    worker.worker_id.clone(),
                    vec![event(worker.worker_id.as_str(), index as u64 + 1)],
                )
            })
            .collect();
        DelegationResponse {
            request_id: id,
            result: Ok(DelegationPayload::Snapshot(DelegationSnapshot {
                workers,
                worktrees: Some(Vec::new()),
                events,
            })),
        }
    }

    fn completed_worker(
        id: &str,
        group: &str,
        worktree_id: &WorktreeId,
        artifact_id: Option<&ArtifactId>,
    ) -> WorkerSnapshot {
        let mut value = serde_json::to_value(worker(id, "completed", "candidate", Some(group)))
            .expect("serialize worker fixture");
        value["result"] = json!({
            "schema_version": 1,
            "worker_id": id,
            "status": "completed",
            "summary": "done",
            "inline_output": null,
            "artifacts": artifact_id.map(|id| vec![json!({
                "id": id,
                "bytes": 64,
                "media_type": "text/plain"
            })]).unwrap_or_default(),
            "usage": {},
            "changed_paths": ["src/lib.rs"],
            "worktree": {
                "id": worktree_id,
                "path": "/tmp/worker-candidate",
                "base_commit": "abc123",
                "creation_mode": "linked"
            },
            "apply_plan_id": null,
            "host": {"schema_version": 1, "kind": "none", "value": null},
            "message": null
        });
        serde_json::from_value(value).expect("completed worker fixture")
    }

    fn worktree(
        id: &WorktreeId,
        worker_id: Option<&WorkerId>,
        group_id: Option<&GroupId>,
        status: WorktreeStatus,
        selected: bool,
    ) -> WorktreeRecord {
        serde_json::from_value(json!({
            "schema_version": 1,
            "id": id,
            "path": format!("/tmp/{}", id),
            "source_repo": "/tmp/repository",
            "repo_name": "repository",
            "kind": "worker",
            "creation_mode": "linked",
            "git_ref": null,
            "base_commit": "abc123",
            "session_id": "session-1",
            "worker_id": worker_id,
            "group_id": group_id,
            "selected": selected,
            "applied_to_parent": false,
            "parent_worker_id": null,
            "owner_pid": 7,
            "owner_instance_id": InstanceId::new(),
            "created_at_ms": 1,
            "last_accessed_at_ms": 2,
            "status": status,
            "metadata": {"schema_version": 1, "kind": "none", "value": null}
        }))
        .expect("worktree fixture")
    }

    fn apply_plan() -> ApplyPlan {
        serde_json::from_value(json!({
            "schema_version": 1,
            "id": ApplyPlanId::new(),
            "worktree_id": WorktreeId::new(),
            "base_commit": "abc123",
            "parent_head": "def456",
            "parent_root": "/tmp/repository",
            "child_root": "/tmp/worker-candidate",
            "operations": [],
            "digest": "0123456789abcdef"
        }))
        .expect("apply plan fixture")
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_worker_status_has_the_dashboard_symbol_and_label() {
        let cases = [
            (WorkerStatus::Queued, ("○", "QUEUED")),
            (WorkerStatus::Initializing, ("●", "STARTING")),
            (WorkerStatus::Running, ("●", "RUNNING")),
            (WorkerStatus::WaitingForApproval, ("▲", "REVIEW")),
            (WorkerStatus::Completed, ("◆", "DONE")),
            (WorkerStatus::Failed, ("■", "ERROR")),
            (WorkerStatus::Cancelled, ("□", "CANCELLED")),
            (WorkerStatus::Interrupted, ("▲", "INTERRUPTED")),
            (WorkerStatus::Adoptable, ("▲", "ADOPTABLE")),
        ];
        for (status, expected) in cases {
            assert_eq!(DelegationDashboard::worker_state(status), expected);
        }
    }

    #[test]
    fn every_worktree_status_has_a_glyph_and_explicit_label() {
        let cases = [
            (WorktreeStatus::Alive, ("●", "ALIVE")),
            (WorktreeStatus::Adoptable, ("▲", "ADOPTABLE")),
            (WorktreeStatus::Ignored, ("□", "IGNORED")),
            (WorktreeStatus::Applied, ("◆", "APPLIED")),
            (WorktreeStatus::Removed, ("□", "REMOVED")),
            (WorktreeStatus::Corrupt, ("■", "CORRUPT")),
        ];
        for (status, expected) in cases {
            assert_eq!(worktree_state(status), expected);
        }
    }

    #[test]
    fn snapshot_groups_workers_and_preserves_selection_by_opaque_id() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        let group = "grp_00000000000000000000000000000001";
        dashboard.apply_response(response(
            initial.request_id,
            vec![
                worker(
                    "wrk_00000000000000000000000000000001",
                    "completed",
                    "one",
                    Some(group),
                ),
                worker(
                    "wrk_00000000000000000000000000000002",
                    "running",
                    "two",
                    Some(group),
                ),
                worker(
                    "wrk_00000000000000000000000000000003",
                    "queued",
                    "three",
                    None,
                ),
            ],
        ));
        dashboard.move_selection(1);
        let selected = dashboard.selected_worker().unwrap().worker_id.clone();

        let refresh = dashboard
            .request_refresh(Instant::now() + WORKER_REFRESH)
            .unwrap();
        let mut workers = dashboard.workers.clone();
        workers.reverse();
        dashboard.apply_response(response(refresh.request_id, workers));

        assert_eq!(dashboard.selected_worker().unwrap().worker_id, selected);
        let rendered = text(&dashboard.render_budgeted(100, 30));
        assert!(
            rendered.contains("GROUP grp_00000000 · 2 candidates"),
            "{rendered}"
        );
    }

    #[test]
    fn filter_owns_printable_input_and_removed_selection_falls_to_nearest_row() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        dashboard.apply_response(response(
            initial.request_id,
            vec![
                worker(
                    "wrk_00000000000000000000000000000001",
                    "queued",
                    "alpha",
                    None,
                ),
                worker(
                    "wrk_00000000000000000000000000000002",
                    "queued",
                    "beta",
                    None,
                ),
            ],
        ));
        dashboard.handle_key(ModalKey::Down);
        dashboard.handle_key(ModalKey::Char('/'));
        for ch in "alpha".chars() {
            dashboard.handle_key(ModalKey::Char(ch));
        }
        assert_eq!(dashboard.visible_worker_indices().len(), 1);
        assert_eq!(
            dashboard.selected_worker().unwrap().request.description,
            "alpha"
        );

        dashboard.handle_key(ModalKey::Esc);
        let refresh = dashboard
            .request_refresh(Instant::now() + WORKER_REFRESH)
            .unwrap();
        dashboard.apply_response(response(
            refresh.request_id,
            vec![worker(
                "wrk_00000000000000000000000000000002",
                "queued",
                "beta",
                None,
            )],
        ));
        assert!(
            dashboard.selected_worker().is_none(),
            "active filter has no survivor"
        );
    }

    #[test]
    fn stale_and_out_of_order_responses_cannot_replace_newer_or_reopened_state() {
        let mut closed = DelegationDashboard::new(DelegationScope::Workers);
        let first = closed.request_initial().unwrap();
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let second = dashboard.request_initial().unwrap();
        assert!(second.request_id > first.request_id);
        assert!(!dashboard.apply_response(response(
            first.request_id,
            vec![worker(
                "wrk_00000000000000000000000000000001",
                "queued",
                "old",
                None,
            )],
        )));
        assert!(dashboard.apply_response(response(
            second.request_id,
            vec![worker(
                "wrk_00000000000000000000000000000002",
                "running",
                "new",
                None,
            )],
        )));
        assert_eq!(dashboard.workers[0].request.description, "new");
    }

    #[test]
    fn refresh_errors_mark_last_good_data_stale_but_action_errors_do_not() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        dashboard.apply_response(response(
            initial.request_id,
            vec![worker(
                "wrk_00000000000000000000000000000001",
                "running",
                "kept",
                None,
            )],
        ));
        dashboard.handle_key(ModalKey::Enter);
        let ModalOutcome::Redraw = dashboard.handle_key(ModalKey::Char('c')) else {
            panic!("cancel did not enter confirmation");
        };
        let ModalOutcome::Emit(ModalAction::Delegation(action)) =
            dashboard.handle_key(ModalKey::Char('y'))
        else {
            panic!("cancel confirmation did not emit");
        };
        dashboard.apply_response(DelegationResponse {
            request_id: action.request_id,
            result: Err("cancel refused".to_string()),
        });
        assert!(!dashboard.stale);
        assert_eq!(dashboard.workers[0].request.description, "kept");

        let refresh = dashboard
            .request_refresh(Instant::now() + WORKER_REFRESH)
            .unwrap();
        dashboard.apply_response(DelegationResponse {
            request_id: refresh.request_id,
            result: Err("refresh unavailable".to_string()),
        });
        assert!(dashboard.stale);
        assert_eq!(dashboard.workers[0].request.description, "kept");
    }

    #[test]
    fn worker_list_render_matches_the_deterministic_golden() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        dashboard.apply_response(response(
            initial.request_id,
            vec![worker(
                "wrk_00000000000000000000000000000001",
                "running",
                "inspect parser boundaries",
                None,
            )],
        ));

        assert_eq!(
            text(&dashboard.render_budgeted(80, 12)),
            "DELEGATION · WORKERS\n\
● RUNNING  wrk_00000000  inspect parser boundaries  read-only  \n\
\n\
↑↓ move · ↵ detail · tab scope · / filter · r refresh · q close"
        );
    }

    #[test]
    fn rendering_degrades_at_required_widths_and_stays_height_bounded() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        let workers = (0..12)
            .map(|index| {
                worker(
                    &format!("wrk_{index:032x}"),
                    "running",
                    "inspect a long repository path without overflowing the pane",
                    None,
                )
            })
            .collect();
        dashboard.apply_response(response(initial.request_id, workers));

        for (width, height) in [(120usize, 40usize), (80, 24), (50, 16), (50, 12)] {
            let lines = dashboard.render_budgeted(width, height.saturating_sub(6));
            assert!(
                lines.len() <= height.saturating_sub(3),
                "{width}x{height}: {}",
                lines.len()
            );
            assert!(lines.iter().all(|line| {
                textengine::display_width(
                    &line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>(),
                ) <= width
            }));
        }
        let narrow = text(&dashboard.render_budgeted(50, 10));
        assert!(narrow.contains("inspect a long repository"), "{narrow}");
        assert!(
            !narrow.contains("↑0 ↓0"),
            "usage degrades before identity: {narrow}"
        );
    }

    #[test]
    fn detail_navigation_and_scope_switch_preserve_each_scope_cursor() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        let worker_id = "wrk_00000000000000000000000000000001";
        let worktree_id = WorktreeId::new();
        let group_id = GroupId::new();
        let candidate = completed_worker(worker_id, group_id.as_str(), &worktree_id, None);
        let record = worktree(
            &worktree_id,
            Some(&candidate.worker_id),
            Some(&group_id),
            WorktreeStatus::Alive,
            false,
        );
        let mut snapshot = response(initial.request_id, vec![candidate]);
        let Ok(DelegationPayload::Snapshot(snapshot)) = &mut snapshot.result else {
            unreachable!();
        };
        snapshot.worktrees = Some(vec![record]);
        dashboard.apply_response(snapshot_response(snapshot.clone(), initial.request_id));

        assert!(matches!(
            dashboard.handle_key(ModalKey::Enter),
            ModalOutcome::Redraw
        ));
        assert!(matches!(dashboard.detail, Some(Detail::Worker(_))));
        dashboard.handle_key(ModalKey::Tab);
        assert_eq!(dashboard.scope, DelegationScope::Worktrees);
        assert!(dashboard.detail.is_none());
        dashboard.handle_key(ModalKey::Enter);
        assert!(matches!(dashboard.detail, Some(Detail::Worktree(_))));
        dashboard.handle_key(ModalKey::Left);
        assert!(dashboard.detail.is_none());
        dashboard.handle_key(ModalKey::Tab);
        assert_eq!(dashboard.scope, DelegationScope::Workers);
        assert_eq!(
            dashboard.selected_worker().unwrap().worker_id.as_str(),
            worker_id
        );
    }

    #[test]
    fn cancellation_requires_confirmation_and_terminal_workers_are_no_ops() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        let group = GroupId::new();
        dashboard.apply_response(response(
            initial.request_id,
            vec![worker(
                "wrk_00000000000000000000000000000001",
                "running",
                "running candidate",
                Some(group.as_str()),
            )],
        ));
        dashboard.handle_key(ModalKey::Enter);

        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('c')),
            ModalOutcome::Redraw
        ));
        assert!(dashboard.confirmation.is_some());
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('y')),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::CancelGroup(actual),
                ..
            })) if actual == group
        ));

        dashboard.pending_request = None;
        dashboard.in_flight = None;
        dashboard.workers[0].status = WorkerStatus::Completed;
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('c')),
            ModalOutcome::Redraw
        ));
        assert!(dashboard.confirmation.is_none());
        assert!(
            dashboard
                .message
                .as_deref()
                .is_some_and(|message| message.contains("cannot be cancelled"))
        );
    }

    #[test]
    fn candidate_selection_and_artifact_loading_use_typed_backend_requests() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        let group_id = GroupId::new();
        let worktree_id = WorktreeId::new();
        let artifact_id = ArtifactId::new();
        let candidate = completed_worker(
            "wrk_00000000000000000000000000000001",
            group_id.as_str(),
            &worktree_id,
            Some(&artifact_id),
        );
        let record = worktree(
            &worktree_id,
            Some(&candidate.worker_id),
            Some(&group_id),
            WorktreeStatus::Alive,
            false,
        );
        dashboard.worktrees = vec![record];
        dashboard.apply_response(response(initial.request_id, vec![candidate.clone()]));
        dashboard.worktrees = vec![worktree(
            &worktree_id,
            Some(&candidate.worker_id),
            Some(&group_id),
            WorktreeStatus::Alive,
            false,
        )];

        dashboard.handle_key(ModalKey::Enter);
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('s')),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::SelectCandidate(actual),
                ..
            })) if actual == worktree_id
        ));

        dashboard.pending_request = None;
        dashboard.in_flight = None;
        assert!(matches!(
            dashboard.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::ReadArtifact(actual),
                ..
            })) if actual == artifact_id
        ));
        let request_id = dashboard.pending_request.unwrap();
        dashboard.apply_response(DelegationResponse {
            request_id,
            result: Ok(DelegationPayload::Artifact(ArtifactContent {
                id: artifact_id,
                total_bytes: 64,
                truncated: false,
                text: None,
            })),
        });
        let rendered = text(&dashboard.render_budgeted(80, 20));
        assert!(
            rendered.contains("Binary or non-UTF-8 artifact"),
            "{rendered}"
        );
    }

    #[test]
    fn apply_confirmation_carries_the_exact_plan_identity_and_deny_is_non_mutating() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let plan = apply_plan();
        dashboard.detail = Some(Detail::Apply(plan.clone()));

        dashboard.handle_key(ModalKey::Char('a'));
        assert!(dashboard.confirmation.is_some());
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('n')),
            ModalOutcome::Redraw
        ));
        assert!(dashboard.pending_request.is_none());
        assert!(matches!(dashboard.detail, Some(Detail::Apply(_))));

        dashboard.handle_key(ModalKey::Char('a'));
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('y')),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::Apply {
                    plan_id,
                    digest,
                    approved_overwrites,
                    approved_escaping_symlinks,
                    skipped_paths,
                },
                ..
            })) if plan_id == plan.id
                && digest == plan.digest
                && approved_overwrites.is_empty()
                && approved_escaping_symlinks.is_empty()
                && skipped_paths.is_empty()
        ));
    }

    #[test]
    fn worktree_actions_are_legal_state_gated_and_force_remove_is_two_stage() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Worktrees);
        let initial = dashboard.request_initial().unwrap();
        let alive_id = WorktreeId::new();
        let corrupt_id = WorktreeId::new();
        let mut snapshot = DelegationSnapshot {
            workers: Vec::new(),
            worktrees: Some(vec![
                worktree(&alive_id, None, None, WorktreeStatus::Alive, false),
                worktree(&corrupt_id, None, None, WorktreeStatus::Corrupt, false),
            ]),
            events: BTreeMap::new(),
        };
        dashboard.apply_response(snapshot_response(snapshot.clone(), initial.request_id));

        dashboard.handle_key(ModalKey::Enter);
        dashboard.handle_key(ModalKey::Char('x'));
        assert!(
            dashboard
                .confirmation
                .as_ref()
                .is_some_and(|value| value.force_stage)
        );
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('x')),
            ModalOutcome::Redraw
        ));
        assert!(dashboard.confirmation.is_some());
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('y')),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::RemoveWorktree { id, force: true },
                ..
            })) if id == alive_id
        ));

        dashboard.pending_request = None;
        dashboard.in_flight = None;
        snapshot.worktrees.as_mut().unwrap().swap(0, 1);
        let refresh = dashboard
            .request_refresh(Instant::now() + WORKTREE_REFRESH)
            .unwrap();
        dashboard.apply_response(snapshot_response(snapshot, refresh.request_id));
        dashboard.detail = Some(Detail::Worktree(corrupt_id));
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('x')),
            ModalOutcome::Ignore
        ));
    }

    #[test]
    fn adopt_ignore_gc_and_rebuild_follow_their_confirmation_contracts() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Worktrees);
        let initial = dashboard.request_initial().unwrap();
        let id = WorktreeId::new();
        dashboard.apply_response(snapshot_response(
            DelegationSnapshot {
                workers: Vec::new(),
                worktrees: Some(vec![worktree(
                    &id,
                    None,
                    None,
                    WorktreeStatus::Adoptable,
                    false,
                )]),
                events: BTreeMap::new(),
            },
            initial.request_id,
        ));
        dashboard.handle_key(ModalKey::Enter);

        dashboard.handle_key(ModalKey::Char('a'));
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('y')),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::AdoptWorktree(actual),
                ..
            })) if actual == id
        ));
        dashboard.pending_request = None;
        dashboard.in_flight = None;

        dashboard.handle_key(ModalKey::Char('i'));
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('y')),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::IgnoreWorktree(actual),
                ..
            })) if actual == id
        ));
        dashboard.pending_request = None;
        dashboard.in_flight = None;
        dashboard.handle_key(ModalKey::Left);

        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('g')),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::GcWorktrees,
                ..
            }))
        ));
        dashboard.pending_request = None;
        dashboard.in_flight = None;

        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('b')),
            ModalOutcome::Redraw
        ));
        assert!(matches!(
            dashboard.handle_key(ModalKey::Char('y')),
            ModalOutcome::Emit(ModalAction::Delegation(DelegationRequest {
                kind: DelegationRequestKind::RebuildWorktrees,
                ..
            }))
        ));
    }

    #[test]
    fn late_artifact_and_plan_responses_do_not_reopen_abandoned_details() {
        let group = GroupId::new();
        let worktree_id = WorktreeId::new();
        let artifact_id = ArtifactId::new();
        let candidate = completed_worker(
            "wrk_00000000000000000000000000000001",
            group.as_str(),
            &worktree_id,
            Some(&artifact_id),
        );
        let record = worktree(
            &worktree_id,
            Some(&candidate.worker_id),
            Some(&group),
            WorktreeStatus::Alive,
            true,
        );

        let mut artifact_dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = artifact_dashboard.request_initial().unwrap();
        artifact_dashboard.apply_response(response(initial.request_id, vec![candidate.clone()]));
        artifact_dashboard.handle_key(ModalKey::Enter);
        let ModalOutcome::Emit(ModalAction::Delegation(artifact_request)) =
            artifact_dashboard.handle_key(ModalKey::Enter)
        else {
            panic!("artifact request was not emitted");
        };
        artifact_dashboard.handle_key(ModalKey::Left);
        assert!(!artifact_dashboard.apply_response(DelegationResponse {
            request_id: artifact_request.request_id,
            result: Ok(DelegationPayload::Artifact(ArtifactContent {
                id: artifact_id,
                total_bytes: 4,
                text: Some("late".to_string()),
                truncated: false,
            })),
        }));
        assert!(artifact_dashboard.detail.is_none());

        let mut plan_dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = plan_dashboard.request_initial().unwrap();
        let mut plan_response = response(initial.request_id, vec![candidate]);
        let Ok(DelegationPayload::Snapshot(snapshot)) = &mut plan_response.result else {
            unreachable!();
        };
        snapshot.worktrees = Some(vec![record]);
        plan_dashboard.apply_response(plan_response);
        plan_dashboard.handle_key(ModalKey::Enter);
        let ModalOutcome::Emit(ModalAction::Delegation(plan_request)) =
            plan_dashboard.handle_key(ModalKey::Char('p'))
        else {
            panic!("plan request was not emitted");
        };
        plan_dashboard.handle_key(ModalKey::Tab);
        assert!(!plan_dashboard.apply_response(DelegationResponse {
            request_id: plan_request.request_id,
            result: Ok(DelegationPayload::Plan(apply_plan())),
        }));
        assert!(plan_dashboard.detail.is_none());
        assert_eq!(plan_dashboard.scope, DelegationScope::Worktrees);
    }

    #[test]
    fn linked_details_restore_the_linked_row_cursor_in_both_scopes() {
        let group = GroupId::new();
        let first_worktree = WorktreeId::new();
        let second_worktree = WorktreeId::new();
        let first = completed_worker(
            "wrk_00000000000000000000000000000001",
            group.as_str(),
            &first_worktree,
            None,
        );
        let second = completed_worker(
            "wrk_00000000000000000000000000000002",
            group.as_str(),
            &second_worktree,
            None,
        );
        let records = vec![
            worktree(
                &first_worktree,
                Some(&first.worker_id),
                Some(&group),
                WorktreeStatus::Alive,
                false,
            ),
            worktree(
                &second_worktree,
                Some(&second.worker_id),
                Some(&group),
                WorktreeStatus::Alive,
                false,
            ),
        ];
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        dashboard.workers = vec![first.clone(), second.clone()];
        dashboard.worktrees = records;

        dashboard.detail = Some(Detail::Worker(second.worker_id.clone()));
        dashboard.open_linked();
        dashboard.handle_key(ModalKey::Left);
        assert_eq!(dashboard.selected_worktree().unwrap().id, second_worktree);

        dashboard.detail = Some(Detail::Worktree(first_worktree));
        dashboard.open_linked();
        dashboard.handle_key(ModalKey::Left);
        assert_eq!(
            dashboard.selected_worker().unwrap().worker_id,
            first.worker_id
        );
    }

    #[test]
    fn equal_timestamp_groups_remain_contiguous_and_sort_by_group_id() {
        let first_group = "grp_00000000000000000000000000000001";
        let second_group = "grp_00000000000000000000000000000002";
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let initial = dashboard.request_initial().unwrap();
        dashboard.apply_response(response(
            initial.request_id,
            vec![
                worker(
                    "wrk_00000000000000000000000000000001",
                    "queued",
                    "a",
                    Some(second_group),
                ),
                worker(
                    "wrk_00000000000000000000000000000002",
                    "queued",
                    "b",
                    Some(first_group),
                ),
                worker(
                    "wrk_00000000000000000000000000000003",
                    "queued",
                    "c",
                    Some(second_group),
                ),
                worker(
                    "wrk_00000000000000000000000000000004",
                    "queued",
                    "d",
                    Some(first_group),
                ),
            ],
        ));
        for worker in &dashboard.workers {
            dashboard.events.insert(
                worker.worker_id.clone(),
                vec![event(worker.worker_id.as_str(), 1)],
            );
        }

        let groups = dashboard
            .visible_worker_indices()
            .into_iter()
            .map(|index| dashboard.workers[index].group_id.as_ref().unwrap().as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            groups,
            vec![first_group, first_group, second_group, second_group]
        );
    }

    #[test]
    fn artifact_truncation_preserves_valid_utf8_at_a_split_code_point() {
        let mut bytes = vec![b'a'; ARTIFACT_DISPLAY_LIMIT - 1];
        bytes.extend_from_slice("étail".as_bytes());
        let content = artifact_content(ArtifactId::new(), bytes);

        assert!(content.truncated);
        let text = content.text.expect("valid UTF-8 remains text");
        assert_eq!(text.len(), ARTIFACT_DISPLAY_LIMIT - 1);
        assert!(text.bytes().all(|byte| byte == b'a'));

        let binary = artifact_content(ArtifactId::new(), vec![0xff]);
        assert!(binary.text.is_none());
    }

    #[test]
    fn artifact_details_distinguish_empty_binary_and_bounded_text() {
        let id = ArtifactId::new();
        let empty = artifact_lines(&ArtifactContent {
            id: id.clone(),
            total_bytes: 0,
            truncated: false,
            text: Some(String::new()),
        });
        assert!(empty.iter().any(|line| line.contains("Empty UTF-8")));

        let binary = artifact_lines(&ArtifactContent {
            id: id.clone(),
            total_bytes: 12,
            truncated: false,
            text: None,
        });
        assert!(
            binary
                .iter()
                .any(|line| line.contains("Binary or non-UTF-8"))
        );

        let bounded = artifact_lines(&ArtifactContent {
            id,
            total_bytes: ARTIFACT_DISPLAY_LIMIT + 1,
            truncated: true,
            text: Some("safe text".to_string()),
        });
        assert!(bounded[0].contains("bounded preview"));
        assert_eq!(bounded[1], "safe text");
    }

    fn snapshot_response(snapshot: DelegationSnapshot, request_id: u64) -> DelegationResponse {
        DelegationResponse {
            request_id,
            result: Ok(DelegationPayload::Snapshot(snapshot)),
        }
    }

    #[test]
    fn artifact_text_is_escape_free_before_it_reaches_rendering() {
        let cleaned = textengine::clean_text("safe\x1b[31m red\x1b[0m\x07");
        assert_eq!(cleaned, "safe red");
    }

    #[test]
    fn idle_handoff_only_requests_an_initial_snapshot_for_a_new_dashboard() {
        let mut active = DelegationDashboard::new(DelegationScope::Workers);
        let initial = active.request_initial().unwrap();
        active.apply_response(response(initial.request_id, Vec::new()));
        assert!(active.request_initial_if_needed().is_none());

        let mut idle = DelegationDashboard::new(DelegationScope::Workers);
        assert!(idle.request_initial_if_needed().is_some());
    }

    #[test]
    fn refresh_requests_coalesce_until_the_response_arrives() {
        let mut dashboard = DelegationDashboard::new(DelegationScope::Workers);
        let first = dashboard.request_initial().unwrap();
        assert!(dashboard.request_refresh(Instant::now()).is_none());
        dashboard.apply_response(response(first.request_id, Vec::new()));
        let now = Instant::now() + WORKER_REFRESH;
        let next = dashboard.request_refresh(now).unwrap();
        assert!(dashboard.request_refresh(now + WORKER_REFRESH).is_none());
        assert_eq!(dashboard.pending_request, Some(next.request_id));
    }
}
