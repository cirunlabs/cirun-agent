use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct VmCreateRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "disk")]
    pub disk_size: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmRunRequest {
    pub image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "disk")]
    pub disk_size: Option<String>,
    /// VFIO device paths for PCI passthrough (meda `devices` field).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// VFIO devices currently attached (reported by meda); drives GPU
    /// lease reconciliation after agent restart.
    #[serde(default)]
    pub devices: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmListResponse {
    pub vms: Vec<VmInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmDetailResponse {
    pub name: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
}
