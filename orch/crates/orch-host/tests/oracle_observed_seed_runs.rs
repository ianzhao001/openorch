//! B339 / D65: actual Cargo outcomes, never static tests-minus-failures.
//! Production entry: oracle::observed_cargo_seed_counts, called by the real oracle.
//! First red: missing CargoSeedRunSpec / observed_cargo_seed_counts imports.
//! Mutations, each must be detected independently after green:
//! M1 infer missing cases as passed; M2 ignore target/binary ownership;
//! M3 accept partial runs; M4 deduplicate conflicting/repeated outcomes;
//! M5 treat ignored as passed; M6 accept truncated/inconsistent summaries;
//! M7 weaken E12; M8 remove public API documentation.
use orch_host::oracle::{
    observed_cargo_seed_counts, report_claim_ok, CargoSeedRunSpec, Measured, SuiteCounts,
};

fn spec(names: &[&str]) -> CargoSeedRunSpec {
    CargoSeedRunSpec {
        target: "orch/crates/orch-host/tests/seed.rs".to_string(),
        test_names: names.iter().map(|name| (*name).to_string()).collect(),
    }
}

fn block(target: &str, binary: &str, lines: &str, passed: usize, failed: usize, ignored: usize) -> String {
    format!(
        "     Running tests/{target}.rs (orch/target/debug/deps/{binary}-0123456789abcdef)\n\nrunning {} tests\n{lines}\ntest result: {}. {passed} passed; {failed} failed; {ignored} ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n",
        passed + failed + ignored,
        if failed == 0 { "ok" } else { "FAILED" },
    )
}

#[test]
fn real_mixed_outcomes_are_counted_only_in_the_seed_target() {
    let foreign = block("foreign", "foreign", "test alpha ... ok", 1, 0, 0);
    let own = block("seed", "seed", "test alpha ... ok\ntest beta ... FAILED", 1, 1, 0);
    let counts = observed_cargo_seed_counts(&(foreign + &own), &[spec(&["alpha", "beta"])]).unwrap();
    assert_eq!((counts.passed, counts.failed, counts.total), (1, 1, 2));
    let green = block("seed", "seed", "test alpha ... ok\ntest beta ... ok", 2, 0, 0);
    let counts = observed_cargo_seed_counts(&green, &[spec(&["alpha", "beta"])]).unwrap();
    assert_eq!((counts.passed, counts.failed, counts.total), (2, 0, 2));
}

#[test]
fn preceding_failure_cannot_invent_seed_passes() {
    let log = block("before", "before", "test unrelated ... FAILED", 0, 1, 0);
    let error = observed_cargo_seed_counts(&log, &[spec(&["alpha", "beta"])]).unwrap_err();
    assert!(error.contains("seed-not-observed"), "{error}");
    assert!(error.contains("seed.rs"), "{error}");
}

#[test]
fn partial_ignored_and_filtered_seed_runs_are_unobserved() {
    let partial = block("seed", "seed", "test alpha ... ok", 1, 0, 0);
    let ignored = block("seed", "seed", "test alpha ... ok\ntest beta ... ignored", 1, 0, 1);
    let filtered = block("seed", "seed", "", 0, 0, 0);
    for log in [partial, ignored, filtered] {
        let error = observed_cargo_seed_counts(&log, &[spec(&["alpha", "beta"])]).unwrap_err();
        assert!(error.contains("seed-not-observed"), "{error}");
    }
}

#[test]
fn foreign_binary_and_duplicate_results_cannot_supply_identity() {
    let foreign = block("foreign", "foreign", "test alpha ... ok", 1, 0, 0);
    let wrong_binary = block("seed", "foreign", "test alpha ... ok", 1, 0, 0);
    let duplicate = block("seed", "seed", "test alpha ... ok\ntest alpha ... FAILED", 1, 1, 0);
    let duplicate_blocks = block("seed", "seed", "test alpha ... ok", 1, 0, 0)
        + &block("seed", "seed", "test alpha ... ok", 1, 0, 0);
    for log in [foreign, wrong_binary, duplicate, duplicate_blocks] {
        assert!(observed_cargo_seed_counts(&log, &[spec(&["alpha"])]).is_err());
    }
}

#[test]
fn truncated_unbounded_and_inconsistent_logs_are_rejected() {
    let complete = block("seed", "seed", "test alpha ... ok", 1, 0, 0);
    let truncated = complete.split("test result:").next().unwrap().to_string();
    let unbounded = "test alpha ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored\n".to_string();
    let inconsistent = block("seed", "seed", "test alpha ... ok", 2, 0, 0);
    for log in [truncated, unbounded, inconsistent] {
        assert!(observed_cargo_seed_counts(&log, &[spec(&["alpha"])]).is_err());
    }
}

#[test]
fn measured_history_and_e12_remain_compatible_and_strict() {
    let old = r#"{"fileFailed":1,"filePassed":1,"totalFailed":1,"totalPassed":9,"total":10,"failedCases":["beta"],"redForm":"assertion"}"#;
    let measured: Measured = serde_json::from_str(old).unwrap();
    assert!(report_claim_ok(&SuiteCounts { failed: 1, passed: 1, total: 2 }, &measured).is_ok());
    assert!(report_claim_ok(&SuiteCounts { failed: 1, passed: 2, total: 3 }, &measured).is_err());
    let value = serde_json::to_value(&measured).unwrap();
    assert!(value.get("fileNotObserved").is_none());
    assert_eq!(value.get("filePassed").and_then(serde_json::Value::as_u64), Some(1));
    let mut compile = measured;
    compile.red_form = Some("compile".into());
    assert!(report_claim_ok(&SuiteCounts { failed: 0, passed: 0, total: 0 }, &compile).is_ok());
}

#[test]
fn public_observation_api_has_specific_documentation() {
    let source = include_str!("../src/oracle.rs");
    for marker in ["pub struct CargoSeedRunSpec", "pub fn observed_cargo_seed_counts"] {
        let before = source.split(marker).next().unwrap();
        assert_ne!(before, source, "missing production API {marker}");
        let docs = before.lines().rev()
            .skip_while(|line| line.trim().is_empty() || line.trim().starts_with("#["))
            .take_while(|line| line.trim().starts_with("///"))
            .collect::<Vec<_>>();
        assert!(!docs.is_empty(), "public API lacks attached /// docs: {marker}");
        assert!(docs.iter().any(|line| line.contains("Cargo") || line.contains("seed")), "API docs lack subject: {marker}");
    }
}
