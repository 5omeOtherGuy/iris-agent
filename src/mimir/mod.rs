//! Mimir: Iris's AI/provider package (the pi-ai equivalent).
//!
//! Owns the concrete provider adapters and their auth flows. Named for Mimir,
//! the Norse keeper of the well of wisdom whom Odin consults for counsel: the
//! layer you query to get answers from an external source. The provider-neutral
//! `ChatProvider` contract stays in `nexus` (Tier 1); Mimir implements it.
pub(crate) mod anthropic_models;
pub(crate) mod auth;
pub(crate) mod model_capabilities;
pub(crate) mod model_catalog;
pub(crate) mod providers;
pub(crate) mod retry;
pub(crate) mod selection;
