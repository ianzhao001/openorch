//! Retained manual verifier pre-spawn rejection; wave/runloop callbacks retired.
use std::fs;
use std::path::PathBuf;

#[test]
fn direct_verify_rejects_before_log_creation_or_spawn() {
    let root: PathBuf = orch_host::util::test_scratch_dir("b130-direct-verify");
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r48")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r48\n").unwrap();
    fs::write(
        root.join("coordination/rounds/r48/ROUND-IR.yaml"),
        r#"round: r48
revision: 1
policy: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff, autoMergeOnPass: true}
budgets: {}
verification: {mode: root-manual-fixed-head, adapter: root-manual}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling: {allowedAgents: [], capacities: {}}
tasks: []
"#,
    )
    .unwrap();

    let error = orch_host::verify::run_verify(&root, "B", "sentinel-do-not-spawn", 1)
        .err()
        .expect("direct verify must reject")
        .to_string();
    assert!(!error.is_empty());
    assert!(!root.join("coordination/runtime/logs").exists());
}
