use log::{error, info, warn};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::{thread, time::Duration, time::SystemTime};

use chrono::{DateTime, Utc};
use std::path::Path;

pub async fn download_and_run_lume() {
    // Spawn a blocking task to handle the file operations
    let result = tokio::task::spawn_blocking(download_and_run_lume_internal).await;

    // Handle the result
    match result {
        Ok(Ok(_)) => info!("Lume setup complete"),
        Ok(Err(e)) => error!("Lume setup failed: {}", e),
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

fn download_and_run_lume_internal() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Define constants
    let lume_version = std::env::var("LUME_VERSION").unwrap_or_else(|_| String::from("0.1.21"));
    let lume_url = format!(
        "https://github.com/trycua/cua/releases/download/lume-v{}/lume-{}-darwin-arm64.tar.gz",
        lume_version, lume_version
    );
    let install_dir = PathBuf::from(format!("{}/.lume", std::env::var("HOME")?));
    let lume_bin_path = install_dir.join("lume");

    // Create installation directory if it doesn't exist
    if !install_dir.exists() {
        fs::create_dir_all(&install_dir)?;
        info!("Created directory: {:?}", install_dir);
    }

    // Check if lume is already downloaded
    if !lume_bin_path.exists() {
        info!("Lume not found, downloading version {}...", lume_version);

        // Create a temporary directory for the download
        let temp_dir = std::env::temp_dir().join("lume_download");
        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir)?;
        }
        fs::create_dir_all(&temp_dir)?;

        let tar_gz_path = temp_dir.join("lume.tar.gz");

        // Use curl command to download the file (most reliable method)
        let status = Command::new("curl")
            .arg("-L")
            .arg("-o")
            .arg(&tar_gz_path)
            .arg(&lume_url)
            .status()?;

        if !status.success() {
            return Err("Failed to download lume archive".into());
        }

        // Use tar to extract the archive
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&tar_gz_path)
            .arg("-C")
            .arg(&temp_dir)
            .status()?;

        if !status.success() {
            return Err("Failed to extract lume archive".into());
        }

        // Find the lume binary
        let mut lume_binary = None;
        for entry in walkdir::WalkDir::new(&temp_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("lume") {
                lume_binary = Some(path.to_path_buf());
                break;
            }
        }

        let lume_temp_path = lume_binary.ok_or("Could not find lume binary in extracted files")?;

        // Copy the binary to the installation directory
        fs::copy(&lume_temp_path, &lume_bin_path)?;

        // Make the binary executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&lume_bin_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&lume_bin_path, perms)?;
        }

        // Clean up the temporary directory
        fs::remove_dir_all(&temp_dir)?;

        info!(
            "Lume v{} installed successfully at {:?}",
            lume_version, lume_bin_path
        );
    } else {
        info!("Lume is already installed at {:?}", lume_bin_path);
    }

    // Check if lume is already running
    let is_running = Command::new("pgrep")
        .arg("-f")
        .arg("lume serve")
        .stdout(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if is_running {
        info!("Lume is already running");
    } else {
        // Run "lume serve" in the background
        info!("Starting 'lume serve' in the background...");

        // Spawn lume serve as a detached process with output redirected to log files
        let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let log_dir = PathBuf::from(&home_dir).join(".lume/logs");
        fs::create_dir_all(&log_dir).unwrap_or_else(|e| {
            warn!("Could not create log directory: {}", e);
        });

        let stdout_log = log_dir.join("lume-stdout.log");
        let stderr_log = log_dir.join("lume-stderr.log");

        let stdout_file = fs::File::create(&stdout_log).unwrap_or_else(|e| {
            warn!("Could not create stdout log file: {}", e);
            fs::File::create("/dev/null").expect("Failed to open /dev/null")
        });

        let stderr_file = fs::File::create(&stderr_log).unwrap_or_else(|e| {
            warn!("Could not create stderr log file: {}", e);
            fs::File::create("/dev/null").expect("Failed to open /dev/null")
        });

        let child = Command::new(&lume_bin_path)
            .arg("serve")
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()?;

        info!(
            "Lume server started in the background with PID: {}",
            child.id()
        );
        info!("Lume logs available at {:?}", log_dir);

        // Give lume some time to start
        thread::sleep(Duration::from_secs(2));

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
                "Lume process terminated immediately after starting. Check logs at {:?}",
                stderr_log
            );
        }
    }
    Ok(())
}
