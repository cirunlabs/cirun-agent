use crate::lume::{LumeClient, RunConfig};
use anyhow::Result;
use backon::{ExponentialBuilder, Retryable};
use log::{error, info, warn};
use std::io::Write;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use tokio::time::sleep;

pub async fn run_script_on_vm(
    lume: &LumeClient,
    vm_name: &str,
    script_content: &str,
    username: &str,
    password: &str,
    timeout_seconds: u64,
    run_detached: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    // Step 1: Get VM details and verify it does not exists
    info!("Getting details for VM: {}", vm_name);
    let vm = lume.get_vm(vm_name).await?;
    info!("Found VM: {} ({})", vm.name, vm.state);

    // Step 2: If the VM is not running, try to start it with retries
    if vm.state != "running" {
        info!(
            "VM is not running. Current state: {}. Attempting to start...",
            vm.state
        );

        let start_vm = || async {
            let run_config = RunConfig {
                no_display: Some(true),
                shared_directories: None,
                recovery_mode: None,
            };
            lume.run_vm(vm_name, Some(run_config))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to start VM: {:?}", e))
        };

        start_vm
            .retry(ExponentialBuilder::default().with_max_times(5))
            .sleep(tokio::time::sleep)
            .when(|e| e.to_string().contains("Failed to start VM"))
            .notify(|err, dur| warn!("Retrying VM start after {:?}: {:?}", dur, err))
            .await?;

        info!("Start command sent successfully");
    }

    // Step 3: Wait for the VM to be running and get its IP
    info!("Waiting for VM to be fully running and get its IP address");
    let ip_address = wait_for_vm_ip(lume, vm_name, timeout_seconds).await?;
    info!("VM is running with IP: {}", ip_address);

    use crate::ssh::{copy_file, exec, test_connection, SshAuth, SshTarget};

    // password_tmp must outlive the SshTarget (dropped on function exit auto-deletes).
    let password_tmp = create_password_file(password)?;
    info!(
        "SSH target: {}@{} (password auth, password length={})",
        username,
        ip_address,
        password.len()
    );
    let target = SshTarget::new(
        ip_address.clone(),
        username.to_string(),
        SshAuth::PasswordFile(password_tmp.path().to_path_buf()),
    )?;

    // Detached path is shared with meda via `provision_push` so both
    // SSH-based executors get the same kill-after-PID-read fix and
    // the same structured-diagnostics observability bag. The blocking
    // path (run_detached=false) stays inline — it owns the 10-minute
    // "wait for the script to complete" lifetime which the shared
    // module deliberately does not handle.
    if run_detached {
        let ctx = crate::provision_push::PushContext {
            vm_name,
            vm_ip: &ip_address,
            target,
            script: script_content,
            use_sudo: false, // lume's macOS template user is already admin
            detached_exec_timeout: Duration::from_secs(60),
        };
        let pid = crate::provision_push::push_and_run_detached(&ctx)
            .await
            .map_err(|f| -> Box<dyn std::error::Error> { f.message.into() })?;
        drop(password_tmp);
        return Ok(format!("{pid}\n"));
    }

    info!("Creating temporary script file");
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(script_content.as_bytes())?;

    // Test SSH (VM may still be booting). Use backon for exponential retry.
    (|| async { test_connection(&target).await })
        .retry(ExponentialBuilder::default().with_max_times(10))
        .sleep(tokio::time::sleep)
        .notify(|err, dur| warn!("Retrying SSH connection after {:?}: {:?}", dur, err))
        .await?;
    info!("✔ SSH connection successful");

    // Copy script to VM (with retry).
    let remote_script_path = format!("/tmp/script_{}.sh", Instant::now().elapsed().as_secs());
    let temp_path = temp_file.path().to_path_buf();
    (|| {
        let target = &target;
        let temp_path = temp_path.clone();
        let remote_script_path = remote_script_path.clone();
        async move { copy_file(target, &temp_path, &remote_script_path).await }
    })
    .retry(ExponentialBuilder::default().with_max_times(5))
    .sleep(tokio::time::sleep)
    .notify(|err, dur| warn!("Retrying SCP transfer after {:?}: {:?}", dur, err))
    .await?;

    // Blocking path: run the script foreground, wait up to 10 minutes
    // for it to finish, return its stdout. Used by lume template
    // creation today, not by per-runner provision.
    let cmd = format!("chmod +x {p} && {p}", p = remote_script_path);
    let script_output = (|| {
        let target = &target;
        let cmd = cmd.clone();
        async move { exec(target, &cmd, tokio::time::Duration::from_secs(600)).await }
    })
    .retry(ExponentialBuilder::default().with_max_times(3))
    .sleep(tokio::time::sleep)
    .notify(|err, dur| warn!("Retrying script execution after {:?}: {:?}", dur, err))
    .await?;

    // password_tmp drops here, auto-removing the file. No manual cleanup needed.
    drop(password_tmp);

    info!("Script execution completed successfully.");
    Ok(script_output)
}

/// Write the SSH password to a 0600 tempfile using `NamedTempFile` (O_EXCL +
/// random suffix → not symlink-attackable, unlike the previous predictable
/// `/tmp/sshpass_<millis>.txt`). Returns the `NamedTempFile` — keep it alive
/// for the full SSH session; drop it to auto-delete. Permissions are tightened
/// BEFORE the password is written to the file.
fn create_password_file(password: &str) -> Result<NamedTempFile, Box<dyn std::error::Error>> {
    let mut tmp = NamedTempFile::new()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(password.as_bytes())?;
    Ok(tmp)
}

async fn wait_for_vm_ip(
    lume: &LumeClient,
    vm_name: &str,
    timeout_seconds: u64,
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
            }
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
