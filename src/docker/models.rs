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
