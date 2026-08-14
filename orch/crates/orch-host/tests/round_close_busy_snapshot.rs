use std::fs;
use std::sync::{Arc, Barrier};

#[test]
fn busy_round_close_changes_neither_snapshot_nor_ledger() {
    let root = orch_host::util::test_scratch_dir("b130-close-busy");
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/rT")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rT\n").unwrap();
    let snapshot = root.join("coordination/rounds/rT/SNAPSHOT.json");
    let ledger = root.join("coordination/rounds/rT/events.jsonl");
    fs::write(&snapshot, b"sentinel snapshot\n").unwrap();
    fs::write(&ledger, b"").unwrap();
    let before_snapshot = fs::read(&snapshot).unwrap();
    let before_ledger = fs::read(&ledger).unwrap();

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let child_root = root.clone();
    let child_entered = entered.clone();
    let child_release = release.clone();
    let child = std::thread::spawn(move || {
        orch_host::close::with_protocol_effect(&child_root, "held effect", || {
            child_entered.wait();
            child_release.wait();
            Ok(())
        })
        .unwrap();
    });
    entered.wait();
    let error = match orch_host::round::run_close(&root, false, None) {
        Ok(_) => panic!("busy round close unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("lease busy"));
    assert_eq!(fs::read(&snapshot).unwrap(), before_snapshot);
    assert_eq!(fs::read(&ledger).unwrap(), before_ledger);
    release.wait();
    child.join().unwrap();
}
