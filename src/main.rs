use clap::Parser;
use reqwest::{Client, Error};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    time::{sleep, Duration},
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use log::{info, error};
use env_logger;
use uuid::Uuid;
use std::fs;
use std::path::Path;
use std::env;
use std::process::Command as StdCommand;

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
    command: String,
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

    async fn get_command(&self) -> Result<ApiResponse, Error> {
        let url = format!("{}/agent", self.base_url);
        info!("Fetching command from: {}", url);
        let request_data = json!({
            "agent": self.agent,
        });
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .json(&request_data)
            .send()
            .await?;
        info!("Response: {}", response.status());
        let json: ApiResponse = response.json().await?;
        info!("Received command: {}", json.command);
        Ok(json)
    }

    async fn send_command_response(&self, response: CommandResponse) {
        let url = format!("{}/agent", self.base_url);
        let res = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .json(&response)
            .send()
            .await;

        match res {
            Ok(_) => info!("Successfully sent command response"),
            Err(e) => error!("Failed to send command response: {}", e),
        }
    }

    async fn execute_command(&self, command: &str) {
        info!("Executing command: {}", command);
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                error!("Failed to start command: {}", e);
                let response = CommandResponse {
                    command: command.to_string(),
                    output: String::new(),
                    error: e.to_string(),
                    agent: self.agent.clone(),
                };
                self.send_command_response(response).await;
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut reader_out = BufReader::new(stdout).lines();
        let mut reader_err = BufReader::new(stderr).lines();

        let agent_info = self.agent.clone();
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_token = self.api_token.clone();
        let command_string = command.to_string();

        let client_clone = client.clone();
        let base_url_clone = base_url.clone();
        let api_token_clone = api_token.clone();
        let command_clone = command_string.clone();
        let agent_clone = agent_info.clone();

        tokio::spawn(async move {
            while let Ok(Some(line)) = reader_out.next_line().await {
                info!("STDOUT: {}", line);
                let response = CommandResponse {
                    command: command_clone.clone(),
                    output: line.clone(),
                    error: String::new(),
                    agent: agent_clone.clone(),
                };
                let _ = client_clone
                    .post(format!("{}/agent", base_url_clone))
                    .header("Authorization", format!("Bearer {}", api_token_clone))
                    .json(&response)
                    .send()
                    .await;
            }
        });

        let client_clone = client.clone();
        let base_url_clone = base_url.clone();
        let api_token_clone = api_token.clone();
        let command_clone = command_string.clone();
        let agent_clone = agent_info.clone();

        tokio::spawn(async move {
            while let Ok(Some(line)) = reader_err.next_line().await {
                error!("STDERR: {}", line);
                let response = CommandResponse {
                    command: command_clone.clone(),
                    output: String::new(),
                    error: line.clone(),
                    agent: agent_clone.clone(),
                };
                let _ = client_clone
                    .post(format!("{}/agent", base_url_clone))
                    .header("Authorization", format!("Bearer {}", api_token_clone))
                    .json(&response)
                    .send()
                    .await;
            }
        });

        let _ = child.wait().await;
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

    // Get or generate a persistent agent information
    let agent_info = get_agent_info(&args.id_file);
    info!("Agent ID: {}", agent_info.id);
    info!("Hostname: {}", agent_info.hostname);
    info!("OS: {} ({})", agent_info.os, agent_info.arch);

    let client = CirunClient::new("http://localhost:8080/api/v1", &args.api_token, agent_info);

    loop {
        match client.get_command().await {
            Ok(response) => {
                if !response.command.is_empty() {
                    client.execute_command(&response.command).await;
                }
            }
            Err(e) => error!("Error fetching command: {}", e),
        }
        sleep(Duration::from_secs(args.interval)).await;
    }
}
