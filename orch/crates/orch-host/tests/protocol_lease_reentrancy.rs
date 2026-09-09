use std::fs;
use std::sync::{Arc, Barrier};

use anyhow::bail;
use orch_host::close::{with_protocol_effect, with_protocol_transition};

fn root(tag: &str) -> std::path::PathBuf {
    let root = orch_host::util::test_scratch_dir(tag);
    fs::create_dir_all(root.join("coordination/runtime/locks")).unwrap();
    root
}

#[test]
fn same_root_nesting_err_and_panic_restore_tls_state() {
    let root = root("b130-lease-nesting");
    with_protocol_transition(&root, "outer", || {
        with_protocol_transition(&root, "inner", || Ok(()))?;
        with_protocol_effect(&root, "nested ledger effect", || Ok(()))
    })
    .unwrap();

    let error = with_protocol_effect(&root, "shared", || {
        with_protocol_transition(&root, "upgrade", || Ok(()))
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("shared→exclusive"));

    assert!(with_protocol_effect::<()>(&root, "err", || bail!("injected")).is_err());
    with_protocol_transition(&root, "after err", || Ok(())).unwrap();

    let unwind = std::panic::catch_unwind(|| {
        let _ = with_protocol_effect::<()>(&root, "panic", || panic!("injected panic"));
    });
    assert!(unwind.is_err());
    with_protocol_transition(&root, "after panic", || Ok(())).unwrap();
}

#[test]
fn cross_root_and_cross_thread_conflicts_fail_fast() {
    let first = root("b130-lease-first");
    let second = root("b130-lease-second");
    let cross = with_protocol_effect(&first, "outer shared", || {
        with_protocol_effect(&second, "cross root", || Ok(()))
    })
    .unwrap_err();
    assert!(format!("{cross:#}").contains("cross-root"));

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let child_root = first.clone();
    let child_entered = entered.clone();
    let child_release = release.clone();
    let child = std::thread::spawn(move || {
        with_protocol_effect(&child_root, "held shared", || {
            child_entered.wait();
            child_release.wait();
            Ok(())
        })
        .unwrap();
    });
    entered.wait();
    let busy = with_protocol_transition(&first, "concurrent exclusive", || Ok(())).unwrap_err();
    assert!(format!("{busy:#}").contains("lease busy"));
    release.wait();
    child.join().unwrap();
}
