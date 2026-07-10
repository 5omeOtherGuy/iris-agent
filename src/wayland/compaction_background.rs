//! Background compaction job lifecycle used by the context governor.

use super::*;

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
        obs.on_event(AgentEvent::CompactionLifecycle {
            job_id: job.job_id.clone(),
            state,
            covered_messages: job.covered_messages,
            original_tokens_estimate: job.original_tokens,
            origin: job.origin,
            worker_usage,
            trigger_tier: job.trigger_tier,
            message,
        })
    }

    pub(super) fn emit_breaker_notice(&mut self, obs: &dyn AgentObserver) -> Result<()> {
        if self.consecutive_failures < self.max_consecutive_failures || self.breaker_notice_emitted
        {
            return Ok(());
        }
        self.breaker_notice_emitted = true;
        obs.on_event(AgentEvent::Notice(format!(
            "background compaction disabled after {} consecutive failures; deterministic compaction remains active.",
            self.consecutive_failures
        )))
    }

    pub(super) fn start_background(
        &mut self,
        messages: &[Message],
        plan: CompactionPlan,
        workspace: &Path,
        obs: &dyn AgentObserver,
        trigger_tier: Option<ContextPressureTier>,
    ) -> Result<()> {
        let factory = self
            .summarizer_factory
            .as_ref()
            .expect("caller checks factory")
            .clone();
        let covered = messages[plan.start..plan.end].to_vec();
        let covered_messages = covered.len();
        let original_tokens = context_tokens(&covered);
        let job_id = format!("compaction_{:08x}", self.next_job_seq);
        self.next_job_seq = self.next_job_seq.saturating_add(1);
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let workspace_for_worker = workspace.to_path_buf();
        let mode = self.summarizer;
        let worker = self.worker.clone();
        let origin = match mode {
            SummarizerKind::Subagent => CompactionOrigin::Subagent,
            SummarizerKind::Provider => CompactionOrigin::Provider,
            SummarizerKind::Excerpts => CompactionOrigin::Excerpts,
        };
        let (tx, receiver) = mpsc::channel();
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
        Ok(())
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
                self.finish_background_at_boundary(job, result, messages, cx)
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
                self.apply_job_fallback(&job, messages, cx)
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
                cx.observer.on_event(AgentEvent::CompactionLifecycle {
                    job_id,
                    state: CompactionLifecycleState::Cancelled,
                    covered_messages,
                    original_tokens_estimate: original_tokens,
                    origin,
                    worker_usage: None,
                    trigger_tier,
                    message: Some("background compaction cancelled with the turn".to_string()),
                })?;
                return Ok(None);
            }
            joined = waiter => joined,
        };
        let (job, result) = match joined {
            Ok(joined) => joined,
            Err(error) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                cx.observer.on_event(AgentEvent::CompactionLifecycle {
                    job_id,
                    state: CompactionLifecycleState::Failed,
                    covered_messages,
                    original_tokens_estimate: original_tokens,
                    origin,
                    worker_usage: None,
                    trigger_tier,
                    message: Some(format!("background hard-wait task failed: {error}")),
                })?;
                return Ok(None);
            }
        };
        match result {
            Ok(result) => self.finish_background_at_boundary(job, result, messages, cx),
            Err(RecvTimeoutError::Timeout) => {
                job.token.cancel();
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.emit_lifecycle(
                    cx.observer,
                    &job,
                    CompactionLifecycleState::Cancelled,
                    None,
                    Some(format!(
                        "background compaction exceeded the {} ms hard wait; using deterministic fallback",
                        self.hard_wait.as_millis()
                    )),
                )?;
                self.apply_job_fallback(&job, messages, cx)
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
                self.apply_job_fallback(&job, messages, cx)
            }
        }
    }

    fn finish_background_at_boundary(
        &mut self,
        job: BackgroundCompaction,
        result: BackgroundSummaryResult,
        messages: &[Message],
        cx: ApplyContext<'_>,
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
                    return self.apply_job_fallback(&job, messages, cx);
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
                        self.apply_job_fallback(&job, messages, cx)
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
                self.apply_job_fallback(&job, messages, cx)
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
    ) -> Result<Option<Vec<Message>>> {
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

    fn revalidate(
        &self,
        job: &BackgroundCompaction,
        messages: &[Message],
    ) -> Option<CompactionPlan> {
        if self.session_id().map(str::to_string) != job.session_id {
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
        let mut end = k.min(n);
        if end < messages.len() && messages[end].role != Role::User {
            end = assistant_turn_start(messages, end);
        }
        let mut start =
            (0..end).find(|&index| self.entry_ids.get(index).is_some_and(Option::is_some))?;
        if let Some(none_at) = (start..end).find(|&index| self.entry_ids[index].is_none()) {
            end = none_at;
        }
        while start < end && matches!(messages[start].role, Role::Tool | Role::AssistantToolCall) {
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
        if start >= end {
            return None;
        }
        Some(CompactionPlan {
            start,
            end,
            from_id: self.entry_ids[start].clone()?,
            to_id: self.entry_ids[end - 1].clone()?,
        })
    }
}
