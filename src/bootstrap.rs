//! Agent boot-time helpers: hostname, sshpass probe, agent-id persistence.
//! Lives here so `main.rs` stays under the file-size cap and is purely the
//! `tokio::main` entrypoint + polling loop.

use crate::api::AgentInfo;
use log::{error, info, warn};
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command as StdCommand;
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

    AgentInfo {
        id,
        hostname: get_hostname(),
        os,
        arch: env::consts::ARCH.to_string(),
    }
}
