//! Shared provider retry policy.
//!
//! A single definition of the transient-retry budget and exponential backoff
//! shape, used by every streaming provider adapter (OpenAI Codex, Anthropic,
//! Antigravity) so the retry constants live in exactly one place. Resolved from
//! [`crate::config::RetrySettings`] in `mimir::selection`, with defaults aligned
//! to pi-mono (`coding-agent` RetrySettings: maxRetries=3, baseDelayMs=2000;
//! `ai/types` maxRetryDelayMs cap = 60s).

use std::time::Duration;

use crate::config::RetrySettings;

/// Default maximum transient retries before giving up (pi-mono `maxRetries`).
pub(crate) const DEFAULT_MAX_RETRIES: u32 = 3;
/// Default base backoff, doubled per retry (pi-mono `baseDelayMs`).
pub(crate) const DEFAULT_BASE_BACKOFF: Duration = Duration::from_millis(2000);
/// Default backoff ceiling (pi-mono `maxRetryDelayMs`).
pub(crate) const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Jitter ceiling added to every computed delay so concurrent requests in the
/// same rate-limit window do not retry in lockstep.
const JITTER_CEILING_MS: u64 = 250;

/// Resolved retry/backoff policy shared by all provider adapters. `Copy` so a
/// provider can stash it and a cloned provider (`self.clone()` before a blocking
/// task) carries it without ceremony.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    /// Maximum transient retries before the loop returns the last error.
    pub(crate) max_retries: u32,
    /// Base backoff, doubled per retry.
    pub(crate) base_backoff: Duration,
    /// Backoff ceiling; exponential growth and a `Retry-After` hint are both
    /// bounded against this (the hint up to 4x, for pathological values).
    pub(crate) max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            base_backoff: DEFAULT_BASE_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

impl RetryPolicy {
    /// Resolve a policy from raw settings, filling any absent subfield with the
    /// built-in default.
    pub(crate) fn from_settings(settings: &RetrySettings) -> Self {
        let default = Self::default();
        Self {
            max_retries: settings.max_retries.unwrap_or(default.max_retries),
            base_backoff: settings
                .base_delay_ms
                .map(Duration::from_millis)
                .unwrap_or(default.base_backoff),
            max_backoff: settings
                .max_delay_ms
                .map(Duration::from_millis)
                .unwrap_or(default.max_backoff),
        }
    }

    /// Compute the delay before the next transient retry (`retry` is 1-based).
    /// Honors a server `Retry-After` hint when present (bounded against
    /// pathological values at 4x the ceiling); otherwise exponential backoff
    /// from the base doubling per retry, clamped to the ceiling. Either way up
    /// to [`JITTER_CEILING_MS`] of jitter is added.
    pub(crate) fn backoff_delay(&self, retry: u32, retry_after: Option<Duration>) -> Duration {
        let jitter = Duration::from_millis(rand::random::<u64>() % JITTER_CEILING_MS);
        if let Some(after) = retry_after {
            return after.min(self.max_backoff.saturating_mul(4)) + jitter;
        }
        let shift = retry.saturating_sub(1).min(10);
        let exp = self
            .base_backoff
            .checked_mul(1u32 << shift)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff);
        exp + jitter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_pi_mono() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.base_backoff, Duration::from_millis(2000));
        assert_eq!(policy.max_backoff, Duration::from_secs(60));
    }

    #[test]
    fn from_settings_fills_defaults_for_absent_subfields() {
        let policy = RetryPolicy::from_settings(&RetrySettings {
            max_retries: Some(5),
            base_delay_ms: None,
            max_delay_ms: Some(10_000),
        });
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.base_backoff, DEFAULT_BASE_BACKOFF);
        assert_eq!(policy.max_backoff, Duration::from_millis(10_000));
    }

    #[test]
    fn backoff_grows_exponentially_and_clamps() {
        let policy = RetryPolicy::default();
        let d1 = policy.backoff_delay(1, None);
        let d2 = policy.backoff_delay(2, None);
        assert!(d1 >= policy.base_backoff && d1 < policy.base_backoff + Duration::from_millis(250));
        assert!(d2 >= policy.base_backoff * 2);
        let big = policy.backoff_delay(20, None);
        assert!(big <= policy.max_backoff + Duration::from_millis(250));
    }

    #[test]
    fn backoff_honors_retry_after_and_bounds_pathological() {
        let policy = RetryPolicy::default();
        let hint = policy.backoff_delay(1, Some(Duration::from_secs(2)));
        assert!(hint >= Duration::from_secs(2));
        let pathological = policy.backoff_delay(1, Some(Duration::from_secs(86_400)));
        assert!(pathological <= policy.max_backoff * 4 + Duration::from_millis(250));
    }
}
