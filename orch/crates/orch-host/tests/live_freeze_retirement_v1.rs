//! r84 contract. Relocate byte-for-byte; do not change until this task is Recorded.
//! Evidence also requires real production entry, full signed gates and each named negative mutation.
//! M1 restore live enforcer; M2 weaken current seed; M3 create supersession;
//! M4 alter historical bytes. Functional seed-evolution and archive replay companions are required.

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf()
}

#[test]
fn retired_live_enforcers_are_physically_absent() {
 for p in ["orch/crates/orch-host/src/srcshape.rs", "coordination/scripts/check-frozen-prefix-conflicts.sh", "coordination/scripts/check-landed-seed-writeset.sh", "coordination/scripts/check-frozen-reader-conflicts.sh"] {
  assert!(!root().join(p).exists(),"live enforcer survived: {p}");
 }
 for (p,symbol) in [("plan.rs","collect_landed_write_set_conflicts_for_plan("),("oracle.rs","validate_landed_seed_digests_exact("),("verify.rs","build_frozen_contract_superseded_events")] {
  let s=std::fs::read_to_string(root().join("orch/crates/orch-host/src").join(p)).unwrap();
  assert!(!s.contains(symbol),"live permanent enforcement still exists: {p}::{symbol}");
 }
}
