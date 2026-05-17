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
}
