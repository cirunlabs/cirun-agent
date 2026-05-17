//! Lume adapter: wraps `crate::lume::client::LumeClient` behind the `Executor` trait.

use super::{Executor, GpuRequest, OwnedRunner, ProvisionError, RunnerSpec, RunnerState};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

pub(super) fn validate_gpu(spec: &RunnerSpec) -> Result<(), ProvisionError> {
    if !matches!(spec.gpu, GpuRequest::None) {
        return Err(ProvisionError::Incompatible(
            "lume executor does not support GPU passthrough".into(),
        ));
    }
    Ok(())
}

pub(super) fn map_vm_state(status: &str) -> RunnerState {
    match status {
        "running" => RunnerState::Healthy,
        "starting" | "creating" => RunnerState::Starting,
        "stopped" | "paused" | "saved" | "error" => RunnerState::Terminated {
            exit_code: None,
            last_logs: String::new(),
        },
        _ => RunnerState::Starting,
    }
}

pub struct LumeExecutor {
    client: Arc<crate::lume::client::LumeClient>,
}

impl LumeExecutor {
    pub fn new() -> Result<Self, ProvisionError> {
        let client = crate::lume::client::LumeClient::new()
            .map_err(|e| ProvisionError::transient(format!("lume init: {e}")))?;
        Ok(Self {
            client: Arc::new(client),
        })
    }
}

#[async_trait]
impl Executor for LumeExecutor {
    fn settle_timeout(&self) -> Duration {
        Duration::from_secs(300)
    }

    /// Apple Virtualization framework does not expose GPUs to guests.
    fn validate(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        validate_gpu(spec)
    }

    async fn inspect(&self, name: &str) -> Result<RunnerState, ProvisionError> {
        match self.client.get_vm(name).await {
            Ok(info) => Ok(map_vm_state(&info.state)),
            Err(_) => Ok(RunnerState::Absent),
        }
    }

    /// Lume "spawn" = clone from the template VM named in `spec.image`, then
    /// kick off `run_vm` AND wait for the VM to actually leave the `stopped`
    /// state. Lume's `POST /vms/{name}/run` returns `202 Accepted` —
    /// the start is async, and `inspect` immediately after returns `stopped`
    /// for a brief window. `map_vm_state("stopped")` is `Terminated`, so
    /// without the post-run wait the trait's settle loop sees Terminated on
    /// its very first poll and kills the VM before it ever reached `starting`.
    async fn spawn(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        use crate::lume::models::RunConfig;
        // Verify template exists first so a typo doesn't waste a clone attempt.
        self.client.get_vm(&spec.image).await.map_err(|e| {
            ProvisionError::Permanent(format!("template '{}' not found: {e:?}", spec.image))
        })?;
        self.client
            .clone_vm(&spec.image, &spec.name)
            .await
            .map_err(|e| ProvisionError::transient(format!("lume clone_vm: {e:?}")))?;
        let run_config = RunConfig {
            no_display: Some(true),
            shared_directories: None,
            recovery_mode: None,
        };
        self.client
            .run_vm(&spec.name, Some(run_config))
            .await
            .map_err(|e| ProvisionError::transient(format!("lume run_vm: {e:?}")))?;
        // Block until the daemon shows the VM has left `stopped`. The xcode
        // image is larger and slower to start than vanilla — 30s wasn't
        // enough on m1 (observed 2026-05-15). 120s is generous; actual boot
        // is then still covered by the trait's settle loop afterwards.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            match self.client.get_vm(&spec.name).await {
                Ok(info) if info.state != "stopped" => return Ok(()),
                Ok(_) => {} // still stopped — keep waiting
                Err(e) => log::debug!("lume get_vm during spawn-wait: {e:?}"),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ProvisionError::transient(format!(
                    "lume VM '{}' did not leave 'stopped' state within 120s of run_vm",
                    spec.name
                )));
            }
        }
    }

    async fn kill(&self, name: &str) -> Result<(), ProvisionError> {
        self.client
            .delete_vm(name)
            .await
            .map_err(|e| ProvisionError::transient(format!("lume delete_vm: {e:?}")))
    }

    async fn list_owned(&self) -> Result<Vec<OwnedRunner>, ProvisionError> {
        let vms = self
            .client
            .list_vms()
            .await
            .map_err(|e| ProvisionError::transient(format!("lume list_vms: {e:?}")))?;
        Ok(vms
            .into_iter()
            .filter(|v| v.name.starts_with("cirun-"))
            .map(|v| OwnedRunner {
                name: v.name,
                state: map_vm_state(&v.state),
            })
            .collect())
    }

    /// After clone, the VM is stopped. `run_script_on_vm` starts it and runs
    /// the provision script over SSH.
    async fn run_post_spawn(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        crate::run_script_on_vm(
            &self.client,
            &spec.name,
            &spec.provision_script,
            &spec.login.username,
            &spec.login.password,
            20,
            true,
        )
        .await
        .map(|_| ())
        .map_err(|e| ProvisionError::transient(format!("provision script: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_maps_to_healthy() {
        assert_eq!(map_vm_state("running"), RunnerState::Healthy);
    }

    #[test]
    fn stopped_maps_to_terminated() {
        assert!(matches!(
            map_vm_state("stopped"),
            RunnerState::Terminated { .. }
        ));
    }

    fn dummy_spec(gpu: GpuRequest) -> RunnerSpec {
        RunnerSpec {
            name: "r1".into(),
            provision_script: String::new(),
            image: "tmpl".into(),
            cpu: 2,
            memory_gb: 4,
            disk_gb: 20,
            gpu,
            login: crate::executor::RunnerLogin {
                username: "u".into(),
                password: "p".into(),
            },
        }
    }

    #[test]
    fn validate_rejects_gpu() {
        assert!(validate_gpu(&dummy_spec(GpuRequest::None)).is_ok());
        assert!(matches!(
            validate_gpu(&dummy_spec(GpuRequest::All)),
            Err(ProvisionError::Incompatible(_))
        ));
        assert!(matches!(
            validate_gpu(&dummy_spec(GpuRequest::Count(1))),
            Err(ProvisionError::Incompatible(_))
        ));
    }
}
