//! B264 supplemental guard: every production rejection caller must select a
//! typed disposition/outcome. The frozen B113 integer API remains available
//! only as a compatibility surface for its byte-frozen integration tests.

use std::fs;

fn compact(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[test]
fn all_production_rejection_callers_are_typed() {
    let source_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut paths = fs::read_dir(&source_dir)
        .expect("read orch-host/src")
        .map(|entry| entry.expect("read source entry").path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("rs"))
        .filter(|path| path.file_name().and_then(|value| value.to_str()) != Some("failure.rs"))
        .collect::<Vec<_>>();
    paths.sort();
    assert!(
        paths.len() > 20,
        "source scan unexpectedly narrow: {paths:?}"
    );

    let legacy_calls = [
        "reject_action(",
        "reject_action_with_events(",
        "reject_action_from_ledger(",
        "reject_attempt_action_from_ledger(",
        "rejection_with_current_attempt(",
        "ActionRejection::new(",
    ];
    let typed_calls = [
        "reject_action_disposition(",
        "reject_action_command_outcome(",
        "reject_action_from_ledger_disposition(",
        "reject_action_from_ledger_command_outcome(",
        "reject_attempt_action_from_ledger_disposition(",
        "rejection_with_current_attempt_disposition(",
    ];

    let mut typed_count = 0;
    let mut typed_direct_constructors = 0;
    for path in paths {
        let source = fs::read_to_string(&path).expect("read Rust source");
        let compact = compact(&source);
        for legacy in legacy_calls {
            assert!(
                !compact.contains(legacy),
                "{} still calls legacy integer rejection API {legacy}",
                path.display()
            );
        }
        typed_count += typed_calls
            .iter()
            .map(|needle| compact.matches(needle).count())
            .sum::<usize>();
        typed_direct_constructors += compact
            .matches("ActionRejection::from_command_outcome(")
            .count();
    }

    assert_eq!(
        typed_count, 23,
        "the signed production caller closure must remain exactly 23 typed helper calls"
    );
    assert_eq!(
        typed_direct_constructors, 1,
        "the checked-ledger backend receipt seam must also avoid a bare integer"
    );
}

#[test]
fn typed_apis_do_not_accept_integer_laundering() {
    let failure = compact(include_str!("../src/failure.rs"));
    for forbidden in [
        "implFrom<i32>forCliDisposition",
        "implTryFrom<i32>forCliDisposition",
        "implInto<CliDisposition>",
        "implFrom<i32>forCommandOutcome",
        "implTryFrom<i32>forCommandOutcome",
        "implInto<CommandOutcome>",
    ] {
        assert!(
            !failure.contains(forbidden),
            "typed rejection API launders integers through {forbidden}"
        );
    }
}
