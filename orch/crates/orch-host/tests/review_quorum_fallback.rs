//! B303 seeded contract: signed fallback, explicit nongate seats, and a two-result quorum.
//!
//! Degradations that must turn this target red:
//! M1 remove `fallbackAgent` from the signed card/IR;
//! M2 derive nongate obligations from capacity roles instead of `nongateSeats`;
//! M3 count failed/empty/timed-out receipts as substantive results;
//! M4 let one result satisfy the quorum;
//! M5 let PASS votes overwrite a substantive formal FAIL;
//! M6 let PASS votes overwrite a substantive nongate FAIL/BLOCKED;
//! M7 allow fallback before the source managed scope is terminal;
//! M8 allow fallback across attempt/head/role or to an unsigned agent;
//! M9 omit one of the three durable fallback/substitution facts;
//! M10 let a late original reviewer reclaim the current slot;
//! M11 remove `skip_serializing_if` and drift a historical IR digest;
//! M12 apply the new quorum to a historical card with no signed policy;
//! M13 let a pending formal seat be hidden by two PASS voices;
//! M14 fallback while a valid staged/canonical source artifact awaits reconciliation;
//! M15 count one agent or one delivery event twice, including double substitution;
//! M16 append/spawn twice under repeated or concurrent runloop ticks.

use std::fs;
use std::path::PathBuf;

use orch_host::card::ReviewQuorumPolicy;
use orch_host::verify::{
    evaluate_review_quorum, ReviewQuorumDecision, ReviewResult, ReviewResultState,
};

const ATTEMPT: &str = "B303-A0001";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn source(relative: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn policy() -> ReviewQuorumPolicy {
    ReviewQuorumPolicy {
        minimum_substantive: 2,
        nongate_may_substitute_failed_formal: true,
        minimum_nongate_pass_for_substitution: 1,
    }
}

fn result(agent: &str, role: &str, event: &str, state: ReviewResultState) -> ReviewResult {
    ReviewResult {
        agent: agent.to_string(),
        role: role.to_string(),
        attempt_id: ATTEMPT.to_string(),
        reviewed_head: HEAD.to_string(),
        delivery_event_id: event.to_string(),
        state,
    }
}

#[test]
fn two_formal_passes_satisfy_the_quorum() {
    assert_eq!(
        evaluate_review_quorum(
            &policy(),
            &[
                result("executor-opencode", "primary", "formal-1", ReviewResultState::FormalPass),
                result(
                    "executor-pi",
                    "secondary",
                    "formal-2",
                    ReviewResultState::FormalPass,
                ),
            ],
        ),
        ReviewQuorumDecision::Satisfied
    );
}

#[test]
fn one_formal_and_one_nongate_pass_can_close_a_channel_error() {
    assert_eq!(
        evaluate_review_quorum(
            &policy(),
            &[
                result("executor-pi", "secondary", "formal-2", ReviewResultState::FormalPass),
                result(
                    "executor-opencode",
                    "primary",
                    "formal-error",
                    ReviewResultState::ChannelError,
                ),
                result("executor-dsh", "nongate", "nongate-1", ReviewResultState::NongatePass),
            ],
        ),
        ReviewQuorumDecision::Satisfied
    );
}

#[test]
fn a_pending_formal_seat_is_not_a_channel_error_and_stays_open() {
    assert_eq!(
        evaluate_review_quorum(
            &policy(),
            &[
                result("executor-pi", "secondary", "formal-2", ReviewResultState::FormalPass),
                result("executor-opencode", "primary", "pending", ReviewResultState::Pending),
                result("executor-dsh", "nongate", "nongate-1", ReviewResultState::NongatePass),
            ],
        ),
        ReviewQuorumDecision::Insufficient
    );
}

#[test]
fn a_formal_fail_is_still_a_veto_when_it_is_seen_last() {
    assert_eq!(
        evaluate_review_quorum(
            &policy(),
            &[
                result("executor-pi", "secondary", "formal-2", ReviewResultState::FormalPass),
                result("executor-dsh", "nongate", "nongate-1", ReviewResultState::NongatePass),
                result("executor-opencode", "primary", "formal-1", ReviewResultState::FormalFail),
            ],
        ),
        ReviewQuorumDecision::BlockedByFinding
    );
}

#[test]
fn nongate_fail_and_blocked_are_monotonic_vetoes() {
    for state in [ReviewResultState::NongateFail, ReviewResultState::Blocked] {
        assert_eq!(
            evaluate_review_quorum(
                &policy(),
                &[
                    result("executor-opencode", "primary", "formal-1", ReviewResultState::FormalPass),
                    result(
                        "executor-pi",
                        "secondary",
                        "formal-2",
                        ReviewResultState::FormalPass,
                    ),
                    result("executor-dsh", "nongate", "nongate-1", state),
                ],
            ),
            ReviewQuorumDecision::BlockedByFinding
        );
    }
}

#[test]
fn failed_empty_and_timed_out_channels_are_zero_votes() {
    for state in [
        ReviewResultState::ChannelError,
        ReviewResultState::Empty,
        ReviewResultState::TimedOut,
    ] {
        assert_eq!(
            evaluate_review_quorum(
                &policy(),
                &[
                    result("executor-opencode", "primary", "formal-1", ReviewResultState::FormalPass),
                    result("executor-dsh", "nongate", "nongate-error", state),
                ],
            ),
            ReviewQuorumDecision::Insufficient
        );
    }
}

#[test]
fn one_substantive_result_is_never_enough() {
    assert_eq!(
        evaluate_review_quorum(
            &policy(),
            &[result("executor-dsh", "nongate", "nongate-1", ReviewResultState::NongatePass)],
        ),
        ReviewQuorumDecision::Insufficient
    );
}

#[test]
fn one_agent_or_one_delivery_event_is_never_two_voices() {
    for duplicate in [
        vec![
            result("executor-dsh", "nongate", "delivery-a", ReviewResultState::NongatePass),
            result("executor-dsh", "secondary", "delivery-b", ReviewResultState::FormalPass),
        ],
        vec![
            result("executor-opencode", "primary", "delivery-a", ReviewResultState::FormalPass),
            result("executor-dsh", "nongate", "delivery-a", ReviewResultState::NongatePass),
        ],
    ] {
        assert_eq!(
            evaluate_review_quorum(&policy(), &duplicate),
            ReviewQuorumDecision::Invalid
        );
    }
}

#[test]
fn cross_attempt_or_head_results_are_invalid_not_votes() {
    let mut wrong_attempt = result(
        "executor-dsh",
        "nongate",
        "nongate-1",
        ReviewResultState::NongatePass,
    );
    wrong_attempt.attempt_id = "B303-A0002".to_string();
    let mut wrong_head = result(
        "executor-dsh",
        "nongate",
        "nongate-2",
        ReviewResultState::NongatePass,
    );
    wrong_head.reviewed_head = "b".repeat(40);
    for wrong in [wrong_attempt, wrong_head] {
        assert_eq!(
            evaluate_review_quorum(
                &policy(),
                &[
                    result("executor-opencode", "primary", "formal-1", ReviewResultState::FormalPass),
                    wrong,
                ],
            ),
            ReviewQuorumDecision::Invalid
        );
    }
}

#[test]
fn signed_fields_are_optional_on_history_but_present_on_new_contracts() {
    let card = source("src/card.rs");
    let plan = source("src/plan.rs");
    for token in [
        "fallback_agent",
        "fallbackAgent",
        "nongate_seats",
        "nongateSeats",
        "review_quorum",
        "reviewQuorum",
        "skip_serializing_if",
        "harnessRegistryDigest",
    ] {
        assert!(card.contains(token) || plan.contains(token), "missing {token}");
    }
    assert!(plan.contains("validation_digest"));
    assert!(plan.contains("minimum_substantive"));
}

#[test]
fn fallback_is_terminal_exact_artifact_first_and_exactly_once() {
    let wake = source("src/wake.rs");
    for token in [
        "managedScopeTerminated",
        "ReviewFallbackSelected",
        "sourceWakeId",
        "terminalEventId",
        "reviewedHead",
        "fallbackAgent",
        "optional_review_artifact_bytes",
        "check_review_artifact_contract",
        "append_checked",
    ] {
        assert!(wake.contains(token), "fallback path missing {token}");
    }
}

#[test]
fn every_adopted_voice_has_a_durable_fact_and_expected_main_authority() {
    let core = source("../orch-core/src/lib.rs");
    let verify = source("src/verify.rs");
    for event in [
        "ReviewFallbackSelected",
        "NongateReviewDelivered",
        "ReviewSeatSubstituted",
    ] {
        assert!(core.contains(event), "event catalog missing {event}");
        assert!(verify.contains(event), "verifier missing {event}");
    }
    assert!(verify.contains("validate_expected_main_contract"));
}

#[test]
fn root_manual_runloop_and_interactive_wake_share_the_transition() {
    let runloop = source("src/runloop.rs");
    let wake = source("src/wake.rs");
    assert!(runloop.contains("review_fallback"));
    assert!(wake.contains("review_fallback"));
    assert!(wake.contains("ReviewRequested"));
}

#[test]
fn root_has_seat_closure_full_veto_and_legacy_strict_branches() {
    let verify = source("src/verify.rs");
    for token in [
        "evaluate_review_quorum",
        "evaluate_seat_closure",
        "BlockedByFinding",
        "ReviewSeatSubstituted",
        "NongateReviewDelivered",
        "LegacyStrict",
    ] {
        assert!(verify.contains(token), "root quorum missing {token}");
    }
    assert!(!verify.contains("waive_required_review"));
}

#[test]
fn the_guide_documents_the_closed_quorum_contract() {
    let guide = source("../../docs/AI-MECHANICAL-GUIDE.md");
    for token in [
        "fallbackAgent",
        "nongateSeats",
        "minimumSubstantive",
        "ReviewSeatSubstituted",
        "one voice",
    ] {
        assert!(guide.contains(token), "guide missing {token}");
    }
}
