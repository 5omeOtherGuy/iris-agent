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
