//! Meda adapter: wraps `crate::meda::client::MedaClient` behind the `Executor` trait.

use super::{Executor, OwnedRunner, ProvisionError, RunnerLogin, RunnerSpec, RunnerState};
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
            .map_err(|e| ProvisionError::Transient(format!("meda init: {e}")))?;
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
            Err(e) => Err(ProvisionError::Transient(format!("meda run_vm: {e:?}"))),
        }
    }

    async fn kill(&self, name: &str) -> Result<(), ProvisionError> {
        self.client
            .delete_vm(name)
            .await
            .map_err(|e| ProvisionError::Transient(format!("meda delete_vm: {e:?}")))
    }

    async fn list_owned(&self) -> Result<Vec<OwnedRunner>, ProvisionError> {
        let vms = self
            .client
            .list_vms()
            .await
            .map_err(|e| ProvisionError::Transient(format!("meda list_vms: {e:?}")))?;
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
    /// SSH before the provision script can be pushed. That belongs here, not
    /// in the trait's settle loop.
    async fn run_post_spawn(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        log::info!("Waiting for VM '{}' to get an IP address...", spec.name);
        let ip = self
            .client
            .wait_for_vm_ip(&spec.name, 300)
            .await
            .map_err(|e| ProvisionError::Transient(format!("wait_for_vm_ip: {e:?}")))?;
        log::info!("VM '{}' has IP {}; pushing provision script", spec.name, ip);

        push_provision_script_via_ssh(&spec.name, &ip, &spec.provision_script, &spec.login, true)
            .await
            .map(|_| ())
            .map_err(|e| ProvisionError::Transient(format!("provision script: {e}")))
    }
}

/// SSH the meda VM with its ed25519 key, scp the provision script, run it
/// under `sudo bash`. Detached mode launches in background (short timeout);
/// blocking mode waits up to 10 minutes for completion. Lives next to the
/// `MedaExecutor` that owns the SSH-push lifecycle.
async fn push_provision_script_via_ssh(
    vm_name: &str,
    ip_address: &str,
    script_content: &str,
    login: &RunnerLogin,
    run_detached: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    use crate::ssh::{copy_file, exec, test_connection, SshAuth, SshTarget};
    use std::io::Write;
    use std::time::Instant;
    use tempfile::NamedTempFile;

    log::info!("VM '{}' is ready with IP: {}", vm_name, ip_address);

    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(script_content.as_bytes())?;

    let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let target = SshTarget::new(
        ip_address.to_string(),
        login.username.clone(),
        SshAuth::Key(std::path::PathBuf::from(format!(
            "{}/.meda/ssh/id_ed25519",
            home_dir
        ))),
    )?;

    // Wait for SSH to be ready (VM may still be booting).
    let max_retries = 6usize;
    let mut last_err: Option<String> = None;
    for attempt in 1..=max_retries {
        match test_connection(&target).await {
            Ok(()) => {
                log::info!("✔ SSH ready (attempt {}/{})", attempt, max_retries);
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e.to_string());
                if attempt < max_retries {
                    log::info!("SSH not ready (attempt {}/{}): {}", attempt, max_retries, e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        }
    }
    if let Some(e) = last_err {
        return Err(format!("SSH not reachable after {max_retries} retries: {e}").into());
    }

    let remote_path = format!("/tmp/script_{}.sh", Instant::now().elapsed().as_secs());
    copy_file(&target, temp_file.path(), &remote_path)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // meda scripts run with sudo. Detached: short launch timeout. Blocking: 10min.
    let (timeout_secs, cmd) = if run_detached {
        (
            60u64,
            format!(
                "chmod +x {p} && sudo nohup bash {p} > /tmp/script_stdout.log 2> /tmp/script_stderr.log & echo $!",
                p = remote_path
            ),
        )
    } else {
        (
            600u64,
            format!("chmod +x {p} && sudo bash {p}", p = remote_path),
        )
    };
    let stdout = exec(
        &target,
        &cmd,
        tokio::time::Duration::from_secs(timeout_secs),
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    log::info!("Script execution completed successfully.");
    Ok(stdout)
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
