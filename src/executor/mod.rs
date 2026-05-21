//! Executor abstraction — unifies docker / meda / lume runtime backends behind
//! a single trait. State machine, settle-poll, idempotency, and cleanup live
//! in default-impl methods; each executor provides 4 primitives.

pub mod docker;
#[cfg(target_os = "macos")]
pub mod lume;
#[cfg(target_os = "linux")]
pub mod meda;
pub mod registry;

use async_trait::async_trait;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutorKind {
    Docker,
    Meda,
    Lume,
}

impl ExecutorKind {
    /// Every known executor kind. Any code that iterates over kinds
    /// (CLI parsing, sorted name lists for the agent registration
    /// payload, registry probing) goes through this slice so adding a
    /// new variant is a single edit, not a hunt-and-update.
    pub const ALL: &'static [ExecutorKind] =
        &[ExecutorKind::Docker, ExecutorKind::Meda, ExecutorKind::Lume];

    /// Lowercase wire name. The inverse of [`ExecutorKind::from_name`].
    /// Used on the agent registration payload so the cirun api can
    /// route by capability, and as the legal value set for the
    /// `--executors` CLI flag.
    pub fn name(self) -> &'static str {
        match self {
            ExecutorKind::Docker => "docker",
            ExecutorKind::Meda => "meda",
            ExecutorKind::Lume => "lume",
        }
    }

    /// Which runner OS this executor produces. Docker → linux even on
    /// macOS hosts (Docker Desktop runs linux containers). Meda → linux
    /// only. Lume → macOS only. Drives `executor_serves_os` and the
    /// dispatch gate in [`crate::cirun_client::CirunClient`].
    pub fn produced_os(self) -> &'static str {
        match self {
            ExecutorKind::Docker => "linux",
            ExecutorKind::Meda => "linux",
            ExecutorKind::Lume => "macos",
        }
    }

    /// Default executor when a runner specifies `runner.os` but no
    /// explicit `executor` field — historical contract for older SaaS
    /// dispatch shapes. Returns `None` for unknown OS strings; callers
    /// surface that as a misconfig.
    pub fn default_for_host_os(os: &str) -> Option<ExecutorKind> {
        match os {
            "linux" => Some(ExecutorKind::Meda),
            "macos" => Some(ExecutorKind::Lume),
            _ => None,
        }
    }

    /// Parse a wire-name string back into a kind. Case- and
    /// whitespace-insensitive. Driven through `ALL` + `name()` so a
    /// new variant only needs the name match arm to be reachable.
    pub fn from_name(s: &str) -> Result<ExecutorKind, String> {
        let lower = s.trim().to_ascii_lowercase();
        ExecutorKind::ALL
            .iter()
            .find(|k| k.name() == lower)
            .copied()
            .ok_or_else(|| format!("unknown executor '{}'", s.trim()))
    }
}

/// Which executors an agent is allowed to register and probe at
/// startup. `allow_all()` is the historical behaviour (every kind
/// available on the host). `allow_only(set)` honours the
/// `--executors` flag from issue #15 — anything not listed is skipped
/// at probe time and its setup step is skipped at boot.
///
/// Wrapping the inner `Option<HashSet<ExecutorKind>>` kills the
/// `allows(kind)` closure that used to be redeclared in both
/// `Registry::probe_filtered` and `main.rs`.
#[derive(Debug, Default, Clone)]
pub struct ExecutorFilter {
    allow: Option<std::collections::HashSet<ExecutorKind>>,
}

impl ExecutorFilter {
    /// No restriction — every kind available on the host is enabled.
    /// Matches the agent's default when `--executors` is unset.
    pub fn allow_all() -> Self {
        Self { allow: None }
    }

    /// Restrict to exactly the given set. An empty set is rejected by
    /// `parse_executor_filter` upstream; callers that build this
    /// directly are trusted not to pass an empty set (the agent would
    /// silently have no executors).
    pub fn allow_only(set: std::collections::HashSet<ExecutorKind>) -> Self {
        Self { allow: Some(set) }
    }

    /// Whether `kind` is admitted by the filter.
    pub fn allows(&self, kind: ExecutorKind) -> bool {
        self.allow.as_ref().is_none_or(|s| s.contains(&kind))
    }
}

/// Parse a `--executors` CLI value into an [`ExecutorFilter`]. `None`
/// (no flag) yields [`ExecutorFilter::allow_all`] — the historical
/// behaviour. `Some("docker,meda")` yields an `allow_only` filter.
///
/// Issue #15: lets a docker-only operator suppress the auto-install of
/// meda (linux) or lume (macos), which both pull binaries on first run.
pub fn parse_executor_filter(raw: Option<&str>) -> Result<ExecutorFilter, String> {
    let Some(s) = raw else {
        return Ok(ExecutorFilter::allow_all());
    };
    let mut set = std::collections::HashSet::new();
    for part in s.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        set.insert(ExecutorKind::from_name(trimmed)?);
    }
    if set.is_empty() {
        return Err("--executors cannot be empty".into());
    }
    Ok(ExecutorFilter::allow_only(set))
}

/// Resolve executor kind, honouring all signals in priority order:
///   1. `top_executor` — cirun-api-set top-level field (preferred contract).
///   2. `extra_config.executor` — same string under extra_config (legacy).
///   3. `extra_config.container == true` — legacy bool, maps to Docker.
///   4. OS default — linux → Meda, macos → Lume.
pub fn resolve_executor_kind(
    top_executor: Option<&str>,
    extra_config: Option<&serde_json::Value>,
    os: &str,
) -> Result<ExecutorKind, String> {
    if let Some(s) = top_executor {
        if !s.is_empty() {
            return ExecutorKind::from_name(s);
        }
    }
    if let Some(cfg) = extra_config {
        if let Some(name) = cfg.get("executor").and_then(|v| v.as_str()) {
            return ExecutorKind::from_name(name);
        }
        if cfg.get("container").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(ExecutorKind::Docker);
        }
    }
    ExecutorKind::default_for_host_os(os)
        .ok_or_else(|| format!("no default executor for os '{os}'"))
}

/// Whether `kind` can serve a runner with the given `runner_os`. Thin
/// alias over [`ExecutorKind::produced_os`]; kept as a free function
/// because the dispatch gate reads better as `executor_serves_os(k, os)`
/// than `k.produced_os().eq_ignore_ascii_case(os)`.
pub fn executor_serves_os(kind: ExecutorKind, runner_os: &str) -> bool {
    runner_os.eq_ignore_ascii_case(kind.produced_os())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuRequest {
    None,
    All,
    Count(u32),
}

/// Parse a `gpu` field from `extra_config` into a `GpuRequest`.
///
/// Accepted forms (matches the `.cirun.yml` schema):
///   - missing / null → `None`
///   - `"none"` (any case) → `None`
///   - `"all"` (any case) → `All`
///   - integer `1`, `2`, ... → `Count(n)`
///   - numeric string `"1"`, `"2"`, ... → `Count(n)`
pub fn parse_gpu_request(value: Option<&serde_json::Value>) -> Result<GpuRequest, String> {
    let v = match value {
        None => return Ok(GpuRequest::None),
        Some(v) if v.is_null() => return Ok(GpuRequest::None),
        Some(v) => v,
    };
    if let Some(s) = v.as_str() {
        let s = s.trim().to_ascii_lowercase();
        if s.is_empty() || s == "none" {
            return Ok(GpuRequest::None);
        }
        if s == "all" {
            return Ok(GpuRequest::All);
        }
        if let Ok(n) = s.parse::<u32>() {
            if n == 0 {
                return Ok(GpuRequest::None);
            }
            return Ok(GpuRequest::Count(n));
        }
        return Err(format!("invalid gpu request: '{s}'"));
    }
    if let Some(n) = v.as_u64() {
        return match u32::try_from(n) {
            Ok(0) => Ok(GpuRequest::None),
            Ok(n) => Ok(GpuRequest::Count(n)),
            Err(_) => Err(format!("gpu count {n} out of range")),
        };
    }
    Err(format!("gpu must be string or integer, got {v}"))
}

#[derive(Debug, Clone)]
pub struct RunnerLogin {
    pub username: String,
    /// Read by the lume executor only (sshpass auth to the macOS VM). The
    /// field is still populated on linux builds so the struct shape stays
    /// uniform across the codebase; the `allow(dead_code)` only kicks in
    /// where no executor consumes it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct RunnerSpec {
    pub name: String,
    pub provision_script: String,
    pub image: String,
    pub cpu: u32,
    pub memory_gb: u32,
    /// Read by `meda` only (cloud-hypervisor disk size); other executors
    /// take disk size from their image. `allow(dead_code)` keeps the field
    /// available on macos builds where the meda module is cfg'd out.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub disk_gb: u32,
    pub gpu: GpuRequest,
    /// Docker-only: pass `--privileged` to `docker run`. Surfaces from
    /// `.cirun.yml` as `extra_config.privileged: true`. Meda/lume ignore.
    pub docker_privileged: bool,
    /// Docker-only: bind `/var/run/docker.sock` from the host into the
    /// container. Surfaces from `.cirun.yml` as
    /// `extra_config.docker_socket_mount: true`.
    pub docker_mount_socket: bool,
    pub login: RunnerLogin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerState {
    /// Workload is alive and serving.
    Healthy,
    /// Created but not yet healthy. Settle-poll keeps waiting.
    Starting,
    /// Workload exited or crashed. Last-logs aids diagnosis.
    Terminated {
        exit_code: Option<i32>,
        last_logs: String,
    },
    /// No such runner.
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedRunner {
    pub name: String,
    pub state: RunnerState,
}

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// Caller may retry. `diagnostics` is an open-ended JSON bag that
    /// rides up the chain and lands in the `AgentEvent.metadata` cirun-go
    /// receives — use it for structured root-cause hints (elapsed_ms,
    /// partial SSH stdio, VM-side state captures).
    #[error("transient: {message}")]
    Transient {
        message: String,
        diagnostics: serde_json::Map<String, serde_json::Value>,
    },
    /// Permanent runtime failure — do not retry. Constructed by the lume
    /// executor (macos-only); kept on linux for parity with the trait
    /// surface so callers can match exhaustively without cfg branches.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    #[error("permanent: {0}")]
    Permanent(String),
    /// Spec is incompatible with this executor (e.g. lume + GPU). Never retry.
    #[error("incompatible: {0}")]
    Incompatible(String),
    /// Underlying host (meda's admission control today) is at capacity.
    /// The caller MUST NOT count this against the runner's retry budget
    /// and SHOULD signal upstream so the backend can mark the GH check
    /// run as "queued, host at capacity" rather than "provisioning
    /// failed". The runner stays in the backend's `requested` pool and
    /// gets re-fanned on the next poll.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    #[error("host_full ({code}): {message}")]
    HostFull {
        code: String,
        message: String,
        retry_after_secs: u64,
    },
}

impl ProvisionError {
    /// Build a transient error with no structured diagnostics — the
    /// common case for callers that only have a human message. Use
    /// `transient_with` when you've already gathered a metadata bag.
    pub fn transient(msg: impl Into<String>) -> Self {
        Self::Transient {
            message: msg.into(),
            diagnostics: serde_json::Map::new(),
        }
    }

    /// Build a transient error with a structured diagnostics bag.
    /// Meda's detached-exec failure path uses this to attach the SSH
    /// timing + VM-side state capture so the cirun-go event log carries
    /// enough context to root-cause the failure without an SSH shell.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn transient_with(
        msg: impl Into<String>,
        diagnostics: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self::Transient {
            message: msg.into(),
            diagnostics,
        }
    }
}

#[async_trait]
pub trait Executor: Send + Sync {
    // ── primitives ───────────────────────────────────────────────
    async fn inspect(&self, name: &str) -> Result<RunnerState, ProvisionError>;
    async fn spawn(&self, spec: &RunnerSpec) -> Result<(), ProvisionError>;
    async fn kill(&self, name: &str) -> Result<(), ProvisionError>;
    async fn list_owned(&self) -> Result<Vec<OwnedRunner>, ProvisionError>;

    // ── hooks (override if needed) ───────────────────────────────
    fn settle_timeout(&self) -> Duration {
        Duration::from_secs(120)
    }
    fn settle_poll_interval(&self) -> Duration {
        Duration::from_secs(3)
    }
    fn validate(&self, _spec: &RunnerSpec) -> Result<(), ProvisionError> {
        Ok(())
    }
    async fn prepare(&self, _spec: &RunnerSpec) -> Result<(), ProvisionError> {
        Ok(())
    }
    async fn run_post_spawn(&self, _spec: &RunnerSpec) -> Result<(), ProvisionError> {
        Ok(())
    }

    // ── default-impl orchestration ───────────────────────────────

    /// The whole provisioning state machine. Caller invokes this; everything
    /// else (idempotency, settle-poll, cleanup-on-fail) lives in here.
    async fn provision(&self, spec: &RunnerSpec) -> Result<(), ProvisionError> {
        self.validate(spec)?;
        match self.inspect(&spec.name).await? {
            RunnerState::Healthy | RunnerState::Starting => return Ok(()),
            RunnerState::Terminated { .. } => {
                self.kill(&spec.name).await?;
            }
            RunnerState::Absent => {}
        }
        self.prepare(spec).await?;
        self.spawn(spec).await?;

        // Settle-poll: keep checking until Healthy, timeout, or definitive failure.
        let deadline = tokio::time::Instant::now() + self.settle_timeout();
        loop {
            tokio::time::sleep(self.settle_poll_interval()).await;
            match self.inspect(&spec.name).await? {
                RunnerState::Healthy => {
                    // Settle done; run any post-spawn step (e.g. SSH-push provisioning
                    // for meda/lume). Docker's CMD already ran the script, so its
                    // default no-op is correct.
                    if let Err(e) = self.run_post_spawn(spec).await {
                        let _ = self.kill(&spec.name).await;
                        return Err(e);
                    }
                    return Ok(());
                }
                RunnerState::Starting => {
                    if tokio::time::Instant::now() >= deadline {
                        let _ = self.kill(&spec.name).await;
                        return Err(ProvisionError::transient(format!(
                            "runner '{}' did not reach Healthy within {:?}",
                            spec.name,
                            self.settle_timeout()
                        )));
                    }
                }
                RunnerState::Terminated {
                    exit_code,
                    last_logs,
                } => {
                    let _ = self.kill(&spec.name).await;
                    return Err(ProvisionError::transient(format!(
                        "runner '{}' exited (code={:?}) during settle: {}",
                        spec.name, exit_code, last_logs
                    )));
                }
                RunnerState::Absent => {
                    return Err(ProvisionError::transient(format!(
                        "runner '{}' disappeared during settle",
                        spec.name
                    )));
                }
            }
        }
    }

    /// Count of runners currently in `Healthy` state.
    async fn count_running(&self) -> Result<usize, ProvisionError> {
        let runners = self.list_owned().await?;
        Ok(runners
            .into_iter()
            .filter(|r| r.state == RunnerState::Healthy)
            .count())
    }
}

#[cfg(test)]
mod tests;
