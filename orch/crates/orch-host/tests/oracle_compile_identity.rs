//! B186 frozen seed — compile-red oracle identity must bind the Rust failure, not counts only.
//!
//! Relocate byte-for-byte to `orch/crates/orch-host/tests/oracle_compile_identity.rs`.
//! After relocation this file is frozen; do not edit it to make the gate green.
//!
//! M1: compare only redForm/counts/error code -> same-code/different-symbol remains falsely green.
//! M2: keep `--expected-red` write-only -> E0583 expectation accepts an E0432 observation.
//! M3: preserve diagnostic order/duplicates/ANSI/locations -> equivalent compiler output drifts.
//! M4: accept a generic `error:` line -> non-rustc build/tool failure becomes an admissible seed red.
//! M5: allow v2 -> legacy downgrade -> a task can cross the identity high-water and shed its proof.
//! M6: accept malformed v2 without identity/proof -> a new event silently regains legacy semantics.

use orch_host::oracle::{
    canonical_rust_compile_identity, compile_oracle_baseline_match,
    validate_expected_compile_red, CompileOracleMatch,
};
use serde_json::{json, Value};

const E0583_SUPPORT: &str = r#"
error[E0583]: file not found for module `support`
 --> /private/tmp/a/tests/fixture_git_fsmonitor.rs:12:1
  |
12 | mod support;
  | ^^^^^^^^^^^^
error: could not compile `orch-cli` (test "fixture_git_fsmonitor") due to 1 previous error
"#;

const E0432_FIXTURE_HELPERS: &str = r#"
error[E0432]: unresolved imports `support::configure_fixture_git_env`, `support::fixture_git_command`
 --> /private/tmp/b/tests/fixture_git_fsmonitor.rs:27:15
  |
27 | use support::{configure_fixture_git_env, fixture_git_command};
error: could not compile `orch-cli` (test "fixture_git_fsmonitor") due to 1 previous error
"#;

fn v2_payload(expected: &str, log: &str) -> Value {
    let identity = canonical_rust_compile_identity(log).expect("rust identity");
    let proof = validate_expected_compile_red(expected, &identity).expect("expected-red proof");
    json!({
        "oracleSchemaVersion": 2,
        "expectedRed": expected,
        "expectedRedProof": proof,
        "measured": {
            "fileFailed": 3,
            "filePassed": 0,
            "totalFailed": 3,
            "totalPassed": 0,
            "total": 3,
            "failedCases": ["<compile-red>"],
            "redForm": "compile",
            "compileIdentity": identity
        }
    })
}

#[test]
fn b185_e0583_and_e0432_are_different_failures() {
    let before_dependency = canonical_rust_compile_identity(E0583_SUPPORT).unwrap();
    let after_dependency = canonical_rust_compile_identity(E0432_FIXTURE_HELPERS).unwrap();

    assert_ne!(before_dependency, after_dependency);
    assert!(validate_expected_compile_red("error[E0583]", &after_dependency).is_err());
    assert!(validate_expected_compile_red("error[E0432]", &after_dependency).is_ok());

    let baseline = v2_payload("error[E0583]", E0583_SUPPORT);
    let error = compile_oracle_baseline_match(&[baseline], &after_dependency).unwrap_err();
    assert!(error.contains("E0583") || error.contains("identity"), "{error}");
}

#[test]
fn same_error_code_still_binds_the_unresolved_symbol() {
    let fixture_helpers = canonical_rust_compile_identity(E0432_FIXTURE_HELPERS).unwrap();
    let wrong_helpers = canonical_rust_compile_identity(
        "error[E0432]: unresolved imports `support::configure_fixture_git_env`, \
         `support::another_fixture_git_command`\n",
    )
    .unwrap();

    assert_ne!(fixture_helpers, wrong_helpers);
    let baseline = v2_payload("error[E0432]", E0432_FIXTURE_HELPERS);
    assert!(compile_oracle_baseline_match(&[baseline], &wrong_helpers).is_err());

    let symbol_specific =
        "error[E0432]: unresolved import `support::fixture_git_command`";
    assert!(validate_expected_compile_red(symbol_specific, &fixture_helpers).is_ok());
    let wrong_symbol = "error[E0432]: unresolved import `support::other_command`";
    assert!(validate_expected_compile_red(wrong_symbol, &fixture_helpers).is_err());
}

#[test]
fn rust_identity_ignores_order_duplicates_ansi_paths_and_line_numbers() {
    let first = canonical_rust_compile_identity(
        "\u{1b}[31merror[E0432]\u{1b}[0m: unresolved import `crate::missing_api`\n\
         --> /one/worktree/tests/seed.rs:9:7\n\
         error[E0583]: file not found for module `support`\n",
    )
    .unwrap();
    let reordered = canonical_rust_compile_identity(
        "error[E0583]:   file not found for module `support`\n\
         --> /different/worktree/tests/seed.rs:900:70\n\
         error[E0432]: unresolved import `crate::missing_api`\n\
         error[E0432]: unresolved import `crate::missing_api`\n",
    )
    .unwrap();

    assert_eq!(first, reordered);
}

#[test]
fn generic_or_non_rust_compile_failures_are_not_rust_seed_identity() {
    for log in [
        "error: failed to run custom build command for `native-lib`\n",
        "clang: error: unknown argument: '-fexample'\n",
        "error: could not compile `orch-host` due to 1 previous error\n",
    ] {
        assert!(
            canonical_rust_compile_identity(log).is_err(),
            "non-rustc coded failure was accepted: {log:?}"
        );
    }
}

#[test]
fn legacy_history_replays_but_identity_high_water_is_fail_closed() {
    let replay = canonical_rust_compile_identity(E0432_FIXTURE_HELPERS).unwrap();
    let legacy = json!({
        "expectedRed": "error[E0583]",
        "measured": {
            "fileFailed": 3,
            "filePassed": 0,
            "totalFailed": 3,
            "redForm": "compile"
        }
    });

    assert_eq!(
        compile_oracle_baseline_match(&[legacy.clone()], &replay).unwrap(),
        CompileOracleMatch::LegacyCountsOnly
    );
    let legacy_record_only = json!({
        "expectedRed": "compile-red",
        "recordOnly": true
    });
    assert_eq!(
        compile_oracle_baseline_match(&[legacy_record_only], &replay).unwrap(),
        CompileOracleMatch::NoBaseline
    );

    let modern = v2_payload("error[E0432]", E0432_FIXTURE_HELPERS);
    assert_eq!(
        compile_oracle_baseline_match(&[legacy.clone(), modern.clone()], &replay).unwrap(),
        CompileOracleMatch::IdentityV2
    );
    assert!(compile_oracle_baseline_match(&[modern, legacy], &replay).is_err());
}

#[test]
fn v2_requires_canonical_identity_and_expected_red_proof() {
    let replay = canonical_rust_compile_identity(E0432_FIXTURE_HELPERS).unwrap();
    let missing_identity = json!({
        "oracleSchemaVersion": 2,
        "expectedRed": "error[E0432]",
        "expectedRedProof": {"form": "compile", "rustCodes": ["E0432"], "rustKeys": []},
        "measured": {"redForm": "compile"}
    });
    let missing_proof = json!({
        "oracleSchemaVersion": 2,
        "expectedRed": "error[E0432]",
        "measured": {"redForm": "compile", "compileIdentity": replay.clone()}
    });

    assert!(compile_oracle_baseline_match(&[missing_identity], &replay).is_err());
    assert!(compile_oracle_baseline_match(&[missing_proof], &replay).is_err());
    assert_eq!(
        compile_oracle_baseline_match(&[], &replay).unwrap(),
        CompileOracleMatch::NoBaseline
    );
}
