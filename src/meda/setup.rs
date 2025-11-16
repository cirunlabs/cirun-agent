use log::{error, info, warn};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::{thread, time::Duration, time::SystemTime};

use chrono::{DateTime, Utc};
use std::path::Path;

pub async fn download_and_run_meda() {
    // Spawn a blocking task to handle the file operations
    let result = tokio::task::spawn_blocking(download_and_run_meda_internal).await;

    // Handle the result
    match result {
        Ok(Ok(_)) => info!("Meda setup complete"),
        Ok(Err(e)) => error!("Meda setup failed: {}", e),
        Err(e) => error!("Task error: {}", e),
    }
}

// Function to clean up old log files
pub fn cleanup_log_files(
    log_dir: &Path,
    max_age_days: u64,
    max_size_mb: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Checking log files for cleanup...");

    if !log_dir.exists() {
        return Ok(());
    }

    let max_age = Duration::from_secs(max_age_days * 24 * 60 * 60);
    let max_size = max_size_mb * 1024 * 1024; // Convert MB to bytes
    let now = SystemTime::now();

    let entries = fs::read_dir(log_dir)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Skip if not a file or doesn't have .log extension
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("log") {
            continue;
        }

        let metadata = fs::metadata(&path)?;
        let file_size = metadata.len();

        // Check file age
        if let Ok(modified) = metadata.modified() {
            if let Ok(age) = now.duration_since(modified) {
                if age > max_age {
                    info!(
                        "Removing old log file: {:?} (age: {} days)",
                        path,
                        age.as_secs() / (24 * 60 * 60)
                    );
                    fs::remove_file(&path)?;
                    continue;
                }
            }
        }

        // Check file size
        if file_size > max_size {
            info!(
                "Log file too large, rotating: {:?} (size: {:.2} MB)",
                path,
                file_size as f64 / 1024.0 / 1024.0
            );

            // Create a backup with timestamp
            let timestamp: DateTime<Utc> = metadata
                .modified()
                .unwrap_or_else(|_| SystemTime::now())
                .into();

            let backup_path =
                path.with_extension(format!("log.{}", timestamp.format("%Y%m%d%H%M%S")));

            // Rename the current log file to the backup name
            fs::rename(&path, &backup_path)?;

            // Create a new empty log file
            fs::File::create(&path)?;

            // Limit the number of backup files (keep the 5 most recent)
            let mut backups: Vec<_> = fs::read_dir(log_dir)?
                .filter_map(Result::ok)
                .filter(|e| {
                    let p = e.path();
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    name.starts_with(&path.file_name().unwrap().to_str().unwrap().to_string())
                        && name.contains("log.")
                })
                .collect();

            backups.sort_by_key(|e| std::cmp::Reverse(e.path()));

            // Remove older backups (keep 5 newest)
            for old_backup in backups.into_iter().skip(5) {
                let old_path = old_backup.path();
                info!("Removing old backup log: {:?}", old_path);
                let _ = fs::remove_file(old_path);
            }
        }
    }

    info!("Log cleanup complete");
    Ok(())
}

fn download_and_run_meda_internal() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let install_dir = PathBuf::from(format!("{}/.meda", std::env::var("HOME")?));
    let meda_bin_path = install_dir.join("meda");

    // Create installation directory if it doesn't exist
    if !install_dir.exists() {
        fs::create_dir_all(&install_dir)?;
        info!("Created directory: {:?}", install_dir);
    }

    // Check if meda is already installed in common locations
    let possible_paths = vec![
        meda_bin_path.clone(),
        PathBuf::from("/usr/local/bin/meda"),
        PathBuf::from(format!("{}/.local/bin/meda", std::env::var("HOME")?)),
        PathBuf::from(format!("{}/.cargo/bin/meda", std::env::var("HOME")?)),
    ];

    let mut found_meda = None;
    for path in &possible_paths {
        if path.exists() {
            found_meda = Some(path.clone());
            info!("Found existing meda installation at {:?}", path);
            break;
        }
    }

    // Also check if meda is in PATH
    if found_meda.is_none() {
        if let Ok(output) = Command::new("which").arg("meda").output() {
            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path_str.is_empty() {
                    let path = PathBuf::from(path_str);
                    found_meda = Some(path.clone());
                    info!("Found meda in PATH at {:?}", path);
                }
            }
        }
    }

    // If meda is not found anywhere, install it
    if found_meda.is_none() {
        info!("Meda not found, installing...");

        // Download and run the installation script
        info!("Running meda installation script...");

        // Create a temporary directory for the installation
        let temp_dir = std::env::temp_dir().join("meda_install");
        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir)?;
        }
        fs::create_dir_all(&temp_dir)?;

        let install_script = temp_dir.join("install-meda.sh");

        // Download the installation script
        let status = Command::new("curl")
            .arg("-fsSL")
            .arg("https://raw.githubusercontent.com/cirunlabs/meda/main/scripts/install-release.sh")
            .arg("-o")
            .arg(&install_script)
            .status()?;

        if !status.success() {
            return Err("Failed to download meda installation script".into());
        }

        // Make the script executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&install_script)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&install_script, perms)?;
        }

        // Run the installation script
        let status = Command::new("bash")
            .arg(&install_script)
            .env("HOME", std::env::var("HOME")?)
            .status()?;

        if !status.success() {
            return Err("Failed to install meda".into());
        }

        // Verify the binary was installed - check multiple possible locations
        let home_dir = std::env::var("HOME")?;
        let possible_install_locations = vec![
            PathBuf::from(&home_dir).join(".local/bin/meda"),
            PathBuf::from(&home_dir).join(".cargo/bin/meda"),
            PathBuf::from("/usr/local/bin/meda"),
        ];

        let mut installed_meda = None;
        for location in &possible_install_locations {
            if location.exists() {
                installed_meda = Some(location.clone());
                break;
            }
        }

        let installed_meda = installed_meda
            .ok_or("Meda binary not found after installation in any expected location")?;

        info!("Meda installed successfully at {:?}", installed_meda);
        found_meda = Some(installed_meda);

        // Clean up the temporary directory
        fs::remove_dir_all(&temp_dir)?;
    }

    // Use the found meda binary path
    let meda_binary = found_meda.ok_or("Meda binary not found")?;

    // Check if meda serve is already running
    let is_running = Command::new("pgrep")
        .arg("-f")
        .arg("meda serve")
        .stdout(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if is_running {
        info!("Meda server is already running");
    } else {
        // Run "meda serve" in the background
        info!("Starting 'meda serve' in the background...");

        // Spawn meda serve as a detached process with output redirected to log files
        let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let log_dir = PathBuf::from(&home_dir).join(".meda/logs");
        fs::create_dir_all(&log_dir).unwrap_or_else(|e| {
            warn!("Could not create log directory: {}", e);
        });

        let stdout_log = log_dir.join("meda-stdout.log");
        let stderr_log = log_dir.join("meda-stderr.log");

        let stdout_file = fs::File::create(&stdout_log).unwrap_or_else(|e| {
            warn!("Could not create stdout log file: {}", e);
            fs::File::create("/dev/null").expect("Failed to open /dev/null")
        });

        let stderr_file = fs::File::create(&stderr_log).unwrap_or_else(|e| {
            warn!("Could not create stderr log file: {}", e);
            fs::File::create("/dev/null").expect("Failed to open /dev/null")
        });

        let child = Command::new(&meda_binary)
            .arg("serve")
            .arg("--port")
            .arg("7777")
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()?;

        info!(
            "Meda server started in the background with PID: {}",
            child.id()
        );
        info!("Meda logs available at {:?}", log_dir);

        // Give meda some time to start
        thread::sleep(Duration::from_secs(5));

        // Check if the process is still running
        let is_running = Command::new("ps")
            .arg("-p")
            .arg(child.id().to_string())
            .stdout(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);

        if !is_running {
            warn!(
                "Meda process terminated immediately after starting. Check logs at {:?}",
                stderr_log
            );
        }
    }
    Ok(())
}
