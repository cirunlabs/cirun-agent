//! Docker adapter: wraps `crate::docker::client::DockerClient` behind the `Executor` trait.

use super::{Executor, GpuRequest, OwnedRunner, ProvisionError, RunnerSpec, RunnerState};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// `image` and `name` flow into `docker run` argv positionally. A value
/// starting with `-` is parsed as a flag (`--privileged`, `-v=/:/host`, etc.)
/// → container escape / host mount / RCE. Reject anything that doesn't look
/// like a normal Docker reference (`registry/path/image:tag`,
/// `image@sha256:digest`) or a runner name (`cirun-foo--bar`). `--` argv
/// separators downstream are belt-and-braces; this is the suspenders.
fn is_safe_docker_identifier(s: &str) -> bool {
    if s.is_empty() || s.len() > 255 {
        return false;
    }
    if s.starts_with('-') {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':' | b'@'))
}

/// True when `docker inspect` stderr means "container does not exist". The
/// CLI prints different cases on different platforms — Docker Desktop on
/// macOS emits `Error: No such object: <name>` while docker-ce on Linux emits
/// `error: no such object: <name>`. Match case-insensitively against the
/// stable substring so the inspect→Absent path works on both. Without this,
/// the pre-spawn idempotency check returns `Transient` on Linux and the state
/// machine never reaches `spawn`.
pub(crate) fn is_docker_not_found(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("no such object") || lower.contains("no such container")
}

/// True when the Actions runner's `Runner.Listener` process is up inside the
/// container. Uses `pgrep -x` (exact basename) — NOT `-f` (full cmdline) —
/// because the provision_script bash invocation embeds the literal string
/// "Runner.Listener" in its argv (download URLs, log lines), which makes
/// `-f` falsely match the script wrapper itself well before the actual
/// runner binary is up. Any non-zero exit, including "container not
/// running" or "pgrep missing", is treated as "not yet" so the settle
/// loop keeps polling. Cheap (~50ms per call).
fn runner_listener_running(bin: &str, container: &str) -> bool {
    std::process::Command::new(bin)
        .args(["exec", container, "pgrep", "-x", "Runner.Listener"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Map a docker container state string (as returned by
/// `docker inspect --format {{.State.Status}}` or `docker ps --format {{.State}}`)
/// onto the executor-trait's RunnerState.
///
/// Note: `Absent` is NEVER returned here — the caller resolves "no such container"
/// (e.g. via inspect exit code) before calling this mapper.
pub(super) fn map_container_status(status: &str) -> RunnerState {
    match status {
        "running" => RunnerState::Healthy,
        "created" | "restarting" => RunnerState::Starting,
        "exited" | "dead" => RunnerState::Terminated {
            exit_code: None,
            last_logs: String::new(),
        },
        _ => RunnerState::Starting, // unknown statuses (paused, removing) treated as transitional
    }
}

pub struct DockerExecutor {
    client: Arc<crate::docker::client::DockerClient>,
}

impl DockerExecutor {
    pub fn new() -> Self {
        Self {
            client: Arc::new(crate::docker::client::DockerClient::new()),
        }
    }

    /// Cheap reachability check used by the registry probe. Returns the
    /// docker server version string on success.
    pub fn client_ping(&self) -> Result<String, crate::docker::errors::DockerError> {
        self.client.ping()
    }
}

#[async_trait]
impl Executor for DockerExecutor {
    fn settle_timeout(&self) -> Duration {
        // Long enough to cover config.sh + sudo + svc.sh install on a fresh
        // container. The old 45s default was too short — see HANDOFF.md #1.
        Duration::from_secs(120)
    }

    /// Reject `image`/`name` values that could be smuggled past `docker run`'s
    /// argument parser as flags (`--privileged`, `-v=/:/host`, etc.). Called
    /// by the state machine before any I/O — failure is `Incompatible`, never
    /// retried.
    fn validate(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        if !is_safe_docker_identifier(&spec.name) {
            return Err(ProvisionError::Incompatible(format!(
                "unsafe docker runner name '{}'",
                spec.name
            )));
        }
        if !is_safe_docker_identifier(&spec.image) {
            return Err(ProvisionError::Incompatible(format!(
                "unsafe docker image '{}'",
                spec.image
            )));
        }
        Ok(())
    }

    async fn inspect(&self, name: &str) -> Result<RunnerState, ProvisionError> {
        // `docker inspect --format {{.State.Status}}` returns nonzero + "Error: No such ..."
        // when the container is gone. Translate that to Absent.
        let bin = "docker";
        let out = std::process::Command::new(bin)
            .args([
                "inspect",
                "--format",
                "{{.State.Status}} {{.State.ExitCode}}",
                "--",
                name,
            ])
            .output()
            .map_err(|e| ProvisionError::transient(format!("docker inspect: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if is_docker_not_found(&stderr) {
                return Ok(RunnerState::Absent);
            }
            return Err(ProvisionError::transient(format!(
                "docker inspect failed: {}",
                stderr
            )));
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        let raw = raw.trim();
        let mut parts = raw.split_whitespace();
        let status = parts.next().unwrap_or("");
        let exit_code: Option<i32> = parts.next().and_then(|s| s.parse().ok());
        let mut state = map_container_status(status);
        // Decorate Terminated with the real exit code + last 30 log lines.
        if let RunnerState::Terminated { .. } = state {
            let logs = std::process::Command::new(bin)
                .args(["logs", "--tail", "30", "--", name])
                .output()
                .map(|o| {
                    let s = String::from_utf8_lossy(&o.stdout);
                    let e = String::from_utf8_lossy(&o.stderr);
                    format!("{e}\n{s}")
                })
                .unwrap_or_default();
            state = RunnerState::Terminated {
                exit_code,
                last_logs: logs,
            };
        }
        // Inner-runner readiness: a container being `running` only means the
        // process tree exists, not that the GitHub Actions runner inside has
        // finished `config.sh` and started polling. Until `Runner.Listener`
        // is up, the runner is invisible to GitHub and the api will class it
        // as orphan and issue a delete (~25-30s). Demote `Healthy` to
        // `Starting` while the listener isn't there yet — the trait's settle
        // loop then keeps `provision()` blocked, which keeps the runner in
        // the agent's `in_flight` set, which makes `handle_orphaned_runners`
        // refuse the racing delete. Empirically Runner.Listener appears
        // 20-35s after `docker run` returns; settle_timeout is 120s.
        if state == RunnerState::Healthy && !runner_listener_running(bin, name) {
            state = RunnerState::Starting;
        }
        Ok(state)
    }

    async fn spawn(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        use crate::docker::models::{ContainerCommand, GpuSelection, RunnerContainerSpec};
        let gpus = match &spec.gpu {
            GpuRequest::None => GpuSelection::None,
            GpuRequest::All => GpuSelection::All,
            GpuRequest::Count(n) => GpuSelection::Count(*n),
        };
        let container_spec = RunnerContainerSpec {
            name: spec.name.clone(),
            image: spec.image.clone(),
            gpus,
            cpus: Some(spec.cpu),
            memory_gb: Some(spec.memory_gb),
            env: vec![("CIRUN_RUNNER_NAME".into(), spec.name.clone())],
            command: ContainerCommand::Script(spec.provision_script.clone()),
        };
        self.client
            .run_runner(&container_spec)
            .map(|_| ())
            .map_err(|e| ProvisionError::transient(format!("docker run failed: {e}")))
    }

    async fn kill(&self, name: &str) -> Result<(), ProvisionError> {
        self.client
            .stop_and_remove(name)
            .map_err(|e| ProvisionError::transient(format!("docker rm: {e}")))
    }

    async fn list_owned(&self) -> Result<Vec<OwnedRunner>, ProvisionError> {
        let infos = self
            .client
            .list_runner_containers("cirun.runner=true")
            .map_err(|e| ProvisionError::transient(format!("docker ps: {e}")))?;
        Ok(infos
            .into_iter()
            .map(|i| OwnedRunner {
                name: i.name,
                state: map_container_status(&i.state),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_maps_to_healthy() {
        assert_eq!(map_container_status("running"), RunnerState::Healthy);
    }

    #[test]
    fn created_maps_to_starting() {
        assert_eq!(map_container_status("created"), RunnerState::Starting);
    }

    #[test]
    fn restarting_maps_to_starting() {
        assert_eq!(map_container_status("restarting"), RunnerState::Starting);
    }

    #[test]
    fn exited_maps_to_terminated() {
        assert!(matches!(
            map_container_status("exited"),
            RunnerState::Terminated { .. }
        ));
    }

    #[test]
    fn dead_maps_to_terminated() {
        assert!(matches!(
            map_container_status("dead"),
            RunnerState::Terminated { .. }
        ));
    }

    #[test]
    fn safe_identifier_accepts_normal_images_and_names() {
        for s in [
            "ubuntu:24.04",
            "ghcr.io/aktech/runner:latest",
            "cirun-gpu-runner:latest",
            "image@sha256:abcdef0123",
            "cirun-aktech--repo-1a5bc0163a",
        ] {
            assert!(is_safe_docker_identifier(s), "rejected: {s}");
        }
    }

    #[test]
    fn safe_identifier_rejects_flag_shapes() {
        for s in [
            "--privileged",
            "-v=/:/host",
            "--security-opt=apparmor=unconfined",
            "-",
        ] {
            assert!(!is_safe_docker_identifier(s), "accepted leading-dash: {s}");
        }
    }

    #[test]
    fn safe_identifier_rejects_shell_metachars_and_whitespace() {
        for s in [
            "ubuntu;rm -rf /",
            "ubuntu image",
            "ubuntu`whoami`",
            "ubuntu|sh",
        ] {
            assert!(!is_safe_docker_identifier(s), "accepted: {s:?}");
        }
    }

    fn rspec(name: &str, image: &str) -> RunnerSpec {
        RunnerSpec {
            name: name.into(),
            provision_script: String::new(),
            image: image.into(),
            cpu: 2,
            memory_gb: 4,
            disk_gb: 20,
            gpu: GpuRequest::None,
            login: crate::executor::RunnerLogin {
                username: "u".into(),
                password: "p".into(),
            },
        }
    }

    #[test]
    fn validate_rejects_flag_shaped_image() {
        let exec = DockerExecutor::new();
        let err = exec
            .validate(&rspec("ok", "--privileged"))
            .expect_err("must reject flag-shaped image");
        assert!(matches!(err, ProvisionError::Incompatible(_)));
    }

    #[test]
    fn validate_rejects_flag_shaped_name() {
        let exec = DockerExecutor::new();
        let err = exec
            .validate(&rspec("--rm", "ubuntu:24.04"))
            .expect_err("must reject flag-shaped name");
        assert!(matches!(err, ProvisionError::Incompatible(_)));
    }

    #[test]
    fn validate_accepts_normal_spec() {
        let exec = DockerExecutor::new();
        assert!(exec.validate(&rspec("cirun-r1", "ubuntu:24.04")).is_ok());
    }

    #[test]
    fn not_found_classifier_matches_macos_docker_desktop_form() {
        assert!(is_docker_not_found(
            "Error: No such object: cirun-aktech--demo-25b350fc9b\n"
        ));
    }

    #[test]
    fn not_found_classifier_matches_linux_docker_ce_form() {
        // docker-ce 29.x on Ubuntu emits the lowercase variant — observed on
        // 192.168.50.106 on 2026-05-15. Pre-fix this slipped past the
        // case-sensitive check and the state machine never spawned.
        assert!(is_docker_not_found(
            "\nerror: no such object: cirun-aktech--demo-25b350fc9b\n"
        ));
    }

    #[test]
    fn not_found_classifier_matches_no_such_container_phrasing() {
        assert!(is_docker_not_found("Error: No such container: foo"));
        assert!(is_docker_not_found("error: no such container: foo"));
    }

    #[test]
    fn not_found_classifier_rejects_unrelated_errors() {
        assert!(!is_docker_not_found(
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock"
        ));
        assert!(!is_docker_not_found(
            "permission denied while trying to connect"
        ));
    }
}
