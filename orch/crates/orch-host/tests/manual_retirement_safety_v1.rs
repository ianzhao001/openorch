//! B329 exclusive manual lifecycle canary. No model is called: review facts are
//! explicit fixtures; git, dispatch, collect, review delivery, seal, WAL and GC are real.
mod manual_lifecycle_support;
use std::fs;

#[test]
#[ignore = "testExclusive:manual_lifecycle_wal_and_gc_have_positive_and_negative_evidence"]
fn manual_lifecycle_wal_and_gc_have_positive_and_negative_evidence() {
    manual_lifecycle_support::exercise(false, |root, worktree| {
        assert!(
            !worktree.exists(),
            "normal Recorded cleanup must reclaim the released implementation"
        );
        let replay_gc = orch_host::sites::reap_released_sites(root, "r83").unwrap();
        assert!(
            replay_gc.removed.is_empty(),
            "complete GC must replay without another deletion"
        );
        let ledger = root.join("coordination/rounds/r83/events.jsonl");
        let wal = root.join("coordination/runtime/ledger-wal/r83.jsonl");
        let original = fs::read(&ledger).unwrap();
        let wal_before = fs::read(&wal).unwrap();
        assert_eq!(
            original, wal_before,
            "seal must leave ledger and WAL identical"
        );
        let previous_end = original[..original.len() - 1]
            .iter()
            .rposition(|b| *b == b'\n')
            .unwrap()
            + 1;
        fs::write(&ledger, &original[..previous_end]).unwrap();
        let plan = orch_host::ledger::run_ledger_recover(root, "r83", false).unwrap();
        assert!(
            matches!(plan, orch_host::ledger::RecoverPlan::Append { ref lines } if lines.len()==1)
        );
        assert_eq!(
            fs::read(&ledger).unwrap(),
            original[..previous_end],
            "dry recovery must not write"
        );
        orch_host::ledger::run_ledger_recover(root, "r83", true).unwrap();
        assert_eq!(fs::read(&ledger).unwrap(), original);
        assert_eq!(fs::read(&wal).unwrap(), wal_before);
        assert!(matches!(
            orch_host::ledger::run_ledger_recover(root, "r83", true).unwrap(),
            orch_host::ledger::RecoverPlan::NothingToDo
        ));

        let end = original.iter().position(|b| *b == b'\n').unwrap();
        let mut first: orch_core::EventRecord = serde_json::from_slice(&original[..end]).unwrap();
        first.event_id = ulid::Ulid::new().to_string();
        let mut forged = serde_json::to_vec(&first).unwrap();
        forged.extend_from_slice(&original[end..]);
        fs::write(&ledger, &forged).unwrap();
        assert!(
            orch_host::ledger::run_ledger_recover(root, "r83", true).is_err(),
            "a valid but unbacked event must never be repaired into authority"
        );
        assert_eq!(
            fs::read(&ledger).unwrap(),
            forged,
            "refusal must preserve diagnostic bytes"
        );
        assert_eq!(fs::read(&wal).unwrap(), wal_before);
        fs::write(&ledger, &original).unwrap();
    });
}
