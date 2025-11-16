mod lume;
mod meda;
mod vm_provision;

use crate::lume::client::LumeClient;
use crate::lume::setup::cleanup_log_files as cleanup_lume_logs;
use crate::lume::{
    check_template_exists, create_template, find_matching_template, generate_template_name,
};
use crate::meda::client::MedaClient;
use crate::meda::setup::cleanup_log_files as cleanup_meda_logs;
use crate::vm_provision::run_script_on_vm;
use clap::Parser;
use log::{debug, error, info, warn};
use reqwest::{Client, Error};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::SystemTime;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

const CIRUN_BANNER: &str = r#"
       _                       _                    _
   ___(_)_ __ _   _ _ __      / \   __ _  ___ _ __ | |_
  / __| | '__| | | | '_ \    / _ \ / _` |/ _ \ '_ \| __|
 | (__| | |  | |_| | | | |  / ___ \ (_| |  __/ | | | |_
  \___|_|_|   \__,_|_| |_| /_/   \_\__, |\___|_| |_|\__|
                                   |___/
"#;

// Command line arguments
#[derive(Parser, Debug)]
#[command(version, about = "Cirun Agent", long_about = None)]
struct Args {
    /// API token for authentication
    #[arg(short, long)]
    api_token: String,

    /// Polling interval in seconds
    #[arg(short, long, default_value_t = 5)]
    interval: u64,

    /// Agent ID file path (optional)
    #[arg(short = 'f', long, default_value = ".agent_id")]
    id_file: String,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

// Structs for agent and API data
#[derive(Debug, Serialize, Deserialize, Clone)]
struct AgentInfo {
    id: String,
    hostname: String,
    os: String,
    arch: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    runners_to_provision: Vec<RunnerToProvision>,
    runners_to_delete: Vec<RunnerToDelete>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TemplateConfig {
    image: String,
    registry: Option<String>,
    organization: Option<String>,
    cpu: u32,
    memory: u32,
    disk: u32,
    os: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RunnerLogin {
    username: String,
    password: String,
}

#[derive(Debug, Clone)]
struct RunnerResources {
    cpu: u32,
    memory: u32,
    disk: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunnerToProvision {
    name: String,
    provision_script: String,
    os: String, // This is actually the image to use
    cpu: u32,
    memory: u32,
    #[serde(default)]
    disk: u32,
    login: RunnerLogin,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunnerToDelete {
    name: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct CommandResponse {
    command: String,
    output: String,
    error: String,
    agent: AgentInfo,
}

// Helper function to determine if we should use meda (Linux host) or lume (macOS host)
fn use_meda() -> bool {
    env::consts::OS == "linux"
}

// Helper function to determine OS from image name
fn get_os_from_image(image: &str) -> String {
    let image_lower = image.to_lowercase();

    if image_lower.contains("macos")
        || image_lower.contains("mac-os")
        || image_lower.contains("sonoma")
        || image_lower.contains("ventura")
        || image_lower.contains("monterey")
    {
        return "macOS".to_string();
    } else if image_lower.contains("ubuntu")
        || image_lower.contains("debian")
        || image_lower.contains("mint")
        || image_lower.contains("linux")
    {
        return "linux".to_string();
    } else if image_lower.contains("windows") {
        return "windows".to_string();
    }

    // Default to linux if we can't determine
    "linux".to_string()
}

// Get system hostname
fn get_hostname() -> String {
    if let Ok(hostname) = env::var("HOSTNAME") {
        return hostname;
    }

    if let Ok(output) = StdCommand::new("hostname").output() {
        if let Ok(hostname) = String::from_utf8(output.stdout) {
            return hostname.trim().to_string();
        }
    }

    "unknown-host".to_string()
}

// Generate or retrieve a persistent agent information
fn get_agent_info(id_file: &str) -> AgentInfo {
    let id = if Path::new(id_file).exists() {
        match fs::read_to_string(id_file) {
            Ok(id) => {
                let id = id.trim().to_string();
                info!("Using existing agent ID: {}", id);
                id
            }
            Err(e) => {
                error!("Failed to read agent ID file: {}", e);
                // Generate a new UUID v4
                let new_id = Uuid::new_v4().to_string();
                info!("Generated new agent ID: {}", new_id);

                // Save the ID to file for persistence
                if let Err(e) = fs::write(id_file, &new_id) {
                    error!("Failed to write agent ID to file: {}", e);
                }

                new_id
            }
        }
    } else {
        // Generate a new UUID v4
        let new_id = Uuid::new_v4().to_string();
        info!("Generated new agent ID: {}", new_id);

        // Save the ID to file for persistence
        if let Err(e) = fs::write(id_file, &new_id) {
            error!("Failed to write agent ID to file: {}", e);
        }

        new_id
    };

    AgentInfo {
        id,
        hostname: get_hostname(),
        os: env::consts::OS.to_string(),
        arch: env::consts::ARCH.to_string(),
    }
}

// Client for interacting with the CiRun API
struct CirunClient {
    client: Client,
    base_url: String,
    api_token: String,
    agent: AgentInfo,
}

impl CirunClient {
    fn new(base_url: &str, api_token: &str, agent: AgentInfo) -> Self {
        CirunClient {
            client: Client::new(),
            base_url: base_url.to_string(),
            api_token: api_token.to_string(),
            agent,
        }
    }

    // Helper method to create a request builder with common headers
    fn create_request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let request_id = Uuid::new_v4().to_string();
        info!("Creating request with ID: {}", request_id);

        self.client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .header("X-Request-ID", request_id)
            .header("X-Agent-ID", &self.agent.id)
    }

    async fn report_running_vms(&self) {
        info!("Reporting running VMs to API");

        if use_meda() {
            // Use meda for Linux
            match MedaClient::new() {
                Ok(meda) => {
                    match meda.list_vms().await {
                        Ok(vms) => {
                            let running_vms: Vec<_> =
                                vms.into_iter().filter(|vm| vm.state == "running").collect();
                            let url = format!("{}/agent", self.base_url);

                            let res = self
                                .create_request(reqwest::Method::POST, &url)
                                .json(&json!({
                                    "agent": self.agent,
                                    "running_vms": running_vms.iter().map(|vm| {
                                        json!({
                                            "name": vm.name,
                                            "os": "linux",
                                            "cpu": vm.cpus.unwrap_or(2),
                                            "memory": vm.memory.as_ref().and_then(|m| m.trim_end_matches("GB").trim_end_matches("G").parse::<u64>().ok()).unwrap_or(2048),
                                            "disk_size": 0  // Meda doesn't report disk size in list
                                        })
                                    }).collect::<Vec<_>>()
                                }))
                                .send()
                                .await;

                            match res {
                                Ok(response) => {
                                    let status = response.status();
                                    info!("API response status: {}", status);
                                    if let Some(req_id) = response.headers().get("X-Request-ID") {
                                        if let Ok(id) = req_id.to_str() {
                                            info!("Response received with request ID: {}", id);
                                        }
                                    }
                                }
                                Err(e) => error!("Failed to send running VMs: {}", e),
                            }
                        }
                        Err(e) => error!("Failed to list VMs: {:?}", e),
                    }
                }
                Err(e) => error!("Failed to initialize Meda client: {:?}", e),
            }
        } else {
            // Use lume for macOS
            match LumeClient::new() {
                Ok(lume) => {
                    match lume.list_vms().await {
                        Ok(vms) => {
                            let running_vms: Vec<_> =
                                vms.into_iter().filter(|vm| vm.state == "running").collect();
                            let url = format!("{}/agent", self.base_url);

                            // Use the helper method instead of direct client access
                            let res = self
                                .create_request(reqwest::Method::POST, &url)
                                .json(&json!({
                                    "agent": self.agent,
                                    "running_vms": running_vms.iter().map(|vm| {
                                        json!({
                                            "name": vm.name,
                                            "os": vm.os,
                                            "cpu": vm.cpu,
                                            "memory": vm.memory,
                                            "disk_size": vm.disk_size.total
                                        })
                                    }).collect::<Vec<_>>()
                                }))
                                .send()
                                .await;

                            match res {
                                Ok(response) => {
                                    let status = response.status();
                                    info!("API response status: {}", status);
                                    if let Some(req_id) = response.headers().get("X-Request-ID") {
                                        if let Ok(id) = req_id.to_str() {
                                            info!("Response received with request ID: {}", id);
                                        }
                                    }
                                }
                                Err(e) => error!("Failed to send running VMs: {}", e),
                            }
                        }
                        Err(e) => error!("Failed to list VMs: {:?}", e),
                    }
                }
                Err(e) => error!("Failed to initialize Lume client: {:?}", e),
            }
        }
    }

    async fn provision_runner(
        &self,
        runner_name: &str,
        provision_script: &str,
        template_name: &str,
        runner_login: &RunnerLogin,
        resources: &RunnerResources,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if use_meda() {
            self.provision_runner_meda(
                runner_name,
                provision_script,
                template_name,
                runner_login,
                resources,
            )
            .await
        } else {
            self.provision_runner_lume(runner_name, provision_script, template_name, runner_login)
                .await
        }
    }

    async fn provision_runner_lume(
        &self,
        runner_name: &str,
        provision_script: &str,
        template_name: &str,
        runner_login: &RunnerLogin,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match LumeClient::new() {
            Ok(lume) => {
                // Check if VM exists by trying to get its details
                let vm_result = lume.get_vm(runner_name).await;
                let vm_exists = vm_result.is_ok();

                let vm = if vm_exists {
                    vm_result.unwrap() // VM exists, unwrap safely
                } else {
                    info!(
                        "VM '{}' does not exist. Attempting to clone from template '{}'...",
                        runner_name, template_name
                    );

                    // Check if template exists
                    match lume.get_vm(template_name).await {
                        Ok(_) => {
                            // Template exists, clone it
                            match lume.clone_vm(template_name, runner_name).await {
                                Ok(_) => {
                                    info!(
                                        "VM '{}' cloned successfully from template '{}'",
                                        runner_name, template_name
                                    );
                                    lume.get_vm(runner_name).await? // Get VM details after cloning
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to clone VM from template '{}': {:?}",
                                        template_name, e
                                    );
                                    return Err(format!(
                                        "Failed to clone VM from template '{}': {:?}",
                                        template_name, e
                                    )
                                    .into());
                                }
                            }
                        }
                        Err(e) => {
                            // Template doesn't exist
                            error!("Template '{}' not found: {:?}", template_name, e);
                            return Err(format!(
                                "Template '{}' not found. Cannot provision runner.",
                                template_name
                            )
                            .into());
                        }
                    }
                };

                info!("VM '{}' is now available", runner_name);

                // If VM exists but is not stopped, skip provisioning
                if vm.state != "stopped" {
                    info!(
                        "VM '{}' exists and is not stopped. Skipping provisioning.",
                        runner_name
                    );
                    return Ok(());
                }

                // Read SSH credentials from environment variables or use defaults
                let username = runner_login.username.clone();
                let password = runner_login.password.clone();

                info!("Provisioning runner: {}", runner_name);
                info!("Running provision script on VM");

                match run_script_on_vm(
                    &lume,
                    runner_name,
                    provision_script,
                    &username,
                    &password,
                    20,
                    true,
                )
                .await
                {
                    Ok(output) => {
                        info!("Runner provisioning completed successfully");
                        info!("Script output: {}", output);
                        Ok(())
                    }
                    Err(e) => {
                        error!("Failed to provision runner: {}", e);
                        Err(e)
                    }
                }
            }
            Err(e) => {
                error!("Failed to initialize Lume client: {:?}", e);
                Err(e.into())
            }
        }
    }

    async fn provision_runner_meda(
        &self,
        runner_name: &str,
        provision_script: &str,
        image: &str,
        runner_login: &RunnerLogin,
        resources: &RunnerResources,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use crate::meda::models::VmRunRequest;

        match MedaClient::new() {
            Ok(meda) => {
                // Check if VM already exists
                match meda.get_vm(runner_name).await {
                    Ok(vm_info) => {
                        if vm_info.state == "running" {
                            info!(
                                "VM '{}' already exists and is running. Skipping creation.",
                                runner_name
                            );
                            // Still try to run provisioning script
                        } else {
                            info!(
                                "VM '{}' exists but is not running. Starting it...",
                                runner_name
                            );
                            meda.start_vm(runner_name).await?;
                        }
                    }
                    Err(_) => {
                        // VM doesn't exist, create and run it from image
                        info!(
                            "VM '{}' does not exist. Creating from image '{}'...",
                            runner_name, image
                        );

                        // For meda, we use the image name directly (template_name parameter contains the image)
                        let run_request = VmRunRequest {
                            image: image.to_string(),
                            name: Some(runner_name.to_string()),
                            memory: Some(format!("{}G", resources.memory)),
                            cpus: Some(resources.cpu),
                            disk_size: Some(format!("{}G", resources.disk)),
                        };

                        match meda.run_vm(run_request).await {
                            Ok(_) => {
                                info!("VM '{}' created and started successfully", runner_name);
                            }
                            Err(e) => {
                                error!("Failed to create and run VM '{}': {:?}", runner_name, e);
                                return Err(format!(
                                    "Failed to create and run VM from image '{}': {:?}",
                                    image, e
                                )
                                .into());
                            }
                        }
                    }
                }

                // Wait for VM to get an IP address
                info!("Waiting for VM '{}' to get an IP address...", runner_name);
                let ip_address = match meda.wait_for_vm_ip(runner_name, 300).await {
                    Ok(ip) => ip,
                    Err(e) => {
                        error!("Failed to get VM IP address: {:?}", e);
                        return Err(e.into());
                    }
                };

                info!("VM '{}' has IP address: {}", runner_name, ip_address);

                info!("Provisioning runner: {}", runner_name);
                info!("Running provision script on VM");

                // For meda, we need to use a simplified approach since we don't have the lume client
                // We'll use run_script_on_vm but we need to adapt it for meda
                match run_script_on_vm_meda(
                    &meda,
                    runner_name,
                    &ip_address,
                    provision_script,
                    runner_login,
                    true,
                )
                .await
                {
                    Ok(output) => {
                        info!("Runner provisioning completed successfully");
                        info!("Script output: {}", output);
                        Ok(())
                    }
                    Err(e) => {
                        error!("Failed to provision runner: {}", e);
                        Err(e)
                    }
                }
            }
            Err(e) => {
                error!("Failed to initialize Meda client: {:?}", e);
                Err(e.into())
            }
        }
    }

    async fn delete_runner(&self, runner_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        if use_meda() {
            match MedaClient::new() {
                Ok(meda) => {
                    info!("Attempting to delete runner VM: {}", runner_name);
                    match meda.get_vm(runner_name).await {
                        Ok(_) => match meda.delete_vm(runner_name).await {
                            Ok(_) => {
                                info!("Successfully deleted runner VM: {}", runner_name);
                                Ok(())
                            }
                            Err(e) => {
                                error!("Failed to delete runner VM {}: {:?}", runner_name, e);
                                Err(format!("Failed to delete VM: {:?}", e).into())
                            }
                        },
                        Err(e) => {
                            warn!(
                                "VM '{}' not found or error retrieving VM details: {:?}",
                                runner_name, e
                            );
                            info!("VM '{}' doesn't exist or can't be accessed - considering delete successful", runner_name);
                            Ok(())
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to initialize Meda client: {:?}", e);
                    Err(e.into())
                }
            }
        } else {
            match LumeClient::new() {
                Ok(lume) => {
                    info!("Attempting to delete runner VM: {}", runner_name);

                    // Check if VM exists by trying to get its details
                    match lume.get_vm(runner_name).await {
                        Ok(vm) => {
                            info!("Found VM '{}' with status: {}", runner_name, vm.state);

                            // Delete the VM
                            match lume.delete_vm(runner_name).await {
                                Ok(_) => {
                                    info!("VM '{}' deleted successfully", runner_name);
                                    Ok(())
                                }
                                Err(e) => {
                                    error!("Failed to delete VM '{}': {:?}", runner_name, e);
                                    Err(format!("Failed to delete VM '{}': {:?}", runner_name, e)
                                        .into())
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "VM '{}' not found or error retrieving VM details: {:?}",
                                runner_name, e
                            );
                            // Consider this a success since the VM doesn't exist anyway
                            info!("VM '{}' doesn't exist or can't be accessed - considering delete successful", runner_name);
                            Ok(())
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to initialize Lume client: {:?}", e);
                    Err(e.into())
                }
            }
        }
    }

    async fn manage_runner_lifecycle(&self) -> Result<ApiResponse, Error> {
        let url = format!("{}/agent", self.base_url);
        info!("Fetching runner provision/deletion data from: {}", url);

        let request_data = json!({
            "agent": self.agent,
        });

        // Use the helper method instead of direct client access
        let response = self
            .create_request(reqwest::Method::GET, &url)
            .json(&request_data)
            .send()
            .await?;

        info!("Response status: {}", response.status());
        let json: ApiResponse = response.json().await?;

        // Handle any runners that need deletion
        if !json.runners_to_delete.is_empty() {
            info!(
                "Received {} runners to delete",
                json.runners_to_delete.len()
            );

            for runner in &json.runners_to_delete {
                match self.delete_runner(&runner.name).await {
                    Ok(_) => {
                        info!("✅ Successfully deleted runner: {}", runner.name);
                        self.report_running_vms().await;
                    }

                    Err(e) => error!("❌ Failed to delete runner {}: {}", runner.name, e),
                }
            }
        }

        // Handle runners that need provisioning
        if !json.runners_to_provision.is_empty() {
            info!(
                "Received {} runners to provision",
                json.runners_to_provision.len()
            );

            for runner in &json.runners_to_provision {
                info!("Processing runner: {}", runner.name);
                info!("  - Image/OS: {}", runner.os);
                info!(
                    "  - CPU: {}, Memory: {}GB, Disk: {}GB",
                    runner.cpu, runner.memory, runner.disk
                );

                // Create a template config from the runner specification
                let template_config = TemplateConfig {
                    image: runner.os.clone(),
                    registry: None,     // Default registry
                    organization: None, // Default organization
                    cpu: runner.cpu,
                    memory: runner.memory,
                    disk: runner.disk,
                    os: get_os_from_image(&runner.os), // Determine OS type from image name
                };

                // For meda (Linux), use the image name directly. Templates are only for lume (macOS).
                let template_name = if use_meda() {
                    info!(
                        "Using meda on Linux - using image name directly: {}",
                        runner.os
                    );
                    runner.os.clone()
                } else {
                    // For lume (macOS), try to find an existing template with matching configuration
                    if let Some(existing_template) = find_matching_template(&template_config).await
                    {
                        info!(
                            "Found existing template with matching configuration: {}",
                            existing_template
                        );
                        existing_template
                    } else {
                        // Generate a new template name
                        let generated_name = generate_template_name(&template_config);

                        // Check if the template with this specific name already exists
                        let template_exists = check_template_exists(&generated_name).await;

                        if !template_exists {
                            // Create the template if it doesn't exist
                            info!("No matching template found. Creating new template '{}' from image '{}'",
                                 generated_name, template_config.image);

                            match create_template(&template_config, &generated_name).await {
                                Ok(_) => {
                                    info!("✅ Successfully created template: {}", generated_name);
                                    generated_name
                                }
                                Err(e) => {
                                    error!(
                                        "❌ Failed to create template {}: {}",
                                        generated_name, e
                                    );
                                    // If template creation fails, fall back to default template
                                    info!("Falling back to default template due to template creation failure");
                                    "cirun-runner-template".to_string()
                                }
                            }
                        } else {
                            info!("Using existing template: {}", generated_name);
                            generated_name
                        }
                    }
                };

                // Provision the runner using the template
                info!(
                    "Provisioning runner '{}' with template '{}'",
                    runner.name, template_name
                );

                let resources = RunnerResources {
                    cpu: runner.cpu,
                    memory: runner.memory,
                    disk: runner.disk,
                };

                match self
                    .provision_runner(
                        &runner.name,
                        &runner.provision_script,
                        &template_name,
                        &runner.login,
                        &resources,
                    )
                    .await
                {
                    Ok(_) => {
                        info!(
                            "✅ Successfully provisioned runner: {} using template {}",
                            runner.name, template_name
                        );
                        self.report_running_vms().await;
                    }
                    Err(e) => {
                        error!(
                            "❌ Failed to provision runner {} using template {}: {}",
                            runner.name, template_name, e
                        );

                        // If provisioning fails with the dynamic template, try the default template as fallback
                        if template_name != "cirun-runner-template" {
                            info!(
                                "Attempting fallback to default template for runner '{}'",
                                runner.name
                            );
                            match self
                                .provision_runner(
                                    &runner.name,
                                    &runner.provision_script,
                                    "cirun-runner-template",
                                    &runner.login,
                                    &resources,
                                )
                                .await
                            {
                                Ok(_) => {
                                    info!("✅ Successfully provisioned runner: {} using default template", runner.name);
                                    self.report_running_vms().await;
                                }
                                Err(fallback_err) => {
                                    error!(
                                        "❌ Fallback also failed for runner {}: {}",
                                        runner.name, fallback_err
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(json)
    }
}

// Helper function for running scripts on VMs using meda (simpler version without lume client)
async fn run_script_on_vm_meda(
    _meda: &MedaClient,
    vm_name: &str,
    ip_address: &str,
    script_content: &str,
    login: &RunnerLogin,
    run_detached: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    use std::fs::{remove_file, File};
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Instant;
    use tempfile::NamedTempFile;

    info!("VM '{}' is ready with IP: {}", vm_name, ip_address);

    // Step 1: Create a temporary file for the script
    info!("Creating temporary script file");
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(script_content.as_bytes())?;
    let temp_file_path = temp_file
        .path()
        .to_str()
        .ok_or("Failed to get temporary file path")?;

    // Step 2: Create a temporary password file for sshpass
    let temp_dir = std::env::temp_dir();
    let password_file_path = temp_dir.join(format!(
        "sshpass_{}.txt",
        Instant::now().elapsed().as_millis()
    ));

    let mut file = File::create(&password_file_path)?;
    file.write_all(login.password.as_bytes())?;

    // Restrict permissions on the password file
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = file.metadata()?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&password_file_path, permissions)?;
    }

    let password_file_str = password_file_path.to_string_lossy().to_string();
    info!("Created temporary password file for SSH authentication");

    // Step 3: Setup SSH options
    let ssh_options = vec![
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "ConnectTimeout=10",
    ];

    // Step 4: Test SSH connection with retries (SSH may not be ready immediately after VM boot)
    info!("Waiting for SSH to be ready on VM (max 60 seconds)...");
    let max_ssh_retries = 12; // 12 retries * 5 seconds = 60 seconds max
    let mut ssh_ready = false;

    for attempt in 1..=max_ssh_retries {
        let output = Command::new("sshpass")
            .arg("-f")
            .arg(&password_file_str)
            .arg("ssh")
            .args(&ssh_options)
            .arg(format!("{}@{}", login.username, ip_address))
            .arg("echo 'SSH connection test successful'")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;

        if output.status.success() {
            info!(
                "✔ SSH connection successful (attempt {}/{})",
                attempt, max_ssh_retries
            );
            ssh_ready = true;
            break;
        } else {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            info!(
                "SSH not ready yet (attempt {}/{}): {}",
                attempt,
                max_ssh_retries,
                error_msg.trim()
            );
            if attempt < max_ssh_retries {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }

    if !ssh_ready {
        remove_file(&password_file_path).ok();
        return Err(
            "SSH connection failed after multiple retries - VM may not be fully booted".into(),
        );
    }

    // Step 5: Copy the script to the VM
    let remote_script_path = format!("/tmp/script_{}.sh", Instant::now().elapsed().as_secs());
    info!("Copying script to VM at {}", remote_script_path);

    let output = Command::new("sshpass")
        .arg("-f")
        .arg(&password_file_str)
        .arg("scp")
        .args(&ssh_options)
        .arg(temp_file_path)
        .arg(format!(
            "{}@{}:{}",
            login.username, ip_address, remote_script_path
        ))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        let error_msg = String::from_utf8_lossy(&output.stderr);
        remove_file(&password_file_path).ok();
        return Err(format!("SCP failed: {}", error_msg).into());
    }

    info!("✔ SCP transfer successful");

    // Step 6: Execute the script on the VM with sudo (provision scripts need root privileges)
    let output = if run_detached {
        info!("Executing script on VM in detached mode with sudo");
        Command::new("sshpass")
            .arg("-f")
            .arg(&password_file_str)
            .arg("ssh")
            .args(&ssh_options)
            .arg(format!("{}@{}", login.username, ip_address))
            .arg(format!(
                "chmod +x {} && sudo nohup bash {} > /tmp/script_stdout.log 2> /tmp/script_stderr.log & echo $!",
                remote_script_path, remote_script_path
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?
    } else {
        info!("Executing script on VM and waiting for completion with sudo");
        Command::new("sshpass")
            .arg("-f")
            .arg(&password_file_str)
            .arg("ssh")
            .args(&ssh_options)
            .arg(format!("{}@{}", login.username, ip_address))
            .arg(format!(
                "chmod +x {} && sudo bash {}",
                remote_script_path, remote_script_path
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?
    };

    // Step 7: Clean up password file
    remove_file(&password_file_path).ok();

    if !output.status.success() {
        let error_msg = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Script execution failed: {}", error_msg).into());
    }

    let script_output = String::from_utf8_lossy(&output.stdout).to_string();
    info!("Script execution completed successfully.");
    Ok(script_output)
}

#[tokio::main]
async fn main() {
    println!("{}", CIRUN_BANNER);
    let args = Args::parse();
    // Initialize logger with the appropriate level
    if args.verbose {
        env::set_var("RUST_LOG", "debug");
    } else {
        env::set_var("RUST_LOG", "info");
    }
    env_logger::init();
    let version = env!("CARGO_PKG_VERSION");
    info!("Cirun Agent version: {}", version);

    // Get or generate a persistent agent information
    let agent_info = get_agent_info(&args.id_file);
    info!("Agent ID: {}", agent_info.id);
    info!("Hostname: {}", agent_info.hostname);
    info!("OS: {} ({})", agent_info.os, agent_info.arch);

    let default_api_url = "https://api.cirun.io/api/v1";
    let cirun_api_url = env::var("CIRUN_API_URL").unwrap_or_else(|_| default_api_url.to_string());
    info!("Cirun API URL: {}", cirun_api_url);
    let client = CirunClient::new(&cirun_api_url, &args.api_token, agent_info);

    // Set up log cleanup parameters based on platform
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let log_dir: PathBuf;

    // Download and run the appropriate VM manager based on platform
    if use_meda() {
        info!("Detected Linux platform - using Meda for VM management");
        meda::setup::download_and_run_meda().await;
        log_dir = PathBuf::from(&home_dir).join(".meda/logs");

        info!("Checking Meda connectivity...");
        match MedaClient::new() {
            Ok(meda) => match meda.list_vms().await {
                Ok(vms) => {
                    info!("✅ Successfully connected to Meda. Found {} VMs", vms.len());
                    for vm in vms {
                        info!("- {} ({})", vm.name, vm.state);
                    }
                }
                Err(e) => {
                    error!("❌ Failed to connect to Meda API: {:?}", e);
                    error!("Agent will continue but VM operations will likely fail");
                }
            },
            Err(e) => {
                error!("❌ Failed to initialize Meda client: {:?}", e);
                error!("Agent will continue but VM operations will likely fail");
            }
        }
    } else {
        info!("Detected macOS platform - using Lume for VM management");
        lume::download_and_run_lume().await;
        log_dir = PathBuf::from(&home_dir).join(".lume/logs");

        info!("Checking Lume connectivity...");
        match LumeClient::new() {
            Ok(lume) => match lume.list_vms().await {
                Ok(vms) => {
                    info!("✅ Successfully connected to Lume. Found {} VMs", vms.len());
                    for vm in vms {
                        info!(
                            "- {} ({}, {}, CPU: {}, Memory: {}, Disk: {})",
                            vm.name, vm.state, vm.os, vm.cpu, vm.memory, vm.disk_size.total
                        );
                    }
                }
                Err(e) => {
                    error!("❌ Failed to connect to Lume API: {:?}", e);
                    error!("Agent will continue but VM operations will likely fail");
                }
            },
            Err(e) => {
                error!("❌ Failed to initialize Lume client: {:?}", e);
                error!("Agent will continue but VM operations will likely fail");
            }
        }
    }

    let mut last_cleanup = SystemTime::now();
    let cleanup_interval = Duration::from_secs(24 * 60 * 60); // Daily log cleanup

    // Main loop
    loop {
        match client.manage_runner_lifecycle().await {
            Ok(response) => {
                info!(
                    "Attempted runners to provision: {}",
                    response.runners_to_provision.len()
                );
                info!(
                    "Attempted runners to delete: {}",
                    response.runners_to_delete.len()
                );
            }
            Err(e) => error!("Error fetching command: {}", e),
        }

        // Report running VMs after all operations
        client.report_running_vms().await;

        // Check if it's time to clean up logs
        if let Ok(duration) = SystemTime::now().duration_since(last_cleanup) {
            if duration >= cleanup_interval {
                let cleanup_result = if use_meda() {
                    cleanup_meda_logs(&log_dir, 7, 100)
                } else {
                    cleanup_lume_logs(&log_dir, 7, 100)
                };

                match cleanup_result {
                    // Keep logs for 7 days, rotate at 100MB
                    Ok(_) => {
                        last_cleanup = SystemTime::now();
                        debug!("Updated last cleanup time: {:?}", last_cleanup);
                    }
                    Err(e) => error!("Failed to clean up logs: {}", e),
                }
            }
        }

        sleep(Duration::from_secs(args.interval)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    #[test]
    fn test_get_os_from_image() {
        // macOS variants
        assert_eq!(get_os_from_image("macos-sonoma"), "macOS");
        assert_eq!(get_os_from_image("macos-ventura"), "macOS");
        assert_eq!(get_os_from_image("macos-monterey"), "macOS");
        assert_eq!(get_os_from_image("mac-os-something"), "macOS");
        assert_eq!(
            get_os_from_image("cirunlabs/macos-sequoia-xcode:15.3.1"),
            "macOS"
        );

        // Linux variants
        assert_eq!(get_os_from_image("ubuntu-20.04"), "linux");
        assert_eq!(get_os_from_image("debian-11"), "linux");
        assert_eq!(get_os_from_image("mint-21"), "linux");
        assert_eq!(get_os_from_image("linux-server"), "linux");
        assert_eq!(get_os_from_image("cirunlabs/ubuntu:22.04"), "linux");

        // Windows variants
        assert_eq!(get_os_from_image("windows-11"), "windows");
        assert_eq!(get_os_from_image("windows-server-2022"), "windows");
        assert_eq!(get_os_from_image("cirunlabs/windows:10"), "windows");

        // Unknown should default to linux
        assert_eq!(get_os_from_image("unknown-os"), "linux");
        assert_eq!(get_os_from_image("cirunlabs/unknown:latest"), "linux");
    }

    #[test]
    fn test_template_name_generation() {
        let config1 = TemplateConfig {
            image: "cirunlabs/macos-sequoia-xcode:15.3.1".to_string(),
            registry: Some("ghcr.io".to_string()),
            organization: Some("cirunlabs".to_string()),
            cpu: 4,
            memory: 8,
            disk: 100,
            os: "macOS".to_string(),
        };

        let config2 = TemplateConfig {
            image: "cirunlabs/macos-sequoia-xcode:15.3.1".to_string(),
            registry: Some("ghcr.io".to_string()),
            organization: Some("cirunlabs".to_string()),
            cpu: 4,
            memory: 8,
            disk: 100,
            os: "macOS".to_string(),
        };

        let config3 = TemplateConfig {
            image: "cirunlabs/macos-sequoia-xcode:15.3.1".to_string(),
            registry: Some("ghcr.io".to_string()),
            organization: Some("cirunlabs".to_string()),
            cpu: 8, // Different CPU
            memory: 8,
            disk: 100,
            os: "macOS".to_string(),
        };

        // Same configs should produce same template names
        let name1 = generate_template_name(&config1);
        let name2 = generate_template_name(&config2);
        assert_eq!(name1, name2);

        // Different configs should produce different template names
        let name3 = generate_template_name(&config3);
        assert_ne!(name1, name3);

        // Check that template name contains expected parts
        assert!(name1.contains("cirun-template"));
        assert!(name1.contains("cirunlabs-macos-sequoia-xcode"));
        assert!(name1.contains("15.3.1"));
        assert!(name1.contains("4-8")); // CPU and memory
    }

    #[test]
    fn test_organization_extraction() {
        // Test function to simulate organization extraction
        fn extract_org_and_image(
            image: &str,
            organization: Option<String>,
        ) -> (String, Option<String>) {
            let mut image_name = image.to_string();
            let mut org = organization;

            // If image contains a slash, it likely has an organization prefix
            if image_name.contains('/') {
                let parts: Vec<&str> = image_name.split('/').collect();
                if parts.len() > 1 {
                    // If no explicit organization was provided, use the one from the image name
                    if org.is_none() {
                        org = Some(parts[0].to_string());
                    }

                    // Update image_name to only contain the repository part (after the slash)
                    image_name = parts[1..].join("/");
                }
            }

            (image_name, org)
        }

        // Test cases

        // Case 1: Image with organization, no explicit organization
        let (image1, org1) = extract_org_and_image("cirunlabs/macos-sequoia-xcode:15.3.1", None);
        assert_eq!(image1, "macos-sequoia-xcode:15.3.1");
        assert_eq!(org1, Some("cirunlabs".to_string()));

        // Case 2: Image with organization, with explicit organization (explicit should take precedence)
        let (image2, org2) = extract_org_and_image(
            "cirunlabs/macos-sequoia-xcode:15.3.1",
            Some("explicit-org".to_string()),
        );
        assert_eq!(image2, "macos-sequoia-xcode:15.3.1");
        assert_eq!(org2, Some("explicit-org".to_string()));

        // Case 3: Image without organization
        let (image3, org3) = extract_org_and_image("macos-sequoia-xcode:15.3.1", None);
        assert_eq!(image3, "macos-sequoia-xcode:15.3.1");
        assert_eq!(org3, None);

        // Case 4: Image without organization, with explicit organization
        let (image4, org4) = extract_org_and_image(
            "macos-sequoia-xcode:15.3.1",
            Some("explicit-org".to_string()),
        );
        assert_eq!(image4, "macos-sequoia-xcode:15.3.1");
        assert_eq!(org4, Some("explicit-org".to_string()));

        // Case 5: Image with multiple slashes (like Docker Hub official images)
        let (image5, org5) = extract_org_and_image("library/ubuntu:20.04", None);
        assert_eq!(image5, "ubuntu:20.04");
        assert_eq!(org5, Some("library".to_string()));
    }

    #[test]
    fn test_get_hostname() {
        // This test is limited since it depends on the environment
        // but we can at least verify it returns a non-empty string
        let hostname = get_hostname();
        assert!(!hostname.is_empty());

        // If HOSTNAME env var is set, it should use that
        std::env::set_var("HOSTNAME", "test-hostname");
        let hostname_from_env = get_hostname();
        assert_eq!(hostname_from_env, "test-hostname");

        // Clean up
        std::env::remove_var("HOSTNAME");
    }

    #[test]
    fn test_hash_stability() {
        // Test that the hashing is stable across runs
        let mut hasher1 = DefaultHasher::new();
        "ghcr.io".hash(&mut hasher1);
        "cirunlabs".hash(&mut hasher1);
        "macOS".hash(&mut hasher1);
        4u32.hash(&mut hasher1);
        8u32.hash(&mut hasher1);
        100u32.hash(&mut hasher1);
        let hash1 = hasher1.finish() % 10000;

        let mut hasher2 = DefaultHasher::new();
        "ghcr.io".hash(&mut hasher2);
        "cirunlabs".hash(&mut hasher2);
        "macOS".hash(&mut hasher2);
        4u32.hash(&mut hasher2);
        8u32.hash(&mut hasher2);
        100u32.hash(&mut hasher2);
        let hash2 = hasher2.finish() % 10000;

        assert_eq!(hash1, hash2);
    }

    // Mock tests that would require integration testing
    #[test]
    fn test_agent_info_creation() {
        let id_file = ".test_agent_id";

        // Cleanup in case file exists
        let _ = std::fs::remove_file(id_file);

        // First call should generate a new ID
        let agent_info1 = get_agent_info(id_file);
        assert!(!agent_info1.id.is_empty());

        // Second call should use the same ID
        let agent_info2 = get_agent_info(id_file);
        assert_eq!(agent_info1.id, agent_info2.id);

        // Clean up
        let _ = std::fs::remove_file(id_file);
    }
}
