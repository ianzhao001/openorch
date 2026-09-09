//! B187 seeded-red contract — review consumption must recognize the runtime's
//! structured sentinel read without turning arbitrary SmartClaw traffic into
//! proof of consumption.
//!
//! Incident manifest (content deliberately redacted): the B177 primary-review
//! per-attempt log had sha256
//! c3e9fa4d2456faeb805da84741651aee1d1c2bf74c6f649c976f8718117b0c3d,
//! length 105_692 bytes, and 52 valid JSON-lines: 20 text, 16 tool_use, and
//! 16 tool_result records under one non-empty sessionId.  It contained the
//! exact runtime sentinel challenge in a tool_result but contained no literal
//! thread.started or turn.started marker.  This seed recreates only that public
//! structural shape and byte volume; no prompt, tool input, tool output, path,
//! session identifier, or other raw incident content is copied into the repo.
//!
//! Required negative mutations:
//! M1. Delete StructuredProbe support and retain only thread/turn.started:
//!     redacted_b177_claw_incident_has_a_consumption_proof turns red.
//! M2. Treat any text/tool_use/tool_result frame as consumption:
//!     claw_frames_without_the_exact_tool_output_challenge_are_not_proof turns red.
//! M3. Search assistant text or tool input for the challenge:
//!     claw_frames_without_the_exact_tool_output_challenge_are_not_proof turns red.
//! M4. Ignore the exact [start,end) window or accept a partial JSON record:
//!     stale_or_partial_evidence_is_not_consumption turns red.
//! M5. Permit an empty challenge:
//!     an_empty_challenge_is_rejected turns red.

use orch_host::wake::{
    review_consumption_proof, ReviewConsumptionProof, ReviewProbeSource,
};

const CHALLENGE: &str = "019fb34b-0000-4000-8000-000000000187";
const REDACTED_SESSION: &str = "orch-wake-redacted";
const INCIDENT_BYTES: usize = 105_692;

fn json_line(value: serde_json::Value) -> String {
    serde_json::to_string(&value).expect("synthetic JSON must serialize")
}

/// Construct a content-free structural replay with the exact incident byte
/// length and top-level record counts.  Filler is inert assistant text.
fn redacted_incident_replay() -> Vec<u8> {
    let mut lines = Vec::new();
    for index in 0..19 {
        lines.push(json_line(serde_json::json!({
            "type": "text",
            "sessionId": REDACTED_SESSION,
            "text": format!("redacted-text-{index}"),
        })));
    }
    for index in 0..16 {
        lines.push(json_line(serde_json::json!({
            "type": "tool_use",
            "sessionId": REDACTED_SESSION,
            "callId": format!("Redacted_{index}"),
            "tool": "Read",
            "input": {"file_path": "<redacted>"},
        })));
    }
    for index in 0..16 {
        lines.push(json_line(serde_json::json!({
            "type": "tool_result",
            "sessionId": REDACTED_SESSION,
            "callId": format!("Redacted_{index}"),
            "output": if index == 15 { CHALLENGE } else { "<redacted>" },
        })));
    }

    // The first 51 records leave one inert text record whose text field pads
    // the replay to the exact observed byte volume without carrying raw data.
    let prefix = format!("{}\n", lines.join("\n"));
    let empty_tail = json_line(serde_json::json!({
        "type": "text",
        "sessionId": REDACTED_SESSION,
        "text": "",
    }));
    let fixed = prefix.len() + empty_tail.len() + 1;
    assert!(fixed < INCIDENT_BYTES);
    let tail = json_line(serde_json::json!({
        "type": "text",
        "sessionId": REDACTED_SESSION,
        "text": "x".repeat(INCIDENT_BYTES - fixed),
    }));
    let replay = format!("{prefix}{tail}\n").into_bytes();
    assert_eq!(replay.len(), INCIDENT_BYTES);
    assert_eq!(replay.iter().filter(|byte| **byte == b'\n').count(), 52);
    replay
}

#[test]
fn redacted_b177_claw_incident_has_a_consumption_proof() {
    let replay = redacted_incident_replay();
    let proof = review_consumption_proof(&replay, 0, replay.len() as u64, CHALLENGE)
        .expect("a valid immutable window must evaluate");
    assert_eq!(
        proof,
        Some(ReviewConsumptionProof::StructuredProbe(
            ReviewProbeSource::ClawToolOutput
        )),
        "the exact sentinel read is stronger evidence than provider-specific turn-start spelling"
    );
}

#[test]
fn claw_frames_without_the_exact_tool_output_challenge_are_not_proof() {
    let cases = [
        // Assistant prose is not harness-owned evidence.
        format!(
            "{{\"type\":\"text\",\"sessionId\":\"{REDACTED_SESSION}\",\"text\":\"{CHALLENGE}\"}}\n"
        ),
        // A tool request containing a path/value is not proof the tool ran.
        format!(
            "{{\"type\":\"tool_use\",\"sessionId\":\"{REDACTED_SESSION}\",\"callId\":\"Read_0\",\"tool\":\"Read\",\"input\":{{\"probe\":\"{CHALLENGE}\"}}}}\n"
        ),
        // Backend activity and a completed tool result with the wrong value
        // still do not prove this exact review injection was consumed.
        format!(
            "{{\"type\":\"text\",\"sessionId\":\"{REDACTED_SESSION}\",\"text\":\"working\"}}\n\
             {{\"type\":\"tool_use\",\"sessionId\":\"{REDACTED_SESSION}\",\"callId\":\"Read_0\",\"tool\":\"Read\",\"input\":{{}}}}\n\
             {{\"type\":\"tool_result\",\"sessionId\":\"{REDACTED_SESSION}\",\"callId\":\"Read_0\",\"output\":\"wrong challenge\"}}\n"
        ),
        String::new(),
    ];
    for log in cases {
        assert_eq!(
            review_consumption_proof(log.as_bytes(), 0, log.len() as u64, CHALLENGE)
                .expect("well-bounded input must evaluate"),
            None,
            "ordinary SmartClaw traffic must not be promoted to exact consumption proof"
        );
    }
}

#[test]
fn stale_or_partial_evidence_is_not_consumption() {
    let stale = format!(
        "{{\"type\":\"tool_result\",\"sessionId\":\"old\",\"callId\":\"Read_0\",\"output\":\"{CHALLENGE}\"}}\n"
    );
    let partial = format!(
        r#"{{"type":"tool_result","sessionId":"{REDACTED_SESSION}","callId":"Read_1","output":"{CHALLENGE}""#
    );
    let log = format!("{stale}{partial}");
    assert_eq!(
        review_consumption_proof(
            log.as_bytes(),
            stale.len() as u64,
            log.len() as u64,
            CHALLENGE,
        )
        .expect("the exact window itself is valid"),
        None,
        "evidence before offset and an incomplete current record are both non-proof"
    );
    assert!(review_consumption_proof(
        log.as_bytes(),
        (log.len() + 1) as u64,
        log.len() as u64,
        CHALLENGE,
    )
    .is_err());
}

#[test]
fn an_empty_challenge_is_rejected() {
    let log = b"{\"type\":\"tool_result\",\"output\":\"anything\"}\n";
    assert!(review_consumption_proof(log, 0, log.len() as u64, "").is_err());
    assert!(review_consumption_proof(log, 0, log.len() as u64, "   ").is_err());
}
