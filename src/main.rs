mod lume;
mod vm_provision;
mod lume_setup;

use clap::Parser;
use reqwest::{Client, Error};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::{sleep, Duration};
use log::{info, error, warn, debug};
use env_logger;
use uuid::Uuid;
use std::fs;
use std::path::{Path, PathBuf};
use std::env;
use std::process::Command as StdCommand;
use crate::lume::lume::LumeClient;
use crate::vm_provision::run_script_on_vm;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::SystemTime;
use crate::lume_setup::cleanup_log_files;

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

#[derive(Debug, Serialize, Deserialize)]
struct RunnerToProvision {
    name: String,
    provision_script: String,
    os: String,         // This is actually the image to use
    cpu: u32,
    memory: u32,
    disk: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunnerToDelete {
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CommandResponse {
    command: String,
    output: String,
    error: String,
    agent: AgentInfo,
}

// Helper function to determine OS from image name
fn get_os_from_image(image: &str) -> String {
    let image_lower = image.to_lowercase();

    if image_lower.contains("macos") || image_lower.contains("mac-os") ||
        image_lower.contains("sonoma") || image_lower.contains("ventura") ||
        image_lower.contains("monterey") {
        return "macOS".to_string();
    } else if image_lower.contains("ubuntu") || image_lower.contains("debian") ||
        image_lower.contains("mint") || image_lower.contains("linux") {
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

    match StdCommand::new("hostname").output() {
        Ok(output) => {
            if let Ok(hostname) = String::from_utf8(output.stdout) {
                return hostname.trim().to_string();
            }
        }
        Err(_) => {}
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
        match LumeClient::new() {
            Ok(lume) => {
                match lume.list_vms().await {
                    Ok(vms) => {
                        let running_vms: Vec<_> = vms.into_iter().filter(|vm| vm.state == "running").collect();
                        let url = format!("{}/agent", self.base_url);

                        // Use the helper method instead of direct client access
                        let res = self.create_request(reqwest::Method::POST, &url)
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
                            },
                            Err(e) => error!("Failed to send running VMs: {}", e),
                        }
                    },
                    Err(e) => error!("Failed to list VMs: {:?}", e),
                }
            },
            Err(e) => error!("Failed to initialize Lume client: {:?}", e),
        }
    }

    // Check if a template exists with the given name
    async fn check_template_exists(&self, template_name: &str) -> bool {
        match LumeClient::new() {
            Ok(lume) => {
                match lume.get_vm(template_name).await {
                    Ok(_) => {
                        info!("Template '{}' already exists", template_name);
                        true
                    },
                    Err(_) => {
                        info!("Template '{}' does not exist", template_name);
                        false
                    }
                }
            },
            Err(e) => {
                error!("Failed to initialize Lume client: {:?}", e);
                false
            }
        }
    }

    // Generate a template name based on the image configuration
    fn generate_template_name(config: &TemplateConfig) -> String {
        // Parse the image name and tag
        let image_parts: Vec<&str> = config.image.split(':').collect();
        let image_name = image_parts[0];
        let image_tag = if image_parts.len() > 1 { image_parts[1] } else { "latest" };

        // Create a sanitized image name (replace slashes and other invalid characters)
        let sanitized_image = image_name.replace('/', "-").replace('.', "-");

        // Create a configuration hash using registry, organization if present
        let mut hasher = DefaultHasher::new();
        config.registry.as_ref().unwrap_or(&"default".to_string()).hash(&mut hasher);
        config.organization.as_ref().unwrap_or(&"default".to_string()).hash(&mut hasher);
        config.os.hash(&mut hasher);
        config.cpu.hash(&mut hasher);
        config.memory.hash(&mut hasher);
        config.disk.hash(&mut hasher);
        let config_hash = hasher.finish() % 10000; // Limit to 4 digits for readability

        // Format: cirun-template-{image}-{tag}-{cpu}-{mem}-{config_hash}
        format!("cirun-template-{}-{}-{}-{}-{:04}",
                sanitized_image,
                image_tag,
                config.cpu,
                config.memory,
                config_hash)
    }

    // Find an existing template with matching configuration
    async fn find_matching_template(&self, config: &TemplateConfig) -> Option<String> {
        match LumeClient::new() {
            Ok(lume) => {
                // Attempt to list all VMs
                match lume.list_vms().await {
                    Ok(vms) => {
                        // Look for template VMs with matching specs
                        for vm in vms {
                            // Check if this is a template VM (starts with cirun-template)
                            if vm.name.starts_with("cirun-template-") {
                                // Check if specs match what we need
                                if vm.cpu == config.cpu &&
                                    vm.memory / 1024 == config.memory as u64 &&
                                    vm.disk_size.total / 1024 >= config.disk as u64 &&
                                    vm.os == config.os {
                                    info!("Found existing template with matching specs: {}", vm.name);
                                    return Some(vm.name);
                                }
                            }
                        }
                        None
                    },
                    Err(e) => {
                        error!("Failed to list VMs when searching for matching template: {:?}", e);
                        None
                    }
                }
            },
            Err(e) => {
                error!("Failed to initialize Lume client when searching for matching template: {:?}", e);
                None
            }
        }
    }

    // Pull an image using the Lume API
    async fn pull_image(&self, config: &TemplateConfig, vm_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        match LumeClient::new() {
            Ok(lume) => {
                info!("Pulling image '{}' for VM '{}'", config.image, vm_name);

                // Parse the image name to extract organization if included in the format org/image:tag
                let mut image_name = config.image.clone();
                let mut organization = config.organization.clone();

                // If image contains a slash, it likely has an organization prefix
                if image_name.contains('/') {
                    let parts: Vec<&str> = image_name.split('/').collect();
                    if parts.len() > 1 {
                        // If no explicit organization was provided, use the one from the image name
                        if organization.is_none() {
                            organization = Some(parts[0].to_string());
                        }

                        // Update image_name to only contain the repository part (after the slash)
                        image_name = parts[1..].join("/");

                        info!("Extracted organization '{}' from image name", organization.as_ref().unwrap());
                        info!("Image name updated to '{}'", image_name);
                    }
                }

                // Prepare the pull request data
                let mut pull_data = json!({
                    "image": image_name,
                    "name": vm_name,
                    // caching is a problem for large images
                    // until this is fixed: https://github.com/trycua/cua/issues/60
                    "noCache": true,
                });

                // Add optional parameters if present
                if let Some(registry) = &config.registry {
                    pull_data["registry"] = json!(registry);
                }

                if let Some(org) = &organization {
                    pull_data["organization"] = json!(org);
                }

                // Construct the URL for the pull endpoint
                let url = format!("{}/pull", lume.get_base_url());

                // Send the pull request
                info!("Sending pull request: {}", pull_data);

                let client = Client::builder()
                    .timeout(Duration::from_secs(3600))  // 1 hour timeout for the request itself
                    .build()?;

                let response = client.post(&url)
                    .json(&pull_data)
                    .send()
                    .await?;

                if !response.status().is_success() {
                    let error_text = response.text().await?;
                    error!("Failed to pull image: {}", error_text);
                    return Err(format!("Failed to pull image: {}", error_text).into());
                }

                info!("Image pull request sent successfully for '{}'", config.image);
                info!("Waiting for VM creation - this may take up to 30 minutes for large images...");

                // Wait for the pull to complete with exponential backoff
                let start_time = tokio::time::Instant::now();
                let max_timeout = Duration::from_secs(1800);  // 30 minute max timeout

                // Initial backoff of 10 seconds, then increasing
                let mut backoff_seconds = 10;
                let mut attempts = 0;

                while start_time.elapsed() < max_timeout {
                    attempts += 1;

                    // Check if the VM exists after pulling
                    match lume.get_vm(vm_name).await {
                        Ok(vm) => {
                            info!("✅ VM '{}' is now available after image pull. State: {}", vm_name, vm.state);
                            return Ok(());
                        },
                        Err(e) => {
                            // Calculate time elapsed and time remaining
                            let elapsed = start_time.elapsed();
                            let elapsed_minutes = elapsed.as_secs() / 60;
                            let elapsed_seconds = elapsed.as_secs() % 60;
                            let remaining = max_timeout.checked_sub(elapsed).unwrap_or_default();
                            let remaining_minutes = remaining.as_secs() / 60;

                            info!(
                                "Still waiting for image pull to complete (attempt {}, elapsed: {}m {}s, remaining: ~{}m)... {}",
                                attempts,
                                elapsed_minutes,
                                elapsed_seconds,
                                remaining_minutes,
                                e
                            );

                            // Sleep with exponential backoff, capped at 60 seconds
                            sleep(Duration::from_secs(backoff_seconds)).await;

                            // Increase backoff period for next attempt, but cap at 60 seconds
                            backoff_seconds = std::cmp::min(backoff_seconds * 2, 60);
                        }
                    }

                    // Every 5 minutes, query the list of all VMs to see progress
                    if attempts % 15 == 0 {  // Approximately every 5 minutes with 20s backoff
                        info!("Checking overall VM list to monitor progress...");
                        match lume.list_vms().await {
                            Ok(vms) => {
                                info!("Current VMs in system: {}", vms.len());
                                for vm in vms {
                                    info!("- {} ({}, {})", vm.name, vm.state, vm.os);
                                }
                            },
                            Err(e) => info!("Unable to list VMs: {}", e)
                        }
                    }
                }

                error!("Timed out after 30 minutes waiting for image pull to complete");
                Err("Timed out waiting for image pull to complete".into())
            },
            Err(e) => {
                error!("Failed to initialize Lume client: {:?}", e);
                Err(e.into())
            }
        }
    }

    // Check if an image has already been pulled, regardless of VM configuration
    async fn check_image_exists(&self, image: &str) -> Option<String> {
        match LumeClient::new() {
            Ok(lume) => {
                // Extract base image name without organization
                let base_image_name;
                let image_tag;

                // Parse the image string to extract name and tag
                if image.contains('/') {
                    // Handle image with organization
                    let parts: Vec<&str> = image.split('/').collect();
                    if parts.len() > 1 {
                        // Get the part after the organization
                        let repo_part = parts[1];

                        // Split by colon to separate name and tag
                        let repo_parts: Vec<&str> = repo_part.split(':').collect();
                        base_image_name = repo_parts[0];
                        image_tag = if repo_parts.len() > 1 { repo_parts[1] } else { "latest" };
                    } else {
                        // Unlikely case, but handle it anyway
                        let repo_parts: Vec<&str> = image.split(':').collect();
                        base_image_name = repo_parts[0];
                        image_tag = if repo_parts.len() > 1 { repo_parts[1] } else { "latest" };
                    }
                } else {
                    // Handle image without organization
                    let parts: Vec<&str> = image.split(':').collect();
                    base_image_name = parts[0];
                    image_tag = if parts.len() > 1 { parts[1] } else { "latest" };
                }

                info!("Looking for VMs with base image: {} (tag: {})", base_image_name, image_tag);

                // Attempt to list all VMs
                match lume.list_vms().await {
                    Ok(vms) => {
                        // Look for template VMs with matching image
                        for vm in vms {
                            // For each VM, check if the name contains the base image name and tag
                            if vm.name.contains(base_image_name) && vm.name.contains(image_tag) {
                                info!("Found existing VM with the requested image: {}", vm.name);
                                return Some(vm.name);
                            }

                            // Also check template names that might contain the image name
                            if vm.name.starts_with("cirun-template-") &&
                                vm.name.contains(&base_image_name.replace('-', "")) &&
                                vm.name.contains(image_tag) {
                                info!("Found existing template with the requested image: {}", vm.name);
                                return Some(vm.name);
                            }
                        }
                        None
                    },
                    Err(e) => {
                        error!("Failed to list VMs when searching for existing image: {:?}", e);
                        None
                    }
                }
            },
            Err(e) => {
                error!("Failed to initialize Lume client when searching for existing image: {:?}", e);
                None
            }
        }
    }

    // Create a template VM from the image
    async fn create_template(&self, config: &TemplateConfig, template_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        match LumeClient::new() {
            Ok(lume) => {
                // First, check if we already have a VM with this image
                let existing_image = self.check_image_exists(&config.image).await;

                if let Some(existing_vm) = existing_image {
                    info!("Found existing VM with image '{}': {}", config.image, existing_vm);

                    // If the existing VM is not the template we want to create, clone it
                    if existing_vm != template_name {
                        info!("Cloning existing VM '{}' to create template '{}'", existing_vm, template_name);
                        match lume.clone_vm(&existing_vm, template_name).await {
                            Ok(_) => {
                                info!("Successfully cloned VM '{}' to '{}'", existing_vm, template_name);
                            },
                            Err(e) => {
                                error!("Failed to clone VM '{}' to '{}': {:?}", existing_vm, template_name, e);
                                // Fall back to pulling the image
                                info!("Falling back to pulling the image directly");
                                self.pull_image(config, template_name).await?;
                            }
                        }
                    } else {
                        info!("The existing VM is already the template we want to create");
                    }
                } else {
                    // No existing VM with this image, need to pull
                    info!("No existing VM found with image '{}', pulling it", config.image);
                    info!("Creating template '{}' from image '{}'", template_name, config.image);
                    info!("This process may take up to 30 minutes for large images");

                    // Pull the image with the template name as the VM name
                    self.pull_image(config, template_name).await?;
                }

                // Now configure the VM with the specified resources
                info!("Configuring VM resources (CPU: {}, Memory: {}GB, Disk: {}GB)",
                     config.cpu, config.memory, config.disk);

                let update_config = json!({
                    "cpu": config.cpu,
                    "memory": format!("{}GB", config.memory),
                    "diskSize": format!("{}GB", config.disk)
                });

                let update_url = format!("{}/vms/{}", lume.get_base_url(), template_name);

                let client = Client::builder()
                    .timeout(Duration::from_secs(600))  // 10 minute timeout for the configuration
                    .build()?;

                info!("Sending request to update VM configuration: {}", update_config);

                let response = client.patch(&update_url)
                    .json(&update_config)
                    .send()
                    .await?;

                if !response.status().is_success() {
                    let error_text = response.text().await?;
                    error!("Failed to update template VM configuration: {}", error_text);
                    return Err(format!("Failed to update template VM configuration: {}", error_text).into());
                }

                // Verify the configuration was applied correctly
                match lume.get_vm(template_name).await {
                    Ok(vm) => {
                        info!("Template '{}' created and configured with: CPU: {}, Memory: {}MB, Disk: {}GB",
                             template_name, vm.cpu, vm.memory / 1024, vm.disk_size.total / 1024);
                    },
                    Err(e) => {
                        warn!("Unable to verify template configuration: {}", e);
                    }
                }

                info!("✅ Template '{}' successfully created and ready for use", template_name);
                Ok(())
            },
            Err(e) => {
                error!("Failed to initialize Lume client: {:?}", e);
                Err(e.into())
            }
        }
    }

    async fn provision_runner(&self, runner_name: &str, provision_script: &str, template_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        match LumeClient::new() {
            Ok(lume) => {
                // Check if VM exists by trying to get its details
                let vm_result = lume.get_vm(runner_name).await;
                let vm_exists = vm_result.is_ok();

                let vm = if vm_exists {
                    vm_result.unwrap() // VM exists, unwrap safely
                } else {
                    info!("VM '{}' does not exist. Attempting to clone from template '{}'...", runner_name, template_name);

                    // Check if template exists
                    match lume.get_vm(template_name).await {
                        Ok(_) => {
                            // Template exists, clone it
                            match lume.clone_vm(template_name, runner_name).await {
                                Ok(_) => {
                                    info!("VM '{}' cloned successfully from template '{}'", runner_name, template_name);
                                    lume.get_vm(runner_name).await? // Get VM details after cloning
                                },
                                Err(e) => {
                                    error!("Failed to clone VM from template '{}': {:?}", template_name, e);
                                    return Err(format!("Failed to clone VM from template '{}': {:?}", template_name, e).into());
                                }
                            }
                        },
                        Err(e) => {
                            // Template doesn't exist
                            error!("Template '{}' not found: {:?}", template_name, e);
                            return Err(format!("Template '{}' not found. Cannot provision runner.", template_name).into());
                        }
                    }
                };

                info!("VM '{}' is now available", runner_name);

                // If VM exists but is not stopped, skip provisioning
                if vm.state != "stopped" {
                    info!("VM '{}' exists and is not stopped. Skipping provisioning.", runner_name);
                    return Ok(());
                }

                // Read SSH credentials from environment variables or use defaults
                let username = env::var("LUME_SSH_USER").unwrap_or_else(|_| "cirun".to_string());
                let password = env::var("LUME_SSH_PASSWORD").unwrap_or_else(|_| "cirun".to_string());

                info!("Provisioning runner: {}", runner_name);
                info!("Running provision script on VM");

                match run_script_on_vm(&lume, runner_name, provision_script, &username, &password, 20, true).await {
                    Ok(output) => {
                        info!("Runner provisioning completed successfully");
                        info!("Script output: {}", output);
                        Ok(())
                    },
                    Err(e) => {
                        error!("Failed to provision runner: {}", e);
                        Err(e.into())
                    }
                }
            },
            Err(e) => {
                error!("Failed to initialize Lume client: {:?}", e);
                Err(e.into())
            }
        }
    }

    async fn delete_runner(&self, runner_name: &str) -> Result<(), Box<dyn std::error::Error>> {
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
                            },
                            Err(e) => {
                                error!("Failed to delete VM '{}': {:?}", runner_name, e);
                                Err(format!("Failed to delete VM '{}': {:?}", runner_name, e).into())
                            }
                        }
                    },
                    Err(e) => {
                        warn!("VM '{}' not found or error retrieving VM details: {:?}", runner_name, e);
                        // Consider this a success since the VM doesn't exist anyway
                        info!("VM '{}' doesn't exist or can't be accessed - considering delete successful", runner_name);
                        Ok(())
                    }
                }
            },
            Err(e) => {
                error!("Failed to initialize Lume client: {:?}", e);
                Err(e.into())
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
        let response = self.create_request(reqwest::Method::GET, &url)
            .json(&request_data)
            .send()
            .await?;

        info!("Response status: {}", response.status());
        let json: ApiResponse = response.json().await?;

        // Handle any runners that need deletion
        if !json.runners_to_delete.is_empty() {
            info!("Received {} runners to delete", json.runners_to_delete.len());

            for runner in &json.runners_to_delete {
                match self.delete_runner(&runner.name).await {
                    Ok(_) => {
                        info!("✅ Successfully deleted runner: {}", runner.name);
                        self.report_running_vms().await;
                    },

                    Err(e) => error!("❌ Failed to delete runner {}: {}", runner.name, e),
                }
            }
        }

        // Handle runners that need provisioning
        if !json.runners_to_provision.is_empty() {
            info!("Received {} runners to provision", json.runners_to_provision.len());

            for runner in &json.runners_to_provision {
                info!("Processing runner: {}", runner.name);
                info!("  - Image/OS: {}", runner.os);
                info!("  - CPU: {}, Memory: {}GB, Disk: {}GB", runner.cpu, runner.memory, runner.disk);

                // Create a template config from the runner specification
                let template_config = TemplateConfig {
                    image: runner.os.clone(),
                    registry: None,           // Default registry
                    organization: None,       // Default organization
                    cpu: runner.cpu,
                    memory: runner.memory,
                    disk: runner.disk,
                    os: get_os_from_image(&runner.os),  // Determine OS type from image name
                };

                // First, try to find an existing template with matching configuration
                let template_name = if let Some(existing_template) = self.find_matching_template(&template_config).await {
                    info!("Found existing template with matching configuration: {}", existing_template);
                    existing_template
                } else {
                    // Generate a new template name
                    let generated_name = Self::generate_template_name(&template_config);

                    // Check if the template with this specific name already exists
                    let template_exists = self.check_template_exists(&generated_name).await;

                    if !template_exists {
                        // Create the template if it doesn't exist
                        info!("No matching template found. Creating new template '{}' from image '{}'",
                             generated_name, template_config.image);

                        match self.create_template(&template_config, &generated_name).await {
                            Ok(_) => {
                                info!("✅ Successfully created template: {}", generated_name);
                                generated_name
                            },
                            Err(e) => {
                                error!("❌ Failed to create template {}: {}", generated_name, e);
                                // If template creation fails, fall back to default template
                                info!("Falling back to default template due to template creation failure");
                                "cirun-runner-template".to_string()
                            }
                        }
                    } else {
                        info!("Using existing template: {}", generated_name);
                        generated_name
                    }
                };

                // Provision the runner using the template
                info!("Provisioning runner '{}' with template '{}'", runner.name, template_name);

                match self.provision_runner(&runner.name, &runner.provision_script, &template_name).await {
                    Ok(_) => {
                        info!("✅ Successfully provisioned runner: {} using template {}", runner.name, template_name);
                        self.report_running_vms().await;
                    },
                    Err(e) => {
                        error!("❌ Failed to provision runner {} using template {}: {}", runner.name, template_name, e);

                        // If provisioning fails with the dynamic template, try the default template as fallback
                        if template_name != "cirun-runner-template" {
                            info!("Attempting fallback to default template for runner '{}'", runner.name);
                            match self.provision_runner(&runner.name, &runner.provision_script, "cirun-runner-template").await {
                                Ok(_) => {
                                    info!("✅ Successfully provisioned runner: {} using default template", runner.name);
                                    self.report_running_vms().await;
                                },
                                Err(fallback_err) => {
                                    error!("❌ Fallback also failed for runner {}: {}", runner.name, fallback_err);
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

    // Check Lume connectivity before entering the main loop
    lume_setup::download_and_run_lume().await;
    info!("Checking Lume connectivity...");
    match LumeClient::new() {
        Ok(lume) => {
            match lume.list_vms().await {
                Ok(vms) => {
                    info!("✅ Successfully connected to Lume. Found {} VMs", vms.len());
                    for vm in vms {
                        info!("- {} ({}, {}, CPU: {}, Memory: {}, Disk: {})",
                             vm.name, vm.state, vm.os, vm.cpu, vm.memory, vm.disk_size.total);
                    }
                },
                Err(e) => {
                    error!("❌ Failed to connect to Lume API: {:?}", e);
                    error!("Agent will continue but VM operations will likely fail");
                }
            }
        },
        Err(e) => {
            error!("❌ Failed to initialize Lume client: {:?}", e);
            error!("Agent will continue but VM operations will likely fail");
        }
    }

    // Set up log cleanup parameters
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let log_dir = PathBuf::from(&home_dir).join(".lume/logs");
    let mut last_cleanup = SystemTime::now();
    let cleanup_interval = Duration::from_secs(24 * 60 * 60); // Daily log cleanup

    // Main loop
    loop {
        match client.manage_runner_lifecycle().await {
            Ok(response) => {
                info!("Attempted runners to provision: {}", response.runners_to_provision.len());
                info!("Attempted runners to delete: {}", response.runners_to_delete.len());
            }
            Err(e) => error!("Error fetching command: {}", e),
        }

        // Report running VMs after all operations
        client.report_running_vms().await;

        // Check if it's time to clean up logs
        if let Ok(duration) = SystemTime::now().duration_since(last_cleanup) {
            if duration >= cleanup_interval {
                match cleanup_log_files(&log_dir, 7, 100) { // Keep logs for 7 days, rotate at 100MB
                    Ok(_) => {
                        last_cleanup = SystemTime::now();
                        debug!("Updated last cleanup time: {:?}", last_cleanup);
                    },
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
        assert_eq!(get_os_from_image("cirunlabs/macos-sequoia-xcode:15.3.1"), "macOS");

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
        let name1 = CirunClient::generate_template_name(&config1);
        let name2 = CirunClient::generate_template_name(&config2);
        assert_eq!(name1, name2);

        // Different configs should produce different template names
        let name3 = CirunClient::generate_template_name(&config3);
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
        fn extract_org_and_image(image: &str, organization: Option<String>) -> (String, Option<String>) {
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
        let (image2, org2) = extract_org_and_image("cirunlabs/macos-sequoia-xcode:15.3.1", Some("explicit-org".to_string()));
        assert_eq!(image2, "macos-sequoia-xcode:15.3.1");
        assert_eq!(org2, Some("explicit-org".to_string()));

        // Case 3: Image without organization
        let (image3, org3) = extract_org_and_image("macos-sequoia-xcode:15.3.1", None);
        assert_eq!(image3, "macos-sequoia-xcode:15.3.1");
        assert_eq!(org3, None);

        // Case 4: Image without organization, with explicit organization
        let (image4, org4) = extract_org_and_image("macos-sequoia-xcode:15.3.1", Some("explicit-org".to_string()));
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
