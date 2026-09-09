//! B190 seeded-red contract — an exact expected supervisor argv may contain a
//! reviewer name, and a missing fixture prerequisite must remain the primary
//! diagnostic when a test hook was never reached.
//!
//! Fixed incident anchors:
//! - B177 reviewed HEAD: 40dac99d33adee578a6afa2eafb745dc35bf0888
//! - implementation HEAD: 40d19ab917ccea75d3fd350ded2eed42613c7d7c
//! - main.rs sha256: 3867c01b942a78a228bdb1e01fd57be423dce07120f8462c6468cfffd845df9d
//! - wake.rs sha256: 9a66de1715e72a5061fddacee191091f8f17c3ddf10975af85fab2b1102d0450
//!
//! Required negative mutations:
//! M1. Search the raw command line for the provider basename:
//!     reviewer_named_paths_are_not_leaks turns red.
//! M2. Accept a prefix/substring match instead of the one exact expected
//!     command: any_extra_provider_value_is_a_leak turns red.
//! M3. Accept a command line without the exact hidden entry token:
//!     the_hidden_entry_token_is_required turns red.
//! M4. Replace a missing hook capture with the old generic panic and discard
//!     the spawn error: a_prerequisite_error_remains_the_root_diagnostic turns
//!     red.
//! M5. Treat the expected forged-protocol Err as fatal even after the hook has
//!     captured cleanup receipts: a_capture_wins_over_the_expected_rejection
//!     turns red.

use orch_host::wake::{
    require_test_hook_capture_or_explain, validate_hidden_supervisor_command_line_for_test,
};

const REVIEW_SITE: &str = "/repo/.worktrees/B177-secondary-executor-opencode";
const ORCH_BIN: &str = "/repo/.worktrees/B177-secondary-executor-opencode/orch/target/debug/orch";
const SCRATCH_ROOT: &str =
    "/repo/.worktrees/B177-secondary-executor-opencode/orch/target/test-tmp/b177-hidden";
const SECRET: &str = "prompt-must-never-appear-in-process-argv";

fn clean_command() -> String {
    format!("{ORCH_BIN} --root {SCRATCH_ROOT} __wake-supervise")
}

#[test]
fn reviewer_named_paths_are_not_leaks() {
    assert!(REVIEW_SITE.contains("opencode"));
    validate_hidden_supervisor_command_line_for_test(&clean_command(), ORCH_BIN, SCRATCH_ROOT)
        .expect("the exact expected command may contain a reviewer name in its paths");
    validate_hidden_supervisor_command_line_for_test(
        &format!("  {}\n", clean_command()),
        ORCH_BIN,
        SCRATCH_ROOT,
    )
    .expect("ps framing whitespace is not an argv field");
}

#[test]
fn any_extra_provider_value_is_a_leak() {
    let provider_program = format!("{SCRATCH_ROOT}/bin/opencode");
    for leaked in [
        format!("{} {provider_program}", clean_command()),
        format!("{} --skip {SECRET}", clean_command()),
        format!("{} --exact provider-case", clean_command()),
        format!(
            "{} wake::tests::b177_wake_fixture_stubborn_provider_child",
            clean_command()
        ),
    ] {
        let error =
            validate_hidden_supervisor_command_line_for_test(&leaked, ORCH_BIN, SCRATCH_ROOT)
                .expect_err("anything beyond the exact hidden argv is a leak");
        assert!(
            error.contains("exact") || error.contains("unexpected"),
            "diagnostic must name the exact-command mismatch: {error}"
        );
    }
}

#[test]
fn the_hidden_entry_token_is_required() {
    let command = format!("{ORCH_BIN} --root {SCRATCH_ROOT} status");
    let error = validate_hidden_supervisor_command_line_for_test(&command, ORCH_BIN, SCRATCH_ROOT)
        .expect_err("an unrelated orch command is not the hidden supervisor");
    assert!(error.contains("__wake-supervise"), "error={error}");

    assert!(
        validate_hidden_supervisor_command_line_for_test(&clean_command(), "", SCRATCH_ROOT)
            .is_err()
    );
    assert!(
        validate_hidden_supervisor_command_line_for_test(&clean_command(), ORCH_BIN, "").is_err()
    );
    assert!(validate_hidden_supervisor_command_line_for_test("", ORCH_BIN, SCRATCH_ROOT).is_err());
    assert!(validate_hidden_supervisor_command_line_for_test(
        &clean_command(),
        "/wrong/orch",
        SCRATCH_ROOT,
    )
    .is_err());
    assert!(validate_hidden_supervisor_command_line_for_test(
        &clean_command(),
        ORCH_BIN,
        "/wrong/root",
    )
    .is_err());
    for ambiguous in [" bad", "bad\npath", "bad\rpath", "bad\0path"] {
        assert!(validate_hidden_supervisor_command_line_for_test(
            &clean_command(),
            ORCH_BIN,
            ambiguous,
        )
        .is_err());
    }
}

#[test]
fn a_prerequisite_error_remains_the_root_diagnostic() {
    let prerequisite =
        "stat test orch binary failed: /isolated/target/debug/orch: No such file or directory";
    let error = require_test_hook_capture_or_explain::<u32>(
        "B177 forged-ACK hook",
        None,
        Some(prerequisite),
    )
    .expect_err("a hook cannot produce receipts before its spawn prerequisite");
    assert!(error.contains("B177 forged-ACK hook"), "error={error}");
    assert!(error.contains(prerequisite), "error={error}");
    assert!(
        !error.eq("forged-ACK hook did not capture immutable process receipts"),
        "the old secondary panic must not replace the causal error"
    );

    let invariant = require_test_hook_capture_or_explain::<u32>("B177 forged-ACK hook", None, None)
        .expect_err("success without the armed hook is a distinct invariant failure");
    assert!(invariant.contains("without"), "error={invariant}");
    assert!(invariant.contains("capture"), "error={invariant}");
}

#[test]
fn a_capture_wins_over_the_expected_rejection() {
    let receipt = require_test_hook_capture_or_explain(
        "B177 forged-ACK hook",
        Some(177_u32),
        Some("wake supervisor ack identity mismatch"),
    )
    .expect("the forged rejection is expected after immutable cleanup receipts were captured");
    assert_eq!(receipt, 177);
}
