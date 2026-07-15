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
            .map(|ladder| ladder.displayed_context_window)
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
            self.cancel_worker(&job);
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
            // Provider overflow is by definition hard pressure, so the reactive
            // planner covers the current turn's completed content too.
            let Some(plan) = self.plan_with_mode(&current, keep, PlanTurnMode::HardCurrentTurn)
            else {
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
        let mut plans = self.all_pending_folds(messages, workspace);
        let Some((start, end)) = self.background_range() else {
            return plans;
        };
        plans.retain(|plan| plan.index < start || plan.index >= end);
        plans
    }

    pub(super) fn frozen_fold_stats(&self, messages: &[Message], workspace: &Path) -> (usize, u64) {
        let Some((start, end)) = self.background_range() else {
            return (0, 0);
        };
        let plans = self.all_pending_folds(messages, workspace);
        let frozen = plans
            .into_iter()
            .filter(|plan| plan.index >= start && plan.index < end)
            .collect::<Vec<_>>();
        let reclaimable = frozen
            .iter()
            .map(|plan| {
                estimate_tokens(&messages[plan.index].content)
                    .saturating_sub(estimate_tokens(&plan.stub))
            })
            .fold(0u64, u64::saturating_add);
        (frozen.len(), reclaimable)
    }

    fn all_pending_folds(&self, messages: &[Message], workspace: &Path) -> Vec<fold::FoldPlan> {
        if !self.tool_result_policy.enabled || self.session.is_none() {
            return Vec::new();
        }
        let tail_start = fold_tail_start(
            messages,
            self.tool_result_policy
                .semantic_dedupe
                .protect_recent_tokens,
        );
        fold::plan_folds(
            messages,
            &self.entry_ids,
            tail_start,
            workspace,
            &self.tool_result_policy,
        )
    }

    fn background_range(&self) -> Option<(usize, usize)> {
        let job = self.background.as_ref()?;
        let start = self
            .entry_ids
            .iter()
            .position(|id| id.as_deref() == Some(job.from_id.as_str()))?;
        let end = self
            .entry_ids
            .iter()
            .position(|id| id.as_deref() == Some(job.to_id.as_str()))?
            .checked_add(1)?;
        Some((start, end))
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
        emit_context_event(
            obs,
            AgentEvent::FoldApplied {
                folds: plans.len(),
                semantic_dedupe_folds,
                tool_clearing_folds,
                reclaimed_tokens_estimate: reclaimed,
                trigger,
            },
            "microcompaction applied",
        );
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
        if !cx.turn_continues || !self.trigger_v2 || self.session.is_none() {
            return Ok(ContextDirective::Proceed);
        }
        let model_requested = self
            .model_compaction_requested
            .swap(false, Ordering::SeqCst);
        if !self.automatic_enabled && !model_requested {
            return Ok(ContextDirective::Proceed);
        }
        let Some(ladder) = self.ladder else {
            return Ok(ContextDirective::Proceed);
        };
        self.poll_background_ready(obs)?;

        let anchor = cx.last_usage.map(|anchor| UsageAnchor {
            total_tokens: anchor.usage.total_tokens,
            message_count: anchor.message_count,
        });
        let measurement = measure_context(cx.messages, anchor, 0);
        if self.automatic_enabled {
            if let Some(tier) = self.pressure.crossing(measurement.tokens, &ladder) {
                emit_context_event(
                    obs,
                    AgentEvent::ContextPressure {
                        tier,
                        measured: measurement.tokens,
                        effective_window: ladder.displayed_context_window,
                        source: measurement.source,
                    },
                    "context pressure",
                );
            }
            if ladder.deterministic_only && !self.tiny_notice_emitted {
                self.tiny_notice_emitted = true;
                emit_context_event(
                    obs,
                    AgentEvent::Notice(format!(
                        "context window {} is too small for background summarization; automatic compaction will use deterministic excerpts.",
                        ladder.displayed_context_window
                    )),
                    "small-window compaction notice",
                );
            }
        }

        let initial_tier = ladder.tier(measurement.tokens);
        let mut current = cx.messages.to_vec();
        let mut changed = false;
        if model_requested
            && matches!(
                initial_tier,
                ContextPressureTier::Normal | ContextPressureTier::Warn
            )
        {
            let plans = self.pending_folds(&current, workspace);
            if !plans.is_empty() {
                current =
                    self.flush_folds(&current, &plans, FoldTrigger::CompactionBoundary, obs)?;
                changed = true;
            }
            if self.background.is_some() {
                return Ok(if changed {
                    ContextDirective::Replace { messages: current }
                } else {
                    ContextDirective::Proceed
                });
            }
            let Some(plan) = self.plan(&current, ladder.keep_recent_tokens) else {
                obs.on_event(AgentEvent::Notice(
                    "model-requested compaction found no pair-safe history to compact.".to_string(),
                ))?;
                return Ok(if changed {
                    ContextDirective::Replace { messages: current }
                } else {
                    ContextDirective::Proceed
                });
            };
            let model_backed = !ladder.deterministic_only
                && self.has_model_worker()
                && !self.model_compaction_cap_reached(CompactionOrigin::Subagent)
                && self.consecutive_failures < self.max_consecutive_failures;
            if model_backed
                && matches!(
                    self.start_background(&current, plan.clone(), workspace, obs, None)?,
                    BackgroundStart::Started
                )
            {
                return Ok(if changed {
                    ContextDirective::Replace { messages: current }
                } else {
                    ContextDirective::Proceed
                });
            }
            self.emit_breaker_notice(obs)?;
            let directive = self.apply_excerpts(&current, plan, apply_cx)?;
            return Ok(match directive {
                ContextDirective::Replace { messages } => ContextDirective::Replace { messages },
                ContextDirective::Proceed if changed => {
                    ContextDirective::Replace { messages: current }
                }
                ContextDirective::Proceed => ContextDirective::Proceed,
            });
        }
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
                    && self.has_model_worker()
                    && !self.model_compaction_cap_reached(CompactionOrigin::Subagent)
                    && self.consecutive_failures < self.max_consecutive_failures;
                if model_backed
                    && matches!(
                        self.start_background(
                            &current,
                            plan.clone(),
                            workspace,
                            obs,
                            Some(ContextPressureTier::Start),
                        )?,
                        BackgroundStart::Started
                    )
                {
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
                // Drain (bounded-wait) any job started at a prior boundary.
                if let Some(replacement) = self
                    .resolve_hard_at_boundary(&current, apply_cx, token)
                    .await?
                {
                    current = replacement;
                    changed = true;
                }
                if token.is_cancelled() {
                    // A durable mutation already landed if `changed`; return it
                    // even under a post-apply cancellation so the live context
                    // matches what a resume would rebuild. Only suppress work
                    // when cancellation is observed before any mutation.
                    return Ok(if changed {
                        ContextDirective::Replace { messages: current }
                    } else {
                        ContextDirective::Proceed
                    });
                }
                // Once every pre-turn message is compacted, Start could not
                // schedule a job (its turn-respecting plan returns None mid-turn).
                // At hard pressure, start one hard-mode model job covering the
                // current turn's completed content and resolve it under the same
                // bounded wait, so a model-backed summary (and its provider-native
                // fallback) can still win before the deterministic backstop.
                if self.background.is_none()
                    && ladder.tier(measure_context(&current, None, 0).tokens)
                        == ContextPressureTier::Hard
                {
                    let model_backed = !ladder.deterministic_only
                        && self.has_model_worker()
                        && !self.model_compaction_cap_reached(CompactionOrigin::Subagent)
                        && self.consecutive_failures < self.max_consecutive_failures;
                    if model_backed
                        && let Some(plan) = self.plan_with_mode(
                            &current,
                            ladder.keep_recent_tokens,
                            PlanTurnMode::HardCurrentTurn,
                        )
                        && matches!(
                            self.start_background(
                                &current,
                                plan,
                                workspace,
                                obs,
                                Some(ContextPressureTier::Hard),
                            )?,
                            BackgroundStart::Started
                        )
                    {
                        if let Some(replacement) = self
                            .resolve_hard_at_boundary(&current, apply_cx, token)
                            .await?
                        {
                            current = replacement;
                            changed = true;
                        }
                        if token.is_cancelled() {
                            // As above: a durable mid-turn apply must survive a
                            // post-apply cancellation instead of being dropped.
                            return Ok(if changed {
                                ContextDirective::Replace { messages: current }
                            } else {
                                ContextDirective::Proceed
                            });
                        }
                    }
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
                    // Deterministic backstop: hard mode covers the current turn
                    // so context can never run away unbounded within one turn.
                    let Some(plan) =
                        self.plan_with_mode(&current, keep, PlanTurnMode::HardCurrentTurn)
                    else {
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
