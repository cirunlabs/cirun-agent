use backon::{ExponentialBuilder, Retryable};
use log::{info, warn};
use reqwest::Client;
use std::time::Duration;

use crate::meda::errors::MedaError;
use crate::meda::models::{
    VmCreateRequest, VmDetailResponse, VmInfo, VmListResponse, VmRunRequest,
};

const DEFAULT_API_URL: &str = "http://127.0.0.1:7777/api/v1";
const CONNECT_TIMEOUT: u64 = 10; // 10 seconds
const MAX_TIMEOUT: u64 = 300; // 5 minutes

pub struct MedaClient {
    client: Client,
    base_url: String,
}

impl MedaClient {
    pub fn new() -> Result<Self, MedaError> {
        Self::with_base_url(DEFAULT_API_URL)
    }

    #[allow(dead_code)]
    pub fn get_base_url(&self) -> &str {
        &self.base_url
    }

    pub fn with_base_url(base_url: &str) -> Result<Self, MedaError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(MAX_TIMEOUT))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(MedaError::from)?;

        Ok(Self {
            client,
            base_url: base_url.to_string(),
        })
    }

    /// Create a new VM
    #[allow(dead_code)]
    pub async fn create_vm(&self, config: VmCreateRequest) -> Result<(), MedaError> {
        let url = format!("{}/vms", self.base_url);

        let response = self.client.post(&url).json(&config).send().await?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(MedaError::ApiError(format!(
                "Failed to create VM: {}",
                error_text
            )));
        }

        Ok(())
    }

    /// Run a VM from an image (equivalent to "meda run")
    /// This creates and starts the VM in one operation
    pub async fn run_vm(&self, config: VmRunRequest) -> Result<(), MedaError> {
        let url = format!("{}/images/run", self.base_url);

        info!("Running VM from image: {}", config.image);

        let response = self.client.post(&url).json(&config).send().await?;
        let status = response.status();
        let response_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read response body".to_string());

        info!(
            "VM Run API Response: Status = {}, Body = {}",
            status, response_text
        );

        if !status.is_success() {
            return Err(MedaError::ApiError(format!(
                "Failed to run VM: {}",
                response_text
            )));
        }

        info!("Successfully started VM from image: {}", config.image);
        Ok(())
    }

    /// Start an existing VM
    pub async fn start_vm(&self, name: &str) -> Result<(), MedaError> {
        let url = format!("{}/vms/{}/start", self.base_url, name);

        info!("Starting VM: {}", name);

        let response = self.client.post(&url).send().await?;
        let status = response.status();
        let response_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read response body".to_string());

        info!(
            "VM Start API Response: Status = {}, Body = {}",
            status, response_text
        );

        if !status.is_success() {
            return Err(MedaError::ApiError(format!(
                "Failed to start VM: {}",
                response_text
            )));
        }

        info!("Successfully started VM: {}", name);
        Ok(())
    }

    /// Stop a running VM
    #[allow(dead_code)]
    pub async fn stop_vm(&self, name: &str) -> Result<(), MedaError> {
        let url = format!("{}/vms/{}/stop", self.base_url, name);

        info!("Stopping VM: {}", name);

        let response = self.client.post(&url).send().await?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(MedaError::ApiError(format!(
                "Failed to stop VM: {}",
                error_text
            )));
        }

        info!("Successfully stopped VM: {}", name);
        Ok(())
    }

    /// Delete a VM
    pub async fn delete_vm(&self, name: &str) -> Result<(), MedaError> {
        let url = format!("{}/vms/{}", self.base_url, name);

        info!("Deleting VM {}", name);

        let send_delete_request =
            || async {
                let response =
                    self.client.delete(&url).send().await.map_err(|e| {
                        MedaError::ApiError(format!("HTTP request failed: {:?}", e))
                    })?;

                let status = response.status();
                let response_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());

                info!("Delete operation response status: {}", status);
                info!("Delete operation response body: {}", response_text);

                if !status.is_success() {
                    return Err(MedaError::ApiError(format!(
                        "Failed to delete VM: {}",
                        response_text
                    )));
                }
                Ok(())
            };

        // Retry logic with proper error conversion
        send_delete_request
            .retry(ExponentialBuilder::default().with_max_times(5))
            .sleep(tokio::time::sleep)
            .when(|e| matches!(e, MedaError::ApiError(_)))
            .notify(|err, dur| warn!("Retrying VM deletion after {:?}: {:?}", dur, err))
            .await
            .map_err(|e| MedaError::ApiError(format!("Retry exhausted: {:?}", e)))?;

        info!("VM {} successfully deleted", name);
        Ok(())
    }

    /// List all VMs
    pub async fn list_vms(&self) -> Result<Vec<VmInfo>, MedaError> {
        let url = format!("{}/vms", self.base_url);

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(MedaError::ApiError(format!(
                "Failed to list VMs: {}",
                error_text
            )));
        }

        let vm_list = response.json::<VmListResponse>().await?;
        Ok(vm_list.vms)
    }

    /// Get details of a specific VM
    pub async fn get_vm(&self, name: &str) -> Result<VmDetailResponse, MedaError> {
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
                        match response.json::<VmDetailResponse>().await {
                            Ok(vm_info) => return Ok(vm_info),
                            Err(e) => {
                                warn!(
                                    "Failed to parse VM details JSON (attempt {}/{}): {:?}",
                                    attempts, max_retries, e
                                );
                                if attempts >= max_retries {
                                    return Err(MedaError::RequestError(e));
                                }
                            }
                        }
                    } else {
                        let error_text = response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Unknown error".to_string());
                        if attempts >= max_retries {
                            return Err(MedaError::ApiError(format!(
                                "Failed to get VM details: {}",
                                error_text
                            )));
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to get VM details (attempt {}/{}): {:?}",
                        attempts, max_retries, e
                    );
                    if attempts >= max_retries {
                        return Err(MedaError::RequestError(e));
                    }
                }
            }

            tokio::time::sleep(retry_delay).await;
        }
    }

    /// Wait for a VM to have an IP address
    pub async fn wait_for_vm_ip(
        &self,
        vm_name: &str,
        timeout_seconds: u64,
    ) -> Result<String, MedaError> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_seconds);

        info!(
            "Waiting for VM {} to get an IP address (timeout: {}s)...",
            vm_name, timeout_seconds
        );

        loop {
            if start.elapsed() > timeout {
                return Err(MedaError::ApiError(format!(
                    "Timeout waiting for VM {} to get an IP address",
                    vm_name
                )));
            }

            match self.get_vm(vm_name).await {
                Ok(vm_info) => {
                    if let Some(ip) = vm_info.ip {
                        if !ip.is_empty() {
                            info!("VM {} has IP address: {}", vm_name, ip);
                            return Ok(ip);
                        }
                    }
                }
                Err(e) => {
                    warn!("Error getting VM info: {:?}", e);
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
