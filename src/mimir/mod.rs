//! Mimir: Iris's AI/provider package (the pi-ai equivalent).
//!
//! Owns the concrete provider adapters and their auth flows. Named for Mimir,
//! the Norse keeper of the well of wisdom whom Odin consults for counsel: the
//! layer you query to get answers from an external source. The provider-neutral
//! `ChatProvider` contract stays in `nexus` (Tier 1); Mimir implements it.
pub(crate) mod auth;
pub(crate) mod providers;
