//! B263 non-seed regression: every production gate spawn reaches fresh storage admission.
//!
//! Raw gate runners remain available to isolated tests, but the production prefixes of collect,
//! oracle, verify, and close must use either a fresh guard or the explicitly held collect permit.

use std::fs;
use std::path::Path;

fn source(name: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name))
        .unwrap_or_else(|error| panic!("read {name}: {error}"))
}

fn production_prefix(source: &str) -> &str {
    source.split("\n#[cfg(test)]\nmod tests").next().unwrap()
}

fn item<'a>(source: &'a str, anchor: &str) -> &'a str {
    let start = source
        .find(anchor)
        .unwrap_or_else(|| panic!("missing item anchor {anchor}"));
    let tail = &source[start..];
    let end = tail
        .find("\n}\n")
        .unwrap_or_else(|| panic!("item {anchor} lacks column-zero closing brace"));
    &tail[..end + 3]
}

fn position(source: &str, needle: &str) -> usize {
    source
        .find(needle)
        .unwrap_or_else(|| panic!("missing {needle:?}"))
}

fn count(source: &str, needle: &str) -> usize {
    source.match_indices(needle).count()
}

fn assert_refresh_pairs(source: &str, typed_calls: &[&str]) {
    let mut events = source
        .match_indices("crate::storage::refresh_gate_permit(")
        .map(|(position, _)| (position, "refresh"))
        .collect::<Vec<_>>();
    for call in typed_calls {
        events.extend(
            source
                .match_indices(call)
                .map(|(position, _)| (position, "spawn")),
        );
    }
    events.sort_unstable_by_key(|(position, _)| *position);
    assert!(
        !events.is_empty() && events.len() % 2 == 0,
        "refresh/spawn event set must be non-empty and paired: {events:?}"
    );
    for pair in events.chunks_exact(2) {
        assert_eq!(
            [pair[0].1, pair[1].1],
            ["refresh", "spawn"],
            "every typed spawn must be immediately preceded by its own refresh: {events:?}"
        );
    }
}

#[test]
fn production_modules_have_no_raw_gate_callers() {
    for name in ["collect.rs", "oracle.rs", "close.rs"] {
        let source = source(name);
        let production = production_prefix(&source);
        assert_eq!(
            count(production, "gate::run_gate("),
            0,
            "{name} production code must not bypass typed storage admission"
        );
        assert_eq!(
            count(production, "gate::run_trial_gate("),
            0,
            "{name} production code must not bypass typed trial admission"
        );
    }
    let verify = source("verify.rs");
    assert_eq!(
        count(&verify, "gate::run_gate("),
        0,
        "verify has an early test module, so scan its complete source for raw gate callers"
    );
    assert_eq!(count(&verify, "gate::run_trial_gate("), 0);

    for name in ["collect.rs", "oracle.rs", "verify.rs", "close.rs"] {
        let source = source(name);
        let production = if name == "verify.rs" {
            source.as_str()
        } else {
            production_prefix(&source)
        };
        for legacy in [
            "gate::run_gate_guarded(",
            "gate::run_gate_with_permit(",
            "gate::run_trial_gate_with_permit(",
        ] {
            assert_eq!(
                count(production, legacy),
                0,
                "{name} production code must not bypass the closed audit identity via {legacy}"
            );
        }
    }
}

#[test]
fn typed_gate_wrappers_keep_admission_in_the_type_boundary() {
    let gate = source("gate.rs");
    let guarded = item(&gate, "pub fn run_gate_with_audit_identity(");
    assert!(guarded.contains("identity: crate::ledger::GateAuditIdentity<'_>"));
    assert!(
        position(guarded, "crate::storage::guard_gate_operation")
            < position(guarded, "run_gate(name, spec"),
        "fresh admission must precede the raw gate runner"
    );

    let held = item(&gate, "pub fn run_gate_with_permit_and_identity(");
    assert!(held.contains("_permit: &crate::storage::StoragePermit"));
    assert!(held.contains("identity: crate::ledger::GateAuditIdentity<'_>"));
    assert!(
        position(held, "identity.validate()") < position(held, "run_gate(name, spec"),
        "held identity validation must precede the raw gate runner"
    );
    assert!(held.contains("run_gate(name, spec"));

    let trial = item(
        &gate,
        "pub fn run_trial_gate_with_permit_and_identity(",
    );
    assert!(trial.contains("_permit: &crate::storage::StoragePermit"));
    assert!(trial.contains("identity: crate::ledger::GateAuditIdentity<'_>"));
    assert!(
        position(trial, "identity.validate()") < position(trial, "run_trial_gate(name, spec"),
        "held trial identity validation must precede the raw gate runner"
    );
    assert!(trial.contains("run_trial_gate(name, spec"));
}

#[test]
fn collect_threads_one_permit_through_replay_trial_fallback_and_final_gate() {
    let collect = source("collect.rs");
    let production = production_prefix(&collect);
    let check = item(production, "pub fn check_and_gate(");
    assert_eq!(count(check, "crate::storage::guard_gate_operation("), 1);
    assert!(check.contains("crate::attempt::resolve_current_dispatch("));
    assert!(check.contains("ledger::GateAuditIdentity::Attempt"));
    assert!(check.contains("attempt_id: &attempt_id"));
    for call in [
        "oracle::replay_seed_red(",
        "run_trial_merge_if_needed(",
        "gate::run_gate_with_permit_and_identity(",
    ] {
        assert!(check.contains(call), "collect is missing {call}");
    }
    assert!(
        count(check, "&_storage_permit") >= 3,
        "the same outer permit must reach replay, trial sequence, and final gates"
    );

    let trial = item(production, "fn execute_trial_merge_at(");
    assert_eq!(
        count(trial, "gate::run_trial_gate_with_permit_and_identity("),
        2
    );
    assert_eq!(
        count(trial, "gate::run_gate_with_permit_and_identity("),
        1
    );
    assert!(
        count(trial, "storage_permit,") >= 3,
        "warm, uncached, and cold-fallback spawns must receive the held permit"
    );
    assert_refresh_pairs(check, &["gate::run_gate_with_permit_and_identity("]);
    assert_refresh_pairs(
        trial,
        &[
            "gate::run_gate_with_permit_and_identity(",
            "gate::run_trial_gate_with_permit_and_identity(",
        ],
    );
}

#[test]
fn fresh_and_held_production_closures_are_complete() {
    let oracle = source("oracle.rs");
    let oracle = production_prefix(&oracle);
    assert_eq!(count(oracle, "gate::run_gate_with_audit_identity("), 1);
    assert_eq!(
        count(oracle, "gate::run_gate_with_permit_and_identity("),
        1
    );
    assert_refresh_pairs(
        oracle,
        &["gate::run_gate_with_permit_and_identity("],
    );
    assert_eq!(count(oracle, "GateAuditIdentity::PreAttempt"), 2);
    assert_eq!(count(oracle, "GateAuditIdentity::Attempt"), 1);
    let replay = item(oracle, "pub fn replay_seed_red(");
    assert!(replay.contains("storage_permit: &crate::storage::StoragePermit"));
    assert!(replay.contains("run_test_gate_and_parse_observation_with_permit("));

    let verify = source("verify.rs");
    let verdict_gates = item(&verify, "fn run_verdict_gates(");
    assert_eq!(
        count(
            verdict_gates,
            "gate::run_gate_with_audit_identity("
        ),
        1,
        "root verdict is the one verify production gate loop"
    );
    assert!(verdict_gates.contains("ledger::GateAuditIdentity::Attempt"));
    assert!(verdict_gates.contains("attempt_id,"));

    let close = source("close.rs");
    assert_eq!(
        count(
            production_prefix(&close),
            "gate::run_gate_with_audit_identity("
        ),
        4,
        "postmerge, record, release, and boundary recovery must all be guarded"
    );
    assert_eq!(
        count(
            production_prefix(&close),
            "attempt_id: &authorization.attempt_id"
        ),
        8,
        "every close gate must inherit the durable root authorization attempt"
    );
}

#[test]
fn gate_audit_uses_the_durable_specialized_append_without_deferred_fallback() {
    let storage = source("storage.rs");
    let append = item(&storage, "fn append_storage_event(");
    assert_eq!(count(append, "ledger::append_storage_audit("), 1);
    assert!(append.contains("entry == GuardEntry::Gate"));
    let audit = item(&storage, "fn audit_admission<");
    assert!(audit.contains("gate_identity.is_some() || refusal_active"));
    assert!(audit.contains("gate_identity.is_some() || !refusal_active"));
    for forbidden in [
        "storageAuditDeferredByMergeBarrier",
        "storageAuditError=",
        "deferred",
    ] {
        assert!(
            !append.contains(forbidden),
            "gate audit must remain durable instead of degrading to {forbidden}"
        );
    }
    assert!(!production_prefix(&storage).contains("storageAuditDeferredByMergeBarrier"));
    assert!(!production_prefix(&storage).contains("storageAuditError="));
}
