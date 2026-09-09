//! B329 replaces only the retired daemon station with one actual manual canary.
//! The original four process/signal/GC stations keep their exact runner arguments.
const RUNNER: &str = include_str!("../../../scripts/test-exclusive.sh");
const CANARY: &str = include_str!("manual_retirement_safety_v1.rs");

#[test]
fn exclusive_lane_keeps_four_original_stations_and_one_bound_manual_canary() {
    let calls = RUNNER.lines().filter(|line| line.starts_with("run_exact ")).collect::<Vec<_>>();
    assert_eq!(calls, [
        "run_exact orch-host --lib _ gate::tests::registry_reap_executes_on_the_normal_gate_exit_at_runtime",
        "run_exact orch-host --lib _ gate::tests::registry_reap_executes_on_the_timeout_gate_exit_at_runtime",
        "run_exact orch-cli --test wake_signal_isolation_cli four_signal_topology_cells_pass_ten_consecutive_runs",
        "run_exact orch-host --test gate_reap_convergence successful_kill_converges_under_pressure",
        "run_exact orch-host --test manual_retirement_safety_v1 manual_lifecycle_wal_and_gc_have_positive_and_negative_evidence",
    ]);
    let marker = CANARY.find("#[ignore = ").expect("manual canary must stay exclusive");
    let function = CANARY.find("fn manual_lifecycle_wal_and_gc_have_positive_and_negative_evidence")
        .expect("manual canary function must exist");
    assert_eq!(CANARY.matches("#[ignore").count(), 1);
    assert_eq!(CANARY.matches("fn manual_lifecycle_wal_and_gc_have_positive_and_negative_evidence").count(), 1);
    assert!(marker < function);
    assert!(!CANARY[marker..function].contains("fn "), "ignore must bind this function");
    assert!(RUNNER.contains("--list --ignored --exact"));
    assert!(RUNNER.contains("--ignored --exact --test-threads=1"));
}
