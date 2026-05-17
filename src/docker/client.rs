use std::process::{Command, Stdio};

use log::{debug, info};

use crate::docker::errors::DockerError;
use crate::docker::models::{ContainerCommand, ContainerInfo, GpuSelection, RunnerContainerSpec};

const DEFAULT_DOCKER_BIN: &str = "docker";

/// Thin wrapper around the `docker` CLI.
///
/// Shells out instead of using bollard to avoid pulling in another dependency
/// for the MVP. The argv it produces is deterministic and unit-tested.
pub struct DockerClient {
    bin: String,
}

impl Default for DockerClient {
    fn default() -> Self {
        Self {
            bin: DEFAULT_DOCKER_BIN.to_string(),
        }
    }
}

impl DockerClient {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    #[allow(dead_code)] // Reserved for future tests that need a non-default docker binary path.
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    /// Build the argv for `docker run` from a runner container spec.
    /// Public so it can be unit-tested without exec'ing docker.
    pub fn build_run_argv(spec: &RunnerContainerSpec) -> Vec<String> {
        let mut argv: Vec<String> = vec!["run".into(), "-d".into(), "--restart=no".into()];

        argv.push("--name".into());
        argv.push(spec.name.clone());

        // Make GitHub Actions' "Machine name" show the runner name
        // instead of the 12-char container-id hash. Linux hostname is
        // capped at 63 bytes (RFC 1035 label limit); truncate so a
        // future longer runner-name pattern doesn't break `docker run`.
        argv.push("--hostname".into());
        argv.push(spec.name.chars().take(63).collect());

        // `cirun.runner=true` is how `list_owned` finds our containers. Every
        // cirun-spawned container MUST carry it — without it the runner is
        // invisible to the registry, max_runners cap, and orphan cleanup.
        argv.push("--label".into());
        argv.push("cirun.runner=true".into());

        // Docker daemon access flags. Both default off; the runner-only
        // mode that results is what the cirun-docker-runner-image
        // README documents as the default. .cirun.yml opts in:
        //   extra_config.privileged: true            → DinD
        //   extra_config.docker_socket_mount: true   → out-of-docker
        if spec.privileged {
            argv.push("--privileged".into());
        }
        if spec.mount_docker_socket {
            argv.push("-v".into());
            argv.push("/var/run/docker.sock:/var/run/docker.sock".into());
        }

        match spec.gpus {
            GpuSelection::All => {
                argv.push("--gpus".into());
                argv.push("all".into());
            }
            GpuSelection::Count(n) => {
                argv.push("--gpus".into());
                argv.push(n.to_string());
            }
            GpuSelection::None => {}
        }

        if let Some(c) = spec.cpus {
            argv.push("--cpus".into());
            argv.push(c.to_string());
        }
        if let Some(m) = spec.memory_gb {
            argv.push("--memory".into());
            argv.push(format!("{}g", m));
        }

        for (k, v) in &spec.env {
            argv.push("-e".into());
            argv.push(format!("{}={}", k, v));
        }

        argv.push(spec.image.clone());

        match &spec.command {
            ContainerCommand::Script(s) => {
                argv.push("bash".into());
                argv.push("-lc".into());
                argv.push(s.clone());
            }
            ContainerCommand::Argv(a) => {
                argv.extend(a.iter().cloned());
            }
        }

        argv
    }

    /// Run a runner container detached. Returns the container ID on success.
    pub fn run_runner(&self, spec: &RunnerContainerSpec) -> Result<String, DockerError> {
        let argv = Self::build_run_argv(spec);
        debug!("docker {}", argv.join(" "));
        let out = Command::new(&self.bin).args(&argv).output()?;
        if !out.status.success() {
            return Err(DockerError::CommandFailed {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        info!(
            "Started container {} (id={})",
            spec.name,
            &id[..id.len().min(12)]
        );
        Ok(id)
    }

    pub fn stop_and_remove(&self, name: &str) -> Result<(), DockerError> {
        // `docker rm -f` stops+removes; ignore the "no such container" case.
        // `--` argv separator prevents a `-flag`-shaped name being parsed as
        // an option (defence-in-depth — `DockerExecutor::validate` rejects
        // such names upstream).
        let out = Command::new(&self.bin)
            .args(["rm", "-f", "--", name])
            .output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // docker-ce on Linux emits lowercase ("error: no such container")
            // whereas Docker Desktop emits capitalised. Reuse the executor's
            // case-insensitive classifier so both paths agree.
            if crate::executor::docker::is_docker_not_found(&stderr) {
                return Ok(());
            }
            return Err(DockerError::CommandFailed {
                code: out.status.code(),
                stderr: stderr.into_owned(),
            });
        }
        Ok(())
    }

    pub fn list_runner_containers(&self, label: &str) -> Result<Vec<ContainerInfo>, DockerError> {
        let out = Command::new(&self.bin)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("label={}", label),
                "--format",
                "{{.Names}}\t{{.State}}",
            ])
            .output()?;
        if !out.status.success() {
            return Err(DockerError::CommandFailed {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let mut infos = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut parts = line.splitn(2, '\t');
            let name = parts.next().unwrap_or("").trim();
            let state = parts.next().unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            infos.push(ContainerInfo {
                name: name.to_string(),
                state: state.to_string(),
            });
        }
        Ok(infos)
    }

    /// Verify the docker daemon is reachable and report `docker version` output.
    pub fn ping(&self) -> Result<String, DockerError> {
        let out = Command::new(&self.bin)
            .args(["version", "--format", "{{.Server.Version}}"])
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            return Err(DockerError::CommandFailed {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Run `docker run --rm --gpus all <image> nvidia-smi` and return stdout.
    /// Used by the `--docker-smoke-test` mode to validate GPU passthrough into a container.
    pub fn smoke_test_gpu(&self, image: &str) -> Result<String, DockerError> {
        let out = Command::new(&self.bin)
            .args(["run", "--rm", "--gpus", "all", image, "nvidia-smi"])
            .output()?;
        if !out.status.success() {
            return Err(DockerError::CommandFailed {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(
        name: &str,
        image: &str,
        gpus: GpuSelection,
        cmd: ContainerCommand,
    ) -> RunnerContainerSpec {
        RunnerContainerSpec {
            name: name.into(),
            image: image.into(),
            gpus,
            cpus: None,
            memory_gb: None,
            env: vec![],
            command: cmd,
            privileged: false,
            mount_docker_socket: false,
        }
    }

    #[test]
    fn run_argv_privileged_emits_flag() {
        let mut s = spec(
            "r1",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["true".into()]),
        );
        s.privileged = true;
        let argv = DockerClient::build_run_argv(&s);
        assert!(argv.contains(&"--privileged".to_string()));
    }

    #[test]
    fn run_argv_docker_socket_emits_volume_mount() {
        let mut s = spec(
            "r1",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["true".into()]),
        );
        s.mount_docker_socket = true;
        let argv = DockerClient::build_run_argv(&s);
        let pos = argv.iter().position(|a| a == "-v").expect("missing -v");
        assert_eq!(argv[pos + 1], "/var/run/docker.sock:/var/run/docker.sock");
    }

    #[test]
    fn run_argv_defaults_omit_both_flags() {
        let s = spec(
            "r1",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["true".into()]),
        );
        let argv = DockerClient::build_run_argv(&s);
        assert!(!argv.iter().any(|a| a == "--privileged"));
        assert!(!argv.iter().any(|a| a == "-v"));
    }

    #[test]
    fn run_argv_basic_no_gpu() {
        let s = spec(
            "r1",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["echo".into(), "hi".into()]),
        );
        let argv = DockerClient::build_run_argv(&s);
        assert_eq!(
            argv,
            vec![
                "run",
                "-d",
                "--restart=no",
                "--name",
                "r1",
                "--hostname",
                "r1",
                "--label",
                "cirun.runner=true",
                "ubuntu:24.04",
                "echo",
                "hi"
            ]
        );
    }

    #[test]
    fn run_argv_sets_hostname_to_runner_name() {
        // GitHub Actions reads the container hostname as "Machine name"
        // in the Set up job step; defaulting to the docker-assigned
        // 12-char container-id hash made job logs uninformative.
        let s = spec(
            "cirun-aktech--demo-abc123",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["true".into()]),
        );
        let argv = DockerClient::build_run_argv(&s);
        let pos = argv
            .iter()
            .position(|a| a == "--hostname")
            .expect("missing --hostname");
        assert_eq!(argv[pos + 1], "cirun-aktech--demo-abc123");
    }

    #[test]
    fn run_argv_truncates_hostname_to_63_chars() {
        // Linux hostnames are capped at 63 bytes (RFC 1035 label
        // limit); docker run rejects longer values with an error.
        let long_name: String = "x".repeat(120);
        let s = spec(
            &long_name,
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["true".into()]),
        );
        let argv = DockerClient::build_run_argv(&s);
        let pos = argv
            .iter()
            .position(|a| a == "--hostname")
            .expect("missing --hostname");
        assert_eq!(argv[pos + 1].len(), 63);
    }

    /// Every cirun-spawned container MUST carry `cirun.runner=true` — the
    /// agent's `list_owned` filters by this label, so without it docker
    /// runners are invisible to the registry, the max_runners cap, and the
    /// orphan-cleanup path.
    #[test]
    fn run_argv_always_stamps_cirun_runner_label() {
        for gpus in [
            GpuSelection::None,
            GpuSelection::All,
            GpuSelection::Count(2),
        ] {
            let s = spec(
                "r",
                "img",
                gpus.clone(),
                ContainerCommand::Argv(vec!["true".into()]),
            );
            let argv = DockerClient::build_run_argv(&s);
            assert!(
                argv.windows(2)
                    .any(|w| w == ["--label", "cirun.runner=true"]),
                "missing --label cirun.runner=true for gpus={:?}: {:?}",
                gpus,
                argv
            );
        }
    }

    #[test]
    fn run_argv_with_gpu_all_includes_flag() {
        let s = spec(
            "r-gpu",
            "nvidia/cuda:12.4.0-base-ubuntu22.04",
            GpuSelection::All,
            ContainerCommand::Argv(vec!["nvidia-smi".into()]),
        );
        let argv = DockerClient::build_run_argv(&s);
        assert!(
            argv.windows(2).any(|w| w == ["--gpus", "all"]),
            "expected --gpus all in argv, got {:?}",
            argv
        );
    }

    #[test]
    fn run_argv_with_gpu_count_emits_number() {
        let s = spec(
            "r-gpu-2",
            "nvidia/cuda:12.4.0-base-ubuntu22.04",
            GpuSelection::Count(2),
            ContainerCommand::Argv(vec!["nvidia-smi".into()]),
        );
        let argv = DockerClient::build_run_argv(&s);
        assert!(
            argv.windows(2).any(|w| w == ["--gpus", "2"]),
            "expected --gpus 2 in argv, got {:?}",
            argv
        );
    }

    #[test]
    fn run_argv_script_uses_bash_lc() {
        let s = spec(
            "r-script",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Script("echo $FOO && nvidia-smi".into()),
        );
        let argv = DockerClient::build_run_argv(&s);
        let tail = &argv[argv.len() - 3..];
        assert_eq!(tail, ["bash", "-lc", "echo $FOO && nvidia-smi"]);
    }

    #[test]
    fn run_argv_env_vars_are_passed_with_dash_e() {
        let mut s = spec(
            "r-env",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Script("env".into()),
        );
        s.env = vec![
            ("RUNNER_TOKEN".into(), "abc".into()),
            ("REPO_URL".into(), "https://github.com/o/r".into()),
        ];
        let argv = DockerClient::build_run_argv(&s);
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            if argv[i] == "-e" {
                let kv = argv[i + 1].splitn(2, '=').collect::<Vec<_>>();
                pairs.push((kv[0].into(), kv[1].into()));
                i += 2;
            } else {
                i += 1;
            }
        }
        assert_eq!(
            pairs,
            vec![
                ("RUNNER_TOKEN".into(), "abc".into()),
                ("REPO_URL".into(), "https://github.com/o/r".into()),
            ]
        );
    }

    #[test]
    fn run_argv_resources_are_applied() {
        let mut s = spec(
            "r-res",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["true".into()]),
        );
        s.cpus = Some(4);
        s.memory_gb = Some(16);
        let argv = DockerClient::build_run_argv(&s);
        assert!(argv.windows(2).any(|w| w == ["--cpus", "4"]));
        assert!(argv.windows(2).any(|w| w == ["--memory", "16g"]));
    }

    #[test]
    fn name_appears_after_name_flag() {
        let s = spec(
            "my-runner-xyz",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["true".into()]),
        );
        let argv = DockerClient::build_run_argv(&s);
        let pos = argv.iter().position(|a| a == "--name").unwrap();
        assert_eq!(argv[pos + 1], "my-runner-xyz");
    }

    #[test]
    fn run_argv_no_gpu_omits_gpus_flag() {
        let s = spec(
            "r-nogpu",
            "ubuntu:24.04",
            GpuSelection::None,
            ContainerCommand::Argv(vec!["true".into()]),
        );
        let argv = DockerClient::build_run_argv(&s);
        assert!(!argv.iter().any(|a| a == "--gpus"));
    }
}
