#[test]
fn capability_is_lexical_and_ordinary_descendants_strip_it() {
    let close = include_str!("../src/close.rs");
    assert!(close.contains("merge_lifecycle: false"));
    assert!(close.contains("ordinary effect invoked anywhere below run_merge/run_record"));
    let ledger = include_str!("../src/ledger.rs");
    assert!(ledger.contains("append_checked_merge_lifecycle"));
    assert!(ledger.contains("requires exclusive capability"));
}

#[test]
fn all_production_lifecycle_appends_use_the_restricted_entry() {
    let close = include_str!("../src/close.rs");
    assert!(close.matches("append_checked_merge_lifecycle").count() >= 3);
    assert!(!close.contains("ledger::append(root, &round, &[merge_executed"));
}

#[test]
fn lifecycle_authority_is_bound_to_current_round_pointer() {
    let ledger = include_str!("../src/ledger.rs");
    assert!(ledger.contains("if current != round"));
    assert!(ledger.contains("与 CURRENT-ROUND {current} 不一致"));
}

// B138 (O2 widening): the merge-lifecycle kinds (MergeStarted / MergeExecuted
// / TaskRecorded) are append_checked_merge_lifecycle-restricted.  These files
// may legitimately *read* those kinds in filters, but must never *write* them
// through a direct `ledger::event(` callsite that names the kind as its first
// string argument.  This catches a future regression that bypasses the
// capability gate by adding a direct lifecycle write to tierf.rs or serve.rs.
//
// B138-A0002 repair (F2/P0): the previous scan matched `ledger::event(` and
// the quoted kind on the *same source line*. rustfmt's normal form spreads a
// call across lines, so the direct multi-line write
//   ledger::event(
//       "MergeStarted",
//       ...
//   )
// was missed (reviewer-confirmed). The scan now parses the call by span: it
// strips `//` comment lines, then for every `ledger::event(` occurrence it
// inspects the *first argument* — skipping whitespace across newlines — and
// flags any whose first string literal is a lifecycle kind. This catches both
// the single-line and the canonical multi-line rustfmt form. A dedicated
// adversarial test (`multiline_direct_lifecycle_write_is_caught`) pins the
// multi-line form so the span parser cannot silently regress.
fn direct_lifecycle_kind_writes(src: &str) -> Vec<(String, usize)> {
    let lifecycle_kinds = ["MergeStarted", "MergeExecuted", "TaskRecorded"];
    // Strip full-line `//` comments so a kind name appearing only in a comment
    // is never mistaken for a real callsite. (`//` mid-line after code is rare
    // for ledger::event callsites; the first-argument parser below additionally
    // requires the call opening, so comment-only markers cannot satisfy it.)
    let stripped: String = src
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let needle = "ledger::event(";
    let mut hits = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel_offset) = stripped[search_from..].find(needle) {
        let call_start = search_from + rel_offset;
        let args_start = call_start + needle.len();
        // Walk forward from the opening paren, skipping whitespace (incl. the
        // newline + indentation rustfmt introduces), to the first non-space
        // token. A direct lifecycle write has the kind as a quoted string
        // literal as the first argument: `"MergeStarted"`. Anything else
        // (a non-kind string, an identifier, a macro, a `crate::ledger::event`
        // alias is still caught because the needle is `ledger::event(`) is not
        // flagged here.
        let rest = &stripped[args_start..];
        let mut chars = rest.char_indices();
        let mut first_arg = String::new();
        // Skip leading whitespace.
        while let Some((_, ch)) = chars.clone().next() {
            if ch.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        // If the first non-whitespace token is a `"..."` string literal, capture
        // its contents up to the closing quote.
        if let Some((_, '"')) = chars.clone().next() {
            chars.next(); // consume opening quote
            for (_, ch) in chars {
                if ch == '"' {
                    break;
                }
                first_arg.push(ch);
            }
        }
        if lifecycle_kinds.contains(&first_arg.as_str()) {
            // 1-based line number in the original source for human-readable
            // diagnostics. Count newlines up to the call site.
            let lineno = src[..call_start.min(src.len())]
                .lines()
                .count()
                .saturating_add(1);
            hits.push((first_arg, lineno));
        }
        search_from = args_start;
    }
    hits
}

#[test]
fn tierf_and_serve_have_no_direct_lifecycle_kind_writes() {
    for surface in ["tierf.rs", "serve.rs"] {
        let src = match surface {
            "tierf.rs" => include_str!("../src/tierf.rs"),
            "serve.rs" => include_str!("../src/serve.rs"),
            _ => unreachable!(),
        };
        let hits = direct_lifecycle_kind_writes(src);
        if !hits.is_empty() {
            panic!(
                "{surface} must not directly write a merge-lifecycle kind via ledger::event \
                 (use append_checked_merge_lifecycle); found: {hits:?}"
            );
        }
    }
}

// Adversarial pin (F2/P0): a canonical multi-line rustfmt `ledger::event(`
// call whose kind string sits on the *next* line must be flagged. The prior
// same-line scan let this through; this test guarantees the span parser
// crosses newlines. The fixture is a synthetic source string so no production
// file is mutated.
#[test]
fn multiline_direct_lifecycle_write_is_caught() {
    let synthetic = r#"
fn evil() {
    // this is a direct lifecycle write spread across lines (rustfmt form)
    ledger::event(
        "MergeStarted",
        "runtime:orch",
        Some("B138"),
        Some("rT"),
        serde_json::json!({}),
    );
}
"#;
    let hits = direct_lifecycle_kind_writes(synthetic);
    assert_eq!(
        hits.len(),
        1,
        "multi-line direct MergeStarted write must be caught: {hits:?}"
    );
    assert_eq!(hits[0].0, "MergeStarted");
}

// Adversarial pin (F2/P0): the single-line form must still be caught too, so
// the span parser did not over-correct by only matching multi-line calls.
#[test]
fn singleline_direct_lifecycle_write_is_caught() {
    let synthetic = r#"
fn evil() {
    ledger::event("TaskRecorded", "runtime:orch", None, Some("rT"), serde_json::json!({}));
}
"#;
    let hits = direct_lifecycle_kind_writes(synthetic);
    assert_eq!(
        hits.len(),
        1,
        "single-line direct TaskRecorded write must be caught: {hits:?}"
    );
    assert_eq!(hits[0].0, "TaskRecorded");
}

// Adversarial pin (F2/P0): a kind name appearing only in a `//` comment must
// NOT be flagged — the scan strips comment lines so review markers and prose
// mentioning the kinds stay legal.
#[test]
fn lifecycle_kind_in_comment_is_not_flagged() {
    let synthetic = r#"
// NOTE: never call ledger::event("MergeStarted", ...) directly here.
fn fine() {
    ledger::event("DispatchIssued", "runtime:orch", None, None, serde_json::json!({}));
}
"#;
    let hits = direct_lifecycle_kind_writes(synthetic);
    assert!(
        hits.is_empty(),
        "kind name in a comment must not be flagged: {hits:?}"
    );
}

// B149: the two actor-classification keys (`event` injects them into every
// minted event) must be on the canonical extra allowlist, otherwise merge
// lifecycle events stop counting as canonical and the merge barrier silently
// disarms.  Behavioral coverage lives in the ledger.rs unit test
// canonical_merge_started_tolerates_classification_keys (pure injection point,
// no global env); this lexical pin guards the allowlist declaration itself.
#[test]
fn classification_keys_are_in_canonical_extra_allowlist() {
    let ledger = include_str!("../src/ledger.rs");
    let line = ledger
        .lines()
        .find(|line| line.contains("CANONICAL_EXTRA_ALLOWLIST") && line.contains('&'))
        .expect("CANONICAL_EXTRA_ALLOWLIST declaration line");
    assert!(
        line.contains("\"initiatorKind\""),
        "allowlist 缺 initiatorKind: {line}"
    );
    assert!(
        line.contains("\"invocationMode\""),
        "allowlist 缺 invocationMode: {line}"
    );
}
