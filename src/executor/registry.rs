//! Executor registry. Holds available executors per host, exposes lookup and
//! aggregation operations (cross-executor list / count) used by the main loop.

use super::{Executor, ExecutorKind, OwnedRunner, ProvisionError};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Registry {
    executors: HashMap<ExecutorKind, Arc<dyn Executor>>,
}

impl Registry {
    /// Probe what executors can plausibly run on this host. Construction
    /// failures (daemon down, binary missing) silently drop that executor —
    /// it just won't appear in the registry. Caller resolves missing
    /// executors as `BackendUnavailable`.
    pub fn probe() -> Self {
        let mut executors: HashMap<ExecutorKind, Arc<dyn Executor>> = HashMap::new();
        // Docker works on any OS (Linux native, macOS via Docker Desktop) —
        // register if the daemon answers a ping.
        let docker = super::docker::DockerExecutor::new();
        match docker.client_ping() {
            Ok(v) => {
                log::info!("docker daemon reachable: {v}");
                executors.insert(ExecutorKind::Docker, Arc::new(docker));
            }
            Err(e) => log::info!("docker daemon not available (skipping): {e}"),
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(m) = super::meda::MedaExecutor::new() {
                executors.insert(ExecutorKind::Meda, Arc::new(m));
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Ok(l) = super::lume::LumeExecutor::new() {
                executors.insert(ExecutorKind::Lume, Arc::new(l));
            }
        }
        Self { executors }
    }

    /// Build a Registry from an explicit map. Intended for tests that need to
    /// inject a fake executor without probing the host. Not part of the
    /// production wiring — `probe()` is the canonical constructor.
    #[cfg(test)]
    pub fn from_executors(executors: HashMap<ExecutorKind, Arc<dyn Executor>>) -> Self {
        Self { executors }
    }

    pub fn get(&self, kind: ExecutorKind) -> Result<Arc<dyn Executor>, ProvisionError> {
        self.executors.get(&kind).cloned().ok_or_else(|| {
            ProvisionError::Incompatible(format!("executor {kind:?} not available on this host"))
        })
    }

    /// Names of registered executors as lowercase strings (`"docker"`, `"meda"`,
    /// `"lume"`), sorted for deterministic wire output. Sent on the agent's GET
    /// /agent body so the cirun api can route by capability instead of host OS.
    pub fn kind_names(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = self
            .executors
            .keys()
            .map(|k| match k {
                ExecutorKind::Docker => "docker",
                ExecutorKind::Meda => "meda",
                ExecutorKind::Lume => "lume",
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// Aggregate count of running runners across every registered executor.
    /// Errors per-executor are logged and that executor counts as 0.
    pub async fn total_count_running(&self) -> usize {
        let mut total = 0;
        for (kind, exec) in &self.executors {
            match exec.count_running().await {
                Ok(n) => total += n,
                Err(e) => log::warn!("count_running({kind:?}) failed: {e}"),
            }
        }
        total
    }

    /// Cross-executor list. Returns `(kind, runner)` pairs so the caller can
    /// preserve the binding for reporting / orphan-cleanup decisions.
    pub async fn list_all(&self) -> Vec<(ExecutorKind, OwnedRunner)> {
        let mut all = Vec::new();
        for (kind, exec) in &self.executors {
            match exec.list_owned().await {
                Ok(runners) => all.extend(runners.into_iter().map(|r| (*kind, r))),
                Err(e) => log::warn!("list_owned({kind:?}) failed: {e}"),
            }
        }
        all
    }
}
