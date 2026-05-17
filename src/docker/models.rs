use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerContainerSpec {
    pub name: String,
    pub image: String,
    pub gpus: GpuSelection,
    pub cpus: Option<u32>,
    pub memory_gb: Option<u32>,
    pub env: Vec<(String, String)>,
    pub command: ContainerCommand,
    /// `--privileged`. Enables docker-in-docker mode of the
    /// cirun-docker-runner-image (the entrypoint starts an internal
    /// dockerd). Off by default; opt in per-runner via
    /// `.cirun.yml`'s `extra_config.privileged: true`.
    #[serde(default)]
    pub privileged: bool,
    /// `-v /var/run/docker.sock:/var/run/docker.sock`. Gives the job
    /// docker-out-of-docker access via the host's daemon. Off by
    /// default; opt in via `extra_config.docker_socket_mount: true`.
    /// Mutually independent from `privileged` — set whichever fits
    /// the security tradeoff for the workload.
    #[serde(default)]
    pub mount_docker_socket: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GpuSelection {
    None,
    All,
    /// Pin N GPUs. Emits `--gpus N` to docker, which picks the first N free devices.
    Count(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContainerCommand {
    /// Run a single shell script via `bash -lc <script>`
    Script(String),
    /// Run an entrypoint with the given argv
    Argv(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub name: String,
    pub state: String,
}
