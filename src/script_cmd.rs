//! Shared shell-command builders for the provision-via-SSH flow.

/// Build the remote shell command that launches the provision script
/// detached and returns its PID on stdout.
///
/// The `< /dev/null` redirect on the backgrounded process is load-bearing:
/// without it the backgrounded `bash` inherits stdin from the SSH channel,
/// OpenSSH keeps the channel open until that fd is released, and the
/// agent's 60s hard timeout on `exec` fires — at which point the caller
/// treats the provision as failed and destroys the VM mid-job, killing
/// any GitHub Actions runner that had already registered inside it.
pub fn detached_provision_cmd(remote_path: &str, use_sudo: bool) -> String {
    let invocation = if use_sudo {
        format!("sudo nohup bash {remote_path}")
    } else {
        format!("nohup {remote_path}")
    };
    format!(
        "chmod +x {remote_path} && {invocation} \
         > /tmp/script_stdout.log 2> /tmp/script_stderr.log < /dev/null & echo $!"
    )
}

/// Build a single-shot diagnostic command that captures the state of a
/// VM mid-provision: process list, the tail of the script's stdout/stderr
/// captures, the cirun provision log, and the fd table of every bash
/// process (so we can see what's still holding open the SSH channel).
///
/// Section headers (`=== name ===`) bracket each block so a Loki search
/// for the failing runner name returns the full payload as one
/// contiguous message. All commands tolerate missing files / lack of
/// permission so the capture never aborts mid-bundle.
// Linux-only consumer (`executor::meda`); macOS lume path doesn't use it yet.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn diagnostic_capture_cmd() -> &'static str {
    "echo '=== uptime ==='; uptime; \
     echo '=== ps ==='; ps -eo pid,ppid,stat,etime,cmd 2>/dev/null | head -50; \
     echo '=== /tmp/script_stdout.log (tail 50) ==='; sudo tail -50 /tmp/script_stdout.log 2>/dev/null; \
     echo '=== /tmp/script_stderr.log (tail 50) ==='; sudo tail -50 /tmp/script_stderr.log 2>/dev/null; \
     echo '=== /tmp/cirun-provision.log (tail 50) ==='; sudo tail -50 /tmp/cirun-provision.log 2>/dev/null; \
     echo '=== bash fd table ==='; for p in $(pgrep bash 2>/dev/null); do echo \"--- pid=$p ---\"; sudo ls -la /proc/$p/fd 2>/dev/null | head -10; done; \
     echo '=== runner-listener? ==='; pgrep -af Runner.Listener 2>/dev/null; \
     echo '=== END ==='"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_stdin_from_dev_null_under_sudo() {
        let cmd = detached_provision_cmd("/tmp/script_0.sh", true);
        assert!(
            cmd.contains("< /dev/null"),
            "detached cmd must redirect stdin or SSH channel hangs \
             and the 60s timeout kills live VMs; got: {cmd}"
        );
    }

    #[test]
    fn redirects_stdin_from_dev_null_without_sudo() {
        let cmd = detached_provision_cmd("/tmp/script_0.sh", false);
        assert!(
            cmd.contains("< /dev/null"),
            "detached cmd must redirect stdin; got: {cmd}"
        );
    }

    #[test]
    fn returns_background_pid() {
        let cmd = detached_provision_cmd("/tmp/script_0.sh", true);
        assert!(
            cmd.ends_with("& echo $!"),
            "detached cmd must background the script and echo its PID; got: {cmd}"
        );
    }

    #[test]
    fn sudo_variant_runs_bash_explicitly() {
        let cmd = detached_provision_cmd("/tmp/script_0.sh", true);
        assert!(cmd.contains("sudo nohup bash /tmp/script_0.sh"));
    }

    #[test]
    fn non_sudo_variant_invokes_path_directly() {
        let cmd = detached_provision_cmd("/tmp/script_0.sh", false);
        assert!(cmd.contains("nohup /tmp/script_0.sh"));
        assert!(!cmd.contains("sudo"));
    }

    #[test]
    fn diagnostic_capture_includes_required_sections() {
        let cmd = diagnostic_capture_cmd();
        for marker in [
            "=== uptime ===",
            "=== ps ===",
            "=== /tmp/script_stdout.log",
            "=== /tmp/script_stderr.log",
            "=== /tmp/cirun-provision.log",
            "=== bash fd table ===",
            "=== runner-listener? ===",
            "=== END ===",
        ] {
            assert!(
                cmd.contains(marker),
                "diagnostic capture missing section {marker:?}"
            );
        }
    }

    #[test]
    fn diagnostic_capture_tolerates_missing_files() {
        // Every tail/ls/pgrep must swallow non-zero exits so the whole
        // capture doesn't bail when a file isn't present yet.
        let cmd = diagnostic_capture_cmd();
        let tail_count = cmd.matches("tail -50").count();
        let suppression_count = cmd.matches("2>/dev/null").count();
        assert!(
            suppression_count >= tail_count,
            "every tail must redirect stderr; got tail_count={tail_count} \
             suppression_count={suppression_count}"
        );
    }
}
