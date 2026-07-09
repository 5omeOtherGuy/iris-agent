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

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap()
    }

    /// Redirect `IRIS_CONFIG_PATH` at a temp file for a save round-trip,
    /// restoring any previous value on drop. Holds the process-wide env lock
    /// so every config-path writer across the crate is serialized and never
    /// races on the shared environment variable.
    pub(crate) struct ConfigPathGuard {
        prev: Option<String>,
        _lock: MutexGuard<'static, ()>,
    }

    impl ConfigPathGuard {
        pub(crate) fn set(path: &std::path::Path) -> Self {
            let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("IRIS_CONFIG_PATH").ok();
            // SAFETY: the env lock serializes all IRIS_CONFIG_PATH writers;
            // the previous value is restored on drop.
            unsafe { std::env::set_var("IRIS_CONFIG_PATH", path) };
            ConfigPathGuard { prev, _lock }
        }
    }

    impl Drop for ConfigPathGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(prev) => unsafe { std::env::set_var("IRIS_CONFIG_PATH", prev) },
                None => unsafe { std::env::remove_var("IRIS_CONFIG_PATH") },
            }
        }
    }
}
