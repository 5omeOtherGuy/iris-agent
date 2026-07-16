//! In-process harness actor for the terminal UI.
//!
//! The actor is the only owner of the mutable Wayland harness while an operation
//! is active. Terminal input and rendering stay in `tui_loop`; typed channels
//! carry commands in and runtime facts out.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::cli::ModelSwitch;
use crate::goal::{Goal, GoalCommand, GoalRuntime, GoalStatus, display_lines, now_seconds};
use crate::mimir::selection::ModelSelection;
use crate::nexus::{
    AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ChatProvider,
    CompactionLifecycleState, InteractionFuture, InteractionOutcome, PermissionMode, ReviewContext,
    ToolCall,
};
use crate::tool_display::approval_dirty_note;
use crate::ui::UiEvent;
use crate::ui::modal::ModalAction;
use crate::ui::picker::{self, ActionResult};
use crate::ui::settings_menu::{Field, PanelView, RowId, Snapshot};
use crate::ui::steering::SteeringQueue;
use crate::wayland::Harness;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveKind {
    Turn,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SteeringMode {
    Steering,
    FollowUp,
}

#[derive(Debug, Clone)]
pub(crate) enum SettingsOrigin {
    Faceplate(Option<PanelView>),
    Command,
    Shortcut,
}

impl SettingsOrigin {
    fn view(&self) -> Option<PanelView> {
        match self {
            Self::Faceplate(view) => view.clone(),
            Self::Command | Self::Shortcut => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct QueuedCounts {
    pub(crate) steering: usize,
    pub(crate) settings: usize,
    pub(crate) commands: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TaskState {
    pub(crate) workflow_enabled: bool,
    pub(crate) active_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ActorState {
    pub(crate) active_kind: Option<ActiveKind>,
    pub(crate) selection: Option<ModelSelection>,
    pub(crate) queued_counts: QueuedCounts,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) compaction_state: Option<CompactionLifecycleState>,
    pub(crate) task_state: TaskState,
    pub(crate) goal: Option<Goal>,
    pub(crate) settings: Option<Snapshot>,
    pub(crate) context_budget: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OfferedDecisions {
    pub(crate) allow_always: bool,
    pub(crate) allow_project: bool,
    pub(crate) dirty_gate: bool,
}

pub(crate) enum HarnessCommand {
    SubmitTurn {
        text: String,
    },
    RequestCompaction {
        focus: Option<String>,
    },
    CancelActive,
    Approve {
        decision: ApprovalDecision,
    },
    ResolveInteraction {
        outcome: InteractionOutcome,
    },
    ApplySettings {
        action: ModalAction,
        origin: SettingsOrigin,
    },
    QueueSteering {
        text: String,
        mode: SteeringMode,
    },
    Goal(GoalCommand),
    Delegation(crate::ui::delegation_dashboard::DelegationRequest),
    RefreshUiState,
    Shutdown,
    /// A non-steering slash command accepted while active. The TUI replays it
    /// through its ordinary idle router at the safe boundary.
    QueueCommand {
        text: String,
    },
}

pub(crate) struct SettingsResultEvent {
    pub(crate) result: ActionResult,
    pub(crate) before: Option<ModelSelection>,
    pub(crate) after: Option<ModelSelection>,
    pub(crate) context_tokens: u64,
}

pub(crate) enum HarnessEvent {
    UiEvent(UiEvent),
    TurnStarted,
    TurnFinished,
    TurnFailed(UiEvent),
    CompactionStarted,
    CompactionFinished,
    ApprovalRequested {
        offered_decisions: OfferedDecisions,
        call: ToolCall,
        reason: Option<String>,
    },
    ApprovalCleared {
        approved: bool,
    },
    InteractionRequested {
        call: ToolCall,
    },
    InteractionCleared,
    SettingsApplied {
        lines: Vec<String>,
    },
    SettingsQueued {
        label: String,
        reason: String,
        row: Option<RowId>,
    },
    PendingSettingsApplied {
        labels: Vec<String>,
    },
    ActorState(Box<ActorState>),
    SettingsResult(Box<SettingsResultEvent>),
    SettingsActionQueued {
        action: ModalAction,
    },
    Delegation(crate::ui::delegation_dashboard::DelegationResponse),
    CommandQueued(String),
}

pub(crate) struct ActiveToken {
    pub(crate) token: CancellationToken,
    pub(crate) esc_cancels: bool,
}

pub(crate) type ActiveTokenSlot = Arc<Mutex<Option<ActiveToken>>>;

pub(crate) struct ActorChannels {
    pub(crate) commands: UnboundedSender<HarnessCommand>,
    pub(crate) events: UnboundedReceiver<HarnessEvent>,
}

pub(crate) fn channels() -> (
    UnboundedReceiver<HarnessCommand>,
    UnboundedSender<HarnessEvent>,
    ActorChannels,
) {
    let (command_tx, command_rx) = unbounded_channel();
    let (event_tx, event_rx) = unbounded_channel();
    (
        command_rx,
        event_tx,
        ActorChannels {
            commands: command_tx,
            events: event_rx,
        },
    )
}

pub(crate) enum Operation {
    Turn(String),
    Compaction(Option<String>),
}

struct ApprovalRequest {
    call: ToolCall,
    offered: OfferedDecisions,
    reason: Option<String>,
    reply: oneshot::Sender<ApprovalDecision>,
}

struct InteractionRequest {
    call: ToolCall,
    reply: oneshot::Sender<InteractionOutcome>,
}

struct ActorBridge {
    event_tx: UnboundedSender<HarnessEvent>,
    approval_tx: UnboundedSender<ApprovalRequest>,
    interaction_tx: UnboundedSender<InteractionRequest>,
}

fn review_reason(call: &ToolCall, ctx: &ReviewContext) -> Option<String> {
    let mut parts = Vec::new();
    if ctx.destructive {
        parts.push("destructive".to_string());
    }
    if let Some(note) = approval_dirty_note(&ctx.dirty_paths, 96) {
        parts.push(note);
    }
    if call.name == "bash" && !crate::tools::platform_can_sandbox() {
        parts.push("unsandboxed".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

impl AgentObserver for ActorBridge {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        let _ = self
            .event_tx
            .send(HarnessEvent::UiEvent(UiEvent::from_agent_event(event)));
        Ok(())
    }
}

impl ApprovalGate for ActorBridge {
    fn review<'a>(
        &'a self,
        call: &'a ToolCall,
        allow_always: bool,
        allow_project: bool,
        ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        let call = call.clone();
        let dirty_gate = !ctx.dirty_paths.is_empty();
        let offered = OfferedDecisions {
            allow_always,
            allow_project,
            dirty_gate,
        };
        let reason = review_reason(&call, &ctx);
        let approval_tx = self.approval_tx.clone();
        Box::pin(async move {
            let (reply, rx) = oneshot::channel();
            if approval_tx
                .send(ApprovalRequest {
                    call,
                    offered,
                    reason,
                    reply,
                })
                .is_err()
            {
                return Ok(ApprovalDecision::Deny);
            }
            Ok(rx.await.unwrap_or(ApprovalDecision::Deny))
        })
    }

    fn interact<'a>(&'a self, call: &'a ToolCall) -> InteractionFuture<'a> {
        let call = call.clone();
        let interaction_tx = self.interaction_tx.clone();
        Box::pin(async move {
            let (reply, rx) = oneshot::channel();
            if interaction_tx
                .send(InteractionRequest { call, reply })
                .is_err()
            {
                return Ok(InteractionOutcome::Rejected { feedback: None });
            }
            Ok(rx
                .await
                .unwrap_or(InteractionOutcome::Rejected { feedback: None }))
        })
    }
}

fn apply_active_goal_command(goal: &GoalRuntime, command: GoalCommand) -> Vec<String> {
    let result = match command {
        GoalCommand::Show => return display_lines(goal.get().as_ref()),
        GoalCommand::Pause => goal.set_status_external(GoalStatus::Paused, now_seconds()),
        GoalCommand::Resume => goal.set_status_external(GoalStatus::Active, now_seconds()),
        GoalCommand::Clear => {
            return match goal.clear_external() {
                Ok(true) => vec!["goal cleared".to_string()],
                Ok(false) => vec!["no goal is set".to_string()],
                Err(error) => vec![format!("could not clear goal: {error:#}")],
            };
        }
        GoalCommand::Set(objective) => {
            return goal
                .create_external(&objective, None, now_seconds())
                .map(|goal| display_lines(Some(&goal)))
                .unwrap_or_else(|error| vec![format!("could not set goal: {error:#}")]);
        }
        GoalCommand::Replace(objective) => {
            return goal
                .replace_external(&objective, None, now_seconds())
                .map(|goal| display_lines(Some(&goal)))
                .unwrap_or_else(|error| vec![format!("could not replace goal: {error:#}")]);
        }
        GoalCommand::Edit => {
            return vec!["use `/goal edit` to open the goal editor".to_string()];
        }
        GoalCommand::EditValue(objective) => {
            return goal
                .edit_external(&objective, now_seconds())
                .map(|goal| display_lines(Some(&goal)))
                .unwrap_or_else(|error| vec![format!("could not edit goal: {error:#}")]);
        }
    };
    match result {
        Ok(updated) => display_lines(Some(&updated)),
        Err(error) => vec![format!("could not update goal: {error:#}")],
    }
}

fn stop_active_operation(
    pending_approval: &mut Option<oneshot::Sender<ApprovalDecision>>,
    pending_interaction: &mut Option<oneshot::Sender<InteractionOutcome>>,
    events: &UnboundedSender<HarnessEvent>,
    steering: &SteeringQueue,
    token: &CancellationToken,
) {
    if let Some(reply) = pending_approval.take() {
        let _ = reply.send(ApprovalDecision::Deny);
        let _ = events.send(HarnessEvent::ApprovalCleared { approved: false });
    }
    if let Some(reply) = pending_interaction.take() {
        let _ = reply.send(InteractionOutcome::Rejected { feedback: None });
        let _ = events.send(HarnessEvent::InteractionCleared);
    }
    steering.clear();
    token.cancel();
}

fn operation_failure_event(active_kind: ActiveKind, error: &anyhow::Error) -> HarnessEvent {
    let failure = UiEvent::from_turn_error(error);
    match active_kind {
        ActiveKind::Turn => HarnessEvent::TurnFailed(failure),
        ActiveKind::Compaction => HarnessEvent::UiEvent(failure),
    }
}

#[derive(Default)]
struct SettingsEventSink {
    events: RefCell<Vec<UiEvent>>,
}

impl SettingsEventSink {
    fn drain(&self) -> Vec<UiEvent> {
        std::mem::take(&mut self.events.borrow_mut())
    }
}

impl AgentObserver for SettingsEventSink {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.events
            .borrow_mut()
            .push(UiEvent::from_agent_event(event));
        Ok(())
    }
}

fn dispatch_delegation(
    tasks: &mut JoinSet<crate::ui::delegation_dashboard::DelegationResponse>,
    backend: Option<Arc<crate::wayland::subagents::SubagentBackend>>,
    request: crate::ui::delegation_dashboard::DelegationRequest,
) {
    tasks.spawn_blocking(move || {
        let request_id = request.request_id;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::ui::delegation_dashboard::execute_request(backend, request)
        }))
        .unwrap_or_else(|_| crate::ui::delegation_dashboard::DelegationResponse {
            request_id,
            result: Err("delegation backend task panicked".to_string()),
        })
    });
}

fn publish_delegation(
    result: std::result::Result<
        crate::ui::delegation_dashboard::DelegationResponse,
        tokio::task::JoinError,
    >,
    events: &UnboundedSender<HarnessEvent>,
) {
    match result {
        Ok(response) => {
            let _ = events.send(HarnessEvent::Delegation(response));
        }
        Err(error) => {
            let _ = events.send(HarnessEvent::UiEvent(UiEvent::Notice(format!(
                "delegation backend task failed: {error}"
            ))));
        }
    }
}

async fn finish_delegation(
    tasks: &mut JoinSet<crate::ui::delegation_dashboard::DelegationResponse>,
    events: &UnboundedSender<HarnessEvent>,
) {
    while let Some(result) = tasks.join_next().await {
        publish_delegation(result, events);
    }
}

pub(crate) struct HarnessActor<'a, 'b, P> {
    harness: &'a mut Harness<P>,
    switch: &'a mut Option<ModelSwitch<'b, P>>,
    commands: UnboundedReceiver<HarnessCommand>,
    events: UnboundedSender<HarnessEvent>,
    steering: Rc<SteeringQueue>,
    token_slot: ActiveTokenSlot,
    pending_settings: Vec<(ModalAction, SettingsOrigin, String)>,
    queued_commands: usize,
}

impl<'a, 'b, P: ChatProvider> HarnessActor<'a, 'b, P> {
    pub(crate) fn new(
        harness: &'a mut Harness<P>,
        switch: &'a mut Option<ModelSwitch<'b, P>>,
        commands: UnboundedReceiver<HarnessCommand>,
        events: UnboundedSender<HarnessEvent>,
        steering: Rc<SteeringQueue>,
        token_slot: ActiveTokenSlot,
    ) -> Self {
        Self {
            harness,
            switch,
            commands,
            events,
            steering,
            token_slot,
            pending_settings: Vec::new(),
            queued_commands: 0,
        }
    }

    pub(crate) async fn run(mut self) -> Result<bool> {
        let operation = loop {
            match self.commands.recv().await {
                Some(HarnessCommand::SubmitTurn { text }) if !text.trim().is_empty() => {
                    break Operation::Turn(text);
                }
                Some(HarnessCommand::RequestCompaction { focus }) => {
                    break Operation::Compaction(focus);
                }
                Some(HarnessCommand::Shutdown) | None => return Ok(false),
                Some(HarnessCommand::RefreshUiState) => {
                    let _ = self
                        .events
                        .send(HarnessEvent::ActorState(Box::new(self.state(None))));
                }
                Some(_) => {}
            }
        };
        crate::signals::reset();
        let token = CancellationToken::new();
        *self.token_slot.lock().expect("turn token lock poisoned") = Some(ActiveToken {
            token: token.clone(),
            esc_cancels: true,
        });

        let active_kind = match operation {
            Operation::Turn(_) => ActiveKind::Turn,
            Operation::Compaction(_) => ActiveKind::Compaction,
        };
        let _ = self.events.send(match active_kind {
            ActiveKind::Turn => HarnessEvent::TurnStarted,
            ActiveKind::Compaction => HarnessEvent::CompactionStarted,
        });
        let mut active_state = self.state(Some(active_kind));
        let goal = self.harness.goal_runtime();
        let workspace = self.harness.workspace().to_path_buf();
        let subagent_backend = self.harness.subagent_backend().ok().cloned();
        let _ = self
            .events
            .send(HarnessEvent::ActorState(Box::new(active_state.clone())));

        let (approval_tx, mut approval_rx) = unbounded_channel();
        let (interaction_tx, mut interaction_rx) = unbounded_channel();
        let bridge = ActorBridge {
            event_tx: self.events.clone(),
            approval_tx,
            interaction_tx,
        };
        let mut pending_approval: Option<oneshot::Sender<ApprovalDecision>> = None;
        let mut pending_interaction: Option<oneshot::Sender<InteractionOutcome>> = None;
        let mut delegation_tasks = JoinSet::new();

        let result = {
            let mut operation_future: futures::future::LocalBoxFuture<'_, Result<()>> =
                match &operation {
                    Operation::Turn(text) => Box::pin(async {
                        self.harness
                            .submit_turn(text, &bridge, &bridge, &token)
                            .await
                            .map(|_| ())
                    }),
                    Operation::Compaction(focus) => Box::pin(self.harness.compact_now_with_focus(
                        &bridge,
                        &token,
                        focus.as_deref().filter(|focus| !focus.is_empty()),
                    )),
                };
            loop {
                tokio::select! {
                    biased;
                    Some(result) = delegation_tasks.join_next(), if !delegation_tasks.is_empty() => {
                        publish_delegation(result, &self.events);
                    }
                    result = &mut operation_future => break result,
                    Some(request) = approval_rx.recv() => {
                        if let Some(previous) = pending_approval.replace(request.reply) {
                            let _ = previous.send(ApprovalDecision::Deny);
                        }
                        let _ = self.events.send(HarnessEvent::ApprovalRequested {
                            offered_decisions: request.offered,
                            call: request.call,
                            reason: request.reason,
                        });
                    }
                    Some(request) = interaction_rx.recv() => {
                        if let Some(previous) = pending_interaction.replace(request.reply) {
                            let _ = previous.send(InteractionOutcome::Rejected { feedback: None });
                        }
                        let _ = self.events.send(HarnessEvent::InteractionRequested {
                            call: request.call,
                        });
                    }
                    Some(command) = self.commands.recv() => {
                        match command {
                            HarnessCommand::Approve { decision } => {
                                if let Some(reply) = pending_approval.take() {
                                    let approved = matches!(
                                        decision,
                                        ApprovalDecision::Allow
                                            | ApprovalDecision::AllowAlways
                                            | ApprovalDecision::AllowProject
                                    );
                                    let _ = reply.send(decision);
                                    let _ = self
                                        .events
                                        .send(HarnessEvent::ApprovalCleared { approved });
                                }
                            }
                            HarnessCommand::ResolveInteraction { outcome } => {
                                if let Some(reply) = pending_interaction.take() {
                                    let _ = reply.send(outcome);
                                    let _ = self.events.send(HarnessEvent::InteractionCleared);
                                }
                            }
                            HarnessCommand::CancelActive | HarnessCommand::Shutdown => {
                                stop_active_operation(
                                    &mut pending_approval,
                                    &mut pending_interaction,
                                    &self.events,
                                    &self.steering,
                                    &token,
                                );
                            }
                            HarnessCommand::Delegation(request) => {
                                dispatch_delegation(
                                    &mut delegation_tasks,
                                    subagent_backend.clone(),
                                    request,
                                );
                            }
                            HarnessCommand::Goal(command) => {
                                for line in apply_active_goal_command(&goal, command) {
                                    let _ = self.events.send(HarnessEvent::UiEvent(UiEvent::Notice(line)));
                                }
                                active_state.goal = goal.get();
                                let _ = self.events.send(HarnessEvent::ActorState(Box::new(active_state.clone())));
                            }
                            HarnessCommand::QueueSteering { text, mode } => {
                                if !text.trim().is_empty() && active_kind == ActiveKind::Turn {
                                    match mode {
                                        SteeringMode::Steering => self.steering.enqueue_steering(text),
                                        SteeringMode::FollowUp => self.steering.enqueue_follow_up(text),
                                    }
                                }
                                active_state.queued_counts.steering = self.steering.len();
                            }
                            HarnessCommand::ApplySettings { action, origin } => {
                                let label = settings_label(&action);
                                let row = settings_row(&action);
                                if let ModalAction::SaveSetting { field, value } = &action
                                    && immediate_during_active(*field)
                                {
                                    let lines = match picker::persist_setting_field(
                                        *field,
                                        value.as_deref(),
                                        &workspace,
                                    ) {
                                        Ok(()) => Vec::new(),
                                        Err(error) => vec![format!("could not save setting: {error:#}")],
                                    };
                                    let _ = self.events.send(HarnessEvent::SettingsApplied { lines });
                                } else if tui_owned_action(&action) {
                                    active_state.queued_counts.settings += 1;
                                    let _ = self.events.send(HarnessEvent::SettingsQueued {
                                        label: label.clone(),
                                        reason: "applies when the active operation reaches a safe boundary"
                                            .to_string(),
                                        row,
                                    });
                                    let _ = self
                                        .events
                                        .send(HarnessEvent::SettingsActionQueued { action });
                                } else {
                                    self.pending_settings.push((action, origin, label.clone()));
                                    active_state.queued_counts.settings += 1;
                                    let _ = self.events.send(HarnessEvent::SettingsQueued {
                                        label,
                                        reason: "applies when the active operation reaches a safe boundary".to_string(),
                                        row,
                                    });
                                }
                            }
                            HarnessCommand::QueueCommand { text } => {
                                self.queued_commands += 1;
                                active_state.queued_counts.commands = self.queued_commands;
                                let _ = self.events.send(HarnessEvent::CommandQueued(text));
                            }
                            HarnessCommand::RequestCompaction { .. } => {
                                let _ = self.events.send(HarnessEvent::UiEvent(UiEvent::Notice(
                                    "cannot compact during an active operation; wait for it to finish"
                                        .to_string(),
                                )));
                            }
                            HarnessCommand::RefreshUiState => {
                                let _ = self.events.send(HarnessEvent::ActorState(Box::new(active_state.clone())));
                            }
                            HarnessCommand::SubmitTurn { text } => {
                                if !text.trim().is_empty() {
                                    self.steering.enqueue_follow_up(text);
                                }
                            }
                        }
                    }
                }
            }
        };
        while let Ok(command) = self.commands.try_recv() {
            if let HarnessCommand::Delegation(request) = command {
                dispatch_delegation(&mut delegation_tasks, subagent_backend.clone(), request);
            }
        }
        finish_delegation(&mut delegation_tasks, &self.events).await;
        let result = result.and(self.harness.persist_goal());

        if let Some(reply) = pending_approval.take() {
            let _ = reply.send(ApprovalDecision::Deny);
            let _ = self
                .events
                .send(HarnessEvent::ApprovalCleared { approved: false });
        }
        if let Some(reply) = pending_interaction.take() {
            let _ = reply.send(InteractionOutcome::Rejected { feedback: None });
            let _ = self.events.send(HarnessEvent::InteractionCleared);
        }
        *self.token_slot.lock().expect("turn token lock poisoned") = None;
        if token.is_cancelled() || active_kind == ActiveKind::Compaction {
            self.steering.clear();
        }

        let succeeded = match result {
            Ok(()) => true,
            Err(error) => {
                let _ = self
                    .events
                    .send(operation_failure_event(active_kind, &error));
                false
            }
        };

        self.apply_pending_settings();
        let _ = self
            .events
            .send(HarnessEvent::ActorState(Box::new(self.state(None))));
        let _ = self.events.send(match active_kind {
            ActiveKind::Turn => HarnessEvent::TurnFinished,
            ActiveKind::Compaction => HarnessEvent::CompactionFinished,
        });
        Ok(succeeded)
    }

    fn apply_pending_settings(&mut self) {
        if self.pending_settings.is_empty() {
            return;
        }
        let mut labels = Vec::new();
        for (action, origin, label) in self.pending_settings.drain(..) {
            let Some(switch) = self.switch.as_mut() else {
                let _ = self.events.send(HarnessEvent::SettingsApplied {
                    lines: vec![format!(
                        "could not apply queued {label}: model switching is unavailable"
                    )],
                });
                continue;
            };
            let before = switch.selection().clone();
            let sink = SettingsEventSink::default();
            let result = picker::apply_action(action, origin.view(), self.harness, switch, &sink);
            for event in sink.drain() {
                let _ = self.events.send(HarnessEvent::UiEvent(event));
            }
            let after = switch.selection().clone();
            labels.push(label);
            let _ = self.events.send(HarnessEvent::SettingsResult(Box::new(
                SettingsResultEvent {
                    result,
                    before: Some(before),
                    after: Some(after),
                    context_tokens: self.harness.context_token_estimate(),
                },
            )));
        }
        let _ = self
            .events
            .send(HarnessEvent::PendingSettingsApplied { labels });
    }

    fn state(&self, active_kind: Option<ActiveKind>) -> ActorState {
        let permission_mode = if self.harness.skip_permissions() {
            PermissionMode::DangerousSkipPermissions
        } else {
            PermissionMode::Approval(self.harness.approval_mode())
        };
        let selection = self
            .switch
            .as_ref()
            .map(|switch| switch.selection().clone());
        let settings = self
            .switch
            .as_ref()
            .map(|switch| picker::settings_snapshot(self.harness, switch));
        ActorState {
            active_kind,
            selection,
            queued_counts: QueuedCounts {
                steering: self.steering.len(),
                settings: self.pending_settings.len(),
                commands: self.queued_commands,
            },
            permission_mode,
            compaction_state: active_kind
                .filter(|kind| *kind == ActiveKind::Compaction)
                .map(|_| CompactionLifecycleState::Running),
            task_state: TaskState {
                workflow_enabled: self.harness.task_workflow_enabled(),
                active_id: self.harness.current_task_id(),
            },
            goal: self.harness.goal(),
            settings,
            context_budget: self.harness.context_budget(),
        }
    }
}

fn tui_owned_action(action: &ModalAction) -> bool {
    matches!(
        action,
        ModalAction::SetNativeJj(_)
            | ModalAction::ResumeSession(_)
            | ModalAction::AdoptTask(_)
            | ModalAction::ViewTaskSessions(_)
            | ModalAction::AcceptTask
            | ModalAction::ShowTaskDiff
            | ModalAction::ListTaskRollback
            | ModalAction::BeginLogin(_)
            | ModalAction::OpenApiKeyDialog(_)
            | ModalAction::SaveApiKey(_)
            | ModalAction::Logout(_)
    )
}

fn settings_row(action: &ModalAction) -> Option<RowId> {
    match action {
        ModalAction::SelectModel { .. }
        | ModalAction::ConfirmModelSwitch { .. }
        | ModalAction::CycleModel { .. } => Some(RowId::Model),
        ModalAction::AdjustEffort(_) => Some(RowId::Reasoning),
        ModalAction::ApplyScoped(_) | ModalAction::SaveScoped(_) => Some(RowId::Scope),
        ModalAction::SaveSetting { field, .. } => Some(RowId::Field(*field)),
        ModalAction::ToggleSkipPermissions => Some(RowId::SkipApprovals),
        ModalAction::SetNativeJj(_) => Some(RowId::Field(Field::NativeJj)),
        ModalAction::EditPolicy(_) => Some(RowId::Permissions),
        ModalAction::BeginLogin(_)
        | ModalAction::OpenApiKeyDialog(_)
        | ModalAction::SaveApiKey(_)
        | ModalAction::Logout(_) => Some(RowId::Providers),
        ModalAction::ResumeSession(_)
        | ModalAction::AdoptTask(_)
        | ModalAction::ViewTaskSessions(_)
        | ModalAction::AcceptTask
        | ModalAction::ShowTaskDiff
        | ModalAction::ListTaskRollback
        | ModalAction::InsertSkillMention { .. }
        | ModalAction::ReplaceGoal(_)
        | ModalAction::EditGoal(_)
        | ModalAction::ResolveUserQuestion(_)
        | ModalAction::Delegation(_) => None,
    }
}

fn settings_label(action: &ModalAction) -> String {
    match action {
        ModalAction::SelectModel { .. }
        | ModalAction::ConfirmModelSwitch { .. }
        | ModalAction::CycleModel { .. } => "model switch".to_string(),
        ModalAction::AdjustEffort(_) => "reasoning switch".to_string(),
        ModalAction::Logout(_) | ModalAction::BeginLogin(_) | ModalAction::SaveApiKey(_) => {
            "provider change".to_string()
        }
        ModalAction::SaveSetting { field, .. } => format!("{field:?} setting"),
        ModalAction::ResumeSession(_) => "session switch".to_string(),
        ModalAction::AcceptTask | ModalAction::AdoptTask(_) => "task settlement".to_string(),
        _ => "settings change".to_string(),
    }
}

fn immediate_during_active(field: Field) -> bool {
    matches!(
        field,
        Field::AltScreen
            | Field::ScrollSpeed
            | Field::ReducedMotion
            | Field::PromptCacheRetention
            | Field::WebSearchBackend
            | Field::ReadWebPageBackend
            | Field::SearxngUrl
            | Field::SearchTimeout
            | Field::ReadTimeout
            | Field::MaxSearchResults
            | Field::MaxSearchResponseBytes
            | Field::MaxReadResponseBytes
            | Field::MaxReadOutputBytes
            | Field::VerifyCommand
            | Field::VerifyMaxAttempts
            | Field::WorktreeRoot
            | Field::Theme
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_goal_replacement_still_requires_a_confirmed_command() {
        let runtime = GoalRuntime::new(Some(Goal::new_at("old", None, 1).unwrap()), true);
        let lines = apply_active_goal_command(&runtime, GoalCommand::Set("new".to_string()));
        assert!(lines[0].contains("unfinished goal already exists"));
        assert_eq!(runtime.get().unwrap().objective, "old");

        apply_active_goal_command(&runtime, GoalCommand::Replace("new".to_string()));
        assert_eq!(runtime.get().unwrap().objective, "new");
    }

    #[test]
    fn active_goal_controls_apply_without_becoming_model_steering() {
        let runtime = GoalRuntime::new(Some(Goal::new_at("ship", None, 1).unwrap()), true);
        let lines = apply_active_goal_command(&runtime, GoalCommand::Pause);
        assert!(lines[0].contains("paused"));
        assert_eq!(runtime.get().unwrap().status, GoalStatus::Paused);
        let lines = apply_active_goal_command(&runtime, GoalCommand::Resume);
        assert!(lines[0].contains("active"));
        assert_eq!(runtime.get().unwrap().status, GoalStatus::Active);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn eof_shutdown_denies_parked_approval_and_cancels_the_operation() {
        let (event_tx, mut event_rx) = unbounded_channel();
        let (approval_tx, mut approval_rx) = unbounded_channel();
        let (interaction_tx, _interaction_rx) = unbounded_channel();
        let bridge = ActorBridge {
            event_tx,
            approval_tx,
            interaction_tx,
        };
        let call = ToolCall {
            id: "call-1".to_string(),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": "echo hi" }),
        };
        let token = CancellationToken::new();
        let steering = SteeringQueue::default();

        let review = bridge.review(&call, true, true, ReviewContext::default());
        let shutdown = async {
            let request = approval_rx.recv().await.expect("approval request");
            let mut pending = Some(request.reply);
            let mut pending_interaction = None;
            stop_active_operation(
                &mut pending,
                &mut pending_interaction,
                &bridge.event_tx,
                &steering,
                &token,
            );
            assert!(pending.is_none());
        };
        let (decision, ()) = tokio::join!(review, shutdown);

        assert_eq!(decision.unwrap(), ApprovalDecision::Deny);
        assert!(token.is_cancelled());
        assert!(matches!(
            event_rx.try_recv(),
            Ok(HarnessEvent::ApprovalCleared { approved: false })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interaction_bridge_routes_submission_and_cancellation() {
        let (event_tx, mut event_rx) = unbounded_channel();
        let (approval_tx, _approval_rx) = unbounded_channel();
        let (interaction_tx, mut interaction_rx) = unbounded_channel();
        let bridge = ActorBridge {
            event_tx,
            approval_tx,
            interaction_tx,
        };
        let call = ToolCall {
            id: "call-question".to_string(),
            thought_signature: None,
            name: "AskUserQuestion".to_string(),
            arguments: serde_json::json!({"questions": []}),
        };
        let interaction = bridge.interact(&call);
        let submitted = serde_json::json!({"answers": {"q": "a"}});
        let respond = async {
            let request = interaction_rx.recv().await.expect("interaction request");
            assert_eq!(request.call.name, "AskUserQuestion");
            request
                .reply
                .send(InteractionOutcome::Submitted(submitted.clone()))
                .expect("submit response");
        };
        let (outcome, ()) = tokio::join!(interaction, respond);
        assert_eq!(outcome.unwrap(), InteractionOutcome::Submitted(submitted));

        let cancelled = bridge.interact(&call);
        let cancel = async {
            let request = interaction_rx.recv().await.expect("interaction request");
            let mut pending_interaction = Some(request.reply);
            let mut pending_approval = None;
            stop_active_operation(
                &mut pending_approval,
                &mut pending_interaction,
                &bridge.event_tx,
                &SteeringQueue::default(),
                &CancellationToken::new(),
            );
        };
        let (outcome, ()) = tokio::join!(cancelled, cancel);
        assert_eq!(
            outcome.unwrap(),
            InteractionOutcome::Rejected { feedback: None }
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(HarnessEvent::InteractionCleared)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delegation_dispatch_is_drained_before_the_actor_boundary() {
        let (event_tx, mut event_rx) = unbounded_channel();
        let mut tasks = tokio::task::JoinSet::new();
        dispatch_delegation(
            &mut tasks,
            None,
            crate::ui::delegation_dashboard::DelegationRequest {
                request_id: 7,
                kind: crate::ui::delegation_dashboard::DelegationRequestKind::GcWorktrees,
            },
        );

        finish_delegation(&mut tasks, &event_tx).await;
        assert!(tasks.is_empty());
        event_tx.send(HarnessEvent::TurnFinished).unwrap();
        let event = event_rx
            .try_recv()
            .expect("delegation response was published before boundary completion");
        assert!(matches!(
            event,
            HarnessEvent::Delegation(crate::ui::delegation_dashboard::DelegationResponse {
                request_id: 7,
                result: Err(message),
            }) if message.contains("not configured")
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(HarnessEvent::TurnFinished)
        ));
    }

    #[test]
    fn turn_failures_preserve_typed_auth_errors() {
        let error = anyhow::Error::new(crate::errors::AuthError::new("expired token"));
        assert!(matches!(
            operation_failure_event(ActiveKind::Turn, &error),
            HarnessEvent::TurnFailed(UiEvent::TurnError {
                kind: crate::ui::TurnErrorKind::Auth,
                ..
            })
        ));
    }

    #[test]
    fn active_setting_classification_keeps_runtime_mutations_for_the_boundary() {
        assert!(immediate_during_active(Field::ReducedMotion));
        assert!(immediate_during_active(Field::ScrollSpeed));
        assert!(!immediate_during_active(Field::MutationSafety));
        assert!(!immediate_during_active(Field::CompactionEnabled));
    }

    #[test]
    fn provider_session_and_task_actions_return_to_tui_at_boundary() {
        assert!(tui_owned_action(&ModalAction::BeginLogin(
            crate::mimir::selection::ProviderId::Anthropic
        )));
        assert!(tui_owned_action(&ModalAction::Logout(
            "anthropic".to_string()
        )));
        assert!(tui_owned_action(&ModalAction::ResumeSession(
            "session-1".to_string()
        )));
        assert!(tui_owned_action(&ModalAction::AcceptTask));
        assert!(!tui_owned_action(&ModalAction::AdjustEffort(
            crate::mimir::selection::ReasoningEffort::High
        )));
    }

    #[test]
    fn model_and_reasoning_actions_have_visible_queue_labels_and_rows() {
        let reasoning = ModalAction::AdjustEffort(crate::mimir::selection::ReasoningEffort::High);
        let model = ModalAction::CycleModel { forward: true };
        assert_eq!(settings_label(&reasoning), "reasoning switch");
        assert_eq!(settings_row(&reasoning), Some(RowId::Reasoning));
        assert_eq!(settings_label(&model), "model switch");
        assert_eq!(settings_row(&model), Some(RowId::Model));
    }
}
