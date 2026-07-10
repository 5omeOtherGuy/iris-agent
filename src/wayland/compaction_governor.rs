//! Between-round-trip context governance.
//!
//! This module owns the fast boundary algorithm and fold-freeze rule. Durable
//! apply state remains on `CompactionEngine`; Nexus sees only a directive.

use super::*;

impl CompactionEngine {
    pub(super) fn recover_overflow(
        &mut self,
        messages: &[Message],
        apply_cx: ApplyContext<'_>,
    ) -> Result<ContextOverflowRecovery> {
        let measured_before = measure_context(messages, None, 0).tokens;
        let effective_window = self
            .ladder
            .map(|ladder| ladder.effective_window)
            .or(self.budget)
            .unwrap_or(0);
        if !self.reactive_enabled
            || !self.trigger_v2
            || !self.automatic_enabled
            || self.session.is_none()
        {
            return Ok(ContextOverflowRecovery::Unrecoverable {
                measured: measured_before,
                effective_window,
            });
        }
        let Some(ladder) = self.ladder else {
            return Ok(ContextOverflowRecovery::Unrecoverable {
                measured: measured_before,
                effective_window,
            });
        };

        if let Some(job) = self.background.take() {
            job.token.cancel();
            self.emit_lifecycle(
                apply_cx.observer,
                &job,
                CompactionLifecycleState::Cancelled,
                None,
                Some(
                    "background compaction cancelled for reactive deterministic recovery"
                        .to_string(),
                ),
            )?;
        }

        let mut current = messages.to_vec();
        let mut changed = false;
        let plans = self.pending_folds(&current, apply_cx.workspace);
        if !plans.is_empty() {
            current = self.flush_folds(
                &current,
                &plans,
                FoldTrigger::CompactionBoundary,
                apply_cx.observer,
            )?;
            changed = true;
        }

        for (index, keep) in [ladder.keep_recent_tokens, MANUAL_COMPACT_KEEP_TOKENS]
            .into_iter()
            .enumerate()
        {
            if index > 0
                && ladder.tier(measure_context(&current, None, 0).tokens)
                    != ContextPressureTier::Hard
            {
                break;
            }
            let Some(plan) = self.plan(&current, keep) else {
                break;
            };
            let ContextDirective::Replace { messages } =
                self.apply_excerpts(&current, plan, apply_cx)?
            else {
                break;
            };
            current = messages;
            changed = true;
        }

        let measured = measure_context(&current, None, 0).tokens;
        if changed {
            Ok(ContextOverflowRecovery::Resend {
                messages: current,
                measured,
                effective_window,
            })
        } else {
            Ok(ContextOverflowRecovery::Unrecoverable {
                measured,
                effective_window,
            })
        }
    }

    /// Derived fold set at the current boundary. While a background job owns a
    /// frozen id range, folds inside that snapshot are withheld so the worker
    /// and parent never race two different rewrites of the same originals.
    pub(super) fn pending_folds(
        &self,
        messages: &[Message],
        workspace: &Path,
    ) -> Vec<fold::FoldPlan> {
        if !self.tool_result_policy.enabled || self.session.is_none() {
            return Vec::new();
        }
        let tail_start = fold_tail_start(
            messages,
            self.tool_result_policy
                .semantic_dedupe
                .protect_recent_tokens,
        );
        let mut plans = fold::plan_folds(
            messages,
            &self.entry_ids,
            tail_start,
            workspace,
            &self.tool_result_policy,
        );
        if let Some(job) = self.background.as_ref()
            && let (Some(start), Some(end)) = (
                self.entry_ids
                    .iter()
                    .position(|id| id.as_deref() == Some(job.from_id.as_str())),
                self.entry_ids
                    .iter()
                    .position(|id| id.as_deref() == Some(job.to_id.as_str()))
                    .and_then(|index| index.checked_add(1)),
            )
        {
            plans.retain(|plan| plan.index < start || plan.index >= end);
        }
        plans
    }

    fn flush_folds(
        &mut self,
        messages: &[Message],
        plans: &[fold::FoldPlan],
        trigger: FoldTrigger,
        obs: &dyn AgentObserver,
    ) -> Result<Vec<Message>> {
        let log = self
            .session
            .as_mut()
            .expect("fold flush callers check the session first");
        let mut folded = messages.to_vec();
        let mut reclaimed = 0u64;
        let mut semantic_dedupe_folds = 0usize;
        let mut tool_clearing_folds = 0usize;
        for plan in plans {
            let stub_tokens = estimate_tokens(&plan.stub);
            let mut reasons = Vec::new();
            if plan.has_reason(fold::FoldReason::SemanticDedupe) {
                semantic_dedupe_folds += 1;
                reasons.push("semanticDedupe");
            }
            if plan.has_reason(fold::FoldReason::ToolClearing) {
                tool_clearing_folds += 1;
                reasons.push("toolClearing");
            }
            log.append_fold_with_reasons(
                &plan.entry_id,
                &plan.stub,
                Some(stub_tokens),
                trigger.code(),
                &reasons,
            )?;
            reclaimed = reclaimed.saturating_add(
                estimate_tokens(&folded[plan.index].content).saturating_sub(stub_tokens),
            );
            folded[plan.index].content = plan.stub.clone();
        }
        obs.on_event(AgentEvent::FoldApplied {
            folds: plans.len(),
            semantic_dedupe_folds,
            tool_clearing_folds,
            reclaimed_tokens_estimate: reclaimed,
            trigger,
        })?;
        Ok(folded)
    }

    /// Evaluate the trigger-v2 ladder at a safe continuing-round boundary.
    /// Model-backed work only starts here; it never blocks this path outside
    /// the hard tier. Every accepted rewrite is durable before it is returned
    /// to Nexus as one atomic whole-context replacement.
    pub(super) async fn govern(
        &mut self,
        cx: BoundaryContext<'_>,
        apply_cx: ApplyContext<'_>,
        token: &CancellationToken,
    ) -> Result<ContextDirective> {
        tracing::trace!(round_trip = cx.round_trip, "evaluating context boundary");
        let workspace = apply_cx.workspace;
        let obs = apply_cx.observer;
        if !cx.turn_continues
            || !self.trigger_v2
            || !self.automatic_enabled
            || self.session.is_none()
        {
            return Ok(ContextDirective::Proceed);
        }
        let Some(ladder) = self.ladder else {
            return Ok(ContextDirective::Proceed);
        };
        if let Some(mut replacement) = self.drain_background_at_boundary(cx.messages, apply_cx)? {
            let plans = self.pending_folds(&replacement, workspace);
            if !plans.is_empty() {
                replacement =
                    self.flush_folds(&replacement, &plans, FoldTrigger::CompactionBoundary, obs)?;
            }
            return Ok(ContextDirective::Replace {
                messages: replacement,
            });
        }

        let anchor = cx.last_usage.map(|anchor| UsageAnchor {
            total_tokens: anchor.usage.total_tokens,
            message_count: anchor.message_count,
        });
        let measurement = measure_context(cx.messages, anchor, 0);
        if let Some(tier) = self.pressure.crossing(measurement.tokens, &ladder) {
            obs.on_event(AgentEvent::ContextPressure {
                tier,
                measured: measurement.tokens,
                effective_window: ladder.effective_window,
                source: measurement.source,
            })?;
        }
        if ladder.deterministic_only && !self.tiny_notice_emitted {
            self.tiny_notice_emitted = true;
            obs.on_event(AgentEvent::Notice(format!(
                "context window {} is too small for background summarization; automatic compaction will use deterministic excerpts.",
                ladder.effective_window
            )))?;
        }

        let initial_tier = ladder.tier(measurement.tokens);
        let mut current = cx.messages.to_vec();
        let mut changed = false;
        if initial_tier == ContextPressureTier::Start {
            let plans = self.pending_folds(&current, workspace);
            if !plans.is_empty() {
                current =
                    self.flush_folds(&current, &plans, FoldTrigger::CompactionBoundary, obs)?;
                changed = true;
            }
        }
        let tier = if changed {
            ladder.tier(measure_context(&current, None, 0).tokens)
        } else {
            initial_tier
        };

        match tier {
            ContextPressureTier::Normal | ContextPressureTier::Warn => Ok(if changed {
                ContextDirective::Replace { messages: current }
            } else {
                ContextDirective::Proceed
            }),
            ContextPressureTier::Start => {
                if self.background.is_some() {
                    return Ok(if changed {
                        ContextDirective::Replace { messages: current }
                    } else {
                        ContextDirective::Proceed
                    });
                }
                let Some(plan) = self.plan(&current, ladder.keep_recent_tokens) else {
                    return Ok(if changed {
                        ContextDirective::Replace { messages: current }
                    } else {
                        ContextDirective::Proceed
                    });
                };
                let model_backed = !ladder.deterministic_only
                    && self.summarizer != SummarizerKind::Excerpts
                    && self.summarizer_factory.is_some()
                    && !self.model_compaction_cap_reached(CompactionOrigin::Subagent)
                    && self.consecutive_failures < self.max_consecutive_failures;
                if model_backed {
                    self.start_background(&current, plan, workspace, obs)?;
                    Ok(if changed {
                        ContextDirective::Replace { messages: current }
                    } else {
                        ContextDirective::Proceed
                    })
                } else {
                    self.emit_breaker_notice(obs)?;
                    let directive = self.apply_excerpts(&current, plan, apply_cx)?;
                    Ok(match directive {
                        ContextDirective::Replace { messages } => {
                            ContextDirective::Replace { messages }
                        }
                        ContextDirective::Proceed if changed => {
                            ContextDirective::Replace { messages: current }
                        }
                        ContextDirective::Proceed => ContextDirective::Proceed,
                    })
                }
            }
            ContextPressureTier::Hard => {
                if let Some(replacement) = self
                    .resolve_hard_at_boundary(&current, apply_cx, token)
                    .await?
                {
                    current = replacement;
                    changed = true;
                }
                if token.is_cancelled() {
                    return Ok(ContextDirective::Proceed);
                }
                let plans = self.pending_folds(&current, workspace);
                if !plans.is_empty() {
                    current =
                        self.flush_folds(&current, &plans, FoldTrigger::CompactionBoundary, obs)?;
                    changed = true;
                }
                for keep in [ladder.keep_recent_tokens, MANUAL_COMPACT_KEEP_TOKENS] {
                    if ladder.tier(measure_context(&current, None, 0).tokens)
                        != ContextPressureTier::Hard
                    {
                        break;
                    }
                    let Some(plan) = self.plan(&current, keep) else {
                        break;
                    };
                    let ContextDirective::Replace { messages } =
                        self.apply_excerpts(&current, plan, apply_cx)?
                    else {
                        break;
                    };
                    current = messages;
                    changed = true;
                }
                Ok(if changed {
                    ContextDirective::Replace { messages: current }
                } else {
                    ContextDirective::Proceed
                })
            }
        }
    }
}
