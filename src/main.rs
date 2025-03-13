use clap::Parser;
use reqwest::{Client, Error};
use serde::{Deserialize, Serialize};
use tokio::{
    time::{sleep, Duration},
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use log::{info, error};
use env_logger;

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
}

struct CirunClient {
    client: Client,
    base_url: String,
    api_token: String,
}

impl CirunClient {
    fn new(base_url: &str, api_token: &str) -> Self {
        CirunClient {
            client: Client::new(),
            base_url: base_url.to_string(),
            api_token: api_token.to_string(),
        }
    }

    async fn get_command(&self) -> Result<ApiResponse, Error> {
        let url = format!("{}/agent/command", self.base_url);
        info!("Fetching command from: {}", url);
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .send()
            .await?;
        let json: ApiResponse = response.json().await?;
        info!("Received command: {}", json.command);
        Ok(json)
    }

    async fn send_command_response(&self, response: CommandResponse) {
        let url = format!("{}/agent/command-response", self.base_url);
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
                };
                self.send_command_response(response).await;
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut reader_out = BufReader::new(stdout).lines();
        let mut reader_err = BufReader::new(stderr).lines();
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_token = self.api_token.clone();
        let command_string = command.to_string();

        let client_clone = client.clone();
        let base_url_clone = base_url.clone();
        let api_token_clone = api_token.clone();
        let command_clone = command_string.clone();

        tokio::spawn(async move {
            while let Ok(Some(line)) = reader_out.next_line().await {
                info!("STDOUT: {}", line);
                let response = CommandResponse {
                    command: command_clone.clone(),
                    output: line.clone(),
                    error: String::new(),
                };
                let _ = client_clone
                    .post(format!("{}/agent/command-response", base_url_clone))
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

        tokio::spawn(async move {
            while let Ok(Some(line)) = reader_err.next_line().await {
                error!("STDERR: {}", line);
                let response = CommandResponse {
                    command: command_clone.clone(),
                    output: String::new(),
                    error: line.clone(),
                };
                let _ = client_clone
                    .post(format!("{}/agent/command-response", base_url_clone))
                    .header("Authorization", format!("Bearer {}", api_token_clone))
                    .json(&response)
                    .send()
                    .await;
            }
        });

        let _ = child.wait().await;
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    println!("{}", CIRUN_BANNER);
    let args = Args::parse();
    let client = CirunClient::new("https://api.cirun.io", &args.api_token);

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
