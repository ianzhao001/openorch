//! Regression for r77/B294: rustc may report one authenticated upstream item
//! with a crate-stripped grouped-import identity.

use std::path::PathBuf;

use orch_host::oracle::introduced_public_symbols_probe;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repository root must be canonical")
}

#[test]
fn b293_recorded_merge_explains_the_b294_grouped_import_shape() {
    let root = repo_root();
    let merge_sha = "affd2236d6cd5b9bf286196116c7417ca23ad69f";
    let write_set = vec!["orch/crates/orch-host/src/harness.rs".to_string()];
    let symbols = introduced_public_symbols_probe(&root, merge_sha, &write_set)
        .expect("B293 is a recorded no-ff merge and must remain attributable");

    assert!(
        symbols.contains("symbol:orch_host::harness::registry_digest"),
        "the fully-qualified identity must remain present: {symbols:?}"
    );
    assert!(
        symbols.contains("symbol:harness::registry_digest"),
        "the grouped-import identity must bind to the same item: {symbols:?}"
    );
    assert!(
        !symbols.contains("symbol:registry_digest"),
        "only the crate segment may be stripped; suffix guessing is forbidden"
    );
}
