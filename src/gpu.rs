//! Exclusive GPU leasing for VM passthrough.
//!
//! A physical GPU can be attached to at most one VM at a time (VFIO takes
//! whole-device ownership), so every provision that wants GPUs must lease
//! them here first. The allocator is the single source of truth inside the
//! agent process; across restarts it is rebuilt by [`GpuAllocator::reconcile`]
//! from what the VMM reports as actually attached, so state can never drift
//! into double-allocation after a crash.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use crate::executor::GpuRequest;

/// PCI vendor id for NVIDIA.
const NVIDIA_VENDOR: &str = "0x10de";

#[derive(Debug, PartialEq, Eq)]
pub enum GpuError {
    /// Not enough free devices; message names current holders for operators.
    Insufficient {
        requested: String,
        free: usize,
        detail: String,
    },
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuError::Insufficient {
                requested,
                free,
                detail,
            } => write!(
                f,
                "insufficient GPUs: requested {requested}, {free} free; {detail}"
            ),
        }
    }
}

pub struct GpuAllocator {
    inner: Mutex<BTreeMap<String, Option<String>>>, // device path -> lease holder
}

impl GpuAllocator {
    /// Build an allocator over a fixed device inventory (sysfs paths).
    pub fn new(devices: Vec<String>) -> Self {
        Self {
            inner: Mutex::new(devices.into_iter().map(|d| (d, None)).collect()),
        }
    }

    /// Rebuild lease state from what the VMM reports as running. Devices
    /// attached to running VMs become leased; devices we have never seen are
    /// adopted into the inventory as leased (a VM holding an unknown device
    /// must still block anyone else from getting it).
    pub fn reconcile(&self, running: &[(String, Vec<String>)]) {
        let mut inner = self.inner.lock().unwrap();
        for holder in inner.values_mut() {
            *holder = None;
        }
        for (vm, devices) in running {
            for d in devices {
                inner.insert(d.clone(), Some(vm.clone()));
            }
        }
    }

    /// Lease devices for a VM. `None` requests lease nothing. `Count(n)`
    /// requires exactly n free devices. `All` takes every currently free
    /// device and requires at least one. Idempotent per VM: if the VM
    /// already holds leases (provision retry), the same devices are
    /// returned without allocating more.
    pub fn allocate(&self, req: &GpuRequest, vm: &str) -> Result<Vec<String>, GpuError> {
        let mut inner = self.inner.lock().unwrap();

        let held: Vec<String> = inner
            .iter()
            .filter(|(_, h)| h.as_deref() == Some(vm))
            .map(|(d, _)| d.clone())
            .collect();
        if !held.is_empty() {
            return Ok(held);
        }

        let wanted = match req {
            GpuRequest::None => return Ok(Vec::new()),
            GpuRequest::Count(n) => *n as usize,
            GpuRequest::All => usize::MAX, // resolved against free below
        };

        let free: Vec<String> = inner
            .iter()
            .filter(|(_, h)| h.is_none())
            .map(|(d, _)| d.clone())
            .collect();

        let take = if matches!(req, GpuRequest::All) {
            free.len()
        } else {
            wanted
        };
        if take == 0 || free.len() < take {
            let holders: Vec<String> = inner
                .iter()
                .filter_map(|(d, h)| h.as_ref().map(|vm| format!("{d} leased to {vm}")))
                .collect();
            let detail = if holders.is_empty() {
                "no GPUs in inventory".to_string()
            } else {
                holders.join(", ")
            };
            return Err(GpuError::Insufficient {
                requested: match req {
                    GpuRequest::All => "all".to_string(),
                    GpuRequest::Count(n) => n.to_string(),
                    GpuRequest::None => unreachable!(),
                },
                free: free.len(),
                detail,
            });
        }

        let leased: Vec<String> = free.into_iter().take(take).collect();
        for d in &leased {
            inner.insert(d.clone(), Some(vm.to_string()));
        }
        Ok(leased)
    }

    /// Release every lease held by a VM. Returns how many were freed.
    /// Safe to call for VMs holding nothing (failed before allocation,
    /// non-GPU VMs, double release).
    pub fn release(&self, vm: &str) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let mut freed = 0;
        for holder in inner.values_mut() {
            if holder.as_deref() == Some(vm) {
                *holder = None;
                freed += 1;
            }
        }
        freed
    }

    /// Current inventory with lease holders, for logs and diagnostics.
    pub fn snapshot(&self) -> Vec<(String, Option<String>)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(d, h)| (d.clone(), h.clone()))
            .collect()
    }
}

/// Scan a sysfs PCI tree for NVIDIA display-class devices. `root` is
/// `/sys/bus/pci/devices` in production, a fixture directory in tests.
/// Display controllers have PCI class 0x03xxxx (VGA 0x0300, 3D 0x0302);
/// this skips NVIDIA audio functions (class 0x0403) that share the card.
pub fn discover_nvidia_gpus(root: &Path) -> Vec<String> {
    let mut gpus = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return gpus,
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .unwrap_or_default()
                .trim()
                .to_lowercase()
        };
        if read("vendor") == NVIDIA_VENDOR && read("class").starts_with("0x03") {
            gpus.push(dir.to_string_lossy().into_owned());
        }
    }
    gpus.sort();
    gpus
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn two_gpu_alloc() -> GpuAllocator {
        GpuAllocator::new(vec!["/sys/pci/gpu0".into(), "/sys/pci/gpu1".into()])
    }

    #[test]
    fn non_gpu_request_leases_nothing() {
        let a = two_gpu_alloc();
        assert_eq!(
            a.allocate(&GpuRequest::None, "vm-cpu").unwrap(),
            Vec::<String>::new()
        );
        assert!(a.snapshot().iter().all(|(_, h)| h.is_none()));
    }

    #[test]
    fn same_gpu_never_leased_twice() {
        let a = two_gpu_alloc();
        let g1 = a.allocate(&GpuRequest::Count(1), "vm-a").unwrap();
        let g2 = a.allocate(&GpuRequest::Count(1), "vm-b").unwrap();
        assert_ne!(g1, g2);
        let err = a.allocate(&GpuRequest::Count(1), "vm-c").unwrap_err();
        match err {
            GpuError::Insufficient {
                free, ref detail, ..
            } => {
                assert_eq!(free, 0);
                assert!(
                    detail.contains("vm-a") && detail.contains("vm-b"),
                    "{detail}"
                );
            }
        }
    }

    #[test]
    fn count_exceeding_free_fails_without_partial_lease() {
        let a = two_gpu_alloc();
        a.allocate(&GpuRequest::Count(1), "vm-a").unwrap();
        assert!(a.allocate(&GpuRequest::Count(2), "vm-b").is_err());
        // the failed request must not have leaked a partial lease
        assert_eq!(a.snapshot().iter().filter(|(_, h)| h.is_some()).count(), 1);
    }

    #[test]
    fn all_takes_every_free_device_and_fails_on_zero() {
        let a = two_gpu_alloc();
        a.allocate(&GpuRequest::Count(1), "vm-a").unwrap();
        let rest = a.allocate(&GpuRequest::All, "vm-b").unwrap();
        assert_eq!(rest.len(), 1);
        assert!(a.allocate(&GpuRequest::All, "vm-c").is_err());
    }

    #[test]
    fn release_frees_for_reuse_and_is_safe_when_empty() {
        let a = two_gpu_alloc();
        let got = a.allocate(&GpuRequest::All, "vm-a").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(a.release("vm-a"), 2);
        assert_eq!(a.release("vm-a"), 0); // double release
        assert_eq!(a.release("vm-never-existed"), 0);
        assert_eq!(a.allocate(&GpuRequest::Count(2), "vm-b").unwrap().len(), 2);
    }

    #[test]
    fn allocate_is_idempotent_per_vm() {
        let a = two_gpu_alloc();
        let first = a.allocate(&GpuRequest::Count(1), "vm-a").unwrap();
        let retry = a.allocate(&GpuRequest::Count(1), "vm-a").unwrap();
        assert_eq!(first, retry);
        assert_eq!(a.snapshot().iter().filter(|(_, h)| h.is_some()).count(), 1);
    }

    #[test]
    fn reconcile_rebuilds_leases_from_running_vms() {
        let a = two_gpu_alloc();
        a.reconcile(&[("vm-old".to_string(), vec!["/sys/pci/gpu1".to_string()])]);
        let got = a.allocate(&GpuRequest::All, "vm-new").unwrap();
        assert_eq!(got, vec!["/sys/pci/gpu0".to_string()]);
        // vm-old's device stays blocked
        assert!(a.allocate(&GpuRequest::Count(1), "vm-other").is_err());
    }

    #[test]
    fn reconcile_adopts_unknown_devices_as_leased() {
        let a = two_gpu_alloc();
        a.reconcile(&[("vm-x".to_string(), vec!["/sys/pci/gpu9".to_string()])]);
        // unknown device is tracked and blocked, inventory devices stay free
        assert_eq!(a.allocate(&GpuRequest::All, "vm-new").unwrap().len(), 2);
        let snap = a.snapshot();
        assert!(snap
            .iter()
            .any(|(d, h)| d == "/sys/pci/gpu9" && h.as_deref() == Some("vm-x")));
    }

    #[test]
    fn concurrent_allocations_never_double_lease() {
        let a = Arc::new(GpuAllocator::new(
            (0..4).map(|i| format!("/sys/pci/gpu{i}")).collect(),
        ));
        let mut handles = Vec::new();
        for t in 0..16 {
            let a = Arc::clone(&a);
            handles.push(std::thread::spawn(move || {
                a.allocate(&GpuRequest::Count(1), &format!("vm-{t}")).ok()
            }));
        }
        let leased: Vec<String> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .flatten()
            .collect();
        assert_eq!(leased.len(), 4, "exactly the inventory size must be leased");
        let mut dedup = leased.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            leased.len(),
            "no device leased twice: {leased:?}"
        );
    }

    #[test]
    fn discovery_finds_only_nvidia_display_devices() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, vendor: &str, class: &str| {
            let d = dir.path().join(name);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("vendor"), vendor).unwrap();
            std::fs::write(d.join("class"), class).unwrap();
        };
        mk("0000:01:00.0", "0x10de", "0x030000"); // NVIDIA VGA -> yes
        mk("0000:01:00.1", "0x10de", "0x040300"); // NVIDIA audio fn -> no
        mk("0000:00:02.0", "0x8086", "0x030000"); // Intel iGPU -> no
        mk("0000:02:00.0", "0x10de", "0x030200"); // NVIDIA 3D -> yes
        let found = discover_nvidia_gpus(dir.path());
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].ends_with("0000:01:00.0"));
        assert!(found[1].ends_with("0000:02:00.0"));
    }
}
