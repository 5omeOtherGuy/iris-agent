//! Mimir provider adapters: each translates a native wire format into the
//! provider-neutral `nexus::ChatProvider` streaming contract.
//!
//! The system prompt is assembled by the Tier-2 Wayland harness
//! ([`crate::wayland::system_prompt`]) and handed to each provider's
//! constructor as a ready string; a provider only wraps it in its own envelope
//! (e.g. Anthropic prepends the required Claude Code identity block as system
//! block 0). Providers no longer build the prompt themselves, so base/runtime/
//! project instructions have a single owner.

pub(crate) mod anthropic_messages;
pub(crate) mod antigravity;
pub(crate) mod openai_codex_responses;
mod transport;

use std::sync::Mutex;

use crate::nexus::ProviderUsage;

const MIN_PROMPT_CACHE_INPUT_TOKENS: u64 = 1024;

/// Provider-local prompt-cache diagnostics shared by adapters. It warns only
/// after a cacheable request has already happened (or after Anthropic reported a
/// cache write) and a later comparable request still has zero cache reads.
#[derive(Debug, Default)]
struct CacheUsageDiagnostics {
    cacheable_requests: u64,
    saw_cache_write: bool,
    warned: bool,
}

impl CacheUsageDiagnostics {
    fn record(&mut self, caching_enabled: bool, usage: &ProviderUsage) -> bool {
        if !caching_enabled {
            return false;
        }
        let cacheable = usage.input_tokens >= MIN_PROMPT_CACHE_INPUT_TOKENS
            || usage.cache_write_input_tokens > 0
            || usage.cache_read_input_tokens > 0;
        if !cacheable {
            return false;
        }
        let should_warn = !self.warned
            && usage.cache_read_input_tokens == 0
            && (self.saw_cache_write || self.cacheable_requests > 0);
        self.cacheable_requests = self.cacheable_requests.saturating_add(1);
        self.saw_cache_write |= usage.cache_write_input_tokens > 0;
        if should_warn {
            self.warned = true;
        }
        should_warn
    }

    fn record_locked(
        diagnostics: &Mutex<Self>,
        caching_enabled: bool,
        usage: &ProviderUsage,
    ) -> bool {
        diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(caching_enabled, usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input_tokens: u64, cache_read: u64, cache_write: u64) -> ProviderUsage {
        ProviderUsage {
            provider: "p".to_string(),
            model: "m".to_string(),
            input_tokens,
            output_tokens: 5,
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: cache_write,
            reasoning_output_tokens: 0,
            total_tokens: input_tokens + 5,
        }
    }

    #[test]
    fn cache_diagnostics_warn_after_prior_cache_write_or_repeat_miss() {
        let mut diagnostics = CacheUsageDiagnostics::default();
        assert!(!diagnostics.record(true, &usage(2_000, 0, 3)));
        assert!(diagnostics.record(true, &usage(2_000, 0, 0)));
        assert!(!diagnostics.record(true, &usage(2_000, 0, 0)), "warn once");

        let mut repeated = CacheUsageDiagnostics::default();
        assert!(!repeated.record(true, &usage(2_000, 0, 0)));
        assert!(repeated.record(true, &usage(2_000, 0, 0)));

        let mut short = CacheUsageDiagnostics::default();
        assert!(!short.record(true, &usage(100, 0, 0)));
        assert!(!short.record(true, &usage(100, 0, 0)));

        let mut disabled = CacheUsageDiagnostics::default();
        assert!(!disabled.record(false, &usage(2_000, 0, 3)));
        assert!(!disabled.record(false, &usage(2_000, 0, 0)));
    }
}
