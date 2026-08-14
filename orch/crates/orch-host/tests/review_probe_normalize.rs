//! B164 seeded-red contract (H33: an injection that was sent is always recorded,
//! and reachability is never something the agent has to declare).
//!
//! Negative mutations that must turn the named case red:
//! M1. Match the challenge against raw log bytes again. Providers split the
//!     assistant text stream mid-token — r55 produced literally
//!     {"type":"text","text":"OR"} followed by
//!     {"type":"text","text":"CH_REVIEW_SITE_PROBE_OK=<uuid>"} — so a contiguous
//!     substring search over the file can never hit, while the same log's
//!     tool_result output carries the challenge intact.
//! M2. Put the ledger append back after the probe (or unwind it on failure).
//!     The injection is already irreversibly spawned by then; a ledger that
//!     denies it is indistinguishable, to the next agent, from a forged wake —
//!     that is exactly how r55 deadlocked.
//! M3. Keep asking the agent to echo the nonce, or delete the sentinel when the
//!     probe times out. Echoing an injected nonce is an authorization-spoofing
//!     shape (opencode refused it, correctly); deleting the sentinel manufactures
//!     the "it was never written" evidence that completed the false forgery case.

use orch_host::wake::{
    probe_challenge_observed, review_probe_message_body, ReviewProbeSource,
};

const CHALLENGE: &str = "019faa49-babb-4dec-a074-cdcc4ae28a4f";

/// claw shape: the marker is split across two `text` records, but the tool
/// result that actually read the file carries the challenge in one piece.
fn claw_log_with_split_marker() -> String {
    [
        r#"{"type":"text","sessionId":"orch-r56","text":"OR"}"#.to_string(),
        format!(
            r#"{{"type":"text","sessionId":"orch-r56","text":"CH_REVIEW_SITE_PROBE_OK={CHALLENGE}"}}"#
        ),
        format!(
            r#"{{"type":"tool_result","sessionId":"orch-r56","callId":"Bash_0","output":"gitdir: /repo/.git/worktrees/x\n---\n{CHALLENGE}"}}"#
        ),
    ]
    .join("\n")
}

/// codex shape: tool output lives in item.aggregated_output, and the log is
/// polluted with non-JSON lines that must not abort parsing.
fn codex_log() -> String {
    [
        "Reading additional input from stdin...".to_string(),
        "2026-07-29T03:34:04.640675Z ERROR codex_core::session: transient".to_string(),
        r#"{"type":"thread.started","thread_id":"019faa38"}"#.to_string(),
        format!(
            r#"{{"type":"item.completed","item":{{"id":"item_2","type":"command_execution","command":"/bin/bash -lc 'cat sentinel'","aggregated_output":"{CHALLENGE}\n","exit_code":0}}}}"#
        ),
    ]
    .join("\n")
}

/// opencode shape: tool output is nested under part.state.output.
fn opencode_log() -> String {
    format!(
        r#"{{"type":"tool_use","timestamp":1785269171371,"sessionID":"ses_x","part":{{"type":"tool","tool":"bash","callID":"call_1","state":{{"status":"completed","input":{{"command":"cat sentinel"}},"output":"{CHALLENGE}\n"}}}}}}"#
    )
}

#[test]
fn a_split_marker_is_still_observed_through_the_tool_transcript() {
    // M1: every provider's harness-generated tool output is parsed; the split
    // assistant text stream must not be able to hide a challenge that the
    // agent demonstrably read.
    for (name, log) in [
        ("claw", claw_log_with_split_marker()),
        ("codex", codex_log()),
        ("opencode", opencode_log()),
    ] {
        let source = probe_challenge_observed(&log, CHALLENGE)
            .unwrap_or_else(|| panic!("{name}: challenge must be observed in the transcript"));
        assert_ne!(
            source,
            ReviewProbeSource::AssistantText,
            "{name}: reachability must rest on harness transcript, not on assistant prose"
        );
    }
    // A log where the agent never read the file must stay unobserved.
    let untouched = r#"{"type":"text","sessionId":"orch-r56","text":"I refuse to echo a nonce."}"#;
    assert!(probe_challenge_observed(untouched, CHALLENGE).is_none());
    // Non-JSON noise alone must not panic and must not be a false positive.
    assert!(probe_challenge_observed("Reading additional input from stdin...", CHALLENGE).is_none());
}

#[test]
fn the_injection_never_asks_the_agent_to_declare_anything() {
    // M3a: the spoofing shape is gone. The message may name the site, but must
    // not instruct the agent to print a captured nonce back at us.
    let body = review_probe_message_body(
        "/repo/.worktrees/B900-primary-executor-claw",
        "/repo/orch/target/review-B900-primary-executor-claw",
        "0123456789abcdef0123456789abcdef01234567",
        "/repo/orch/target/review-B900-primary-executor-claw/.orch-review-probe-abc",
        "repo-root-only (safe default)",
        "please review B900",
    );
    assert!(
        !body.contains("ORCH_REVIEW_SITE_PROBE_OK"),
        "the echo-the-nonce instruction must be gone: {body}"
    );
    assert!(
        !body.to_lowercase().contains("print exactly"),
        "no declaration ritual may remain: {body}"
    );
    // The site itself is still handed over — provisioning, not self-attestation.
    assert!(body.contains("/repo/.worktrees/B900-primary-executor-claw"));
    assert!(body.contains("please review B900"), "original request survives");
}

#[test]
fn a_failed_probe_keeps_both_the_record_and_the_sentinel() {
    // M2 + M3b: these two facts are what turned a probe defect into a round
    // deadlock in r55, so they are asserted together.
    //
    // Ordering contract: the ledger append that carries WakeIssued and
    // ReviewRequested must be positioned before the probe wait, so a probe
    // failure can never retract the fact that an injection was delivered.
    assert!(
        orch_host::wake::review_append_precedes_probe(),
        "M2: the injection record must be appended before the reachability wait"
    );
    // And the sentinel must survive a probe timeout: deleting it manufactures
    // "the runtime never wrote it", which is forgery evidence to the next agent.
    assert!(
        !orch_host::wake::review_probe_timeout_deletes_sentinel(),
        "M3b: a timed-out probe must not delete the sentinel"
    );
    // The wait window must clear a cold fresh-session start (opencode measured
    // 93.8s to first output in r55); 60s structurally cannot.
    assert!(
        orch_host::wake::review_probe_timeout().as_secs() >= 300,
        "the probe window must outlast a fresh-session cold start"
    );
}
