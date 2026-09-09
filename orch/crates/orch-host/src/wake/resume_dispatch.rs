//! Read-only proof of historical task-resume replacement edges.
//! No production path creates a new replacement or grants task-resume authority.

use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use orch_core::EventRecord;

/// Historical fields binding a recorded replacement to its exact
/// `ResumeWakeLaunching` generation; this type does not authorize a new wake.
#[derive(Debug, Clone, Copy)]
pub(super) struct Scope<'a> {
    pub(super) action_id: &'a str,
    pub(super) owner: &'a str,
    pub(super) generation: &'a str,
}

/// Validated implementation wake ids retired by exact resume replacements.
#[derive(Debug, Default)]
pub(super) struct Projection {
    replaced: BTreeSet<String>,
}

impl Projection {
    /// Whether this source wake has one validated resume successor.
    pub(super) fn replaces(&self, wake_id: &str) -> bool {
        self.replaced.contains(wake_id)
    }
}

fn required_text<'a>(payload: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("resume replacement missing nonblank {key}"))
}

fn exact_source<'a>(
    events: &'a [EventRecord],
    round: &str,
    wake_id: &str,
) -> Result<(usize, &'a EventRecord, super::DurableReceiptFacts)> {
    let sources = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "WakeIssued"
                && event.round.as_deref() == Some(round)
                && super::payload_string(event, "wakeId") == Some(wake_id)
        })
        .collect::<Vec<_>>();
    if sources.len() != 1 {
        bail!(
            "resume replacement requires exactly one source WakeIssued for wakeId={wake_id}, found {}",
            sources.len()
        );
    }
    let (index, event) = sources[0];
    let facts = super::durable_receipt_facts(event)?;
    if facts.identity.role.is_some()
        || !facts.continuation_id.starts_with("implementation:")
        || facts.identity.task_id.is_none()
        || facts.identity.attempt_id.is_none()
    {
        bail!("resume replacement source must be an attempt-scoped implementation wake");
    }
    Ok((index, event, facts))
}

fn exact_terminal_evidence(
    events: &[EventRecord],
    round: &str,
    source_index: usize,
    source: &super::DurableReceiptFacts,
) -> Result<(usize, super::SessionDeathEvidence)> {
    let receipt = super::require_exact_accepted_backend_receipt(events, source)?;
    let receipt_index = events
        .iter()
        .position(|event| std::ptr::eq(event, receipt))
        .expect("accepted receipt came from this event slice");
    let terminations = events
        .iter()
        .enumerate()
        .skip(source_index + 1)
        .filter(|(_, event)| {
            event.kind == "ManagedWakeTerminated"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == source.identity.task_id.as_deref()
                && super::payload_string(event, "wakeId") == Some(source.wake_id.as_str())
                && super::payload_string(event, "agent") == Some(source.agent.as_str())
        })
        .collect::<Vec<_>>();
    if terminations.len() != 1 {
        bail!(
            "resume replacement requires exactly one ManagedWakeTerminated for wakeId={}, found {}",
            source.wake_id,
            terminations.len()
        );
    }
    let (termination_index, termination) = terminations[0];
    if receipt_index <= source_index || receipt_index >= termination_index {
        bail!("resume replacement source receipt/termination order is invalid");
    }
    let payload = termination
        .payload
        .as_ref()
        .context("ManagedWakeTerminated missing payload")?;
    let signals = payload
        .get("signals")
        .and_then(serde_json::Value::as_array)
        .context("ManagedWakeTerminated signals must be an array")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .context("ManagedWakeTerminated signal must be a string")
        })
        .collect::<Result<Vec<_>>>()?;
    let cancel_request_id = match payload.get("cancelRequestId") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        _ => bail!("ManagedWakeTerminated cancelRequestId must be null or nonblank"),
    };
    let facts = super::ManagedWakeTerminationFacts {
        wake_id: source.wake_id.clone(),
        agent: source.agent.clone(),
        completion_reason: Some(required_text(payload, "completionReason")?.to_string()),
        terminal_seen: payload
            .get("terminalSeen")
            .and_then(serde_json::Value::as_bool)
            .context("ManagedWakeTerminated missing terminalSeen")?,
        exited_naturally: payload
            .get("exitedNaturally")
            .and_then(serde_json::Value::as_bool)
            .context("ManagedWakeTerminated missing exitedNaturally")?,
        hard_deadline_reached: payload
            .get("hardDeadlineReached")
            .and_then(serde_json::Value::as_bool)
            .context("ManagedWakeTerminated missing hardDeadlineReached")?,
        cancel_request_id,
        signals,
        managed_scope_terminated: payload
            .get("managedScopeTerminated")
            .and_then(serde_json::Value::as_bool)
            .context("ManagedWakeTerminated missing managedScopeTerminated")?,
        log_bytes_read: payload
            .get("logBytesRead")
            .and_then(serde_json::Value::as_u64)
            .context("ManagedWakeTerminated missing logBytesRead")?,
    };
    let evidence = super::SessionDeathEvidence::from_terminal_facts(&facts)
        .context("ManagedWakeTerminated does not prove a reusable terminal scope")?;
    if evidence.outcome() != "DeliveredTerminal"
        || super::payload_string(termination, "outcomeClass") != Some(evidence.outcome())
    {
        bail!("resume replacement requires exact DeliveredTerminal evidence");
    }
    Ok((termination_index, evidence))
}

fn stable_resume_event(
    event: &EventRecord,
    kind: &str,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    action_id: &str,
) -> bool {
    event.kind == kind
        && event.actor == "runtime:orch"
        && event.round.as_deref() == Some(round)
        && event.task_id.as_deref() == Some(task_id)
        && super::payload_string(event, "attemptId") == Some(attempt_id)
        && super::payload_string(event, "agent") == Some(agent)
        && super::payload_string(event, "actionId") == Some(action_id)
        && event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("attemptNo"))
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|value| value > 0)
        && super::payload_string(event, "baseSha").is_some_and(|value| !value.is_empty())
        && super::payload_string(event, "goPath").is_some_and(|value| !value.is_empty())
        && super::payload_string(event, "resumeDigest").is_some_and(super::valid_sha256)
}

fn exact_resume_control(
    events: &[EventRecord],
    round: &str,
    source: &super::DurableReceiptFacts,
    scope: Scope<'_>,
    upper_bound: usize,
) -> Result<(usize, usize, usize)> {
    if !super::valid_sha256(scope.action_id)
        || scope.owner.trim().is_empty()
        || scope.generation.trim().is_empty()
    {
        bail!("resume replacement scope identity is malformed");
    }
    let task_id = source
        .identity
        .task_id
        .as_deref()
        .context("resume source missing taskId")?;
    let attempt_id = source
        .identity
        .attempt_id
        .as_deref()
        .context("resume source missing attemptId")?;
    let matching = |event: &EventRecord, kind: &str| {
        stable_resume_event(
            event,
            kind,
            round,
            task_id,
            attempt_id,
            &source.agent,
            scope.action_id,
        )
    };
    let issued = events
        .iter()
        .enumerate()
        .filter(|(index, event)| *index < upper_bound && matching(event, "ResumeIssued"))
        .collect::<Vec<_>>();
    if issued.len() != 1 {
        bail!("resume replacement requires exactly one matching ResumeIssued");
    }
    let (issued_index, issued_event) = issued[0];
    let issued_payload = issued_event
        .payload
        .as_ref()
        .expect("matched payload fields");
    let prompt = required_text(issued_payload, "prompt")?;
    let digest = required_text(issued_payload, "resumeDigest")?;
    if super::sha256_hex(prompt.as_bytes()) != digest
        || issued_payload
            .get("wakePending")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
    {
        bail!("ResumeIssued prompt/digest/wakePending binding is invalid");
    }
    let exact_generation = |event: &EventRecord, kind: &str| {
        matching(event, kind)
            && super::payload_string(event, "owner") == Some(scope.owner)
            && super::payload_string(event, "leaseGeneration") == Some(scope.generation)
            && super::payload_string(event, "leaseUntil").is_some_and(|value| !value.is_empty())
    };
    let claims = events
        .iter()
        .enumerate()
        .filter(|(index, event)| {
            *index < upper_bound && exact_generation(event, "ResumeWakeClaimed")
        })
        .collect::<Vec<_>>();
    let launches = events
        .iter()
        .enumerate()
        .filter(|(index, event)| {
            *index < upper_bound && exact_generation(event, "ResumeWakeLaunching")
        })
        .collect::<Vec<_>>();
    if claims.len() != 1 || launches.len() != 1 {
        bail!("resume replacement requires one exact Claimed -> Launching generation");
    }
    let claim_index = claims[0].0;
    let launch_index = launches[0].0;
    if !(issued_index < claim_index && claim_index < launch_index && launch_index < upper_bound) {
        bail!("resume replacement control event order is invalid");
    }
    let released_or_terminal = events[claim_index + 1..upper_bound].iter().any(|event| {
        (event.kind == "ResumeWakeReleased"
            && super::payload_string(event, "actionId") == Some(scope.action_id)
            && super::payload_string(event, "owner") == Some(scope.owner)
            && super::payload_string(event, "leaseGeneration") == Some(scope.generation))
            || (matches!(
                event.kind.as_str(),
                "ResumeWakeDelivered" | "ResumeWakeCompleted"
            ) && super::payload_string(event, "actionId") == Some(scope.action_id))
    });
    if released_or_terminal {
        bail!("resume replacement generation was released or terminal before provider spawn");
    }
    Ok((issued_index, claim_index, launch_index))
}

/// Validate every durable resume replacement edge and return the source wakes
/// that no longer own the implementation continuation.
pub(super) fn project(events: &[EventRecord], round: &str) -> Result<Projection> {
    let mut projection = Projection::default();
    for (successor_index, successor_event) in events.iter().enumerate() {
        let Some(payload) = successor_event.payload.as_ref() else {
            continue;
        };
        let Some(source_wake_id) = payload
            .get("resumedFromWakeId")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if source_wake_id.trim().is_empty() {
            bail!("WakeIssued.resumedFromWakeId must be nonblank");
        }
        let scope = Scope {
            action_id: required_text(payload, "resumeActionId")?,
            owner: required_text(payload, "resumeOwner")?,
            generation: required_text(payload, "resumeLeaseGeneration")?,
        };
        let successor = super::durable_receipt_facts(successor_event)?;
        if successor_event.round.as_deref() != Some(round)
            || successor.identity.role.is_some()
            || !successor.continuation_id.starts_with("implementation:")
            || super::payload_string(successor_event, "method") != Some("typed-runtime")
        {
            bail!("resume replacement successor is not an active-round typed implementation wake");
        }
        let (source_index, _source_event, source) = exact_source(events, round, source_wake_id)?;
        if source_index >= successor_index
            || source.wake_id == successor.wake_id
            || source.agent != successor.agent
            || source.provider_kind != successor.provider_kind
            || source.continuation_id != successor.continuation_id
            || source.identity.task_id != successor.identity.task_id
            || source.identity.attempt_id != successor.identity.attempt_id
            || source.identity.role != successor.identity.role
            || source.request_message_sha256 == successor.request_message_sha256
        {
            bail!("resume replacement source/successor identity is invalid");
        }
        let (termination_index, _) = exact_terminal_evidence(events, round, source_index, &source)?;
        let (issued_index, _, launch_index) =
            exact_resume_control(events, round, &source, scope, successor_index)?;
        if !(termination_index < issued_index && launch_index < successor_index) {
            bail!("resume replacement durable event order is invalid");
        }
        if !projection.replaced.insert(source_wake_id.to_string()) {
            bail!("one implementation wake has multiple resume replacements");
        }
    }
    Ok(projection)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUND: &str = "rT";
    const TASK: &str = "BT";
    const ATTEMPT: &str = "BT-A0001";
    const AGENT: &str = "executor-desktop";
    const SOURCE: &str = "019fd000-1111-4222-8333-444455556666";
    const SUCCESSOR: &str = "019fd000-7777-4888-8999-aaaabbbbcccc";
    const OLD_DIGEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const NEW_DIGEST: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const RENDERED: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const WINDOW: &str = "4444444444444444444444444444444444444444444444444444444444444444";

    fn continuation() -> String {
        format!("implementation:{ROUND}:{TASK}:{ATTEMPT}:{AGENT}")
    }

    fn wake(wake_id: &str, request_digest: &str) -> EventRecord {
        super::super::ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "wakeId": wake_id,
                "continuationId": continuation(),
                "attemptId": ATTEMPT,
                "agent": AGENT,
                "providerKind": "codex",
                "requestedProvider": null,
                "requestedModel": null,
                "requestedEffort": null,
                "requestMessageSha256": request_digest,
                "renderedMessageSha256": RENDERED,
                "requestSessionId": null,
                "backendState": "pending",
                "probeOffset": 0,
                "logPath": "/fixture/wake.jsonl",
                "method": "typed-runtime",
            }),
        )
    }

    fn receipt() -> EventRecord {
        super::super::ledger::event(
            "AgentEventReceived",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "agentEvent": "wake-backend-receipt",
                "actionId": SOURCE,
                "wakeId": SOURCE,
                "continuationId": continuation(),
                "attemptId": ATTEMPT,
                "agent": AGENT,
                "providerKind": "codex",
                "receiptKind": "codex",
                "requestedProvider": null,
                "requestedModel": null,
                "requestedEffort": null,
                "requestMessageSha256": OLD_DIGEST,
                "renderedMessageSha256": RENDERED,
                "requestSessionId": null,
                "observedSessionId": "session-fixture",
                "backendState": "accepted",
                "probeOffset": 0,
                "probeEnd": 1,
                "windowSha256": WINDOW,
                "logPath": "/fixture/wake.jsonl",
            }),
        )
    }

    fn terminal(managed: bool) -> EventRecord {
        super::super::ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "wakeId": SOURCE,
                "agent": AGENT,
                "completionReason": "natural-exit",
                "terminalSeen": true,
                "exitedNaturally": true,
                "hardDeadlineReached": false,
                "cancelRequestId": null,
                "signals": [],
                "managedScopeTerminated": managed,
                "logBytesRead": 12,
                "outcomeClass": "DeliveredTerminal",
            }),
        )
    }

    fn control() -> (Vec<EventRecord>, String, String, String) {
        let action_id = "a".repeat(64);
        let owner = "resume-owner".to_string();
        let generation = "resume-generation".to_string();
        let prompt = "repair the exact frozen failure";
        let resume_digest = super::super::sha256_hex(prompt.as_bytes());
        let stable = serde_json::json!({
            "actionId": action_id,
            "attemptId": ATTEMPT,
            "attemptNo": 1,
            "agent": AGENT,
            "baseSha": "base-sha",
            "goPath": "coordination/GO.md",
            "resumeDigest": resume_digest,
        });
        let mut issued = stable.clone();
        issued["prompt"] = serde_json::json!(prompt);
        issued["wakePending"] = serde_json::json!(true);
        let mut claimed = stable.clone();
        claimed["owner"] = serde_json::json!(owner);
        claimed["leaseGeneration"] = serde_json::json!(generation);
        claimed["leaseUntil"] = serde_json::json!("2099-01-01T00:00:00Z");
        let mut old_claim = stable.clone();
        old_claim["owner"] = serde_json::json!("old-owner");
        old_claim["leaseGeneration"] = serde_json::json!("old-generation");
        old_claim["leaseUntil"] = serde_json::json!("2000-01-01T00:00:00Z");
        (
            vec![
                super::super::ledger::event(
                    "ResumeIssued",
                    "runtime:orch",
                    Some(TASK),
                    Some(ROUND),
                    issued,
                ),
                super::super::ledger::event(
                    "ResumeWakeClaimed",
                    "runtime:orch",
                    Some(TASK),
                    Some(ROUND),
                    old_claim.clone(),
                ),
                super::super::ledger::event(
                    "ResumeWakeLaunching",
                    "runtime:orch",
                    Some(TASK),
                    Some(ROUND),
                    old_claim.clone(),
                ),
                super::super::ledger::event(
                    "ResumeWakeReleased",
                    "runtime:orch",
                    Some(TASK),
                    Some(ROUND),
                    old_claim,
                ),
                super::super::ledger::event(
                    "ResumeWakeClaimed",
                    "runtime:orch",
                    Some(TASK),
                    Some(ROUND),
                    claimed.clone(),
                ),
                super::super::ledger::event(
                    "ResumeWakeLaunching",
                    "runtime:orch",
                    Some(TASK),
                    Some(ROUND),
                    claimed,
                ),
            ],
            action_id,
            owner,
            generation,
        )
    }

    fn fixture(managed: bool) -> (Vec<EventRecord>, String, String, String) {
        let (control, action, owner, generation) = control();
        let mut events = vec![wake(SOURCE, OLD_DIGEST), receipt(), terminal(managed)];
        events.extend(control);
        (events, action, owner, generation)
    }

    fn append_historical_successor(
        events: &mut Vec<EventRecord>,
        action: &str,
        owner: &str,
        generation: &str,
    ) {
        let mut successor = wake(SUCCESSOR, NEW_DIGEST);
        let payload = successor.payload.as_mut().unwrap();
        payload["resumedFromWakeId"] = serde_json::json!(SOURCE);
        payload["resumeActionId"] = serde_json::json!(action);
        payload["resumeOwner"] = serde_json::json!(owner);
        payload["resumeLeaseGeneration"] = serde_json::json!(generation);
        events.push(successor);
    }

    #[test]
    fn exact_historical_terminal_control_projects_one_replacement() {
        let (mut events, action, owner, generation) = fixture(true);
        append_historical_successor(&mut events, &action, &owner, &generation);
        assert!(project(&events, ROUND).unwrap().replaces(SOURCE));
    }

    #[test]
    fn historical_projection_refuses_missing_scope_receipt_and_identity_drift() {
        let (mut unterminated, action, owner, generation) = fixture(false);
        append_historical_successor(&mut unterminated, &action, &owner, &generation);
        assert!(project(&unterminated, ROUND).is_err());

        let (mut no_receipt, action, owner, generation) = fixture(true);
        no_receipt.remove(1);
        append_historical_successor(&mut no_receipt, &action, &owner, &generation);
        assert!(project(&no_receipt, ROUND).is_err());

        let (mut wrong_generation, action, owner, _) = fixture(true);
        append_historical_successor(&mut wrong_generation, &action, &owner, "wrong-generation");
        assert!(project(&wrong_generation, ROUND).is_err());

        let (mut wrong_attempt, action, owner, generation) = fixture(true);
        append_historical_successor(&mut wrong_attempt, &action, &owner, &generation);
        wrong_attempt.last_mut().unwrap().payload.as_mut().unwrap()["attemptId"] =
            serde_json::json!("wrong-attempt");
        assert!(project(&wrong_attempt, ROUND).is_err());
    }

    #[test]
    fn historical_projection_refuses_duplicate_edges_and_wrong_order() {
        let (mut duplicate, action, owner, generation) = fixture(true);
        append_historical_successor(&mut duplicate, &action, &owner, &generation);
        duplicate.push(duplicate.last().unwrap().clone());
        assert!(project(&duplicate, ROUND).is_err());

        let (mut wrong_order, action, owner, generation) = fixture(true);
        append_historical_successor(&mut wrong_order, &action, &owner, &generation);
        let successor = wrong_order.pop().unwrap();
        wrong_order.insert(3, successor);
        assert!(project(&wrong_order, ROUND).is_err());
    }
}
