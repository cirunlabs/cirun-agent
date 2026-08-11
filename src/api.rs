//! API JSON types exchanged between agent and SaaS backend.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentInfo {
    pub id: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiResponse {
    #[serde(default)]
    pub runners_to_provision: Vec<RunnerToProvision>,
    pub runners_to_delete: Vec<RunnerToDelete>,
}

/// Lume template-matching spec. Only consumed on macos (the executor
/// that uses it is cfg-gated to macos), so the struct is never
/// constructed on linux — `allow(dead_code)` keeps the type definition
/// available for parity without tripping `-D warnings`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TemplateConfig {
    pub image: String,
    pub registry: Option<String>,
    pub organization: Option<String>,
    pub cpu: u32,
    pub memory: u32,
    pub disk: u32,
    pub os: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RunnerLogin {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct RunnerResources {
    pub cpu: u32,
    pub memory: u32,
    pub disk: u32,
}

fn default_max_retries() -> u32 {
    3
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RunnerToProvision {
    pub name: String,
    pub provision_script: String,
    pub image: String,
    pub os: String,
    /// `#[serde(default)]` because the cirun api uses Go's `omitempty` and
    /// drops zero values from the wire — agent must accept the field missing.
    #[serde(default)]
    pub cpu: u32,
    #[serde(default)]
    pub memory: u32,
    #[serde(default)]
    pub disk: u32,
    pub login: RunnerLogin,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Top-level executor field — SaaS sets this from .cirun.yml's
    /// `extra_config.executor`. Wins over `extra_config.executor`. Old SaaS
    /// builds omit this field; agent falls back to extra_config / OS default.
    #[serde(default)]
    pub executor: Option<String>,
    /// Top-level gpu field — same source-of-truth story as `executor`.
    /// Wire format is a string ("none"/"all"/"1"/...).
    #[serde(default)]
    pub gpu: Option<String>,
    #[serde(default)]
    pub extra_config: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RunnerToDelete {
    pub name: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct CommandResponse {
    pub command: String,
    pub output: String,
    pub error: String,
    pub agent: AgentInfo,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// cirun api uses Go's `omitempty` and drops zero values from JSON output.
    /// Agent must deserialize cleanly when cpu/memory/disk are absent.
    #[test]
    fn runner_to_provision_accepts_omitempty_resource_fields() {
        let payload = json!({
            "name": "r1",
            "provision_script": "echo hi",
            "image": "ubuntu:24.04",
            "os": "linux",
            "login": { "username": "u", "password": "p" },
        });
        let r: RunnerToProvision = serde_json::from_value(payload).expect("must deserialize");
        assert_eq!(r.cpu, 0);
        assert_eq!(r.memory, 0);
        assert_eq!(r.disk, 0);
        assert_eq!(r.max_retries, 3);
        assert_eq!(r.executor, None);
    }

    #[test]
    fn runner_to_provision_reads_top_level_executor_and_gpu() {
        let payload = json!({
            "name": "r1",
            "provision_script": "echo hi",
            "image": "ubuntu:24.04",
            "os": "linux",
            "cpu": 4,
            "memory": 8,
            "login": { "username": "u", "password": "p" },
            "executor": "docker",
            "gpu": "all",
        });
        let r: RunnerToProvision = serde_json::from_value(payload).unwrap();
        assert_eq!(r.executor.as_deref(), Some("docker"));
        assert_eq!(r.gpu.as_deref(), Some("all"));
    }
}
