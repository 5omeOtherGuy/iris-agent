//! Background compaction job lifecycle used by the context governor.

use super::*;

/// Which worker [`CompactionEngine::start_background`] resolved to launch,
/// decided before any engine state mutates so a session with no usable worker
/// degrades cleanly.
enum WorkerSpawn {
    Native(SummarizerFactory),
    Portable(SummarizerFactory),
}

/// Outcome of the provider-native rung of the hard-tier fallback ladder
/// (subagent -> provider-native -> deterministic excerpts).
enum NativeFallbackOutcome {
    /// Provider-native compaction shrank and was applied; carry the rewrite.
    Applied(Vec<Message>),
    /// The turn was cancelled mid-fallback; apply nothing.
    Cancelled,
    /// Provider-native was unsupported, failed, or did not shrink; the caller
    /// falls through to the deterministic-excerpts terminal rung.
    Unavailable,
}

impl CompactionEngine {
    pub(super) fn apply_excerpts(
        &mut self,
        messages: &[Message],
        plan: CompactionPlan,
        cx: ApplyContext<'_>,
    ) -> Result<ContextDirective> {
        let summary = CompactionSummary::excerpts(summarize(&messages[plan.start..plan.end]));
        Ok(match self.apply_summary(messages, plan, summary, cx)? {
            Some((_, messages)) => ContextDirective::Replace { messages },
            None => ContextDirective::Proceed,
        })
    }

    pub(super) fn emit_lifecycle(
        &self,
        obs: &dyn AgentObserver,
        job: &BackgroundCompaction,
        state: CompactionLifecycleState,
        worker_usage: Option<ProviderUsage>,
        message: Option<String>,
    ) -> Result<()> {
        emit_context_event(
            obs,
            AgentEvent::CompactionLifecycle {
                job_id: job.job_id.clone(),
                state,
                covered_messages: job.covered_messages,
                original_tokens_estimate: job.original_tokens,
                origin: job.origin,
                worker_usage,
                trigger_tier: job.trigger_tier,
                message,
            },
            "compaction lifecycle",
        );
        Ok(())
    }

    pub(super) fn emit_breaker_notice(&mut self, obs: &dyn AgentObserver) -> Result<()> {
        if self.consecutive_failures < self.max_consecutive_failures || self.breaker_notice_emitted
        {
            return Ok(());
        }
        self.breaker_notice_emitted = true;
        emit_context_event(
            obs,
            AgentEvent::Notice(format!(
                "background compaction disabled after {} consecutive failures; deterministic compaction remains active.",
                self.consecutive_failures
            )),
            "compaction breaker notice",
        );
        Ok(())
    }

    pub(super) fn start_background(
        &mut self,
        messages: &[Message],
        plan: CompactionPlan,
        workspace: &Path,
        obs: &dyn AgentObserver,
        trigger_tier: Option<ContextPressureTier>,
    ) -> Result<BackgroundStart> {
        let covered = messages[plan.start..plan.end].to_vec();
        let covered_messages = covered.len();
        let original_tokens = context_tokens(&covered);
        let native_factory = if self.provider_native {
            self.provider_compaction_factory
                .as_ref()
                .and_then(|factory| match factory() {
                    Ok(provider)
                        if provider.compaction_capability(original_tokens)
                            == ProviderCompactionCapability::OpaqueBlocks =>
                    {
                        Some(factory.clone())
                    }
                    Ok(_) => None,
                    Err(error) => {
                        tracing::warn!(
                            error = %format!("{error:#}"),
                            "provider-native compaction probe failed; using the portable worker"
                        );
                        None
                    }
                })
        } else {
            None
        };
        // A portable worker only runs for a model-backed summarizer kind. An
        // Excerpts kind never spawns a worker even when a factory is installed;
        // relief comes from the deterministic backstop instead.
        let portable_factory =
            if native_factory.is_none() && self.summarizer != SummarizerKind::Excerpts {
                self.summarizer_factory.clone()
            } else {
                None
            };
        // `has_model_worker()` trusts the native factory's presence, not its live
        // capability, so it can race this probe: neither a usable native
        // capability nor a portable summarizer may exist here. Report "no
        // worker" so the caller degrades to the deterministic backstop instead
        // of panicking.
        let spawn = match (native_factory, portable_factory) {
            (Some(factory), _) => WorkerSpawn::Native(factory),
            (None, Some(factory)) => WorkerSpawn::Portable(factory),
            (None, None) => return Ok(BackgroundStart::NoWorker),
        };
        let origin = match &spawn {
            WorkerSpawn::Native(_) => CompactionOrigin::ProviderNative,
            WorkerSpawn::Portable(_) => match self.summarizer {
                SummarizerKind::Subagent => CompactionOrigin::Subagent,
                SummarizerKind::Provider => CompactionOrigin::Provider,
                SummarizerKind::Excerpts => CompactionOrigin::Excerpts,
            },
        };
        let job_id = format!("compaction_{:08x}", self.next_job_seq);
        self.next_job_seq = self.next_job_seq.saturating_add(1);
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let worker = self.worker.clone();
        let (tx, receiver) = mpsc::channel();
        match spawn {
            WorkerSpawn::Native(factory) => {
                thread::Builder::new()
                    .name(format!("iris-{job_id}"))
                    .spawn(move || {
                        let result =
                            run_provider_native_worker(factory, covered, worker, worker_token);
                        let _ = tx.send(result);
                    })?;
            }
            WorkerSpawn::Portable(factory) => {
                let workspace_for_worker = workspace.to_path_buf();
                let mode = self.summarizer;
                thread::Builder::new()
                    .name(format!("iris-{job_id}"))
                    .spawn(move || {
                        let result = run_compaction_worker(
                            factory,
                            workspace_for_worker,
                            covered,
                            worker,
                            mode,
                            worker_token,
                        );
                        let _ = tx.send(result);
                    })?;
            }
        }
        let job = BackgroundCompaction {
            job_id,
            session_id: self.session_id().map(str::to_string),
            from_id: plan.from_id,
            to_id: plan.to_id,
            covered_messages,
            original_tokens,
            receiver,
            token,
            origin,
            trigger_tier,
            started_at: std::time::Instant::now(),
            selection_generation: self.selection_generation,
        };
        self.emit_lifecycle(
            obs,
            &job,
            CompactionLifecycleState::Running,
            None,
            Some(format!(
                "background compaction running for {covered_messages} message(s), ~{original_tokens} tokens"
            )),
        )?;
        self.background = Some(job);
        Ok(BackgroundStart::Started)
    }

    pub(super) fn drain_background_at_boundary(
        &mut self,
        messages: &[Message],
        cx: ApplyContext<'_>,
    ) -> Result<Option<Vec<Message>>> {
        let Some(job) = self.background.as_ref() else {
            return Ok(None);
        };
        match job.receiver.try_recv() {
            Ok(result) => {
                let job = self.background.take().expect("checked above");
                // Non-blocking drain at a continuing boundary: no provider-native
                // rung here (it blocks and is reserved for hard pressure).
                self.finish_background_at_boundary(job, result, messages, cx, None)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                let job = self.background.take().expect("checked above");
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.emit_lifecycle(
                    cx.observer,
                    &job,
                    CompactionLifecycleState::Failed,
                    None,
                    Some(
                        "background compaction worker stopped before returning a summary"
                            .to_string(),
                    ),
                )?;
                self.apply_job_fallback(&job, messages, cx, None)
            }
        }
    }

    pub(super) async fn resolve_hard_at_boundary(
        &mut self,
        messages: &[Message],
        cx: ApplyContext<'_>,
        token: &CancellationToken,
    ) -> Result<Option<Vec<Message>>> {
        let Some(job) = self.background.take() else {
            return Ok(None);
        };
        if token.is_cancelled() {
            job.token.cancel();
            self.emit_lifecycle(
                cx.observer,
                &job,
                CompactionLifecycleState::Cancelled,
                None,
                Some("background compaction cancelled with the turn".to_string()),
            )?;
            return Ok(None);
        }
        let job_id = job.job_id.clone();
        let covered_messages = job.covered_messages;
        let original_tokens = job.original_tokens;
        let origin = job.origin;
        let trigger_tier = job.trigger_tier;
        let worker_token = job.token.clone();
        let hard_wait = self.hard_wait;
        let waiter = tokio::task::spawn_blocking(move || {
            let result = job.receiver.recv_timeout(hard_wait);
            (job, result)
        });
        let joined = tokio::select! {
            biased;
            _ = token.cancelled() => {
                worker_token.cancel();
                emit_context_event(
                    cx.observer,
                    AgentEvent::CompactionLifecycle {
                        job_id,
                        state: CompactionLifecycleState::Cancelled,
                        covered_messages,
                        original_tokens_estimate: original_tokens,
                        origin,
                        worker_usage: None,
                        trigger_tier,
                        message: Some("background compaction cancelled with the turn".to_string()),
                    },
                    "compaction lifecycle",
                );
                return Ok(None);
            }
            joined = waiter => joined,
        };
        let (job, result) = match joined {
            Ok(joined) => joined,
            Err(error) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                emit_context_event(
                    cx.observer,
                    AgentEvent::CompactionLifecycle {
                        job_id,
                        state: CompactionLifecycleState::Failed,
                        covered_messages,
                        original_tokens_estimate: original_tokens,
                        origin,
                        worker_usage: None,
                        trigger_tier,
                        message: Some(format!("background hard-wait task failed: {error}")),
                    },
                    "compaction lifecycle",
                );
                return Ok(None);
            }
        };
        match result {
            Ok(result) => {
                self.finish_background_at_boundary(job, result, messages, cx, Some(token))
            }
            Err(RecvTimeoutError::Timeout) => {
                job.token.cancel();
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.emit_lifecycle(
                    cx.observer,
                    &job,
                    CompactionLifecycleState::Cancelled,
                    None,
                    Some(format!(
                        "background compaction summary exceeded the {} ms hard wait; escalating fallback",
                        self.hard_wait.as_millis()
                    )),
                )?;
                self.apply_job_fallback(&job, messages, cx, Some(token))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.emit_lifecycle(
                    cx.observer,
                    &job,
                    CompactionLifecycleState::Failed,
                    None,
                    Some(
                        "background compaction worker stopped before returning a summary"
                            .to_string(),
                    ),
                )?;
                self.apply_job_fallback(&job, messages, cx, Some(token))
            }
        }
    }

    fn finish_background_at_boundary(
        &mut self,
        job: BackgroundCompaction,
        result: BackgroundSummaryResult,
        messages: &[Message],
        cx: ApplyContext<'_>,
        native: Option<&CancellationToken>,
    ) -> Result<Option<Vec<Message>>> {
        match result {
            BackgroundSummaryResult::Summary(summary) => {
                let usage = summary.worker_usage.clone();
                self.emit_lifecycle(
                    cx.observer,
                    &job,
                    CompactionLifecycleState::Ready,
                    usage.clone(),
                    Some("background compaction summary ready".to_string()),
                )?;
                if self.model_compaction_cap_reached(summary.origin) {
                    self.emit_lifecycle(
                        cx.observer,
                        &job,
                        CompactionLifecycleState::Discarded,
                        usage,
                        Some(
                            "per-turn model-backed compaction cap reached; using deterministic fallback"
                                .to_string(),
                        ),
                    )?;
                    return self.apply_job_fallback(&job, messages, cx, native);
                }
                let Some(plan) = self.revalidate(&job, messages) else {
                    self.emit_lifecycle(
                        cx.observer,
                        &job,
                        CompactionLifecycleState::Discarded,
                        usage,
                        Some(
                            "background compaction result was stale; keeping current context"
                                .to_string(),
                        ),
                    )?;
                    return Ok(None);
                };
                match self.apply_summary(messages, plan, summary, cx)? {
                    Some((_, replacement)) => {
                        self.consecutive_failures = 0;
                        self.breaker_notice_emitted = false;
                        self.emit_lifecycle(
                            cx.observer,
                            &job,
                            CompactionLifecycleState::Applied,
                            usage,
                            Some("background compaction summary applied".to_string()),
                        )?;
                        Ok(Some(replacement))
                    }
                    None => {
                        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                        self.emit_lifecycle(
                            cx.observer,
                            &job,
                            CompactionLifecycleState::Discarded,
                            usage,
                            Some(
                                "background compaction summary did not shrink; using deterministic fallback"
                                    .to_string(),
                            ),
                        )?;
                        self.apply_job_fallback(&job, messages, cx, native)
                    }
                }
            }
            BackgroundSummaryResult::Failed(message) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.emit_lifecycle(
                    cx.observer,
                    &job,
                    CompactionLifecycleState::Failed,
                    None,
                    Some(format!(
                        "background compaction failed; using deterministic fallback: {message}"
                    )),
                )?;
                self.apply_job_fallback(&job, messages, cx, native)
            }
            BackgroundSummaryResult::Cancelled => {
                self.emit_lifecycle(
                    cx.observer,
                    &job,
                    CompactionLifecycleState::Cancelled,
                    None,
                    Some("background compaction cancelled".to_string()),
                )?;
                Ok(None)
            }
        }
    }

    fn apply_job_fallback(
        &mut self,
        job: &BackgroundCompaction,
        messages: &[Message],
        cx: ApplyContext<'_>,
        native: Option<&CancellationToken>,
    ) -> Result<Option<Vec<Message>>> {
        // Hard-tier fallback ladder: prefer provider-native compaction before
        // the lossy deterministic-excerpts terminal rung. Blocking is accepted
        // here because this path only runs at hard pressure; the wait is bounded
        // and cancellable through the turn token.
        //
        // Only portable-origin failures (Subagent/Provider) escalate to the
        // native rung. A job that was ALREADY ProviderNative origin and failed,
        // timed out, or did not shrink must not fire a second identical
        // provider-native request; it drops straight to the excerpts rung.
        if let Some(token) = native
            && matches!(
                job.origin,
                CompactionOrigin::Subagent | CompactionOrigin::Provider
            )
        {
            match self.try_provider_native_fallback(job, messages, cx, token)? {
                NativeFallbackOutcome::Applied(replacement) => return Ok(Some(replacement)),
                NativeFallbackOutcome::Cancelled => return Ok(None),
                NativeFallbackOutcome::Unavailable => {}
            }
        }
        let Some(plan) = self.revalidate(job, messages) else {
            self.emit_lifecycle(
                cx.observer,
                job,
                CompactionLifecycleState::Discarded,
                None,
                Some(
                    "deterministic fallback skipped because the planned range is stale".to_string(),
                ),
            )?;
            return Ok(None);
        };
        match self.apply_excerpts(messages, plan, cx)? {
            ContextDirective::Replace { messages } => Ok(Some(messages)),
            ContextDirective::Proceed => {
                self.emit_lifecycle(
                    cx.observer,
                    job,
                    CompactionLifecycleState::Discarded,
                    None,
                    Some(
                        "deterministic fallback did not shrink; keeping current context"
                            .to_string(),
                    ),
                )?;
                Ok(None)
            }
        }
    }

    /// Provider-native rung of the hard-tier fallback ladder. Runs one bounded,
    /// cancellable provider-native compaction off the loop (the adapter may use
    /// a blocking client, so it runs on its own OS thread) and applies it
    /// through the same parent-owned path a subagent summary uses. A success
    /// resets the model-backed circuit breaker exactly like a subagent apply.
    /// Provider names never cross this boundary: eligibility is decided only by
    /// the `compaction_capability` seam.
    fn try_provider_native_fallback(
        &mut self,
        job: &BackgroundCompaction,
        messages: &[Message],
        cx: ApplyContext<'_>,
        token: &CancellationToken,
    ) -> Result<NativeFallbackOutcome> {
        let Some(factory) = self.provider_compaction_factory.as_ref() else {
            // Native compaction is not wired for this session; fall straight to
            // the deterministic rung without a notice.
            return Ok(NativeFallbackOutcome::Unavailable);
        };
        let factory = factory.clone();
        let supported = match factory() {
            Ok(provider) => {
                provider.compaction_capability(job.original_tokens)
                    == ProviderCompactionCapability::OpaqueBlocks
            }
            Err(error) => {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "provider-native fallback probe failed; using deterministic excerpts"
                );
                false
            }
        };
        if !supported {
            self.emit_lifecycle(
                cx.observer,
                job,
                CompactionLifecycleState::Discarded,
                None,
                Some(
                    "provider-native compaction unavailable; using deterministic excerpts"
                        .to_string(),
                ),
            )?;
            return Ok(NativeFallbackOutcome::Unavailable);
        }
        if self.model_compaction_cap_reached(CompactionOrigin::ProviderNative) {
            self.emit_lifecycle(
                cx.observer,
                job,
                CompactionLifecycleState::Discarded,
                None,
                Some(
                    "per-turn model-backed compaction cap reached; using deterministic excerpts"
                        .to_string(),
                ),
            )?;
            return Ok(NativeFallbackOutcome::Unavailable);
        }
        let Some(plan) = self.revalidate(job, messages) else {
            return Ok(NativeFallbackOutcome::Unavailable);
        };
        self.emit_lifecycle(
            cx.observer,
            job,
            CompactionLifecycleState::Running,
            None,
            Some(format!(
                "subagent fallback escalating to provider-native compaction (~{} tokens)",
                job.original_tokens
            )),
        )?;
        let covered = messages[plan.start..plan.end].to_vec();
        let worker = self.worker.clone();
        let worker_token = CancellationToken::new();
        let child_token = worker_token.clone();
        let (tx, receiver) = mpsc::channel();
        thread::Builder::new()
            .name(format!("iris-{}-native-fallback", job.job_id))
            .spawn(move || {
                let result = run_provider_native_worker(factory, covered, worker, child_token);
                let _ = tx.send(result);
            })?;
        // Bounded, cancellable wait: reuse the hard-wait budget as the boundary
        // bound and poll the turn token in short slices so a cancelled turn
        // stops the blocking provider request instead of running to completion.
        let deadline = std::time::Instant::now() + self.hard_wait;
        let result = loop {
            if token.is_cancelled() {
                worker_token.cancel();
                self.emit_lifecycle(
                    cx.observer,
                    job,
                    CompactionLifecycleState::Cancelled,
                    None,
                    Some("provider-native fallback cancelled with the turn".to_string()),
                )?;
                return Ok(NativeFallbackOutcome::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                worker_token.cancel();
                break Err(RecvTimeoutError::Timeout);
            }
            match receiver.recv_timeout(remaining.min(std::time::Duration::from_millis(25))) {
                Ok(result) => break Ok(result),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break Err(RecvTimeoutError::Disconnected),
            }
        };
        match result {
            Ok(BackgroundSummaryResult::Summary(summary)) => {
                let usage = summary.worker_usage.clone();
                match self.apply_summary(messages, plan, summary, cx)? {
                    Some((_, replacement)) => {
                        self.consecutive_failures = 0;
                        self.breaker_notice_emitted = false;
                        self.emit_lifecycle(
                            cx.observer,
                            job,
                            CompactionLifecycleState::Applied,
                            usage,
                            Some("provider-native fallback compaction applied".to_string()),
                        )?;
                        Ok(NativeFallbackOutcome::Applied(replacement))
                    }
                    None => {
                        self.emit_lifecycle(
                            cx.observer,
                            job,
                            CompactionLifecycleState::Discarded,
                            usage,
                            Some(
                                "provider-native compaction did not shrink; using deterministic excerpts"
                                    .to_string(),
                            ),
                        )?;
                        Ok(NativeFallbackOutcome::Unavailable)
                    }
                }
            }
            Ok(BackgroundSummaryResult::Cancelled) => Ok(NativeFallbackOutcome::Unavailable),
            Ok(BackgroundSummaryResult::Failed(message)) => {
                self.emit_lifecycle(
                    cx.observer,
                    job,
                    CompactionLifecycleState::Failed,
                    None,
                    Some(format!(
                        "provider-native compaction failed; using deterministic excerpts: {message}"
                    )),
                )?;
                Ok(NativeFallbackOutcome::Unavailable)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.emit_lifecycle(
                    cx.observer,
                    job,
                    CompactionLifecycleState::Failed,
                    None,
                    Some(format!(
                        "provider-native compaction exceeded the {} ms bound; using deterministic excerpts",
                        self.hard_wait.as_millis()
                    )),
                )?;
                Ok(NativeFallbackOutcome::Unavailable)
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.emit_lifecycle(
                    cx.observer,
                    job,
                    CompactionLifecycleState::Failed,
                    None,
                    Some(
                        "provider-native fallback worker stopped before returning a summary; using deterministic excerpts"
                            .to_string(),
                    ),
                )?;
                Ok(NativeFallbackOutcome::Unavailable)
            }
        }
    }

    fn revalidate(
        &self,
        job: &BackgroundCompaction,
        messages: &[Message],
    ) -> Option<CompactionPlan> {
        if self.session_id().map(str::to_string) != job.session_id {
            return None;
        }
        if job.origin == CompactionOrigin::ProviderNative
            && job.selection_generation != self.selection_generation
        {
            return None;
        }
        let start = self
            .entry_ids
            .iter()
            .position(|id| id.as_deref() == Some(job.from_id.as_str()))?;
        let end = self
            .entry_ids
            .iter()
            .position(|id| id.as_deref() == Some(job.to_id.as_str()))?
            .checked_add(1)?;
        if end > self.persisted.min(messages.len())
            || !(start..end).all(|index| self.entry_ids.get(index).is_some_and(Option::is_some))
            || !valid_compaction_range(messages, start, end)
        {
            return None;
        }
        Some(CompactionPlan {
            start,
            end,
            from_id: job.from_id.clone(),
            to_id: job.to_id.clone(),
        })
    }

    pub(super) fn plan(&self, messages: &[Message], keep_target: u64) -> Option<CompactionPlan> {
        self.plan_with_mode(messages, keep_target, PlanTurnMode::Respect)
    }

    /// Manual compaction is an explicit inter-turn rewrite, so it may cover
    /// completed current-turn content. This is required after a hard compaction
    /// has already absorbed the turn's opening user message: the remaining
    /// assistant-only suffix has no user boundary for `Respect` mode to find.
    pub(super) fn plan_manual(
        &self,
        messages: &[Message],
        keep_target: u64,
    ) -> Option<CompactionPlan> {
        self.plan_with_mode(messages, keep_target, PlanTurnMode::HardCurrentTurn)
    }

    /// Plan a coverable range. `mode` decides whether the current (in-flight)
    /// assistant turn is protected. `Respect` keeps today's turn-respecting
    /// walk-back (Start/Warn and model-requested compaction). `HardCurrentTurn`
    /// skips that walk-back so the current turn's completed content becomes
    /// coverable when the keep-tail cut lands mid-turn -- the only way to relieve
    /// context once every pre-turn message is already compacted. Every other
    /// guard is identical in both modes: the keep-tail loop, the persisted bound
    /// `k.min(n)`, entry-id contiguity, and the pair-safety trims (start skips
    /// leading tool fragments; end backs off so no tool-call/result pair splits).
    pub(super) fn plan_with_mode(
        &self,
        messages: &[Message],
        keep_target: u64,
        mode: PlanTurnMode,
    ) -> Option<CompactionPlan> {
        let n = self.persisted.min(messages.len());
        let mut k = messages.len();
        let mut tail = 0u64;
        while k > 0 {
            let tokens = message_token_estimate(&messages[k - 1]);
            if tail.saturating_add(tokens) > keep_target {
                break;
            }
            tail = tail.saturating_add(tokens);
            k -= 1;
        }
        let end_limit = k.min(n);
        let end_limit = if mode == PlanTurnMode::Respect
            && end_limit < messages.len()
            && messages[end_limit].role != Role::User
        {
            assistant_turn_start(messages, end_limit)
        } else {
            end_limit
        };

        // Summaries (`None` ids) divide history into independently coverable
        // runs. Rank every pair-safe run by token mass instead of returning the
        // oldest one: a tiny but valid fragment before an existing summary must
        // not repeatedly block a later run that can reclaim meaningful context.
        let mut best: Option<(u64, CompactionPlan)> = None;
        let mut cursor = 0;
        while cursor < end_limit {
            let Some(mut start) = (cursor..end_limit)
                .find(|&index| self.entry_ids.get(index).is_some_and(Option::is_some))
            else {
                break;
            };
            let mut end = (start..end_limit)
                .find(|&index| self.entry_ids[index].is_none())
                .unwrap_or(end_limit);
            let next_cursor = end.saturating_add(1);

            while start < end
                && matches!(messages[start].role, Role::Tool | Role::AssistantToolCall)
            {
                start += 1;
            }
            while end > start
                && (messages[end - 1].role == Role::AssistantToolCall
                    || messages
                        .get(end)
                        .is_some_and(|message| message.role == Role::Tool))
            {
                end -= 1;
            }
            if start < end {
                let tokens = context_tokens(&messages[start..end]);
                if best
                    .as_ref()
                    .is_none_or(|(best_tokens, _)| tokens > *best_tokens)
                {
                    best = Some((
                        tokens,
                        CompactionPlan {
                            start,
                            end,
                            from_id: self.entry_ids[start].clone()?,
                            to_id: self.entry_ids[end - 1].clone()?,
                        },
                    ));
                }
            }
            cursor = next_cursor;
        }
        best.map(|(_, plan)| plan)
    }
}
