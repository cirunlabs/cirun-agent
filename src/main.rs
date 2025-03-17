mod lume;
mod vm_provision;

use clap::Parser;
use reqwest::{Client, Error};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::{sleep, Duration};
use log::{info, error, warn};
use env_logger;
use uuid::Uuid;
use std::fs;
use std::path::Path;
use std::env;
use std::process::Command as StdCommand;
use crate::lume::lume::LumeClient;
use crate::vm_provision::run_script_on_vm;

const CIRUN_BANNER: &str = r#"
       _                       _                    _
   ___(_)_ __ _   _ _ __      / \   __ _  ___ _ __ | |_
  / __| | '__| | | | '_ \    / _ \ / _` |/ _ \ '_ \| __|
 | (__| | |  | |_| | | | |  / ___ \ (_| |  __/ | | | |_
  \___|_|_|   \__,_|_| |_| /_/   \_\__, |\___|_| |_|\__|
                                   |___/
"#;

#[derive(Parser, Debug)]
#[command(version, about = "Cirun Agent", long_about = None)]
struct Args {
    /// API token for authentication
    #[arg(short, long)]
    api_token: String,

    /// Polling interval in seconds
    #[arg(short, long, default_value_t = 10)]
    interval: u64,

    /// Agent ID file path (optional)
    #[arg(short = 'f', long, default_value = ".agent_id")]
    id_file: String,
}

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

#[derive(Debug, Serialize, Deserialize)]
struct RunnerToProvision {
    name: String,
    provision_script: String,
    os: String,
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

    async fn provision_runner(&self, runner_name: &str, provision_script: &str) -> Result<(), Box<dyn std::error::Error>> {
        match LumeClient::new() {
            Ok(lume) => {
                // Check if VM exists by trying to get its details
                let vm_result = lume.get_vm(runner_name).await;
                let vm_exists = vm_result.is_ok();

                let vm = if vm_exists {
                    vm_result.unwrap() // VM exists, unwrap safely
                } else {
                    info!("VM '{}' does not exist. Attempting to clone from template...", runner_name);

                    // Check if template exists
                    match lume.get_vm("cirun-runner-template").await {
                        Ok(_) => {
                            // Template exists, clone it
                            match lume.clone_vm("cirun-runner-template", runner_name).await {
                                Ok(_) => {
                                    info!("VM '{}' cloned successfully from template", runner_name);
                                    lume.get_vm(runner_name).await? // Get VM details after cloning
                                },
                                Err(e) => {
                                    error!("Failed to clone VM from template: {:?}", e);
                                    return Err(format!("Failed to clone VM from template: {:?}", e).into());
                                }
                            }
                        },
                        Err(e) => {
                            // Template doesn't exist
                            error!("Template 'cirun-runner-template' not found: {:?}", e);
                            return Err("Template 'cirun-runner-template' not found. Cannot provision runner.".into());
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
                let username = env::var("LUME_SSH_USER").unwrap_or_else(|_| "lume".to_string());
                let password = env::var("LUME_SSH_PASSWORD").unwrap_or_else(|_| "lume".to_string());

                info!("Provisioning runner: {}", runner_name);
                info!("Running provision script on VM");

                match run_script_on_vm(&lume, runner_name, provision_script, &username, &password, 180, true).await {
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

        // Handle any runners that need provisioning
        if !json.runners_to_provision.is_empty() {
            for runner in &json.runners_to_provision {
                match self.provision_runner(&runner.name, &runner.provision_script).await {
                    Ok(_) => {
                        info!("Successfully provisioned runner: {}", runner.name);
                        self.report_running_vms().await;
                    },
                    Err(e) => error!("Failed to provision runner {}: {}", runner.name, e),
                }
            }
        }

        // Handle any runners that need deletion
        if !json.runners_to_delete.is_empty() {
            for runner in &json.runners_to_delete {
                match self.delete_runner(&runner.name).await {
                    Ok(_) => {
                        info!("Successfully deleted runner: {}", runner.name);
                        self.report_running_vms().await;
                    },
                    Err(e) => error!("Failed to delete runner {}: {}", runner.name, e),
                }
            }
        }

        Ok(json)
    }
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

#[tokio::main]
async fn main() {
    env_logger::init();
    println!("{}", CIRUN_BANNER);
    let args = Args::parse();
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

    loop {
        match LumeClient::new() {
            Ok(lume) => {
                println!("Lume client initialized");

                // Example: List all VMs
                match lume.list_vms().await {
                    Ok(vms) => {
                        println!("Found {} VMs", vms.len());
                        for vm in vms {
                            println!("- {} ({}, {}, CPU: {}, Memory: {}, Disk: {})",
                                     vm.name, vm.state, vm.os, vm.cpu, vm.memory, vm.disk_size.total);
                        }
                    },
                    Err(e) => eprintln!("Failed to list VMs: {:?}", e),
                }
            },
            Err(e) => {
                eprintln!("Failed to initialize Lume client: {:?}", e);
                // You might want to add a delay before retrying
                // or handle this error differently
            }
        }

        client.report_running_vms().await;
        match client.manage_runner_lifecycle().await {
            Ok(response) => {
                info!("Attempted runners to provision: {}", response.runners_to_provision.len());
                info!("Attempted runners to delete: {}", response.runners_to_delete.len());
            }
            Err(e) => error!("Error fetching command: {}", e),
        }
        sleep(Duration::from_secs(args.interval)).await;
    }
}
