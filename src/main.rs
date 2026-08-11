mod api;
mod bootstrap;
mod cirun_client;
mod config;
mod docker;
mod executor;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod gpu;
#[cfg(target_os = "macos")]
mod lume;
#[cfg(target_os = "linux")]
mod meda;
mod provision;
mod provision_push;
mod reporting;
mod script_cmd;
mod service;
mod ssh;
#[cfg(target_os = "macos")]
mod vm_provision;

// TemplateConfig is only consumed by the macos lume template-name test below.
#[cfg(all(test, target_os = "macos"))]
use api::TemplateConfig;

use crate::cirun_client::CirunClient;
#[cfg(target_os = "macos")]
use crate::lume::setup::cleanup_log_files as cleanup_lume_logs;
#[cfg(target_os = "linux")]
use crate::meda::setup::cleanup_log_files as cleanup_meda_logs;
use crate::provision::ProvisionResult;
#[cfg(target_os = "macos")]
use crate::vm_provision::run_script_on_vm;
use clap::Parser;
use log::{debug, error, info, warn};
use std::env;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::task::JoinSet;
use tokio::time::{sleep, Duration};

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
    /// API token for authentication. Can also be set via the
    /// `CIRUN_API_TOKEN` env var — preferred for CI / docker contexts
    /// where the secret would otherwise appear in argv and be visible
    /// to `ps`, `set -x` shell traces, and process-table inspection.
    #[arg(
        short,
        long,
        env = "CIRUN_API_TOKEN",
        hide_env_values = true,
        required_unless_present_any = ["uninstall_service", "docker_smoke_test"],
    )]
    api_token: Option<String>,

    /// Polling interval in seconds
    #[arg(short, long, default_value_t = 5)]
    interval: u64,

    /// Agent ID file path (optional)
    #[arg(short = 'f', long, default_value = ".agent_id")]
    id_file: String,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Install cirun-agent as a system service (systemd on Linux, launchd on macOS)
    #[arg(long)]
    install_service: bool,

    /// Uninstall cirun-agent system service
    #[arg(long)]
    uninstall_service: bool,

    /// Maximum number of concurrent runners the agent will run (VMs or
    /// containers, depending on the executor). On macOS, the Apple
    /// Virtualization framework caps concurrent VMs at 2 on most hardware,
    /// so the lume executor defaults to 2 if unset. `--max-vms` is kept as
    /// a deprecated alias.
    #[arg(long, alias = "max-vms", value_parser = clap::value_parser!(u32).range(1..))]
    max_runners: Option<u32>,

    /// Run a docker GPU smoke test (`docker run --rm --gpus all <image> nvidia-smi`) and exit.
    /// Use to verify nvidia-container-toolkit + GPU passthrough on the host.
    #[arg(long)]
    docker_smoke_test: bool,

    /// Comma-separated allow-list of executors to enable on this agent.
    /// Accepted values: `docker`, `meda`, `lume`. When unset, the agent
    /// enables every executor available on the host (the historical
    /// default). Setting this suppresses the auto-install / auto-start
    /// of any executor not listed — useful for docker-only operators
    /// who do not want meda (linux) or lume (macOS) pulled in.
    ///
    /// Example: `--executors docker` runs the agent with only the
    /// docker backend and skips meda/lume download.
    #[arg(long, value_name = "LIST")]
    executors: Option<String>,

    /// Image to use for `--docker-smoke-test` (default: `nvidia/cuda:12.4.0-base-ubuntu22.04`).
    #[arg(long, default_value = "nvidia/cuda:12.4.0-base-ubuntu22.04")]
    docker_smoke_image: String,
}

const MACOS_DEFAULT_MAX_RUNNERS: u32 = 2;

use bootstrap::{check_sshpass_installed, get_agent_info};

#[tokio::main]
async fn main() {
    println!("{}", CIRUN_BANNER);
    let args = Args::parse();

    // Handle docker GPU smoke test and exit (no other flags required).
    if args.docker_smoke_test {
        env::set_var("RUST_LOG", "info");
        env_logger::init();
        let client = crate::docker::client::DockerClient::new();
        match client.ping() {
            Ok(v) => info!("docker daemon ok, server={}", v),
            Err(e) => {
                error!("docker daemon not reachable: {}", e);
                std::process::exit(2);
            }
        }
        match client.smoke_test_gpu(&args.docker_smoke_image) {
            Ok(out) => {
                println!("{}", out);
                info!("docker GPU smoke test passed");
                return;
            }
            Err(e) => {
                error!("docker GPU smoke test failed: {}", e);
                std::process::exit(3);
            }
        }
    }

    // Handle install service flag
    if args.install_service {
        service::install(&args);
        return;
    }

    // Handle uninstall service flag
    if args.uninstall_service {
        service::uninstall();
        return;
    }

    // Initialize logger with the appropriate level
    if args.verbose {
        env::set_var("RUST_LOG", "debug");
    } else {
        env::set_var("RUST_LOG", "info");
    }
    env_logger::init();
    let version = env!("CARGO_PKG_VERSION");
    info!("Cirun Agent version: {}", version);

    // sshpass is only required for the lume executor's SSH provisioning path.
    // Warn if missing on macOS but do not exit — docker dispatch (Docker Desktop)
    // works without it, and lume's `run_post_spawn` will surface a clean error
    // at provision time if needed.
    if cfg!(target_os = "macos") && !check_sshpass_installed() {
        warn!("sshpass not installed — lume executor will fail; docker executor is unaffected");
    }

    // Get or generate a persistent agent information
    // Resolve id_file path to use HOME directory if it's relative
    let id_file_path = if Path::new(&args.id_file).is_absolute() {
        args.id_file.clone()
    } else {
        let home_dir = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(&home_dir)
            .join(&args.id_file)
            .to_string_lossy()
            .to_string()
    };
    let agent_info = get_agent_info(&id_file_path);
    info!("Agent ID: {}", agent_info.id);
    info!("Hostname: {}", agent_info.hostname);
    info!("OS: {} ({})", agent_info.os, agent_info.arch);

    let cirun_api_url = match config::resolve_api_url() {
        Ok(u) => u,
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    };
    info!("Cirun API URL: {}", cirun_api_url);

    // Determine effective max_runners:
    // - If explicitly provided, use that value
    // - On macOS: default to 2 (Apple Virtualization framework limit on lume VMs)
    // - On Linux: no limit (None)
    let max_runners = args.max_runners.or(match env::consts::OS {
        "macos" => Some(MACOS_DEFAULT_MAX_RUNNERS),
        _ => None, // No default limit on Linux
    });
    match max_runners {
        Some(limit) => info!("Max concurrent runners: {}", limit),
        None => info!("Max concurrent runners: unlimited"),
    }

    let api_token = args
        .api_token
        .as_ref()
        .expect("API token is required when not installing or uninstalling service");

    // Resolve the `--executors` allow-list. Unset keeps the historical
    // "enable everything available on this host" behaviour
    // (ExecutorFilter::allow_all).
    let executor_filter = match crate::executor::parse_executor_filter(args.executors.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            error!("invalid --executors value: {e}");
            std::process::exit(1);
        }
    };
    if let Some(raw) = args.executors.as_ref() {
        let mut names: Vec<&'static str> = crate::executor::ExecutorKind::ALL
            .iter()
            .filter(|k| executor_filter.allows(**k))
            .map(|k| k.name())
            .collect();
        names.sort_unstable();
        info!(
            "Executors enabled by --executors {}: {}",
            raw,
            names.join(",")
        );
    }

    let mut client = CirunClient::new(
        &cirun_api_url,
        api_token,
        agent_info,
        max_runners,
        &executor_filter,
    );

    // Set up log cleanup parameters based on platform
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let log_dir: PathBuf = match env::consts::OS {
        "macos" => PathBuf::from(&home_dir).join(".lume/logs"),
        _ => PathBuf::from(&home_dir).join(".meda/logs"),
    };

    // Bring up backend daemons that need pre-start (meda + lume; docker uses
    // the host's docker daemon directly). Selection per-runner happens via
    // payload; this is just startup-time setup.
    //
    // `--executors` short-circuits the download/run when the corresponding
    // executor is not on the allow-list — that's the entire point of
    // issue #15. Docker-only operators end up with no out-of-band binary
    // pulls.
    #[cfg(target_os = "linux")]
    {
        if executor_filter.allows(crate::executor::ExecutorKind::Meda) {
            meda::setup::download_and_run_meda().await;
        } else {
            info!("Skipping meda setup (not in --executors allow-list)");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if executor_filter.allows(crate::executor::ExecutorKind::Lume) {
            lume::download_and_run_lume().await;
        } else {
            info!("Skipping lume setup (not in --executors allow-list)");
        }
    }

    // Seed the runner→executor map from live state. Prevents silent mis-routing
    // of deletes after an agent restart with runners already on the host.
    client.seed_runner_executors_from_registry().await;

    let mut last_cleanup = SystemTime::now();
    let cleanup_interval = Duration::from_secs(24 * 60 * 60); // Daily log cleanup

    // Persistent JoinSet for provisioning tasks — lives across loop iterations
    // so in-flight tasks don't block polling.
    let mut provision_set: JoinSet<ProvisionResult> = JoinSet::new();
    // Track runner names currently being provisioned to avoid spawning duplicates.
    let mut in_flight: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Main loop
    loop {
        // Drain completed provisioning results (non-blocking)
        let mut any_provision_succeeded = false;
        while let Some(result) = provision_set.try_join_next() {
            match result {
                Ok(pr) => {
                    in_flight.remove(&pr.runner_name);
                    if let Ok(mut s) = client.in_flight.lock() {
                        s.remove(&pr.runner_name);
                    }
                    if let Some(kind) = pr.executor_kind {
                        if let Ok(mut map) = client.runner_executors.lock() {
                            map.insert(pr.runner_name.clone(), kind);
                        }
                    }
                    // All per-outcome dispatch (retry math, which HTTP
                    // payload to send) lives in the ProvisionReporter
                    // impl on CirunClient. main.rs just lifts the
                    // outcome into the event vocabulary and emits.
                    let event = crate::reporting::ProvisionEvent::from(pr);
                    if event.is_success() {
                        any_provision_succeeded = true;
                    }
                    use crate::reporting::ProvisionReporter;
                    client.report(event).await;
                }
                Err(e) => {
                    error!("Provisioning task panicked: {}", e);
                }
            }
        }

        if any_provision_succeeded {
            client.report_running_vms().await;
        }

        match client
            .manage_runner_lifecycle(&mut provision_set, &mut in_flight)
            .await
        {
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
                let cleanup_result: Result<(), Box<dyn std::error::Error>> = {
                    #[cfg(target_os = "macos")]
                    {
                        cleanup_lume_logs(&log_dir, 7, 100)
                    }
                    #[cfg(target_os = "linux")]
                    {
                        cleanup_meda_logs(&log_dir, 7, 100)
                    }
                    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                    {
                        Ok(())
                    }
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
    #[cfg(all(test, target_os = "macos"))]
    use super::*;
    use crate::bootstrap::{get_agent_info, get_hostname};
    // generate_template_name lives in crate::lume which is macos-only.
    // The tests that rely on it are gated below.
    #[cfg(target_os = "macos")]
    use crate::lume::generate_template_name;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    #[cfg(target_os = "macos")]
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

    #[test]
    fn test_agent_info_includes_own_crate_version() {
        let id_file = ".test_agent_id_version";
        let _ = std::fs::remove_file(id_file);

        let agent_info = get_agent_info(id_file);
        assert_eq!(agent_info.version, env!("CARGO_PKG_VERSION"));

        let _ = std::fs::remove_file(id_file);
    }
}
