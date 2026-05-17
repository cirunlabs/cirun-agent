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
