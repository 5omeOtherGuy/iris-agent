//! Iris adapter around the host-neutral backend scheduler.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use iris_subagent_runtime::{
    ExecutorFactory, FilesystemArtifactStore, HostPayload, RuntimeConfig, RuntimeError,
    RuntimeHandle, WorkerExecutor, WorkerId, WorkerRequest,
};
use serde_json::json;

pub(crate) const EXECUTOR_HOST_PAYLOAD_KIND: &str = "iris_executor_registration";

/// One-shot executor constructor. The backend invokes it on its scheduler
/// thread, preserving `!Send` Nexus/provider state.
type RegisteredFactory =
    Box<dyn FnOnce() -> Result<Box<dyn WorkerExecutor>, RuntimeError> + Send + 'static>;

struct IrisExecutorFactory {
    registrations: Arc<Mutex<HashMap<String, RegisteredFactory>>>,
}

fn register_executor(request: &mut WorkerRequest, registration: &str) {
    let payload = std::mem::take(&mut request.host);
    request.host.kind = EXECUTOR_HOST_PAYLOAD_KIND.to_string();
    request.host.value = json!({
        "registration": registration,
        "payload": payload,
    });
}

/// Recover the host payload that existed before the scheduler adapter attached
/// its one-shot executor registration. Older persisted requests carried only the
/// registration and therefore return `None`.
pub(crate) fn original_host_payload(request: &WorkerRequest) -> Result<Option<HostPayload>> {
    if request.host.kind != EXECUTOR_HOST_PAYLOAD_KIND {
        return Ok(Some(request.host.clone()));
    }
    let Some(payload) = request.host.value.get("payload") else {
        return Ok(None);
    };
    serde_json::from_value(payload.clone())
        .context("Iris worker request has malformed nested host payload")
        .map(Some)
}

impl ExecutorFactory for IrisExecutorFactory {
    fn create(&self, request: &WorkerRequest) -> Result<Box<dyn WorkerExecutor>, RuntimeError> {
        let registration = request
            .host
            .value
            .get("registration")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                RuntimeError::ExecutorFactory(
                    "Iris worker request is missing executor registration".to_string(),
                )
            })?;
        let factory = self
            .registrations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(registration)
            .ok_or_else(|| {
                RuntimeError::ExecutorFactory(format!(
                    "Iris executor registration is unavailable: {registration}"
                ))
            })?;
        factory()
    }
}

/// Shared Wayland-owned adapter used by delegation and compaction.
pub(crate) struct WorkerRuntime {
    handle: RuntimeHandle,
    registrations: Arc<Mutex<HashMap<String, RegisteredFactory>>>,
}

impl WorkerRuntime {
    pub(crate) fn open(state_dir: &Path) -> Result<Arc<Self>> {
        let registrations = Arc::new(Mutex::new(HashMap::new()));
        let factory = Arc::new(IrisExecutorFactory {
            registrations: registrations.clone(),
        });
        let artifacts = Arc::new(FilesystemArtifactStore::new(state_dir.join("artifacts"))?);
        let handle = RuntimeHandle::start(RuntimeConfig::new(state_dir), factory, Some(artifacts))?;
        Ok(Arc::new(Self {
            handle,
            registrations,
        }))
    }

    pub(crate) fn spawn(
        &self,
        mut request: WorkerRequest,
        factory: RegisteredFactory,
    ) -> Result<WorkerId> {
        let registration = format!("iris_{:032x}", rand::random::<u128>());
        register_executor(&mut request, &registration);
        self.registrations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(registration.clone(), factory);
        match self.handle.spawn(request) {
            Ok(id) => Ok(id),
            Err(error) => {
                self.registrations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&registration);
                Err(error.into())
            }
        }
    }

    pub(crate) fn spawn_group(
        &self,
        jobs: Vec<(WorkerRequest, RegisteredFactory)>,
    ) -> Result<iris_subagent_runtime::GroupId> {
        let mut registrations = Vec::with_capacity(jobs.len());
        let mut requests = Vec::with_capacity(jobs.len());
        {
            let mut registry = self
                .registrations
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            for (mut request, factory) in jobs {
                let registration = format!("iris_{:032x}", rand::random::<u128>());
                register_executor(&mut request, &registration);
                registry.insert(registration.clone(), factory);
                registrations.push(registration);
                requests.push(request);
            }
        }
        match self.handle.spawn_group(requests) {
            Ok(id) => Ok(id),
            Err(error) => {
                let mut registry = self
                    .registrations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                for registration in registrations {
                    registry.remove(&registration);
                }
                Err(error.into())
            }
        }
    }

    pub(crate) fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }
}

impl Drop for WorkerRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.handle.shutdown() {
            tracing::warn!(error = %error, "worker runtime shutdown failed");
        }
    }
}

pub(crate) fn session_worker_state_dir(session_path: Option<&Path>) -> Result<std::path::PathBuf> {
    if let Some(path) = session_path {
        return Ok(path.with_extension("workers"));
    }
    Ok(std::env::temp_dir().join(format!(
        "iris-workers-ephemeral-{:032x}",
        rand::random::<u128>()
    )))
}
