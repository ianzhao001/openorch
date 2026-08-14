//! B144 seeded-red contract.
//!
//! Negative mutations that must turn the named case red:
//! M1. Treat a REPORT with neither implementationHead nor headSha as valid.
//! M2. Silently pick one side when both fields are present but disagree.
//! M3. Drop legacy headSha compatibility so every historical REPORT breaks.

use orch_host::attempt::report_head_fields;

#[test]
fn implementation_head_is_the_canonical_field() {
    let heads =
        report_head_fields("implementationHead: aaaa1111\n").expect("canonical field parses");
    assert_eq!(heads.implementation_head, "aaaa1111");
    // M3: legacy headSha still resolves as the implementation head.
    let legacy = report_head_fields("headSha: bbbb2222\n").expect("legacy field parses");
    assert_eq!(legacy.implementation_head, "bbbb2222");
}

#[test]
fn agreeing_duplicates_are_fine_disagreeing_ones_refuse() {
    let same = report_head_fields("implementationHead: cccc3333\nheadSha: cccc3333\n")
        .expect("agreeing fields parse");
    assert_eq!(same.implementation_head, "cccc3333");
    // M2: a disagreement is a contradiction, not a choice.
    assert!(report_head_fields("implementationHead: dddd4444\nheadSha: eeee5555\n").is_err());
}

#[test]
fn missing_both_fields_refuses() {
    // M1: a REPORT that pins no implementation head is not collectible.
    assert!(report_head_fields("wroteAt: 2026-07-27T00:00:00Z\n").is_err());
    assert!(report_head_fields("").is_err());
}
