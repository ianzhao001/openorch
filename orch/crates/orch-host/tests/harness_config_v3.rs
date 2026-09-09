//! B319 · local harness configuration and live discovery contract.
//!
//! Expected red: compile (`E0432`) until the v3 config snapshot API exists.
//! M1 follow a parent/leaf symlink -> `loader_rejects_symlinked_parent_and_leaf_before_read` red.
//! M2 accept relative executable/PATH lookup -> `relative_executable_is_rejected_without_path_lookup` red.
//! M3 accept raw argv/env -> `raw_argv_and_raw_env_are_rejected_independently` red.
//! M4 reread after capture -> `captured_snapshot_survives_disk_replacement_and_next_action_gets_new_digest` red.
//! M5 let one unsupported alias poison peers -> `one_unavailable_alias_does_not_poison_the_table` red.
//! M6 fall back from a selected unavailable alias -> `selected_unavailable_alias_fails_without_fallback` red.

use std::path::Path;

use orch_host::harness_config::{
    parse_harness_config_snapshot, HarnessAction, HarnessAvailability, HARNESS_CONFIG_CONTRACT_V1,
};

const CONFIG: &str = r#"
version: 1
harnesses:
  alpha:
    driver: opencode
    executable: /bin/echo
    enabled: true
    defaults: {provider: local, model: model-a, effort: high, mode: normal}
    review: {model: model-review}
    cwdPolicy: project-root
  disabled:
    driver: pi
    executable: /bin/echo
    enabled: false
    defaults: {}
    cwdPolicy: target-worktree
"#;

#[test]
fn one_snapshot_resolves_action_override_without_copying_transport_truth() {
    assert_eq!(HARNESS_CONFIG_CONTRACT_V1, 1);
    let snapshot = parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), CONFIG)
        .expect("valid local snapshot");
    let selected = snapshot
        .resolve("alpha", HarnessAction::Review)
        .expect("enabled supported alias");
    assert_eq!(selected.executable(), Path::new("/bin/echo"));
    assert_eq!(selected.model(), Some("model-review"));
    assert_eq!(selected.provider(), Some("local"));
    assert_eq!(selected.config_digest(), snapshot.sha256());
    assert!(!snapshot.source_bytes().is_empty());
}

#[test]
fn one_unavailable_alias_does_not_poison_the_table() {
    let snapshot = parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), CONFIG)
        .expect("valid local snapshot");
    let rows = snapshot.discover_without_tokens();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].alias(), "alpha");
    assert!(matches!(
        rows[0].availability(),
        HarnessAvailability::Supported
    ));
    assert_eq!(rows[1].alias(), "disabled");
    assert!(matches!(
        rows[1].availability(),
        HarnessAvailability::Unsupported(_)
    ));
    assert!(snapshot.resolve("disabled", HarnessAction::Review).is_err());
}

#[test]
fn relative_executable_is_rejected_without_path_lookup() {
    let relative = r#"
version: 1
harnesses:
  bad:
    driver: opencode
    executable: opencode
    enabled: true
    cwdPolicy: project-root
"#;
    let error = parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), relative)
        .expect_err("ambient PATH must fail closed")
        .to_string();
    assert!(error.contains("executable"));
}

#[test]
fn raw_argv_and_raw_env_are_rejected_independently() {
    for forbidden in ["argv: [\"--model\", \"x\"]", "env: {MODEL: x}"] {
        let bad = format!(
            "version: 1\nharnesses:\n  bad:\n    driver: opencode\n    executable: /bin/echo\n    enabled: true\n    {forbidden}\n    cwdPolicy: project-root\n"
        );
        let error = parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), &bad)
            .expect_err("raw argv/env must fail closed")
            .to_string();
        assert!(error.contains("argv") || error.contains("env"), "{error}");
    }
}
