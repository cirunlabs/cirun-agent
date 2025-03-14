use std::process::{Command, Stdio};
use std::io::Write;
use std::time::{Duration, Instant};
use log::{info, error, warn};
use tempfile::NamedTempFile;
use tokio::time::sleep;
use crate::lume::lume::LumeClient;
use std::fs::{File, remove_file};

pub async fn run_script_on_vm(
    lume: &LumeClient,
    vm_name: &str,
    script_content: &str,
    username: &str,
    password: &str,
    timeout_seconds: u64,
    run_detached: bool
) -> Result<String, Box<dyn std::error::Error>> {
    // Step 1: Get VM details and verify it exists
    info!("Getting details for VM: {}", vm_name);
    let vm = lume.get_vm(vm_name).await?;
    info!("Found VM: {} ({})", vm.name, vm.state);

    // Step 2: If the VM is not running, try to start it
    if vm.state != "running" {
        info!("VM is not running. Current state: {}. Attempting to start...", vm.state);
        lume.run_vm(vm_name, None).await?;
        info!("Start command sent successfully");
    }

    // Step 3: Wait for the VM to be running and get its IP
    info!("Waiting for VM to be fully running and get its IP address");
    let ip_address = wait_for_vm_ip(lume, vm_name, timeout_seconds).await?;
    info!("VM is running with IP: {}", ip_address);

    // Step 4: Create a temporary file for the script
    info!("Creating temporary script file");
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(script_content.as_bytes())?;
    let temp_file_path = temp_file.path().to_str()
        .ok_or("Failed to get temporary file path")?;

    // Step 5: Create a temporary password file for sshpass
    let password_file_path = create_password_file(password)?;
    info!("Created temporary password file for SSH authentication");

    // Step 6: Setup SSH options
    let ssh_options = vec![
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "ConnectTimeout=10",
    ];

    // NEW: Test SSH connection before proceeding
    info!("Testing SSH connection to VM");
    let test_output = Command::new("sshpass")
        .arg("-f")
        .arg(&password_file_path)
        .arg("ssh")
        .args(&ssh_options)
        .arg(format!("{}@{}", username, ip_address))
        .arg("echo 'SSH connection test successful'")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let test_stdout = String::from_utf8_lossy(&test_output.stdout).to_string();
    let test_stderr = String::from_utf8_lossy(&test_output.stderr).to_string();

    if !test_output.status.success() {
        clean_up_password_file(&password_file_path);
        error!("SSH connection test failed: {}", test_stderr);
        return Err(format!("Could not establish SSH connection to VM: {}. Make sure username and password are correct.", test_stderr).into());
    }

    info!("✔ SSH connection test successful: {}", test_stdout.trim());

    // Step 7: Copy the script to the VM using sshpass with password file
    let remote_script_path = format!("/tmp/script_{}.sh", Instant::now().elapsed().as_secs());
    info!("Copying script to VM at {}", remote_script_path);

    let scp_output = Command::new("sshpass")
        .arg("-f")
        .arg(&password_file_path)
        .arg("scp")
        .args(&ssh_options)
        .arg(temp_file_path)
        .arg(format!("{}@{}:{}", username, ip_address, remote_script_path))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !scp_output.status.success() {
        let scp_stderr = String::from_utf8_lossy(&scp_output.stderr).to_string();
        // Clean up password file before returning error
        clean_up_password_file(&password_file_path);
        error!("SCP failed: {}", scp_stderr);
        return Err(format!("Failed to copy script to VM: exit code {:?}. Error: {}",
                           scp_output.status.code(), scp_stderr).into());
    }

    // Step 8: Execute the script
    let output = if run_detached {
        // Execute in non-interactive mode with no TTY (for long-running scripts)
        info!("Executing script on VM in detached mode");
        Command::new("sshpass")
            .arg("-f")
            .arg(&password_file_path)
            .arg("ssh")
            .args(&ssh_options)
            // Add options to ensure no TTY is allocated
            .arg("-T")  // Disable pseudo-terminal allocation
            .arg("-n")  // Redirect stdin from /dev/null
            .arg(format!("{}@{}", username, ip_address))
            // Run the script in the background and redirect output to files
            .arg(format!("chmod +x {} && nohup {} > /tmp/script_stdout.log 2> /tmp/script_stderr.log & echo $!",
                         remote_script_path, remote_script_path))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?
    } else {
        // Execute in normal mode (for scripts where we want to wait for completion)
        info!("Executing script on VM and waiting for completion");
        Command::new("sshpass")
            .arg("-f")
            .arg(&password_file_path)
            .arg("ssh")
            .args(&ssh_options)
            .arg(format!("{}@{}", username, ip_address))
            .arg(format!("chmod +x {} && {}", remote_script_path, remote_script_path))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?
    };

    // Clean up password file
    clean_up_password_file(&password_file_path);

    // Step 9: Return the output
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    info!("Script execution completed with exit code: {:?}", output.status.code());

    if !output.status.success() {
        warn!("Script execution stderr: {}", stderr);
        return Err(format!("Script execution failed with exit code {:?}. Error: {}",
                           output.status.code(), stderr).into());
    }

    Ok(stdout)
}

// Helper function to create a temporary file containing the password
fn create_password_file(password: &str) -> Result<String, Box<dyn std::error::Error>> {
    let temp_dir = std::env::temp_dir();
    let password_file_path = temp_dir.join(format!("sshpass_{}.txt", Instant::now().elapsed().as_millis()));

    let mut file = File::create(&password_file_path)?;
    file.write_all(password.as_bytes())?;

    // Restrict permissions on the password file (important for security)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = file.metadata()?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600); // Owner read/write only
        std::fs::set_permissions(&password_file_path, permissions)?;
    }

    Ok(password_file_path.to_string_lossy().to_string())
}

// Helper function to clean up the password file
fn clean_up_password_file(file_path: &str) {
    if let Err(e) = remove_file(file_path) {
        error!("Failed to remove temporary password file: {}", e);
    } else {
        info!("Temporary password file removed");
    }
}

async fn wait_for_vm_ip(
    lume: &LumeClient,
    vm_name: &str,
    timeout_seconds: u64
) -> Result<String, Box<dyn std::error::Error>> {
    let start_time = Instant::now();
    let timeout = Duration::from_secs(timeout_seconds);

    while start_time.elapsed() < timeout {
        // Get latest VM state
        match lume.get_vm(vm_name).await {
            Ok(vm) => {
                if vm.state == "running" {
                    // Extract IP address from the VM info
                    if let Some(ip) = &vm.ip_address {
                        if !ip.is_empty() {
                            return Ok(ip.clone());
                        }
                    }
                }
            },
            Err(e) => {
                error!("Error checking VM state: {:?}", e);
            }
        }

        // Sleep before retrying
        sleep(Duration::from_secs(5)).await;
        info!("Waiting for VM '{}' to get an IP address...", vm_name);
    }

    Err(format!("Timed out waiting for VM {} to be running with IP", vm_name).into())
}
