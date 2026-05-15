//! HTTP client + provisioning orchestrator. Polls the cirun api for runners
//! to provision/delete, dispatches via the executor registry, tracks retries
//! and per-runner executor binding.

use crate::api::{AgentInfo, ApiResponse, RunnerToProvision};
use crate::provision::{provision_single_runner, ProvisionResult};
use log::{debug, error, info, warn};
use reqwest::{Client, Error};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

// Client for interacting with the CiRun API
pub struct CirunClient {
    client: Client,
    base_url: String,
    api_token: String,
    agent: AgentInfo,
    pub retry_tracker: HashMap<String, u32>,
    /// None means no limit, Some(n) means max n concurrent VMs
    max_vms: Option<u32>,
    /// Per-runner executor binding, learned at provision time. Cleanup/delete
    /// paths consult this map instead of guessing from env vars.
    /// Mutex (not just inner map) because lookups happen behind `&self` and
    /// inserts behind `&mut self` from the main loop.
    pub runner_executors: std::sync::Mutex<HashMap<String, crate::executor::ExecutorKind>>,
    /// Names of runners currently being provisioned by this agent. Used to
    /// suppress racing delete requests: if the api sends a
    /// `runners_to_delete` for a name still in `in_flight`, the agent ignores
    /// it instead of killing the half-built VM mid-spawn.
    pub in_flight: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Executors available on this host, probed once at startup. `Arc` so it
    /// can be cheaply cloned into per-runner provision tasks.
    pub registry: Arc<crate::executor::registry::Registry>,
}

impl CirunClient {
    pub fn new(base_url: &str, api_token: &str, agent: AgentInfo, max_vms: Option<u32>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        CirunClient {
            client,
            base_url: base_url.to_string(),
            api_token: api_token.to_string(),
            agent,
            retry_tracker: HashMap::new(),
            max_vms,
            runner_executors: std::sync::Mutex::new(HashMap::new()),
            in_flight: std::sync::Mutex::new(std::collections::HashSet::new()),
            registry: Arc::new(crate::executor::registry::Registry::probe()),
        }
    }

    /// Best-effort lookup of which executor owns a runner. Falls back to the
    /// OS-default when the map has no entry (e.g. agent restarted between
    /// provision and cleanup, or runner was created by an older agent build).
    /// `seed_runner_executors_from_registry` should be called at startup to
    /// reduce the chance of the fallback firing on a fresh process.
    fn executor_for_runner(&self, runner_name: &str) -> crate::executor::ExecutorKind {
        if let Ok(map) = self.runner_executors.lock() {
            if let Some(k) = map.get(runner_name) {
                return *k;
            }
        }
        // Log the fallback so an operator can spot it in journald — silent
        // mis-routing was a real risk per the round-1 review.
        warn!(
            "runner '{}' has no executor binding; falling back to OS default",
            runner_name
        );
        match env::consts::OS {
            "macos" => crate::executor::ExecutorKind::Lume,
            _ => crate::executor::ExecutorKind::Meda,
        }
    }

    /// Populate `runner_executors` from the registry's view of the world.
    /// Call at startup so an agent restart with in-flight runners doesn't
    /// silently mis-route deletes to the wrong executor. Best-effort: per-
    /// executor list failures are logged and skipped.
    pub async fn seed_runner_executors_from_registry(&mut self) {
        let runners = self.registry.list_all().await;
        let mut count = 0usize;
        if let Ok(mut map) = self.runner_executors.lock() {
            for (kind, runner) in runners {
                if runner.name.starts_with("cirun-") {
                    map.insert(runner.name, kind);
                    count += 1;
                }
            }
        }
        info!(
            "Seeded runner_executors map with {} entries from live executors",
            count
        );
    }

    // Helper method to create a request builder with common headers
    fn create_request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let request_id = Uuid::new_v4().to_string();
        info!("Creating request with ID: {}", request_id);

        self.client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .header("X-Request-ID", request_id)
            .header("X-Agent-ID", &self.agent.id)
    }

    async fn handle_orphaned_runners(&self, response: reqwest::Response) {
        // Parse response for runners_to_delete (orphaned VMs)
        match response.json::<ApiResponse>().await {
            Ok(api_response) => {
                if !api_response.runners_to_delete.is_empty() {
                    info!(
                        "API returned {} orphaned runners to delete from POST",
                        api_response.runners_to_delete.len()
                    );
                    let in_flight = self.in_flight_snapshot();
                    for runner in &api_response.runners_to_delete {
                        // Same race-protection as the GET-path delete loop:
                        // skip orphan-cleanup for runners we are still
                        // building. cirun api may class a runner as orphan
                        // before its provision flow has finished here, and
                        // killing the half-built VM mid-spawn produces the
                        // "disappeared during settle" error.
                        if in_flight.contains(&runner.name) {
                            warn!(
                                "ignoring SaaS orphan-delete for '{}' — provision in flight on this agent",
                                runner.name
                            );
                            continue;
                        }
                        match self.delete_runner(&runner.name).await {
                            Ok(_) => {
                                info!("[OK] Successfully deleted orphaned runner: {}", runner.name);
                            }
                            Err(e) => {
                                error!("✘ Failed to delete orphaned runner {}: {}", runner.name, e)
                            }
                        }
                    }
                }
            }
            Err(e) => {
                info!(
                    "No runners_to_delete in POST response or parse error: {}",
                    e
                );
            }
        }
    }

    pub async fn report_running_vms(&self) {
        use crate::executor::ExecutorKind;
        info!("Reporting running VMs to API");
        let all = self.registry.list_all().await;
        let vms: Vec<_> = all
            .into_iter()
            .filter(|(_, r)| r.name.starts_with("cirun-"))
            .map(|(k, r)| {
                let os = match k {
                    ExecutorKind::Docker | ExecutorKind::Meda => "linux",
                    ExecutorKind::Lume => "macos",
                };
                json!({
                    "name": r.name,
                    "os": os,
                    "cpu": 0,
                    "memory": 0,
                    "disk_size": 0,
                })
            })
            .collect();

        let url = format!("{}/agent", self.base_url);
        match self
            .create_request(reqwest::Method::POST, &url)
            .json(&json!({ "agent": self.agent, "vms": vms }))
            .send()
            .await
        {
            Ok(response) => {
                info!("API response status: {}", response.status());
                self.handle_orphaned_runners(response).await;
            }
            Err(e) => error!("Failed to send running VMs: {}", e),
        }
    }

    async fn delete_runner(&self, runner_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let kind = self.executor_for_runner(runner_name);
        info!(
            "Attempting to delete runner '{}' via {:?} executor",
            runner_name, kind
        );
        // Binding is intentionally NOT removed on success. The cirun api may
        // re-request a delete for the same runner across consecutive polling
        // cycles (POST orphan-list then GET runners_to_delete); dropping the
        // binding would force the second request through the OS-default
        // fallback (Lume on macOS / Meda on Linux) which then fails with
        // "not found" and burns the retry budget. Every executor's `kill` is
        // idempotent on "already gone", so keeping the binding is safe.
        self.registry
            .get(kind)
            .map_err(|e| -> Box<dyn std::error::Error> {
                Box::new(std::io::Error::other(e.to_string()))
            })?
            .kill(runner_name)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> {
                Box::new(std::io::Error::other(e.to_string()))
            })
    }

    /// Get the current retry count for a runner
    fn get_retry_count(&self, runner_name: &str) -> u32 {
        *self.retry_tracker.get(runner_name).unwrap_or(&0)
    }

    /// Increment the retry count for a runner and return the new count
    pub fn increment_retry(&mut self, runner_name: &str) -> u32 {
        let count = self
            .retry_tracker
            .entry(runner_name.to_string())
            .or_insert(0);
        *count += 1;
        *count
    }

    /// Clear the retry count for a runner
    pub fn clear_retry(&mut self, runner_name: &str) {
        self.retry_tracker.remove(runner_name);
    }

    /// Check if a runner should be retried based on max_retries
    fn should_retry(&self, runner_name: &str, max_retries: u32) -> bool {
        self.get_retry_count(runner_name) < max_retries
    }

    /// Notify the API that a runner provisioning attempt failed
    pub async fn notify_provision_failure(&self, runner_name: &str, error: String, attempt: u32) {
        let url = format!("{}/agent", self.base_url);

        info!(
            "Notifying API of provisioning failure for {} (attempt {})",
            runner_name, attempt
        );

        let request_data = json!({
            "agent": self.agent,
            "provision_failure": {
                "runner_name": runner_name,
                "error": error,
                "attempt": attempt,
            }
        });

        match self
            .create_request(reqwest::Method::POST, &url)
            .json(&request_data)
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    debug!("Successfully notified API of provisioning failure");
                } else {
                    warn!(
                        "API returned non-success status for failure notification: {}",
                        response.status()
                    );
                }
            }
            Err(e) => {
                warn!("Failed to notify API of provisioning failure: {}", e);
            }
        }
    }

    /// Snapshot of the in-flight provision name set. Held briefly under the
    /// mutex; callers that need to hold the lock longer should grab the guard
    /// directly. Returns an empty set if the mutex is poisoned (best-effort).
    fn in_flight_snapshot(&self) -> std::collections::HashSet<String> {
        self.in_flight.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub async fn manage_runner_lifecycle(
        &mut self,
        provision_set: &mut JoinSet<ProvisionResult>,
        in_flight: &mut std::collections::HashSet<String>,
    ) -> Result<ApiResponse, Error> {
        let url = format!("{}/agent", self.base_url);
        info!("Fetching runner provision/deletion data from: {}", url);

        // `executors` advertises which runtimes this agent can serve, so SaaS
        // can route by capability instead of host OS. Lets a single Mac host
        // pick up both linux/docker and macos/lume jobs.
        let request_data = json!({
            "agent": self.agent,
            "executors": self.registry.kind_names(),
        });

        let response = self
            .create_request(reqwest::Method::GET, &url)
            .json(&request_data)
            .send()
            .await?;

        info!("Response status: {}", response.status());
        let json: ApiResponse = response.json().await?;

        // Handle any runners that need deletion
        if !json.runners_to_delete.is_empty() {
            info!(
                "Received {} runners to delete",
                json.runners_to_delete.len()
            );

            for runner in &json.runners_to_delete {
                // Skip delete if this runner is currently being provisioned
                // by THIS agent. The cirun api occasionally orphan-cleans a
                // runner before the agent has finished its create flow
                // (observed 2026-05-15 with meda: VM was created, deleted
                // mid-spawn, then `inspect` returned 500 → "disappeared
                // during settle"). The provision flow itself emits
                // `notify_provision_failure` on real errors, so SaaS will
                // re-evaluate; ignoring the racing delete keeps the
                // half-built VM alive long enough to either finish or fail
                // for a real reason.
                if in_flight.contains(&runner.name) {
                    warn!(
                        "ignoring SaaS delete request for '{}' — provision in flight on this agent",
                        runner.name
                    );
                    continue;
                }
                match self.delete_runner(&runner.name).await {
                    Ok(_) => {
                        info!("[OK] Successfully deleted runner: {}", runner.name);
                        self.report_running_vms().await;
                    }

                    Err(e) => error!("✘ Failed to delete runner {}: {}", runner.name, e),
                }
            }
        }

        // Handle runners that need provisioning
        if !json.runners_to_provision.is_empty() {
            info!(
                "Received {} runners to provision",
                json.runners_to_provision.len()
            );

            // First, handle retry-exhausted runners (notify API, skip them)
            for runner in &json.runners_to_provision {
                let current_attempts = self.get_retry_count(&runner.name);
                if !self.should_retry(&runner.name, runner.max_retries) {
                    warn!(
                        "Runner '{}' has exceeded max retries ({}/{}). Skipping provisioning.",
                        runner.name, current_attempts, runner.max_retries
                    );
                    self.notify_provision_failure(
                        &runner.name,
                        format!("Exceeded max retries ({})", runner.max_retries),
                        current_attempts,
                    )
                    .await;
                }
            }

            // Collect eligible runners (not retry-exhausted, not already
            // in-flight, AND served by an executor we actually have).
            // The capability filter is the important one: when the api
            // fans the same runner out to multiple agents, an agent that
            // can't serve it must drop it silently. Returning a
            // ProvisionFailure for a capability mismatch causes the api
            // to mark the runner failed and then issue an orphan-delete
            // — which can race with the WINNING agent's still-running VM
            // and tear it down mid-job. Filtering here means the api only
            // ever hears from agents that can actually do the work.
            let eligible_runners: Vec<RunnerToProvision> = json
                .runners_to_provision
                .iter()
                .filter(|r| self.should_retry(&r.name, r.max_retries))
                .filter(|r| {
                    if in_flight.contains(&r.name) {
                        info!("Skipping runner '{}' — already in-flight", r.name);
                        false
                    } else {
                        true
                    }
                })
                .filter(|r| {
                    // OS gate: a runner with `os: linux` must not be claimed
                    // by a macos agent (and vice-versa), even if both have a
                    // common executor like docker. Mismatched OS dispatches
                    // arrive when the api fans out by executor capability
                    // alone; without this filter the wrong-OS agent would
                    // attempt provision, fail (image arch mismatch, CPU
                    // bounds, etc.), and the racing failure notification
                    // would cause an orphan-delete on the agent that did
                    // accept the work.
                    if !r.os.eq_ignore_ascii_case(&self.agent.os) {
                        debug!(
                            "Skipping runner '{}' — runner.os={} does not match agent.os={}",
                            r.name, r.os, self.agent.os
                        );
                        return false;
                    }
                    let kind = match crate::executor::resolve_executor_kind(
                        r.executor.as_deref(),
                        r.extra_config.as_ref(),
                        &r.os,
                    ) {
                        Ok(k) => k,
                        Err(_) => return true, // let provision flow surface the misconfig
                    };
                    if self.registry.get(kind).is_ok() {
                        true
                    } else {
                        debug!(
                            "Skipping runner '{}' — executor {:?} not available on this host",
                            r.name, kind
                        );
                        false
                    }
                })
                .cloned()
                .collect();

            if !eligible_runners.is_empty() {
                // Calculate available slots based on VM capacity
                let available_slots = if let Some(max_vms) = self.max_vms {
                    let running_count = self.registry.total_count_running().await;
                    let slots = (max_vms as usize).saturating_sub(running_count);
                    info!(
                        "VM capacity: {}/{} running, {} slots available, {} runners requested",
                        running_count,
                        max_vms,
                        slots,
                        eligible_runners.len()
                    );
                    if slots == 0 {
                        info!("No VM slots available. Runners will be picked up on next poll.");
                    }
                    slots
                } else {
                    eligible_runners.len()
                };

                if available_slots > 0 {
                    // Cap runners to available slots
                    let runners_to_spawn: Vec<RunnerToProvision> =
                        eligible_runners.into_iter().take(available_slots).collect();

                    info!(
                        "Spawning {} runners in parallel (max concurrency: {})",
                        runners_to_spawn.len(),
                        available_slots
                    );

                    let semaphore = Arc::new(Semaphore::new(available_slots));

                    for runner in runners_to_spawn {
                        in_flight.insert(runner.name.clone());
                        // Mirror into self.in_flight so the orphan-delete
                        // path (handle_orphaned_runners, called from the
                        // POST report-running-vms response) can also see
                        // it without taking the local arg.
                        if let Ok(mut s) = self.in_flight.lock() {
                            s.insert(runner.name.clone());
                        }
                        let sem = semaphore.clone();
                        let reg = Arc::clone(&self.registry);
                        provision_set.spawn(provision_single_runner(runner, sem, reg));
                    }

                    info!(
                        "Spawned provisioning tasks. Total in-flight: {}",
                        provision_set.len()
                    );
                }
            }
        }

        Ok(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{
        registry::Registry, Executor, ExecutorKind, OwnedRunner, ProvisionError, RunnerSpec,
        RunnerState,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    /// Fake executor that records every kill() invocation and always reports
    /// success. Used to drive `delete_runner` without touching docker/lume.
    struct RecordingExecutor {
        killed: StdMutex<Vec<String>>,
    }

    impl RecordingExecutor {
        fn new() -> Self {
            Self {
                killed: StdMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Executor for RecordingExecutor {
        fn settle_timeout(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn validate(&self, _spec: &RunnerSpec) -> Result<(), ProvisionError> {
            Ok(())
        }
        async fn inspect(&self, _name: &str) -> Result<RunnerState, ProvisionError> {
            Ok(RunnerState::Absent)
        }
        async fn spawn(&self, _spec: &RunnerSpec) -> Result<(), ProvisionError> {
            Ok(())
        }
        async fn kill(&self, name: &str) -> Result<(), ProvisionError> {
            self.killed.lock().unwrap().push(name.to_string());
            Ok(())
        }
        async fn list_owned(&self) -> Result<Vec<OwnedRunner>, ProvisionError> {
            Ok(Vec::new())
        }
    }

    fn test_client(registry: Arc<Registry>) -> CirunClient {
        CirunClient {
            client: Client::builder().build().unwrap(),
            base_url: "https://example.invalid".to_string(),
            api_token: "tok".to_string(),
            agent: AgentInfo {
                id: "agent-x".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
            },
            retry_tracker: HashMap::new(),
            max_vms: None,
            runner_executors: std::sync::Mutex::new(HashMap::new()),
            in_flight: std::sync::Mutex::new(std::collections::HashSet::new()),
            registry,
        }
    }

    /// Regression test for the binding-loss bug observed in prod 2026-05-15:
    /// a POST-orphan-delete succeeded via Docker, but the binding was removed
    /// from `runner_executors`. The very next GET cycle re-requested the same
    /// delete; with no binding the routing fell back to the OS-default (Lume
    /// on macOS), which then hit "VM not found" and burned the retry budget.
    ///
    /// Successful deletes must preserve the binding so that idempotent re-
    /// requests from the api route to the same executor — where `kill()` is
    /// already a no-op when the runner is already gone.
    #[tokio::test]
    async fn delete_runner_preserves_binding_for_idempotent_retries() {
        let mut execs: HashMap<ExecutorKind, Arc<dyn Executor>> = HashMap::new();
        execs.insert(ExecutorKind::Docker, Arc::new(RecordingExecutor::new()));
        let registry = Arc::new(Registry::from_executors(execs));
        let client = test_client(registry);

        client
            .runner_executors
            .lock()
            .unwrap()
            .insert("cirun-r1".into(), ExecutorKind::Docker);

        client.delete_runner("cirun-r1").await.expect("delete ok");

        let map = client.runner_executors.lock().unwrap();
        assert_eq!(
            map.get("cirun-r1"),
            Some(&ExecutorKind::Docker),
            "binding must persist after a successful delete so SaaS retries do not fall back to the OS-default executor"
        );
    }

    /// Sanity: looking up a runner that was never bound falls back to the
    /// OS-default and emits the fallback warning. Locks in the historical
    /// behaviour the regression test above depends on.
    #[test]
    fn unbound_runner_routes_to_os_default() {
        let registry = Arc::new(Registry::from_executors(HashMap::new()));
        let client = test_client(registry);
        let kind = client.executor_for_runner("nope");
        let expected = match env::consts::OS {
            "macos" => ExecutorKind::Lume,
            _ => ExecutorKind::Meda,
        };
        assert_eq!(kind, expected);
    }
}
