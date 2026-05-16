//! Single funnel for provision-outcome observability.
//!
//! The agent's main loop produces `ProvisionResult`s; each one needs to:
//!   - mutate per-runner retry state (clear on success, increment on
//!     real failure, **not** touch it on host-full backpressure)
//!   - notify the cirun backend with a payload shape that depends on
//!     which case we're in, so the backend can render the right
//!     GitHub check-run status (success / failed / queued-at-capacity)
//!
//! Without this module the main loop owned all three of those concerns
//! inline. That made it 20+ lines of arms per outcome, and each new
//! variant (host-full, future RunnerHealthChanged, …) grew the loop
//! and the test surface around it. The module here exposes ONE entry
//! point — `ProvisionReporter::report` — and hides the dispatch:
//!
//! ```ignore
//! reporter.report(ProvisionEvent::from(pr)).await;
//! ```
//!
//! New outcome types add an enum variant + a match arm in the impl on
//! `CirunClient`. main.rs does not change.
//!
//! Retry-counter mutation stays on `CirunClient` (its natural owner —
//! same struct holds the HTTP binding), but every call to
//! increment/clear flows through the reporter impl. main.rs has no
//! business touching the counter, and now it doesn't.

use async_trait::async_trait;

use crate::provision::{ProvisionOutcome, ProvisionResult};

/// Vocabulary of provision-outcome events the agent emits upstream.
///
/// Each variant carries exactly what the corresponding cirun backend
/// payload needs — the reporter impl translates 1:1, no shape-shifting
/// at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionEvent {
    /// Runner provisioned and registered with GitHub. Reporter clears
    /// retry state; no HTTP notification needed (the SaaS learns via
    /// the periodic `report_running_vms` POST that this runner is now
    /// alive on the agent).
    Succeeded { runner_name: String },

    /// Real provisioning failure (executor error, bad spec, network).
    /// Reporter increments retry count and POSTs a `provision_failure`
    /// payload so the backend can decide retry vs. mark-failed based
    /// on `max_retries`.
    Failed { runner_name: String, error: String },

    /// Host admission control rejected the request (meda 503). The
    /// runner was NEVER spawned, so retry budget MUST be preserved.
    /// Reporter posts an `at_capacity` payload carrying the structured
    /// reason (CPU_EXHAUSTED / MEM_EXHAUSTED / DISK_EXHAUSTED) so the
    /// backend can surface "queued, host at capacity" on the GitHub
    /// check run instead of "failed".
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    AtCapacity {
        runner_name: String,
        code: String,
        message: String,
        retry_after_secs: u64,
    },
}

impl ProvisionEvent {
    /// True when the agent should consider this a successful provision
    /// for purposes of "do we need to call report_running_vms now?".
    /// Kept on the event (not the outcome) so the main loop reads the
    /// same vocabulary it dispatches with.
    pub fn is_success(&self) -> bool {
        matches!(self, ProvisionEvent::Succeeded { .. })
    }
}

impl From<ProvisionResult> for ProvisionEvent {
    fn from(pr: ProvisionResult) -> Self {
        match pr.outcome {
            ProvisionOutcome::Success => ProvisionEvent::Succeeded {
                runner_name: pr.runner_name,
            },
            ProvisionOutcome::Failed(error) => ProvisionEvent::Failed {
                runner_name: pr.runner_name,
                error,
            },
            ProvisionOutcome::HostFull {
                code,
                message,
                retry_after_secs,
            } => ProvisionEvent::AtCapacity {
                runner_name: pr.runner_name,
                code,
                message,
                retry_after_secs,
            },
        }
    }
}

/// Sink for provision-outcome events. The single trait method is the
/// reporter's whole public surface — callers compose dispatch by
/// pattern-matching `ProvisionEvent` arms inside the impl, not by
/// memorizing a fan of `notify_*` method names.
#[async_trait]
pub trait ProvisionReporter: Send + Sync {
    async fn report(&self, event: ProvisionEvent);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test double that records every event in arrival order. Lets the
    /// main-loop tests assert "the reporter saw exactly these events
    /// in this order" without spinning up HTTP.
    pub struct RecordingReporter {
        events: Mutex<Vec<ProvisionEvent>>,
    }

    impl RecordingReporter {
        pub fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }
        pub fn events(&self) -> Vec<ProvisionEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProvisionReporter for RecordingReporter {
        async fn report(&self, event: ProvisionEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn pr_success(name: &str) -> ProvisionResult {
        ProvisionResult {
            runner_name: name.into(),
            executor_kind: None,
            outcome: ProvisionOutcome::Success,
        }
    }

    fn pr_failed(name: &str, err: &str) -> ProvisionResult {
        ProvisionResult {
            runner_name: name.into(),
            executor_kind: None,
            outcome: ProvisionOutcome::Failed(err.into()),
        }
    }

    fn pr_host_full(name: &str, code: &str) -> ProvisionResult {
        ProvisionResult {
            runner_name: name.into(),
            executor_kind: None,
            outcome: ProvisionOutcome::HostFull {
                code: code.into(),
                message: format!("{code} test"),
                retry_after_secs: 10,
            },
        }
    }

    #[test]
    fn lifts_success_outcome_to_succeeded_event() {
        let ev: ProvisionEvent = pr_success("r1").into();
        assert_eq!(
            ev,
            ProvisionEvent::Succeeded {
                runner_name: "r1".into()
            }
        );
        assert!(ev.is_success());
    }

    #[test]
    fn lifts_failed_outcome_to_failed_event() {
        let ev: ProvisionEvent = pr_failed("r1", "boom").into();
        assert_eq!(
            ev,
            ProvisionEvent::Failed {
                runner_name: "r1".into(),
                error: "boom".into()
            }
        );
        assert!(!ev.is_success());
    }

    #[test]
    fn lifts_host_full_outcome_to_at_capacity_event() {
        let ev: ProvisionEvent = pr_host_full("r1", "CPU_EXHAUSTED").into();
        assert_eq!(
            ev,
            ProvisionEvent::AtCapacity {
                runner_name: "r1".into(),
                code: "CPU_EXHAUSTED".into(),
                message: "CPU_EXHAUSTED test".into(),
                retry_after_secs: 10,
            }
        );
        assert!(!ev.is_success());
    }

    #[tokio::test]
    async fn recording_reporter_captures_events_in_order() {
        // Sanity check the test double itself — if this drifts, every
        // downstream test that uses RecordingReporter silently breaks.
        let r = RecordingReporter::new();
        r.report(pr_success("r1").into()).await;
        r.report(pr_failed("r2", "x").into()).await;
        let evs = r.events();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ProvisionEvent::Succeeded { .. }));
        assert!(matches!(evs[1], ProvisionEvent::Failed { .. }));
    }
}
