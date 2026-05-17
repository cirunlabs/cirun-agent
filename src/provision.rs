//! Single-runner provisioning task. Spawned per runner from the main loop.
//! Resolves the executor + GPU spec from the payload, dispatches through
//! the host registry, and reports outcome via `ProvisionResult`.

use crate::api::{RunnerResources, RunnerToProvision};
// Lume template helpers + the TemplateConfig they consume live behind
// macos-only modules. Linux builds never hit the lume branch in template
// resolution (the executor is always Docker or Meda there), so both the
// type and the helpers are macos-gated.
#[cfg(target_os = "macos")]
use crate::api::TemplateConfig;
#[cfg(target_os = "macos")]
use crate::lume::{
    check_template_exists, create_template, find_matching_template, generate_template_name,
};
use log::{error, info};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Outcome of a single runner provisioning attempt. Distinguishes three
/// cases that the main loop must treat differently:
///
/// - `Success` — runner is up and registered; clear retry count.
/// - `Failed` — real failure (executor error, bad spec, network). Burn
///   a retry slot and notify the backend so SaaS can mark the runner
///   failed (or retry it depending on `max_retries`).
/// - `HostFull` — meda admission control denied the request because
///   the host is at capacity. The runner was NEVER spawned; we MUST
///   NOT count this against `max_retries` and we SHOULD push the
///   structured reason upstream so the backend can surface "queued,
///   host at capacity" on the GitHub check run instead of "failed".
pub enum ProvisionOutcome {
    Success,
    /// Real provisioning failure. `diagnostics` is an open-ended bag —
    /// most call sites have nothing structured to attach (empty map),
    /// but the meda detached-exec failure path stuffs SSH timing +
    /// VM-side state captures here so the upstream `AgentEvent` can
    /// carry them to cirun-go.
    Failed {
        error: String,
        diagnostics: serde_json::Map<String, serde_json::Value>,
    },
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    HostFull {
        code: String,
        message: String,
        retry_after_secs: u64,
    },
}

impl ProvisionOutcome {
    /// Build a `Failed` outcome with no structured diagnostics. Sites
    /// that want to attach a metadata bag construct the variant
    /// directly with `ProvisionOutcome::Failed { error, diagnostics }`.
    pub fn failed(msg: impl Into<String>) -> Self {
        Self::Failed {
            error: msg.into(),
            diagnostics: serde_json::Map::new(),
        }
    }
}

/// Result of a single runner provisioning attempt
pub struct ProvisionResult {
    pub runner_name: String,
    /// Executor that handled the runner. `None` when derivation failed
    /// (we never started provisioning).
    pub executor_kind: Option<crate::executor::ExecutorKind>,
    pub outcome: ProvisionOutcome,
}

/// Provision a single runner in its own task. Acquires a semaphore permit to
/// enforce concurrency bounds. Dispatches via the host `Registry` (which was
/// probed at startup) so a backend whose daemon wasn't reachable never gets a
/// late provision attempt against a fresh client.
pub async fn provision_single_runner(
    runner: RunnerToProvision,
    semaphore: Arc<Semaphore>,
    registry: Arc<crate::executor::registry::Registry>,
) -> ProvisionResult {
    let _permit = semaphore.acquire().await.expect("semaphore closed");

    info!(
        "Processing runner: {} (image: {}, os: {}, cpu: {}, mem: {}GB, disk: {}GB)",
        runner.name, runner.image, runner.os, runner.cpu, runner.memory, runner.disk
    );

    // The image-registry split + TemplateConfig only feed the lume
    // template-matching path; on linux that path is cfg'd out so the
    // whole prep block is macos-gated to avoid unused-binding warnings
    // under `-D warnings`.
    #[cfg(target_os = "macos")]
    let template_config = {
        // Parse image registry hostname from image name
        // (e.g. ghcr.io/foo/bar → "ghcr.io").
        let (image_registry, image) = if runner.image.contains('.')
            && runner.image.split('/').next().unwrap().contains('.')
        {
            let parts: Vec<&str> = runner.image.splitn(2, '/').collect();
            if parts.len() == 2 {
                (Some(parts[0].to_string()), parts[1].to_string())
            } else {
                (Some("ghcr.io".to_string()), runner.image.clone())
            }
        } else {
            (Some("ghcr.io".to_string()), runner.image.clone())
        };
        TemplateConfig {
            image,
            registry: image_registry,
            organization: None,
            cpu: runner.cpu,
            memory: runner.memory,
            disk: runner.disk,
            os: runner.os.clone(),
        }
    };

    // Resolve executor: prefer the SaaS-set top-level `executor` field, fall
    // back to `extra_config.executor` (or legacy `container: true`), then OS
    // default. No env-var dispatch.
    let executor_kind = match crate::executor::resolve_executor_kind(
        runner.executor.as_deref(),
        runner.extra_config.as_ref(),
        &runner.os,
    ) {
        Ok(k) => k,
        Err(e) => {
            return ProvisionResult {
                runner_name: runner.name.clone(),
                executor_kind: None,
                outcome: ProvisionOutcome::failed(format!("Cannot derive executor: {e}")),
            };
        }
    };

    // Resolve template: docker + meda use image directly, lume uses
    // template matching. The lume branch only compiles on macos because
    // the helper module is cfg-gated; on linux the executor is always
    // Docker or Meda, so the `else` branch is unreachable there.
    let template_name = if matches!(
        executor_kind,
        crate::executor::ExecutorKind::Docker | crate::executor::ExecutorKind::Meda
    ) {
        info!(
            "Using {:?} executor - using image name directly: {}",
            executor_kind, runner.image
        );
        Some(runner.image.clone())
    } else {
        #[cfg(target_os = "macos")]
        {
            if let Some(existing_template) = find_matching_template(&template_config).await {
                info!(
                    "Found existing template with matching configuration: {}",
                    existing_template
                );
                Some(existing_template)
            } else {
                let generated_name = generate_template_name(&template_config);
                let template_exists = check_template_exists(&generated_name).await;

                if !template_exists {
                    info!(
                        "No matching template found. Creating new template '{}' from image '{}'",
                        generated_name, template_config.image
                    );
                    match create_template(&template_config, &generated_name).await {
                        Ok(_) => {
                            info!("Successfully created template: {}", generated_name);
                            Some(generated_name)
                        }
                        Err(e) => {
                            error!("Failed to create template {}: {}", generated_name, e);
                            return ProvisionResult {
                                runner_name: runner.name.clone(),
                                executor_kind: Some(executor_kind),
                                outcome: ProvisionOutcome::failed(format!(
                                    "Template creation failed: {}",
                                    e
                                )),
                            };
                        }
                    }
                } else {
                    info!("Using existing template: {}", generated_name);
                    Some(generated_name)
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            // Lume executor is macos-only, but the OS filter in
            // cirun_client should have already dropped this dispatch.
            // Fall through with None → ProvisionResult with the
            // standard "no template" error path below.
            None
        }
    };

    let template_name = match template_name {
        Some(t) => t,
        None => {
            return ProvisionResult {
                runner_name: runner.name.clone(),
                executor_kind: Some(executor_kind),
                outcome: ProvisionOutcome::failed("No template available".to_string()),
            };
        }
    };

    info!(
        "Provisioning runner '{}' with template '{}'",
        runner.name, template_name
    );

    let resources = RunnerResources {
        cpu: runner.cpu,
        memory: runner.memory,
        disk: runner.disk,
    };

    // Parse gpu request: prefer top-level `gpu` (SaaS contract), fall back to
    // `extra_config.gpu` (legacy).
    let gpu_raw = runner
        .gpu
        .as_ref()
        .map(|s| serde_json::Value::String(s.clone()))
        .or_else(|| {
            runner
                .extra_config
                .as_ref()
                .and_then(|c| c.get("gpu").cloned())
        });
    let gpu = match crate::executor::parse_gpu_request(gpu_raw.as_ref()) {
        Ok(g) => g,
        Err(e) => {
            return ProvisionResult {
                runner_name: runner.name.clone(),
                executor_kind: Some(executor_kind),
                outcome: ProvisionOutcome::failed(format!("Invalid gpu request: {e}")),
            };
        }
    };

    // Docker-only flags from `.cirun.yml`'s extra_config. Missing /
    // wrong-typed → false (current default behaviour).
    let docker_privileged = runner
        .extra_config
        .as_ref()
        .and_then(|v| v.get("privileged"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let docker_mount_socket = runner
        .extra_config
        .as_ref()
        .and_then(|v| v.get("docker_socket_mount"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let spec = crate::executor::RunnerSpec {
        name: runner.name.clone(),
        provision_script: runner.provision_script.clone(),
        image: template_name.clone(),
        cpu: resources.cpu,
        memory_gb: resources.memory,
        disk_gb: resources.disk,
        gpu,
        docker_privileged,
        docker_mount_socket,
        login: crate::executor::RunnerLogin {
            username: runner.login.username.clone(),
            password: runner.login.password.clone(),
        },
    };

    // Dispatch through the host registry (probed at startup) so a backend
    // that wasn't reachable then doesn't get a late attempt against a fresh
    // client.
    let outcome = match registry.get(executor_kind) {
        Ok(exec) => match exec.provision(&spec).await {
            Ok(()) => {
                info!(
                    "Successfully provisioned runner: {} using template {}",
                    runner.name, template_name
                );
                ProvisionOutcome::Success
            }
            // HostFull is admission backpressure, NOT a real failure.
            // Preserve the structured reason so the main loop can route
            // it to `notify_at_capacity` instead of burning a retry
            // slot via `notify_provision_failure`.
            Err(crate::executor::ProvisionError::HostFull {
                code,
                message,
                retry_after_secs,
            }) => {
                info!(
                    "Host at capacity for runner {}: {} ({}). Retry-After {}s",
                    runner.name, message, code, retry_after_secs
                );
                ProvisionOutcome::HostFull {
                    code,
                    message,
                    retry_after_secs,
                }
            }
            Err(e) => {
                let error_msg = e.to_string();
                error!(
                    "Failed to provision runner {} using template {}: {}",
                    runner.name, template_name, error_msg
                );
                ProvisionOutcome::failed(error_msg)
            }
        },
        Err(e) => {
            let error_msg = e.to_string();
            error!(
                "Failed to provision runner {} (registry lookup): {}",
                runner.name, error_msg
            );
            ProvisionOutcome::failed(error_msg)
        }
    };

    ProvisionResult {
        runner_name: runner.name.clone(),
        executor_kind: Some(executor_kind),
        outcome,
    }
}
