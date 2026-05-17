//! Shared post-spawn lifecycle for SSH-based executors (meda, lume).
//!
//! Every SSH-based executor needs the same sequence after the VM is up:
//!
//!   1. wait for SSH (with retries),
//!   2. SCP the provision script onto the VM,
//!   3. run it detached and read the PID,
//!   4. on any failure, capture a post-mortem from the still-alive VM
//!      and surface it as structured `diagnostics` so the upstream
//!      `AgentEvent.metadata` carries the root-cause hints to
//!      cirun-go.
//!
//! Centralising the lifecycle here means: (a) the OpenSSH-channel
//! kill-after-PID fix lives in one place, (b) observability metadata
//! is identical across executors, (c) adding a new SSH-based backend
//! is a 10-line adapter, not a 200-line copy.

use std::time::Duration;

use crate::ssh::{copy_file, exec_detached_get_pid, test_connection, SshTarget};

/// All the per-attempt context the shared lifecycle needs. Filled in
/// by the executor adapter (meda/lume) before each call. Kept as
/// plain fields rather than a builder — every executor sets all of
/// them, the call site reads the struct top-to-bottom, and there's
/// nothing optional that benefits from a fluent API.
pub struct PushContext<'a> {
    pub vm_name: &'a str,
    pub vm_ip: &'a str,
    /// Pre-built SSH target (key auth for meda, password for lume).
    pub target: SshTarget,
    /// Provision-script body to upload + run.
    pub script: &'a str,
    /// Whether to invoke the remote script under `sudo`. meda needs
    /// it (the cirun user can't write outside its home); lume's
    /// macOS template user is already an admin, so it runs without.
    pub use_sudo: bool,
    /// Hard cap for the detached-exec call. Independent of the
    /// nohup'd script's own runtime — once the PID is read, SSH gets
    /// killed and this timeout no longer applies.
    pub detached_exec_timeout: Duration,
}

/// Failure from `push_and_run_detached`. The `diagnostics` map is
/// built to be folded straight into `AgentEvent.metadata`, so every
/// key here is one cirun-go ends up logging structured-and-greppable
/// in Loki.
///
/// `diagnostics` is currently only consumed by meda (which threads
/// it into `ProvisionError::transient_with`); lume drops it because
/// its legacy error chain doesn't carry structured metadata yet.
/// The `allow(dead_code)` keeps `-D warnings` happy on the macOS
/// build without forcing a parallel lume-side refactor.
pub struct PushFailure {
    pub message: String,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub diagnostics: serde_json::Map<String, serde_json::Value>,
}

/// Run the shared post-spawn lifecycle against the VM in `ctx`.
/// Returns the remote PID on success; on failure, returns a
/// structured error carrying SSH timing + a VM-side state snapshot.
///
/// Steps (in order):
///   1. retry SSH connectivity up to `SSH_RETRY_COUNT` times
///   2. SCP the script to a `/tmp/script_*.sh` path
///   3. fire the detached-exec command (`script_cmd::detached_provision_cmd`)
///   4. on detached-exec failure, run `script_cmd::diagnostic_capture_cmd`
///      against the still-alive VM and stuff the output into the
///      diagnostics bag before returning
pub async fn push_and_run_detached(ctx: &PushContext<'_>) -> Result<u32, PushFailure> {
    use std::io::Write;
    use std::time::Instant;

    log::info!("VM '{}' is ready with IP: {}", ctx.vm_name, ctx.vm_ip);

    // 1. Stage the script locally so scp can transfer it. NamedTempFile
    //    auto-deletes on drop; we hold it through the SCP call.
    let mut tmp = match tempfile::NamedTempFile::new() {
        Ok(f) => f,
        Err(e) => return Err(simple_failure(format!("tempfile: {e}"), ctx)),
    };
    if let Err(e) = tmp.write_all(ctx.script.as_bytes()) {
        return Err(simple_failure(format!("write tempfile: {e}"), ctx));
    }

    // 2. Wait for SSH to be ready. The VM's IP DHCP-leased before
    //    sshd was listening, so a few short retries are normal.
    if let Err(msg) = wait_for_ssh(&ctx.target).await {
        return Err(simple_failure(format!("SSH not reachable: {msg}"), ctx));
    }

    // 3. SCP the script onto the VM at a unique path.
    let remote_path = format!("/tmp/script_{}.sh", Instant::now().elapsed().as_secs());
    if let Err(e) = copy_file(&ctx.target, tmp.path(), &remote_path).await {
        return Err(simple_failure(format!("scp: {e}"), ctx));
    }

    // 4. Fire the detached-exec call. `exec_detached_get_pid` kills
    //    SSH the moment it sees the PID line — no waiting for
    //    channel close, no 60s-timer roulette.
    let cmd = crate::script_cmd::detached_provision_cmd(&remote_path, ctx.use_sudo);
    log::info!(
        "exec start: vm={} ip={} detached=true timeout={}s",
        ctx.vm_name,
        ctx.vm_ip,
        ctx.detached_exec_timeout.as_secs()
    );

    match exec_detached_get_pid(&ctx.target, &cmd, ctx.detached_exec_timeout).await {
        Ok(ok) => {
            log::info!(
                "exec ok: vm={} pid={} elapsed_ms={}",
                ctx.vm_name,
                ok.pid,
                ok.elapsed_ms
            );
            Ok(ok.pid)
        }
        Err(e) => {
            // The VM is still alive at this point. Grab a post-mortem
            // via a separate SSH connection BEFORE the upstream
            // lifecycle deletes the box. Both the Loki log line and
            // the structured bag carry the same payload so we get
            // observability whether or not the agent-event POST
            // succeeds.
            let vm_diag = capture_diagnostics(&ctx.target).await;
            log::warn!(
                "exec failed: vm={} ip={} elapsed_ms={} ssh_error={} \n--- diagnostics ---\n{}",
                ctx.vm_name,
                ctx.vm_ip,
                e.elapsed_ms,
                e.message,
                vm_diag
            );

            let mut diagnostics = base_diagnostics(ctx);
            diagnostics.insert(
                "exec_elapsed_ms".into(),
                serde_json::Value::from(e.elapsed_ms as u64),
            );
            diagnostics.insert(
                "exec_timeout_secs".into(),
                serde_json::Value::from(ctx.detached_exec_timeout.as_secs()),
            );
            diagnostics.insert(
                "ssh_error".into(),
                serde_json::Value::from(e.message.clone()),
            );
            if !e.partial_stdout.is_empty() {
                diagnostics.insert(
                    "partial_stdout".into(),
                    serde_json::Value::from(e.partial_stdout),
                );
            }
            if !e.partial_stderr.is_empty() {
                diagnostics.insert(
                    "partial_stderr".into(),
                    serde_json::Value::from(e.partial_stderr),
                );
            }
            diagnostics.insert("vm_diagnostics".into(), serde_json::Value::from(vm_diag));
            Err(PushFailure {
                message: e.message,
                diagnostics,
            })
        }
    }
}

/// Build the baseline diagnostics map every failure path emits —
/// keeps the per-error code below from re-stating runner identity.
fn base_diagnostics(ctx: &PushContext<'_>) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "vm_name".into(),
        serde_json::Value::from(ctx.vm_name.to_string()),
    );
    m.insert(
        "vm_ip".into(),
        serde_json::Value::from(ctx.vm_ip.to_string()),
    );
    m.insert("use_sudo".into(), serde_json::Value::from(ctx.use_sudo));
    m
}

/// Shape for the "we never reached detached-exec" failure paths
/// (tempfile, SSH-retry, scp). They share the baseline runner-shape
/// metadata so the cirun-go event log entry looks identical to a
/// detached-exec failure modulo the missing `exec_*` keys.
fn simple_failure(message: String, ctx: &PushContext<'_>) -> PushFailure {
    let mut diagnostics = base_diagnostics(ctx);
    diagnostics.insert("phase".into(), serde_json::Value::from("pre_detached_exec"));
    PushFailure {
        message,
        diagnostics,
    }
}

/// SSH-readiness probe loop. Six tries with a 5s sleep between
/// attempts (~30s budget). Returns the last error string if every
/// attempt failed.
async fn wait_for_ssh(target: &SshTarget) -> Result<(), String> {
    const MAX_RETRIES: usize = 6;
    let mut last_err: Option<String> = None;
    for attempt in 1..=MAX_RETRIES {
        match test_connection(target).await {
            Ok(()) => {
                log::info!("✔ SSH ready (attempt {}/{})", attempt, MAX_RETRIES);
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e.to_string());
                if attempt < MAX_RETRIES {
                    log::info!("SSH not ready (attempt {}/{}): {}", attempt, MAX_RETRIES, e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "no attempts made".into()))
}

/// Best-effort post-mortem of a VM that just failed a detached exec.
/// Runs the read-only `script_cmd::diagnostic_capture_cmd` probe via
/// a separate SSH connection with its own short timeout. On any
/// failure (probe times out, ssh drops, …) returns a marker string
/// so the caller can still emit a coherent log line.
async fn capture_diagnostics(target: &SshTarget) -> String {
    let probe = crate::script_cmd::diagnostic_capture_cmd();
    match crate::ssh::exec(target, probe, Duration::from_secs(15)).await {
        Ok(out) => out,
        Err(e) => format!("(diagnostic capture failed: {e})"),
    }
}
