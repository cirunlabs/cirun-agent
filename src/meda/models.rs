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
