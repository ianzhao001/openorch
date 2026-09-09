//! B323 generation boundary for the retained root-verdict and seal writers.
//!
//! The former test target exercised an open schema-1/2 production writer.
//! Historical contracts are now replay-only; schema-3 positive lifecycle
//! coverage lives in `v3_selfhost_lifecycle_e2e`.

#[allow(dead_code)]
mod support_boundary;
#[allow(dead_code)]
mod support_legacy_plan;

use orch_host::verify::{self, RootVerdict};
use support_boundary::LegacyWriterFixture;

#[test]
fn legacy_root_verdict_rejects_before_ledger_wal_git_or_gate_effects() {
    let fixture = LegacyWriterFixture::new("root-verdict");
    let before = fixture.snapshot();
    let head = fixture.head();
    let error = verify::run_root_verdict(
        fixture.path(),
        "B900",
        "B900-A0001",
        &head,
        &head,
        RootVerdict::Pass,
        None,
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("schema 1/2 root verdict writer"), "{error}");
    fixture.assert_unchanged(&before);
}

#[test]
fn legacy_root_verdict_dry_run_is_also_read_only_rejected() {
    let fixture = LegacyWriterFixture::new("root-verdict-dry-run");
    let before = fixture.snapshot();
    let head = fixture.head();
    let error = verify::run_root_verdict(
        fixture.path(),
        "B901",
        "B901-A0001",
        &head,
        &head,
        RootVerdict::Fail,
        Some("legacy writer is retired"),
        true,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("schema 1/2 root verdict writer"), "{error}");
    fixture.assert_unchanged(&before);
}

#[test]
fn legacy_seal_rejects_before_merge_lifecycle_effects() {
    let fixture = LegacyWriterFixture::new("seal");
    let before = fixture.snapshot();
    let head = fixture.head();
    let error = orch_host::close::run_seal(fixture.path(), "B902", "B902-A0001", &head)
        .unwrap_err()
        .to_string();
    assert!(error.contains("schema 3"), "{error}");
    fixture.assert_unchanged(&before);
}
