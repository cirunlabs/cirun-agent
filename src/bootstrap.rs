//! Agent boot-time helpers: hostname, sshpass probe, agent-id persistence.
//! Lives here so `main.rs` stays under the file-size cap and is purely the
//! `tokio::main` entrypoint + polling loop.

use crate::api::AgentInfo;
use log::{error, info, warn};
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command as StdCommand;
use sysinfo::{Disks, System};
use uuid::Uuid;

pub fn get_hostname() -> String {
    if let Ok(hostname) = env::var("HOSTNAME") {
        return hostname;
    }

    if let Ok(output) = StdCommand::new("hostname").output() {
        if let Ok(hostname) = String::from_utf8(output.stdout) {
            return hostname.trim().to_string();
        }
    }

    "unknown-host".to_string()
}

/// `sshpass` is only required for the lume executor's SSH provisioning path.
/// macOS doesn't ship it; we warn (not exit) so docker dispatch can still run.
pub fn check_sshpass_installed() -> bool {
    match StdCommand::new("which").arg("sshpass").output() {
        Ok(output) => {
            if output.status.success() {
                info!("[OK] sshpass is installed");
                true
            } else {
                error!("✘ sshpass is not installed");
                error!("VM provisioning requires sshpass for SSH authentication");
                error!("Install it using: brew install sshpass");
                false
            }
        }
        Err(e) => {
            warn!("Failed to check for sshpass: {}", e);
            false
        }
    }
}

/// Load (or generate + persist) a stable agent UUID. The file lives at
/// `$HOME/.agent_id` by default — survives restarts so the same agent keeps
/// its identity with the cirun api.
pub fn get_agent_info(id_file: &str) -> AgentInfo {
    let id = if Path::new(id_file).exists() {
        match fs::read_to_string(id_file) {
            Ok(id) => {
                let id = id.trim().to_string();
                info!("Using existing agent ID: {}", id);
                id
            }
            Err(e) => {
                error!("Failed to read agent ID file: {}", e);
                let new_id = Uuid::new_v4().to_string();
                info!("Generated new agent ID: {}", new_id);
                if let Err(e) = fs::write(id_file, &new_id) {
                    error!("Failed to write agent ID to file: {}", e);
                }
                new_id
            }
        }
    } else {
        let new_id = Uuid::new_v4().to_string();
        info!("Generated new agent ID: {}", new_id);
        if let Err(e) = fs::write(id_file, &new_id) {
            error!("Failed to write agent ID to file: {}", e);
        }
        new_id
    };

    // CIRUN_AGENT_OS overrides the reported host OS. Useful when the agent
    // serves a different container OS than its host (e.g. macOS host running
    // linux containers via Docker Desktop).
    let os = env::var("CIRUN_AGENT_OS").unwrap_or_else(|_| env::consts::OS.to_string());

    let sys = System::new_all();

    AgentInfo {
        id,
        hostname: get_hostname(),
        os,
        arch: env::consts::ARCH.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        cpu_cores: sys.cpus().len() as u32,
        memory_gb: (sys.total_memory() / BYTES_PER_GB) as u32,
        disk_gb: root_disk_capacity_gb(),
    }
}

const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;

/// Total capacity of the root filesystem in GB. Falls back to the first
/// disk in the list if none is mounted exactly at "/" (e.g. some Windows
/// hosts), and to 0 if the host reports no disks at all.
fn root_disk_capacity_gb() -> u32 {
    let disks = Disks::new_with_refreshed_list();
    let root = disks
        .list()
        .iter()
        .find(|d| d.mount_point() == Path::new("/"))
        .or_else(|| disks.list().first());
    match root {
        Some(disk) => (disk.total_space() / BYTES_PER_GB) as u32,
        None => 0,
    }
}
