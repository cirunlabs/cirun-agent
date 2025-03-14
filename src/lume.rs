// lume.rs
#![allow(dead_code)]
pub mod lume {
    use std::fmt;
    use reqwest::{Client, Error as ReqwestError};
    use serde::{Deserialize, Serialize};
    use std::time::Duration;
    use serde::de::StdError;

    const DEFAULT_API_URL: &str = "http://localhost:3000/lume";
    const CONNECT_TIMEOUT: u64 = 6000; // 5 minutes
    const MAX_TIMEOUT: u64 = 5000; // 5 minutes

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

    #[derive(Debug)]
    pub enum LumeError {
        RequestError(ReqwestError),
        ApiError(String),
    }

    impl fmt::Display for LumeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                LumeError::RequestError(err) => write!(f, "Request error: {}", err),
                LumeError::ApiError(msg) => write!(f, "API error: {}", msg),
            }
        }
    }

    impl StdError for LumeError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            match self {
                LumeError::RequestError(err) => Some(err),
                LumeError::ApiError(_) => None,
            }
        }
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
                .timeout(Duration::from_millis(MAX_TIMEOUT))         // 5 seconds
                .connect_timeout(Duration::from_millis(CONNECT_TIMEOUT))  // 6 seconds
                .pool_idle_timeout(Duration::from_secs(90))        // Keep connections alive
                .pool_max_idle_per_host(10)                        // Connection pooling
                .tcp_keepalive(Duration::from_secs(60))            // TCP keepalive
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
            let url = format!("{}/vms/clone", self.base_url);

            let config = CloneConfig {
                name: source_name.to_string(),
                new_name: new_name.to_string(),
            };

            log::info!("Cloning VM {} to {}", source_name, new_name);
            let response = self.client
                .post(&url)
                .json(&config)
                .send()
                .await?;

            let status = response.status();
            log::info!("Clone operation response status: {}", status);
            if !response.status().is_success() {
                log::info!("Cloning VM {} to {} FAILED", source_name, new_name);
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
            log::info!("Getting VM details for {}", name);
            let url = format!("{}/vms/{}", self.base_url, name);

            let max_retries = 3;
            let mut attempts = 0;
            let retry_delay = Duration::from_secs(2);

            loop {
                attempts += 1;

                match self.client.get(&url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            match response.json::<VmInfo>().await {
                                Ok(vm_info) => return Ok(vm_info),
                                Err(e) => {
                                    log::warn!("Failed to parse VM details JSON (attempt {}/{}): {:?}",
                                      attempts, max_retries, e);

                                    if attempts >= max_retries {
                                        return Err(LumeError::RequestError(e));
                                    }
                                }
                            }
                        } else {
                            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                            log::warn!("Failed to get VM details for {} (attempt {}/{}): {}",
                              name, attempts, max_retries, error_text);

                            if attempts >= max_retries {
                                return Err(LumeError::ApiError(format!("Failed to get VM details: {}", error_text)));
                            }
                        }
                    },
                    Err(e) => {
                        log::warn!("Request error while getting VM details for {} (attempt {}/{}): {:?}",
                          name, attempts, max_retries, e);

                        if attempts >= max_retries {
                            return Err(LumeError::RequestError(e));
                        }
                    }
                }

                // Wait before retrying
                log::info!("Retrying in {} seconds...", retry_delay.as_secs());
                tokio::time::sleep(retry_delay).await;
            }
        }
    }
}
