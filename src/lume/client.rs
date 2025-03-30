use backon::{ExponentialBuilder, Retryable};
use log::{error, info, warn};
use reqwest::Client;
use std::time::Duration;

use crate::lume::errors::LumeError;
use crate::lume::models::{CloneConfig, RunConfig, VmConfig, VmInfo};

const DEFAULT_API_URL: &str = "http://127.0.0.1:3000/lume";
const CONNECT_TIMEOUT: u64 = 6000;
const MAX_TIMEOUT: u64 = 5000;

pub struct LumeClient {
    client: Client,
    base_url: String,
}

impl LumeClient {
    pub fn new() -> Result<Self, LumeError> {
        Self::with_base_url(DEFAULT_API_URL)
    }

    // Get the base URL of the Lume API
    pub fn get_base_url(&self) -> &str {
        &self.base_url
    }

    pub fn with_base_url(base_url: &str) -> Result<Self, LumeError> {
        let client = Client::builder()
            .http1_only()
            .timeout(Duration::from_secs(MAX_TIMEOUT))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(LumeError::from)?;

        Ok(Self {
            client,
            base_url: base_url.to_string(),
        })
    }

    #[allow(dead_code)]
    pub async fn create_vm(&self, config: VmConfig) -> Result<(), LumeError> {
        let url = format!("{}/vms", self.base_url);

        let response = self.client.post(&url).json(&config).send().await?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(LumeError::ApiError(format!(
                "Failed to create VM: {}",
                error_text
            )));
        }

        Ok(())
    }

    pub async fn run_vm(&self, name: &str, config: Option<RunConfig>) -> Result<(), LumeError> {
        let url = format!("{}/vms/{}/run", self.base_url, name);

        let mut request = self.client.post(&url);

        if let Some(run_config) = config {
            request = request.json(&run_config);
        }

        info!("Sending request to start VM: {}", name);

        let response = request.send().await?;
        let status = response.status(); // Clone status before calling .text()
        let response_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read response body".to_string());

        info!(
            "VM Run API Response: Status = {}, Body = {}",
            status, response_text
        );

        if !status.is_success() {
            // Use the cloned status here
            return Err(LumeError::ApiError(format!(
                "Failed to run VM: {}",
                response_text
            )));
        }

        info!("Successfully started VM: {}", name);
        Ok(())
    }

    pub async fn clone_vm(&self, source_name: &str, new_name: &str) -> Result<(), LumeError> {
        let url = format!("{}/vms/clone", self.base_url);

        let config = CloneConfig {
            name: source_name.to_string(),
            new_name: new_name.to_string(),
        };

        info!("Cloning VM {} to {}", source_name, new_name);

        let send_clone_request = || async {
            let response = self
                .client
                .post(&url)
                .json(&config)
                .send()
                .await
                .map_err(|e| LumeError::ApiError(format!("HTTP request failed: {:?}", e)))?;

            let status = response.status();
            info!("Clone operation response status: {}", status);

            if !status.is_success() {
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                return Err(LumeError::ApiError(format!(
                    "Failed to clone VM: {}",
                    error_text
                )));
            }

            Ok(())
        };

        // Retry logic with proper error conversion
        send_clone_request
            .retry(ExponentialBuilder::default().with_max_times(5)) // Retry max 5 times
            .sleep(tokio::time::sleep)
            .when(|e| matches!(e, LumeError::ApiError(_))) // Retry only on API errors
            .notify(|err, dur| warn!("Retrying VM clone after {:?}: {:?}", dur, err))
            .await
            .map_err(|e| LumeError::ApiError(format!("Retry exhausted: {:?}", e)))?; // Convert final error to LumeError

        info!("VM {} successfully cloned to {}", source_name, new_name);
        Ok(())
    }

    pub async fn delete_vm(&self, name: &str) -> Result<(), LumeError> {
        let url = format!("{}/vms/{}", self.base_url, name);

        info!("Deleting VM {}", name);

        let send_delete_request =
            || async {
                let response =
                    self.client.delete(&url).send().await.map_err(|e| {
                        LumeError::ApiError(format!("HTTP request failed: {:?}", e))
                    })?;

                let status = response.status();
                let response_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());

                info!("Delete operation response status: {}", status);
                info!("Delete operation response body: {}", response_text);

                if !status.is_success() {
                    return Err(LumeError::ApiError(format!(
                        "Failed to delete VM: {}",
                        response_text
                    )));
                }
                Ok(())
            };

        // Retry logic with proper error conversion
        send_delete_request
            .retry(ExponentialBuilder::default().with_max_times(5)) // Retry max 5 times
            .sleep(tokio::time::sleep)
            .when(|e| matches!(e, LumeError::ApiError(_))) // Retry only on API errors
            .notify(|err, dur| warn!("Retrying VM deletion after {:?}: {:?}", dur, err))
            .await
            .map_err(|e| LumeError::ApiError(format!("Retry exhausted: {:?}", e)))?; // Convert final error to LumeError

        info!("VM {} successfully deleted", name);
        Ok(())
    }

    pub async fn list_vms(&self) -> Result<Vec<VmInfo>, LumeError> {
        let url = format!("{}/vms", self.base_url);

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(LumeError::ApiError(format!(
                "Failed to list VMs: {}",
                error_text
            )));
        }

        let vms = response.json::<Vec<VmInfo>>().await?;
        Ok(vms)
    }

    pub async fn get_vm(&self, name: &str) -> Result<VmInfo, LumeError> {
        info!("Getting VM details for {}", name);
        let url = format!("{}/vms/{}", self.base_url, name);

        let max_retries = 3;
        let mut attempts = 0;
        let retry_delay = Duration::from_millis(300);

        loop {
            attempts += 1;
            match self.client.get(&url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        match response.json::<VmInfo>().await {
                            Ok(vm_info) => return Ok(vm_info),
                            Err(e) => {
                                warn!(
                                    "Failed to parse VM details JSON (attempt {}/{}): {:?}",
                                    attempts, max_retries, e
                                );
                                if attempts >= max_retries {
                                    return Err(LumeError::RequestError(e));
                                }
                            }
                        }
                    } else {
                        let error_text = response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Unknown error".to_string());
                        if attempts >= max_retries {
                            return Err(LumeError::ApiError(format!(
                                "Failed to get VM details: {}",
                                error_text
                            )));
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Request error while getting VM details for {} (attempt {}/{}): {:?}",
                        name, attempts, max_retries, e
                    );

                    if attempts >= max_retries {
                        return Err(LumeError::RequestError(e));
                    }
                }
            }
            tokio::time::sleep(retry_delay).await;
        }
    }

    pub async fn pull_image(
        &self,
        image: &str,
        vm_name: &str,
        registry: Option<&str>,
        organization: Option<&str>,
        no_cache: bool,
    ) -> Result<(), LumeError> {
        use serde_json::json;

        info!("Pulling image '{}' for VM '{}'", image, vm_name);

        // Prepare the pull request data
        let mut pull_data = json!({
            "image": image,
            "name": vm_name,
            "noCache": no_cache,
        });

        // Add optional parameters if present
        if let Some(registry_val) = registry {
            pull_data["registry"] = json!(registry_val);
        }

        if let Some(org_val) = organization {
            pull_data["organization"] = json!(org_val);
        }

        // Construct the URL for the pull endpoint
        let url = format!("{}/pull", self.base_url);

        // Send the pull request
        info!("Sending pull request: {}", pull_data);

        let response = self.client.post(&url).json(&pull_data).send().await?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            error!("Failed to pull image: {}", error_text);
            return Err(LumeError::ApiError(format!(
                "Failed to pull image: {}",
                error_text
            )));
        }

        info!("Image pull request sent successfully for '{}'", image);
        Ok(())
    }
}
