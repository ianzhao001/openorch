//! B321 · generic review identity and terminal-completeness contract.
//!
//! Expected red: compile (`E0432`) until the production live-path contract is
//! exposed. This seed deliberately contains no voter names or vote threshold.
//! M1 key by `(task, role)` latest-wins -> `different_harnesses_remain_distinct_when_other_identity_parts_match` red.
//! M2 omit/wrong/duplicate `wakeId` -> `wake_id_is_part_of_the_exact_identity` or duplicate companion red.
//! M3 accept wrong HEAD/cwd/request/config/attachment digest -> `review_request_binds_attachment_manifest_with_other_fixed_facts` red.
//! M4 follow symlink/hardlink or wrong artifact bytes -> `review_artifact_is_regular_single_link_no_symlink_and_byte_bound` red.
//! M5 accept an initiated request with no trusted terminal/artifact -> `an_initiated_review_cannot_disappear` companion red.
//! M6 count PASS/apply veto/provider roster in Rust -> `terminal_completeness_is_policy_free` red.

use orch_host::generic_review::{
    generic_review_live_slot_key_v1, validate_generic_review_live_terminals_v1,
    GenericReviewLiveBindingV1, GenericReviewLiveIdentityV1, GenericReviewLiveTerminalV1,
    GENERIC_REVIEW_LIVE_CONTRACT_V1,
};

#[test]
fn different_harnesses_remain_distinct_when_other_identity_parts_match() {
    assert_eq!(GENERIC_REVIEW_LIVE_CONTRACT_V1, 1);
    let first = GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "alpha", "wake-1")
        .expect("first identity");
    let second = GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "beta", "wake-1")
        .expect("second identity");
    assert_ne!(
        generic_review_live_slot_key_v1(&first),
        generic_review_live_slot_key_v1(&second)
    );
    assert_eq!(first.role(), "review");
    assert_eq!(second.role(), "review");
}

#[test]
fn wake_id_is_part_of_the_exact_identity() {
    let first = GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "alpha", "wake-1")
        .expect("first identity");
    let second = GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "alpha", "wake-2")
        .expect("second identity");
    assert_ne!(first, second);
}

const HEAD: &str = "0123456789012345678901234567890123456789";
const REQUEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CONFIG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ATTACHMENTS: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

#[test]
fn review_request_binds_attachment_manifest_with_other_fixed_facts() {
    let identity = GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "alpha", "wake-1")
        .expect("identity");
    let binding = GenericReviewLiveBindingV1::new(
        identity,
        HEAD,
        "/repo/.worktrees/B321-review-alpha",
        REQUEST,
        CONFIG,
        ATTACHMENTS,
    )
    .expect("binding");
    assert!(binding.matches(
        HEAD,
        "/repo/.worktrees/B321-review-alpha",
        REQUEST,
        CONFIG,
        ATTACHMENTS,
    ));
    assert!(!binding.matches(
        HEAD,
        "/repo/.worktrees/B321-review-alpha",
        REQUEST,
        CONFIG,
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
    ));
}

#[test]
fn terminal_completeness_is_policy_free() {
    let first = GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "alpha", "wake-1")
        .expect("first identity");
    let second = GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "beta", "wake-2")
        .expect("second identity");
    validate_generic_review_live_terminals_v1(
        &[first.clone(), second.clone()],
        &[
            GenericReviewLiveTerminalV1::Answered(first),
            GenericReviewLiveTerminalV1::Failed(second),
        ],
    )
    .expect("truth validation must not count PASS votes or apply a veto policy");
}

#[test]
fn an_initiated_review_cannot_disappear() {
    let request = GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "alpha", "wake-1")
        .expect("identity");
    assert!(validate_generic_review_live_terminals_v1(&[request], &[]).is_err());
}
