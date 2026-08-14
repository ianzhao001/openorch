//! B163 seeded-red contract (H28③/H22: WAL write-back is machinery, verbatim,
//! and never lossy).
//!
//! Negative mutations that must turn the named case red:
//! M1. Restore anything but the exact byte-identical WAL suffix (rewrite,
//!     reorder, or "helpfully" normalize lines) — the WAL is the truth source
//!     and recovery must be byte-verbatim.
//! M2. Generate a write plan for a diverged ledger — divergence means the
//!     ledger holds facts the WAL never saw; machinery has no authority to
//!     pick a side, it must refuse and name the line.
//! M3. Apply without a durable receipt, apply non-atomically, or keep writing
//!     when a second run should be a no-op.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::ledger::{recover_plan, run_ledger_recover, RecoverPlan};

static TEMP_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(name: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = TEMP_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root
        .join("target/test-tmp")
        .join(format!("ledger-recover-{name}-{}-{seq}", std::process::id()))
}

fn lines(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_truncated_ledger_is_restored_verbatim_from_the_wal() {
    // M1: the plan is exactly the missing WAL suffix, byte for byte.
    let plan = recover_plan(&lines(&["a", "b"]), &lines(&["a", "b", "c", "d"]));
    match plan {
        RecoverPlan::Append { lines: missing } => {
            assert_eq!(missing, lines(&["c", "d"]), "verbatim suffix, nothing else")
        }
        other => panic!("truncated ledger must yield an append plan, got {other:?}"),
    }
    // An empty ledger with a full WAL is the r53 worst case: restore all.
    match recover_plan(&[], &lines(&["a", "b"])) {
        RecoverPlan::Append { lines: missing } => assert_eq!(missing, lines(&["a", "b"])),
        other => panic!("empty ledger must restore the whole WAL, got {other:?}"),
    }
}

#[test]
fn a_diverged_ledger_is_never_overwritten() {
    // M2: divergence (content mismatch or ledger-only suffix) refuses, and
    // consistency plans nothing.
    assert!(matches!(
        recover_plan(&lines(&["a", "x"]), &lines(&["a", "b", "c"])),
        RecoverPlan::Refuse { at: 1 }
    ));
    assert!(matches!(
        recover_plan(&lines(&["a", "b", "c"]), &lines(&["a", "b"])),
        RecoverPlan::Refuse { at: 2 }
    ));
    assert!(matches!(
        recover_plan(&lines(&["a", "b"]), &lines(&["a", "b"])),
        RecoverPlan::NothingToDo
    ));
}

#[test]
fn recovery_applies_atomically_and_leaves_a_receipt() {
    // M3: apply restores byte-identity with the WAL, leaves one receipt line,
    // and a second apply is a no-op.
    let root = temp_root("apply");
    fs::create_dir_all(root.join("coordination/rounds/r90")).unwrap();
    fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
    let full = "{\"eventId\":\"01A\"}\n{\"eventId\":\"01B\"}\n{\"eventId\":\"01C\"}\n";
    let truncated = "{\"eventId\":\"01A\"}\n";
    fs::write(root.join("coordination/rounds/r90/events.jsonl"), truncated).unwrap();
    fs::write(root.join("coordination/runtime/ledger-wal/r90.jsonl"), full).unwrap();

    let first = run_ledger_recover(&root, "r90", true).expect("recovery applies");
    assert!(matches!(first, RecoverPlan::Append { .. }));
    let restored = fs::read_to_string(root.join("coordination/rounds/r90/events.jsonl")).unwrap();
    assert_eq!(restored, full, "ledger is byte-identical to the WAL after apply");
    let wal_after =
        fs::read_to_string(root.join("coordination/runtime/ledger-wal/r90.jsonl")).unwrap();
    assert_eq!(wal_after, full, "the WAL itself is never touched");
    let receipts =
        fs::read_to_string(root.join("coordination/runtime/ledger-wal/recovery-log.jsonl"))
            .unwrap();
    assert_eq!(receipts.trim().lines().count(), 1, "exactly one receipt line");
    let receipt: serde_json::Value = serde_json::from_str(receipts.trim()).unwrap();
    assert_eq!(receipt["round"], "r90");
    assert_eq!(receipt["appended"], 2);

    let second = run_ledger_recover(&root, "r90", true).expect("second run is a no-op");
    assert!(matches!(second, RecoverPlan::NothingToDo));
    let receipts_after =
        fs::read_to_string(root.join("coordination/runtime/ledger-wal/recovery-log.jsonl"))
            .unwrap();
    assert_eq!(receipts_after, receipts, "no-op leaves no extra receipt");
}
