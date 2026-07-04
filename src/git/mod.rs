//! Presentation-side git knowledge (Tier 3 data layer).
//!
//! [`status`] builds the one read-only snapshot the session bar and its
//! dropdowns render from. Everything here runs through the hardened,
//! non-interactive git helper (`crate::wayland::git_safety::git`) -- no
//! prompts, no LFS smudge, no optional index locks, and never any network.

pub(crate) mod status;
