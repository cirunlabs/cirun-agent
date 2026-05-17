//! Meda adapter: wraps `crate::meda::client::MedaClient` behind the `Executor` trait.

use super::{Executor, OwnedRunner, ProvisionError, RunnerSpec, RunnerState};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// Map a meda VM state string onto `RunnerState`.
///
/// Note: `Absent` is never returned here — caller handles "VM not found"
/// (`get_vm` error) before invoking this mapper.
pub(super) fn map_vm_state(status: &str) -> RunnerState {
    match status {
        "running" => RunnerState::Healthy,
        "starting" | "creating" | "provisioning" => RunnerState::Starting,
        "stopped" | "paused" | "saved" | "error" | "failed" => RunnerState::Terminated {
            exit_code: None,
            last_logs: String::new(),
        },
        _ => RunnerState::Starting,
    }
}

pub struct MedaExecutor {
    client: Arc<crate::meda::client::MedaClient>,
}

impl MedaExecutor {
    pub fn new() -> Result<Self, ProvisionError> {
        let client = crate::meda::client::MedaClient::new()
            .map_err(|e| ProvisionError::transient(format!("meda init: {e}")))?;
        Ok(Self {
            client: Arc::new(client),
        })
    }
}

#[async_trait]
impl Executor for MedaExecutor {
    fn settle_timeout(&self) -> Duration {
        Duration::from_secs(300)
    }

    async fn inspect(&self, name: &str) -> Result<RunnerState, ProvisionError> {
        match self.client.get_vm(name).await {
            Ok(info) => Ok(map_vm_state(&info.state)),
            Err(_) => Ok(RunnerState::Absent),
        }
    }

    async fn spawn(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        use crate::meda::models::VmRunRequest;
        let req = VmRunRequest {
            image: spec.image.clone(),
            name: Some(spec.name.clone()),
            memory: Some(format!("{}G", spec.memory_gb)),
            cpus: Some(spec.cpu),
            disk_size: Some(format!("{}G", spec.disk_gb)),
        };
        // Map admission-control 503s onto ProvisionError::HostFull so
        // the provision flow can signal "at capacity" upstream without
        // burning the runner's retry budget. Other meda errors are
        // genuine failures and stay as Transient.
        match self.client.run_vm(req).await {
            Ok(()) => Ok(()),
            Err(crate::meda::errors::MedaError::HostFull {
                code,
                message,
                retry_after_secs,
            }) => Err(ProvisionError::HostFull {
                code,
                message,
                retry_after_secs,
            }),
            Err(e) => Err(ProvisionError::transient(format!("meda run_vm: {e:?}"))),
        }
    }

    async fn kill(&self, name: &str) -> Result<(), ProvisionError> {
        self.client
            .delete_vm(name)
            .await
            .map_err(|e| ProvisionError::transient(format!("meda delete_vm: {e:?}")))
    }

    async fn list_owned(&self) -> Result<Vec<OwnedRunner>, ProvisionError> {
        let vms = self
            .client
            .list_vms()
            .await
            .map_err(|e| ProvisionError::transient(format!("meda list_vms: {e:?}")))?;
        Ok(vms
            .into_iter()
            .filter(|v| v.name.starts_with("cirun-"))
            .map(|v| OwnedRunner {
                name: v.name,
                state: map_vm_state(&v.state),
            })
            .collect())
    }

    /// After meda reports the VM "running", we still need to wait for DHCP +
    /// SSH before the provision script can be pushed. The actual
    /// push-script + run-detached + diagnostic-capture lifecycle lives
    /// in `crate::provision_push` so every SSH-based executor shares
    /// the same kill-after-PID-read fix and the same structured
    /// observability bag.
    async fn run_post_spawn(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        use crate::ssh::{SshAuth, SshTarget};
        log::info!("Waiting for VM '{}' to get an IP address...", spec.name);
        let ip = self
            .client
            .wait_for_vm_ip(&spec.name, 300)
            .await
            .map_err(|e| ProvisionError::transient(format!("wait_for_vm_ip: {e:?}")))?;
        log::info!("VM '{}' has IP {}; pushing provision script", spec.name, ip);

        let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let target = SshTarget::new(
            ip.clone(),
            spec.login.username.clone(),
            SshAuth::Key(std::path::PathBuf::from(format!(
                "{home_dir}/.meda/ssh/id_ed25519"
            ))),
        )
        .map_err(|e| ProvisionError::transient(format!("ssh target: {e}")))?;

        let ctx = crate::provision_push::PushContext {
            vm_name: &spec.name,
            vm_ip: &ip,
            target,
            script: &spec.provision_script,
            use_sudo: true,
            detached_exec_timeout: Duration::from_secs(60),
        };
        crate::provision_push::push_and_run_detached(&ctx)
            .await
            .map(|_pid| ())
            .map_err(|f| {
                ProvisionError::transient_with(
                    format!("provision script: {}", f.message),
                    f.diagnostics,
                )
            })
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
    fn starting_maps_to_starting() {
        assert_eq!(map_vm_state("starting"), RunnerState::Starting);
        assert_eq!(map_vm_state("creating"), RunnerState::Starting);
    }

    #[test]
    fn stopped_maps_to_terminated() {
        assert!(matches!(
            map_vm_state("stopped"),
            RunnerState::Terminated { .. }
        ));
        assert!(matches!(
            map_vm_state("failed"),
            RunnerState::Terminated { .. }
        ));
    }

    #[test]
    fn unknown_maps_to_starting() {
        assert_eq!(map_vm_state("weirdo"), RunnerState::Starting);
    }
}
