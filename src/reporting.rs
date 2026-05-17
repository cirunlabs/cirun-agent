//! Single funnel for everything the agent wants the cirun backend to
//! observe.
//!
//! Design intent:
//!
//! 1. **One wire format.** The agent emits a single generic shape —
//!    `AgentEvent` — for every observable thing (a runner provisioned,
//!    a runner failed, a host hit admission-control backpressure, a
//!    future runner-health change, …). cirun-go consumes one schema and
//!    decides what to do with each `kind` via its own lookup table.
//!    Adding a new event type means adding an `EventKind` variant on
//!    both sides; no new endpoint, no new payload shape.
//!
//! 2. **One funnel.** The agent's main loop talks to ONE entry point:
//!    `ProvisionReporter::report(event)`. The per-event policy (which
//!    HTTP call, whether to touch retry-counter state) lives behind the
//!    trait — main.rs has no business knowing it. New event types add a
//!    match arm in the impl on `CirunClient`. main.rs does not change.
//!
//! 3. **Internal vocabulary stays typed.** Inside the agent we keep
//!    `ProvisionEvent` as a typed enum so callers can pattern-match
//!    without inspecting strings. The trait's wire-format translation
//!    happens at the boundary inside the reporter impl.

use async_trait::async_trait;
use serde::Serialize;

use crate::provision::{ProvisionOutcome, ProvisionResult};

/// Internal, typed vocabulary of provision-outcome events. Stays close
/// to the executor's `ProvisionOutcome` so the lift is trivial; the
/// `AgentEvent` wire format is built from this inside the reporter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionEvent {
    /// Runner provisioned and registered with GitHub. No wire emit —
    /// the SaaS learns via the periodic `report_running_vms` POST.
    Succeeded { runner_name: String },

    /// Real provisioning failure (executor error, bad spec, network).
    Failed { runner_name: String, error: String },

    /// Host admission control rejected the request (meda 503). Runner
    /// was never spawned; retry budget MUST be preserved.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    AtCapacity {
        runner_name: String,
        code: String,
        message: String,
        retry_after_secs: u64,
    },
}

impl ProvisionEvent {
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

/// Wire format for agent → cirun-go observability. ONE schema for all
/// event types; cirun-go reads `kind` and dispatches its own
/// per-kind behaviour (check-run update, retry-counter bump, etc.).
/// Field semantics:
///
/// - `runner_name`: the runner the event is about (always present).
/// - `kind`: discriminator; cirun-go's per-kind action table keys on this.
/// - `severity`: hint for log-level / check-run conclusion.
/// - `title`: short string suitable for a GH check-run title.
/// - `message`: longer human-readable text for the check-run summary
///   or log body.
/// - `metadata`: open bag for kind-specific structured data
///   (`retry_after_secs`, `code`, `attempt`, …). Keys are stable per
///   kind but the set is open — new kinds can attach new fields
///   without a schema change.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentEvent {
    pub runner_name: String,
    pub kind: EventKind,
    pub severity: Severity,
    pub title: String,
    pub message: String,
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Closed set of event kinds the cirun-go dispatch table needs to know
/// about. Stays small on purpose — only variants the agent currently
/// emits live here. Adding a new event type is: (a) introduce a
/// `ProvisionEvent` (or sibling) variant, (b) add the `to_agent_event`
/// arm that produces this kind, (c) mirror the snake_case token on
/// cirun-go's side.
///
/// `ProvisionSucceeded` is reserved for the day we want a check-run
/// update on successful provision; today the running-VMs heartbeat
/// already conveys that, so Succeeded is internal-only.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Reserved — emitted when we move provision-success notifications
    /// onto the check-run path. Today Succeeded outcomes are conveyed
    /// implicitly via `report_running_vms`.
    #[allow(dead_code)]
    ProvisionSucceeded,
    ProvisionFailed,
    HostAtCapacity,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Reserved — for low-priority events that should land in the
    /// backend's audit log but not change the GitHub check-run status.
    #[allow(dead_code)]
    Info,
    Warning,
    Error,
}

/// Build the wire-format event for a typed `ProvisionEvent`. Public so
/// the reporter impl + tests can share the mapping. Returns `None` for
/// events that intentionally produce no wire emit (today: `Succeeded`,
/// which is reported implicitly via the running-VMs heartbeat).
pub fn to_agent_event(ev: &ProvisionEvent, attempt: u32) -> Option<AgentEvent> {
    match ev {
        ProvisionEvent::Succeeded { .. } => None,
        ProvisionEvent::Failed { runner_name, error } => {
            let mut metadata = serde_json::Map::new();
            metadata.insert("attempt".into(), serde_json::Value::from(attempt));
            metadata.insert("error".into(), serde_json::Value::from(error.clone()));
            Some(AgentEvent {
                runner_name: runner_name.clone(),
                kind: EventKind::ProvisionFailed,
                severity: Severity::Error,
                title: "Provision failed".into(),
                message: error.clone(),
                metadata,
            })
        }
        ProvisionEvent::AtCapacity {
            runner_name,
            code,
            message,
            retry_after_secs,
        } => {
            let mut metadata = serde_json::Map::new();
            metadata.insert("code".into(), serde_json::Value::from(code.clone()));
            metadata.insert(
                "retry_after_secs".into(),
                serde_json::Value::from(*retry_after_secs),
            );
            Some(AgentEvent {
                runner_name: runner_name.clone(),
                kind: EventKind::HostAtCapacity,
                severity: Severity::Warning,
                title: "Host at capacity".into(),
                message: message.clone(),
                metadata,
            })
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

    /// Test double that records every event in arrival order.
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
        assert!(ev.is_success());
    }

    #[test]
    fn lifts_failed_outcome_to_failed_event() {
        let ev: ProvisionEvent = pr_failed("r1", "boom").into();
        assert!(matches!(ev, ProvisionEvent::Failed { .. }));
    }

    #[test]
    fn lifts_host_full_outcome_to_at_capacity_event() {
        let ev: ProvisionEvent = pr_host_full("r1", "CPU_EXHAUSTED").into();
        assert!(matches!(ev, ProvisionEvent::AtCapacity { .. }));
    }

    #[test]
    fn success_emits_no_wire_event() {
        // The running-VMs heartbeat already tells SaaS this runner is up.
        // Emitting a second "succeeded" event would just create
        // duplicate check-run noise.
        let ev = ProvisionEvent::Succeeded {
            runner_name: "r1".into(),
        };
        assert!(to_agent_event(&ev, 0).is_none());
    }

    #[test]
    fn failed_emits_provision_failed_event_with_attempt_and_error() {
        let ev = ProvisionEvent::Failed {
            runner_name: "r1".into(),
            error: "boom".into(),
        };
        let agent_event = to_agent_event(&ev, 3).expect("should emit");
        assert_eq!(agent_event.kind, EventKind::ProvisionFailed);
        assert_eq!(agent_event.severity, Severity::Error);
        assert_eq!(agent_event.message, "boom");
        assert_eq!(
            agent_event.metadata.get("attempt"),
            Some(&serde_json::Value::from(3))
        );
        assert_eq!(
            agent_event.metadata.get("error"),
            Some(&serde_json::Value::from("boom"))
        );
    }

    #[test]
    fn at_capacity_emits_host_at_capacity_event_with_code_and_retry_after() {
        let ev = ProvisionEvent::AtCapacity {
            runner_name: "r1".into(),
            code: "CPU_EXHAUSTED".into(),
            message: "CPU exhausted: ...".into(),
            retry_after_secs: 10,
        };
        let agent_event = to_agent_event(&ev, 0).expect("should emit");
        assert_eq!(agent_event.kind, EventKind::HostAtCapacity);
        assert_eq!(agent_event.severity, Severity::Warning);
        assert_eq!(agent_event.title, "Host at capacity");
        assert_eq!(
            agent_event.metadata.get("code"),
            Some(&serde_json::Value::from("CPU_EXHAUSTED"))
        );
        assert_eq!(
            agent_event.metadata.get("retry_after_secs"),
            Some(&serde_json::Value::from(10u64))
        );
    }

    #[test]
    fn serializes_kind_and_severity_as_snake_lowercase() {
        // Wire-shape stability test: cirun-go relies on these exact
        // tokens for its dispatch table. If you rename a variant, the
        // serde rename attrs need to follow, and this test catches
        // the drift before deploy.
        let ev = ProvisionEvent::AtCapacity {
            runner_name: "r1".into(),
            code: "CPU_EXHAUSTED".into(),
            message: "x".into(),
            retry_after_secs: 5,
        };
        let agent_event = to_agent_event(&ev, 0).unwrap();
        let json = serde_json::to_value(&agent_event).unwrap();
        assert_eq!(json["kind"], "host_at_capacity");
        assert_eq!(json["severity"], "warning");
        assert_eq!(json["title"], "Host at capacity");
        assert_eq!(json["metadata"]["code"], "CPU_EXHAUSTED");
        assert_eq!(json["metadata"]["retry_after_secs"], 5);
    }

    #[tokio::test]
    async fn recording_reporter_captures_events_in_order() {
        let r = RecordingReporter::new();
        r.report(pr_success("r1").into()).await;
        r.report(pr_failed("r2", "x").into()).await;
        let evs = r.events();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ProvisionEvent::Succeeded { .. }));
        assert!(matches!(evs[1], ProvisionEvent::Failed { .. }));
    }
}
