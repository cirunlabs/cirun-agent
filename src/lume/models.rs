use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct VmConfig {
    pub name: String,
    pub os: String,
    pub cpu: u32,
    pub memory: String,
    pub disk_size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipsw: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SharedDirectory {
    pub host_path: String,
    pub read_only: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RunConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "noDisplay")]
    pub no_display: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "sharedDirectories")]
    pub shared_directories: Option<Vec<SharedDirectory>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "recoveryMode")]
    pub recovery_mode: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CloneConfig {
    pub name: String,
    #[serde(rename = "newName")]
    pub new_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiskSize {
    pub allocated: u64,
    pub total: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    #[serde(rename = "status")]
    pub state: String,
    pub os: String,
    #[serde(rename = "cpuCount")]
    pub cpu: u32,
    #[serde(rename = "memorySize")]
    pub memory: u64,
    #[serde(rename = "diskSize")]
    pub disk_size: DiskSize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(rename = "ipAddress", default)]
    pub ip_address: Option<String>,
}
