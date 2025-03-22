use std::fs;
use std::path::{PathBuf};
use std::process::{Command, Stdio};
use log::{info, error, warn};
use std::{thread, time::Duration};

pub async fn download_and_run_lume() {
    // Spawn a blocking task to handle the file operations
    let result = tokio::task::spawn_blocking(move || {
        download_and_run_lume_internal()
    }).await;

    // Handle the result
    match result {
        Ok(Ok(_)) => info!("Lume setup complete"),
        Ok(Err(e)) => error!("Lume setup failed: {}", e),
        Err(e) => error!("Task error: {}", e),
    }
}

fn download_and_run_lume_internal() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Define constants
    let lume_version = "0.1.18";
    let lume_url = format!("https://github.com/trycua/cua/releases/download/lume-v0.1.18/lume-{}-darwin-arm64.tar.gz", lume_version);
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
        for entry in walkdir::WalkDir::new(&temp_dir).into_iter().filter_map(|e| e.ok()) {
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

        info!("Lume v{} installed successfully at {:?}", lume_version, lume_bin_path);
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

        info!("Lume server started in the background with PID: {}", child.id());
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
            warn!("Lume process terminated immediately after starting. Check logs at {:?}", stderr_log);
        }
    }
    Ok(())
}
