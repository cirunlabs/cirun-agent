// lume.rs
#![allow(dead_code)]
pub mod lume {
    use reqwest::{Client, Error as ReqwestError};
    use serde::{Deserialize, Serialize};
    use std::time::Duration;

    const DEFAULT_API_URL: &str = "http://localhost:3000/lume";
    const DEFAULT_TIMEOUT: u64 = 300; // 5 minutes

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
        pub no_display: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub shared_directories: Option<Vec<SharedDirectory>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub recovery_mode: Option<bool>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct CloneConfig {
        pub name: String,
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
    }

    #[derive(Debug)]
    pub enum LumeError {
        RequestError(ReqwestError),
        ApiError(String),
    }

    impl From<ReqwestError> for LumeError {
        fn from(error: ReqwestError) -> Self {
            LumeError::RequestError(error)
        }
    }

    pub struct LumeClient {
        client: Client,
        base_url: String,
    }

    impl LumeClient {
        pub fn new() -> Result<Self, LumeError> {
            Self::with_base_url(DEFAULT_API_URL)
        }

        pub fn with_base_url(base_url: &str) -> Result<Self, LumeError> {
            let client = Client::builder()
                .timeout(Duration::from_secs(DEFAULT_TIMEOUT))
                .connect_timeout(Duration::from_secs(DEFAULT_TIMEOUT))
                .build()
                .map_err(LumeError::from)?;

            Ok(Self {
                client,
                base_url: base_url.to_string(),
            })
        }

        pub async fn create_vm(&self, config: VmConfig) -> Result<(), LumeError> {
            let url = format!("{}/vms", self.base_url);

            let response = self.client
                .post(&url)
                .json(&config)
                .send()
                .await?;

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(LumeError::ApiError(format!("Failed to create VM: {}", error_text)));
            }

            Ok(())
        }

        pub async fn run_vm(&self, name: &str, config: Option<RunConfig>) -> Result<(), LumeError> {
            let url = format!("{}/vms/{}/run", self.base_url, name);

            let mut request = self.client.post(&url);

            if let Some(run_config) = config {
                request = request.json(&run_config);
            }

            let response = request.send().await?;

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(LumeError::ApiError(format!("Failed to run VM: {}", error_text)));
            }

            Ok(())
        }

        pub async fn clone_vm(&self, source_name: &str, new_name: &str) -> Result<(), LumeError> {
            let url = format!("{}/vms/{}/clone", self.base_url, source_name);

            let config = CloneConfig {
                name: source_name.to_string(),
                new_name: new_name.to_string(),
            };

            let response = self.client
                .post(&url)
                .json(&config)
                .send()
                .await?;

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(LumeError::ApiError(format!("Failed to clone VM: {}", error_text)));
            }

            Ok(())
        }

        pub async fn delete_vm(&self, name: &str) -> Result<(), LumeError> {
            let url = format!("{}/vms/{}", self.base_url, name);

            let response = self.client
                .delete(&url)
                .send()
                .await?;

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(LumeError::ApiError(format!("Failed to delete VM: {}", error_text)));
            }

            Ok(())
        }

        pub async fn list_vms(&self) -> Result<Vec<VmInfo>, LumeError> {
            let url = format!("{}/vms", self.base_url);

            let response = self.client
                .get(&url)
                .send()
                .await?;

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(LumeError::ApiError(format!("Failed to list VMs: {}", error_text)));
            }

            let vms = response.json::<Vec<VmInfo>>().await?;
            Ok(vms)
        }

        pub async fn get_vm(&self, name: &str) -> Result<VmInfo, LumeError> {
            let url = format!("{}/vms/{}", self.base_url, name);

            let response = self.client
                .get(&url)
                .send()
                .await?;

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(LumeError::ApiError(format!("Failed to get VM details: {}", error_text)));
            }

            let vm_info = response.json::<VmInfo>().await?;
            Ok(vm_info)
        }
    }
}
