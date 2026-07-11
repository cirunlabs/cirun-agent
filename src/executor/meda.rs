//! Meda adapter: wraps `crate::meda::client::MedaClient` behind the `Executor` trait.

use super::{Executor, GpuRequest, OwnedRunner, ProvisionError, RunnerSpec, RunnerState};
use crate::gpu::GpuAllocator;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

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
    gpus: Arc<GpuAllocator>,
    /// GPU lease state is rebuilt from meda exactly once, lazily, before the
    /// first allocation. Lazy because reconcile needs an async list_vms call
    /// and new() is sync.
    gpu_reconciled: OnceCell<()>,
}

impl MedaExecutor {
    pub fn new() -> Result<Self, ProvisionError> {
        let client = crate::meda::client::MedaClient::new()
            .map_err(|e| ProvisionError::transient(format!("meda init: {e}")))?;
        Ok(Self {
            client: Arc::new(client),
            gpus: Arc::new(GpuAllocator::new(discover_host_gpus())),
            gpu_reconciled: OnceCell::new(),
        })
    }

    /// Rebuild GPU leases from what meda reports as attached, once.
    async fn ensure_gpu_reconciled(&self) -> Result<(), ProvisionError> {
        self.gpu_reconciled
            .get_or_try_init(|| async {
                let vms = self
                    .client
                    .list_vms()
                    .await
                    .map_err(|e| ProvisionError::transient(format!("meda list_vms: {e:?}")))?;
                // Only running VMs physically pin a VFIO device. Stopped VMs
                // (notably meda's image template after a prep boot) keep the
                // device in their recorded config but hold nothing — counting
                // them starves every future GPU lease.
                let running: Vec<(String, Vec<String>)> = vms
                    .into_iter()
                    .filter(|v| v.state == "running" && !v.devices.is_empty())
                    .map(|v| (v.name, v.devices))
                    .collect();
                self.gpus.reconcile(&running);
                for (device, holder) in self.gpus.snapshot() {
                    log::info!(
                        "GPU inventory: {device} {}",
                        holder.map_or("free".to_string(), |vm| format!("leased to {vm}"))
                    );
                }
                Ok(())
            })
            .await
            .map(|_| ())
    }
}

/// Host GPU inventory: CIRUN_VFIO_DEVICES (comma-separated sysfs paths)
/// overrides; otherwise auto-detect NVIDIA display devices on the PCI bus.
fn discover_host_gpus() -> Vec<String> {
    if let Ok(explicit) = std::env::var("CIRUN_VFIO_DEVICES") {
        return explicit
            .split(",")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
    }
    crate::gpu::discover_nvidia_gpus(Path::new("/sys/bus/pci/devices"))
}

/// Map a GPU allocation failure to a provision error. GPU exhaustion ("no
/// free GPU right now") is backpressure, not a failure: return `HostFull` so
/// the backend queues the job as "at capacity" and re-fans it promptly,
/// instead of marking it failed and applying retry backoff (which strands
/// the GPU idle while jobs wait out their backoff timers).
fn gpu_alloc_error(e: crate::gpu::GpuError) -> ProvisionError {
    ProvisionError::HostFull {
        code: "gpu_at_capacity".to_string(),
        message: e.to_string(),
        retry_after_secs: 5,
    }
}

/// Build the meda run request for a spec plus its leased devices. Pure, so
/// the GPU wiring is testable without a live meda.
pub(super) fn build_run_request(
    spec: &RunnerSpec,
    devices: Vec<String>,
) -> crate::meda::models::VmRunRequest {
    crate::meda::models::VmRunRequest {
        image: spec.image.clone(),
        name: Some(spec.name.clone()),
        memory: Some(format!("{}G", spec.memory_gb)),
        cpus: Some(spec.cpu),
        disk_size: Some(format!("{}G", spec.disk_gb)),
        devices,
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
        let devices = if matches!(spec.gpu, GpuRequest::None) {
            Vec::new()
        } else {
            self.ensure_gpu_reconciled().await?;
            self.gpus
                .allocate(&spec.gpu, &spec.name)
                .map_err(gpu_alloc_error)?
        };
        if !devices.is_empty() {
            log::info!("VM '{}' leasing GPUs: {devices:?}", spec.name);
        }
        let req = build_run_request(spec, devices);
        // Map admission-control 503s onto ProvisionError::HostFull so
        // the provision flow can signal "at capacity" upstream without
        // burning the runner's retry budget. Other meda errors are
        // genuine failures and stay as Transient.
        let result = match self.client.run_vm(req).await {
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
        };
        // A failed boot must not strand its GPU lease.
        if result.is_err() {
            self.gpus.release(&spec.name);
        }
        result
    }

    async fn kill(&self, name: &str) -> Result<(), ProvisionError> {
        self.client
            .delete_vm(name)
            .await
            .map_err(|e| ProvisionError::transient(format!("meda delete_vm: {e:?}")))?;
        let freed = self.gpus.release(name);
        if freed > 0 {
            log::info!("VM '{name}' released {freed} GPU(s)");
        }
        Ok(())
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

#[cfg(test)]
mod gpu_wiring_tests {
    use super::*;
    use crate::executor::GpuRequest;

    fn spec(gpu: GpuRequest) -> RunnerSpec {
        RunnerSpec {
            name: "cirun-test-vm".into(),
            provision_script: String::new(),
            image: "ubuntu:latest".into(),
            cpu: 4,
            memory_gb: 8,
            disk_gb: 50,
            gpu,
            docker_privileged: false,
            docker_mount_socket: false,
            login: crate::executor::RunnerLogin {
                username: String::new(),
                password: String::new(),
            },
        }
    }

    #[test]
    fn gpu_exhaustion_maps_to_host_full_not_failure() {
        let a = crate::gpu::GpuAllocator::new(vec!["/sys/pci/gpu0".into()]);
        a.allocate(&GpuRequest::Count(1), "vm-a").unwrap();
        let err = a.allocate(&GpuRequest::All, "vm-b").unwrap_err();
        match gpu_alloc_error(err) {
            ProvisionError::HostFull {
                code,
                retry_after_secs,
                ..
            } => {
                assert_eq!(code, "gpu_at_capacity");
                assert!(retry_after_secs > 0);
            }
            other => panic!("GPU exhaustion must be HostFull backpressure, got {other:?}"),
        }
    }

    #[test]
    fn run_request_carries_leased_devices() {
        let req = build_run_request(
            &spec(GpuRequest::Count(1)),
            vec!["/sys/bus/pci/devices/0000:01:00.0".into()],
        );
        assert_eq!(req.devices, vec!["/sys/bus/pci/devices/0000:01:00.0"]);
        assert_eq!(req.name.as_deref(), Some("cirun-test-vm"));
    }

    #[test]
    fn run_request_omits_devices_for_cpu_jobs() {
        let req = build_run_request(&spec(GpuRequest::None), Vec::new());
        assert!(req.devices.is_empty());
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("devices"),
            "empty devices must not serialize: {json}"
        );
    }
}
