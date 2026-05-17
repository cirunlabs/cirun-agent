use backon::{ExponentialBuilder, Retryable};
use log::{info, warn};
use reqwest::{Client, StatusCode};
use std::time::Duration;

use crate::meda::errors::MedaError;
use crate::meda::models::{
    VmCreateRequest, VmDetailResponse, VmInfo, VmListResponse, VmRunRequest,
};

const DEFAULT_API_URL: &str = "http://127.0.0.1:7777/api/v1";
const CONNECT_TIMEOUT: u64 = 10; // 10 seconds
const MAX_TIMEOUT: u64 = 300; // 5 minutes
const DEFAULT_RETRY_AFTER_SECS: u64 = 10;

/// Classify a meda `POST /images/run` response into Ok / HostFull / other
/// ApiError. Pure: takes only the raw bits the HTTP layer hands us
/// (status code, Retry-After header value, body text), so the run_vm
/// loop stays small and the policy is independently testable.
///
/// 503 is meda's admission-control denial — the host can't take another
/// VM right now. The body is a JSON ApiError with `code` =
/// MEM_EXHAUSTED / CPU_EXHAUSTED / DISK_EXHAUSTED and `error` =
/// operator-readable detail. We expose both via MedaError::HostFull so
/// the executor + provision flow can treat it as backpressure (don't
/// burn the retry budget, ask the backend to mark the runner "at
/// capacity") rather than a real failure.
pub(crate) fn classify_run_vm_response(
    status: StatusCode,
    retry_after: Option<&str>,
    body: &str,
) -> Result<(), MedaError> {
    if status.is_success() {
        return Ok(());
    }
    if status == StatusCode::SERVICE_UNAVAILABLE {
        let parsed: serde_json::Value =
            serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
        let code = parsed
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("HOST_FULL")
            .to_string();
        let message = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Host at capacity")
            .to_string();
        let retry_after_secs = retry_after
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_RETRY_AFTER_SECS);
        return Err(MedaError::HostFull {
            code,
            message,
            retry_after_secs,
        });
    }
    Err(MedaError::ApiError(format!("Failed to run VM: {}", body)))
}

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
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let response_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read response body".to_string());

        info!(
            "VM Run API Response: Status = {}, Body = {}",
            status, response_text
        );

        classify_run_vm_response(status, retry_after.as_deref(), &response_text)?;

        info!("Successfully started VM from image: {}", config.image);
        Ok(())
    }

    /// Start an existing VM
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_passes_2xx_through() {
        assert!(classify_run_vm_response(StatusCode::OK, None, "{}").is_ok());
        assert!(classify_run_vm_response(StatusCode::CREATED, None, "anything").is_ok());
    }

    #[test]
    fn classifier_maps_503_with_full_apierror_body_to_host_full() {
        // Meda PR 7 returns ApiError JSON on admission denial. We must
        // surface the structured code so cirun-go can present a useful
        // GH check-run message ("Host at capacity: CPU exhausted" not
        // a generic "provision failed").
        let body = r#"{"error":"CPU exhausted: need 4 vCPU, 3 vCPU available after reserve","code":"CPU_EXHAUSTED","details":null}"#;
        match classify_run_vm_response(StatusCode::SERVICE_UNAVAILABLE, Some("10"), body) {
            Err(MedaError::HostFull {
                code,
                message,
                retry_after_secs,
            }) => {
                assert_eq!(code, "CPU_EXHAUSTED");
                assert!(message.contains("CPU exhausted"));
                assert_eq!(retry_after_secs, 10);
            }
            other => panic!("expected HostFull, got {other:?}"),
        }
    }

    #[test]
    fn classifier_503_without_retry_after_uses_default() {
        // Meda always sends Retry-After today, but a future operator
        // could disable it via a proxy. Don't blow up — fall back to a
        // sensible 10s default that matches the server's intent.
        let body = r#"{"error":"mem","code":"MEM_EXHAUSTED"}"#;
        match classify_run_vm_response(StatusCode::SERVICE_UNAVAILABLE, None, body) {
            Err(MedaError::HostFull {
                retry_after_secs, ..
            }) => assert_eq!(retry_after_secs, DEFAULT_RETRY_AFTER_SECS),
            other => panic!("expected HostFull, got {other:?}"),
        }
    }

    #[test]
    fn classifier_503_with_unparseable_retry_after_uses_default() {
        // RFC 7231 allows Retry-After to be an HTTP-date too, but we
        // don't parse those. On any non-numeric value, fall back.
        let body = r#"{"error":"x","code":"DISK_EXHAUSTED"}"#;
        match classify_run_vm_response(
            StatusCode::SERVICE_UNAVAILABLE,
            Some("Wed, 16 May 2026 04:00:00 GMT"),
            body,
        ) {
            Err(MedaError::HostFull {
                retry_after_secs, ..
            }) => assert_eq!(retry_after_secs, DEFAULT_RETRY_AFTER_SECS),
            other => panic!("expected HostFull, got {other:?}"),
        }
    }

    #[test]
    fn classifier_503_with_non_json_body_still_classifies_as_host_full() {
        // A proxy 503 may not be JSON. We still know it's backpressure
        // (status code is authoritative) and use sane code/message
        // defaults rather than dropping it into the generic ApiError
        // bucket — that bucket triggers a real failure notify.
        match classify_run_vm_response(
            StatusCode::SERVICE_UNAVAILABLE,
            Some("5"),
            "Service Unavailable",
        ) {
            Err(MedaError::HostFull {
                code,
                message,
                retry_after_secs,
            }) => {
                assert_eq!(code, "HOST_FULL");
                assert_eq!(message, "Host at capacity");
                assert_eq!(retry_after_secs, 5);
            }
            other => panic!("expected HostFull, got {other:?}"),
        }
    }

    #[test]
    fn classifier_500_stays_as_generic_apierror() {
        // Real meda errors (panics, DB problems) must NOT be classified
        // as backpressure — those need to fail the runner and burn a
        // retry slot so we surface the bug to the operator.
        let body = r#"{"error":"panic","code":"INTERNAL"}"#;
        match classify_run_vm_response(StatusCode::INTERNAL_SERVER_ERROR, None, body) {
            Err(MedaError::ApiError(msg)) => assert!(msg.contains("panic")),
            other => panic!("expected ApiError, got {other:?}"),
        }
    }
}
