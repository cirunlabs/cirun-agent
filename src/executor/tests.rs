//! Unit tests for the executor trait + state machine. Lives in a sibling
//! file so `mod.rs` stays under the project's 500-line soft cap.

use super::*;
use serde_json::json;
use std::sync::Mutex;

/// A scriptable executor for testing the default `provision` state machine.
/// `inspect_script` is a queue: each call to `inspect()` pops the front.
struct FakeExecutor {
    inspect_script: Mutex<Vec<RunnerState>>,
    spawn_calls: Mutex<u32>,
    kill_calls: Mutex<u32>,
    post_spawn_calls: Mutex<u32>,
    settle_timeout: Duration,
    settle_interval: Duration,
}

impl FakeExecutor {
    fn with_script(script: Vec<RunnerState>) -> Self {
        Self {
            inspect_script: Mutex::new(script),
            spawn_calls: Mutex::new(0),
            kill_calls: Mutex::new(0),
            post_spawn_calls: Mutex::new(0),
            settle_timeout: Duration::from_millis(500),
            settle_interval: Duration::from_millis(1),
        }
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    fn settle_timeout(&self) -> Duration {
        self.settle_timeout
    }
    fn settle_poll_interval(&self) -> Duration {
        self.settle_interval
    }
    async fn inspect(&self, _name: &str) -> Result<RunnerState, ProvisionError> {
        let mut q = self.inspect_script.lock().unwrap();
        Ok(if q.is_empty() {
            RunnerState::Healthy
        } else {
            q.remove(0)
        })
    }
    async fn spawn(&self, _spec: &RunnerSpec) -> Result<(), ProvisionError> {
        *self.spawn_calls.lock().unwrap() += 1;
        Ok(())
    }
    async fn kill(&self, _name: &str) -> Result<(), ProvisionError> {
        *self.kill_calls.lock().unwrap() += 1;
        Ok(())
    }
    async fn list_owned(&self) -> Result<Vec<OwnedRunner>, ProvisionError> {
        Ok(vec![])
    }
    async fn run_post_spawn(&self, _spec: &RunnerSpec) -> Result<(), ProvisionError> {
        *self.post_spawn_calls.lock().unwrap() += 1;
        Ok(())
    }
}

fn spec(name: &str) -> RunnerSpec {
    RunnerSpec {
        name: name.into(),
        provision_script: String::new(),
        image: "ubuntu:24.04".into(),
        cpu: 2,
        memory_gb: 4,
        disk_gb: 20,
        gpu: GpuRequest::None,
        docker_privileged: false,
        docker_mount_socket: false,
        login: RunnerLogin {
            username: "runner".into(),
            password: "p".into(),
        },
    }
}

#[test]
fn derive_kind_explicit_executor_wins() {
    let cfg = json!({ "executor": "docker" });
    assert_eq!(
        resolve_executor_kind(None, Some(&cfg), "linux").unwrap(),
        ExecutorKind::Docker
    );
    let cfg = json!({ "executor": "lume" });
    assert_eq!(
        resolve_executor_kind(None, Some(&cfg), "linux").unwrap(),
        ExecutorKind::Lume
    );
}

#[test]
fn derive_kind_legacy_container_true_maps_to_docker() {
    let cfg = json!({ "container": true });
    assert_eq!(
        resolve_executor_kind(None, Some(&cfg), "linux").unwrap(),
        ExecutorKind::Docker
    );
}

#[test]
fn derive_kind_os_defaults() {
    assert_eq!(
        resolve_executor_kind(None, None, "linux").unwrap(),
        ExecutorKind::Meda
    );
    assert_eq!(
        resolve_executor_kind(None, None, "macos").unwrap(),
        ExecutorKind::Lume
    );
}

#[test]
fn derive_kind_explicit_overrides_container_legacy() {
    let cfg = json!({ "executor": "meda", "container": true });
    assert_eq!(
        resolve_executor_kind(None, Some(&cfg), "linux").unwrap(),
        ExecutorKind::Meda
    );
}

#[test]
fn derive_kind_unknown_executor_errors() {
    let cfg = json!({ "executor": "kata" });
    assert!(resolve_executor_kind(None, Some(&cfg), "linux").is_err());
}

#[test]
fn resolve_top_level_wins_over_extra_config() {
    let cfg = json!({ "executor": "meda", "container": true });
    assert_eq!(
        resolve_executor_kind(Some("docker"), Some(&cfg), "linux").unwrap(),
        ExecutorKind::Docker
    );
}

#[test]
fn resolve_falls_through_to_extra_config_when_top_empty() {
    let cfg = json!({ "executor": "docker" });
    assert_eq!(
        resolve_executor_kind(None, Some(&cfg), "linux").unwrap(),
        ExecutorKind::Docker
    );
    // empty string is treated as "missing" — fall through, not error
    assert_eq!(
        resolve_executor_kind(Some(""), Some(&cfg), "linux").unwrap(),
        ExecutorKind::Docker
    );
}

#[test]
fn resolve_falls_through_to_os_default() {
    assert_eq!(
        resolve_executor_kind(None, None, "macos").unwrap(),
        ExecutorKind::Lume
    );
}

#[test]
fn resolve_unknown_top_executor_errors() {
    assert!(resolve_executor_kind(Some("kata"), None, "linux").is_err());
}

#[test]
fn resolve_top_executor_is_case_and_whitespace_insensitive() {
    assert_eq!(
        resolve_executor_kind(Some("DOCKER"), None, "linux").unwrap(),
        ExecutorKind::Docker
    );
    assert_eq!(
        resolve_executor_kind(Some(" docker "), None, "linux").unwrap(),
        ExecutorKind::Docker
    );
    assert_eq!(
        resolve_executor_kind(Some("Lume"), None, "macos").unwrap(),
        ExecutorKind::Lume
    );
}

#[test]
fn resolve_extra_config_executor_is_case_insensitive() {
    let cfg = json!({ "executor": "MEDA" });
    assert_eq!(
        resolve_executor_kind(None, Some(&cfg), "linux").unwrap(),
        ExecutorKind::Meda
    );
}

#[test]
fn parse_gpu_missing_means_none() {
    assert_eq!(parse_gpu_request(None).unwrap(), GpuRequest::None);
}

#[test]
fn parse_gpu_all() {
    let v = json!("all");
    assert_eq!(parse_gpu_request(Some(&v)).unwrap(), GpuRequest::All);
    let v = json!("ALL");
    assert_eq!(parse_gpu_request(Some(&v)).unwrap(), GpuRequest::All);
}

#[test]
fn parse_gpu_none_string() {
    let v = json!("none");
    assert_eq!(parse_gpu_request(Some(&v)).unwrap(), GpuRequest::None);
}

#[test]
fn parse_gpu_count_integer() {
    let v = json!(2);
    assert_eq!(parse_gpu_request(Some(&v)).unwrap(), GpuRequest::Count(2));
}

#[test]
fn parse_gpu_count_string() {
    let v = json!("3");
    assert_eq!(parse_gpu_request(Some(&v)).unwrap(), GpuRequest::Count(3));
}

#[test]
fn parse_gpu_zero_is_none() {
    let v = json!(0);
    assert_eq!(parse_gpu_request(Some(&v)).unwrap(), GpuRequest::None);
}

#[test]
fn parse_gpu_invalid_string_errors() {
    let v = json!("seventeen");
    assert!(parse_gpu_request(Some(&v)).is_err());
}

#[tokio::test]
async fn provision_calls_run_post_spawn_after_settle_healthy() {
    let exec = FakeExecutor::with_script(vec![RunnerState::Absent, RunnerState::Healthy]);
    exec.provision(&spec("r1")).await.unwrap();
    assert_eq!(
        *exec.post_spawn_calls.lock().unwrap(),
        1,
        "must call run_post_spawn after VM is healthy"
    );
}

#[tokio::test]
async fn provision_does_not_call_run_post_spawn_on_idempotent_skip() {
    let exec = FakeExecutor::with_script(vec![RunnerState::Healthy]);
    exec.provision(&spec("r1")).await.unwrap();
    assert_eq!(
        *exec.post_spawn_calls.lock().unwrap(),
        0,
        "skip path must not re-run post_spawn"
    );
}

#[tokio::test]
async fn provision_is_noop_when_runner_already_healthy() {
    let exec = FakeExecutor::with_script(vec![RunnerState::Healthy]);
    exec.provision(&spec("r1")).await.unwrap();
    assert_eq!(
        *exec.spawn_calls.lock().unwrap(),
        0,
        "must not spawn when healthy"
    );
    assert_eq!(
        *exec.kill_calls.lock().unwrap(),
        0,
        "must not kill when healthy"
    );
}

#[tokio::test]
async fn provision_spawns_when_absent() {
    let exec = FakeExecutor::with_script(vec![RunnerState::Absent, RunnerState::Healthy]);
    exec.provision(&spec("r1")).await.unwrap();
    assert_eq!(
        *exec.spawn_calls.lock().unwrap(),
        1,
        "must spawn once when absent"
    );
    assert_eq!(
        *exec.kill_calls.lock().unwrap(),
        0,
        "must not kill when absent"
    );
}

#[tokio::test]
async fn provision_polls_until_healthy_after_spawn() {
    // Absent (initial) → spawn called → Starting (poll 1) → Starting (poll 2) → Healthy (poll 3) → Ok
    let exec = FakeExecutor::with_script(vec![
        RunnerState::Absent,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Healthy,
    ]);
    exec.provision(&spec("r1")).await.unwrap();
    assert_eq!(*exec.spawn_calls.lock().unwrap(), 1);
    // script must be fully consumed — 4 inspect calls
    assert!(
        exec.inspect_script.lock().unwrap().is_empty(),
        "all inspects must be consumed"
    );
}

#[tokio::test]
async fn provision_returns_transient_on_settle_timeout() {
    // Absent → spawn → endless Starting (script empty by call 3 — falls through to Healthy default).
    // Use short timeout so we exhaust the deadline before script ends naturally.
    let mut exec = FakeExecutor::with_script(vec![
        RunnerState::Absent,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Starting,
        RunnerState::Starting,
    ]);
    exec.settle_timeout = Duration::from_millis(10);
    exec.settle_interval = Duration::from_millis(3);
    let err = exec.provision(&spec("r1")).await.unwrap_err();
    assert!(
        matches!(err, ProvisionError::Transient { .. }),
        "expected Transient, got {:?}",
        err
    );
    assert!(
        *exec.kill_calls.lock().unwrap() >= 1,
        "must reap on timeout"
    );
}

#[tokio::test]
async fn provision_returns_transient_when_terminated_during_settle() {
    let exec = FakeExecutor::with_script(vec![
        RunnerState::Absent,
        RunnerState::Starting,
        RunnerState::Terminated {
            exit_code: Some(137),
            last_logs: "OOM".into(),
        },
    ]);
    let err = exec.provision(&spec("r1")).await.unwrap_err();
    assert!(
        matches!(err, ProvisionError::Transient { .. }),
        "expected Transient, got {:?}",
        err
    );
    assert!(
        *exec.kill_calls.lock().unwrap() >= 1,
        "must reap exited runner"
    );
}

#[tokio::test]
async fn provision_reaps_then_spawns_when_terminated() {
    let exec = FakeExecutor::with_script(vec![
        RunnerState::Terminated {
            exit_code: Some(1),
            last_logs: "boom".into(),
        },
        RunnerState::Healthy,
    ]);
    exec.provision(&spec("r1")).await.unwrap();
    assert_eq!(
        *exec.kill_calls.lock().unwrap(),
        1,
        "must reap stale corpse"
    );
    assert_eq!(
        *exec.spawn_calls.lock().unwrap(),
        1,
        "must spawn fresh runner"
    );
}

// ── executor_serves_os: which runner OS each executor can produce ──
//
// Regression coverage for issue #14 — Docker on macOS must serve linux
// runners (Docker Desktop runs linux containers on a mac host). The old
// gate compared runner.os to agent.os and silently dropped these jobs.

#[test]
fn docker_serves_linux_runners() {
    assert!(executor_serves_os(ExecutorKind::Docker, "linux"));
}

#[test]
fn docker_rejects_macos_runners() {
    // Docker cannot produce macOS containers, regardless of host OS.
    assert!(!executor_serves_os(ExecutorKind::Docker, "macos"));
}

#[test]
fn meda_serves_only_linux() {
    assert!(executor_serves_os(ExecutorKind::Meda, "linux"));
    assert!(!executor_serves_os(ExecutorKind::Meda, "macos"));
}

#[test]
fn lume_serves_only_macos() {
    assert!(executor_serves_os(ExecutorKind::Lume, "macos"));
    assert!(!executor_serves_os(ExecutorKind::Lume, "linux"));
}

#[test]
fn executor_serves_os_is_case_insensitive() {
    assert!(executor_serves_os(ExecutorKind::Docker, "LINUX"));
    assert!(executor_serves_os(ExecutorKind::Lume, "macOS"));
}

// ── parse_executor_filter: --executors <list> flag (issue #15) ──

#[test]
fn parse_executor_filter_single_value() {
    let f = parse_executor_filter(Some("docker")).unwrap();
    assert!(f.allows(ExecutorKind::Docker));
    assert!(!f.allows(ExecutorKind::Meda));
    assert!(!f.allows(ExecutorKind::Lume));
}

#[test]
fn parse_executor_filter_comma_list() {
    let f = parse_executor_filter(Some("docker,meda")).unwrap();
    assert!(f.allows(ExecutorKind::Docker));
    assert!(f.allows(ExecutorKind::Meda));
    assert!(!f.allows(ExecutorKind::Lume));
}

#[test]
fn parse_executor_filter_trims_and_ignores_blank_segments() {
    let f = parse_executor_filter(Some(" docker , , meda ")).unwrap();
    assert!(f.allows(ExecutorKind::Docker));
    assert!(f.allows(ExecutorKind::Meda));
    assert!(!f.allows(ExecutorKind::Lume));
}

#[test]
fn parse_executor_filter_case_insensitive() {
    let f = parse_executor_filter(Some("DOCKER,Lume")).unwrap();
    assert!(f.allows(ExecutorKind::Docker));
    assert!(f.allows(ExecutorKind::Lume));
    assert!(!f.allows(ExecutorKind::Meda));
}

#[test]
fn parse_executor_filter_unknown_value_errors() {
    assert!(parse_executor_filter(Some("kata")).is_err());
    assert!(parse_executor_filter(Some("docker,podman")).is_err());
}

#[test]
fn parse_executor_filter_all_blank_errors() {
    assert!(parse_executor_filter(Some(",, ,")).is_err());
}

// ── ExecutorKind metadata (Candidate 1: deepen the kind) ──
//
// One source of truth per kind for: wire name, runner OS produced, and
// default-when-no-executor-specified. Locks every iteration over kinds
// through `ALL` so adding a new variant breaks compilation if its
// metadata is missed.

#[test]
fn executor_kind_all_lists_every_variant() {
    // Updating this assertion is the signal that a new ExecutorKind was
    // added and ALL must be extended.
    assert_eq!(ExecutorKind::ALL.len(), 3);
    assert!(ExecutorKind::ALL.contains(&ExecutorKind::Docker));
    assert!(ExecutorKind::ALL.contains(&ExecutorKind::Meda));
    assert!(ExecutorKind::ALL.contains(&ExecutorKind::Lume));
}

#[test]
fn executor_kind_name_roundtrips_through_from_name() {
    for kind in ExecutorKind::ALL {
        assert_eq!(ExecutorKind::from_name(kind.name()).unwrap(), *kind);
    }
}

#[test]
fn executor_kind_from_name_is_case_and_whitespace_insensitive() {
    assert_eq!(
        ExecutorKind::from_name("  DOCKER  ").unwrap(),
        ExecutorKind::Docker
    );
}

#[test]
fn executor_kind_from_name_rejects_unknown() {
    assert!(ExecutorKind::from_name("podman").is_err());
}

#[test]
fn executor_kind_produced_os_matches_serves_os() {
    // The free function `executor_serves_os` is now defined in terms of
    // `produced_os`; this guards the equivalence so the helper can never
    // drift from the underlying metadata.
    for kind in ExecutorKind::ALL {
        assert!(executor_serves_os(*kind, kind.produced_os()));
    }
}

#[test]
fn executor_kind_default_for_host_os() {
    assert_eq!(
        ExecutorKind::default_for_host_os("linux"),
        Some(ExecutorKind::Meda)
    );
    assert_eq!(
        ExecutorKind::default_for_host_os("macos"),
        Some(ExecutorKind::Lume)
    );
    assert_eq!(ExecutorKind::default_for_host_os("freebsd"), None);
}

// ── ExecutorFilter (Candidate 1: consolidate the `allows` closure) ──

#[test]
fn executor_filter_allow_all_admits_every_kind() {
    let f = ExecutorFilter::allow_all();
    for kind in ExecutorKind::ALL {
        assert!(f.allows(*kind), "allow_all must admit {kind:?}");
    }
}

#[test]
fn executor_filter_allow_only_admits_listed_kinds() {
    let mut set = std::collections::HashSet::new();
    set.insert(ExecutorKind::Docker);
    let f = ExecutorFilter::allow_only(set);
    assert!(f.allows(ExecutorKind::Docker));
    assert!(!f.allows(ExecutorKind::Meda));
    assert!(!f.allows(ExecutorKind::Lume));
}

#[test]
fn parse_executor_filter_returns_allow_all_when_unset() {
    let f = parse_executor_filter(None).unwrap();
    for kind in ExecutorKind::ALL {
        assert!(f.allows(*kind));
    }
}

#[test]
fn parse_executor_filter_returns_allow_only_when_listed() {
    let f = parse_executor_filter(Some("docker")).unwrap();
    assert!(f.allows(ExecutorKind::Docker));
    assert!(!f.allows(ExecutorKind::Meda));
}
