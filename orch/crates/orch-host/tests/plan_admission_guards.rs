//! B328: the former permanent landed admission is retired.
//! Retain the actual Rust argv floor and canonical archived storage audits.
//! Negative cases preserve terminator placement, actor, round and payload validation;
//! current seed / successor evolution is exercised by recorded_seed_evolution_v1.

#![allow(dead_code)]

use std::path::Path;

use orch_core::EventRecord;
use orch_host::binding::{self, Binding};
use orch_host::plan::PLAN_ADMISSION_GUARDS_V1;

const _: u32 = PLAN_ADMISSION_GUARDS_V1;

fn outer_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("orch-host manifest 必须位于 <root>/orch/crates/orch-host")
}

fn rust_binding(check: &str) -> Binding {
    serde_yaml::from_str(&format!(
        "project: {{ecosystems: [rust]}}\ncommands:\n  check:\n    argv: [{check}]\n"
    ))
    .unwrap()
}

#[test]
fn rust_check_requires_all_targets_before_the_terminator() {
    let good = rust_binding("cargo, check, --workspace, --all-targets, --locked");
    binding::validate_rust_check_all_targets(&good).unwrap();

    let missing = rust_binding("cargo, check, --workspace, --locked");
    let errors = binding::validate_rust_check_all_targets(&missing).unwrap_err();
    assert!(errors
        .iter()
        .any(|error| error.contains("check") && error.contains("--all-targets")));

    let after = rust_binding("cargo, check, --workspace, --locked, --, --all-targets");
    assert!(binding::validate_rust_check_all_targets(&after).is_err());
}

#[test]
fn non_rust_skips_the_floor_and_the_live_binding_passes() {
    let non_rust: Binding = serde_yaml::from_str(
        "project: {ecosystems: [node]}\ncommands:\n  check: {argv: [npm, test]}\n",
    )
    .unwrap();
    binding::validate_rust_check_all_targets(&non_rust).unwrap();

    let live = binding::load(outer_root()).expect("当前 PROJECT-BINDING 必须可加载");
    binding::validate_rust_check_all_targets(&live)
        .expect("当前 planner-owned check argv 必须满足 --all-targets floor");
}

fn r79_events() -> Vec<EventRecord> {
    let read =
        orch_core::read_ledger(&outer_root().join("coordination/rounds/r79/events.jsonl")).unwrap();
    assert!(read.bad_lines.is_empty());
    read.events
}

fn storage_event_mut(events: &mut [EventRecord]) -> &mut EventRecord {
    events
        .iter_mut()
        .find(|event| {
            event.kind == "EscalationRaised"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("stage"))
                    .and_then(serde_json::Value::as_str)
                    == Some("storage")
        })
        .expect("r79 必须含真实 storage audit 对")
}

#[test]
fn archived_storage_audit_pair_is_accepted_but_forgery_is_not() {
    let root = outer_root();
    let events = r79_events();
    orch_host::verify::validate_archived_record_chain(root, "r79", "B303", &events)
        .expect("r79 的 canonical refused→recovered storage pair 必须通过归档链复验");

    let mut wrong_actor = events.clone();
    storage_event_mut(&mut wrong_actor).actor = "runtime:forged".into();
    assert!(
        orch_host::verify::validate_archived_record_chain(root, "r79", "B303", &wrong_actor,)
            .is_err()
    );

    let mut wrong_round = events.clone();
    storage_event_mut(&mut wrong_round).round = Some("r80".into());
    assert!(
        orch_host::verify::validate_archived_record_chain(root, "r79", "B303", &wrong_round,)
            .is_err()
    );

    let mut missing_field = events;
    storage_event_mut(&mut missing_field)
        .payload
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("thresholdBytes");
    assert!(
        orch_host::verify::validate_archived_record_chain(root, "r79", "B303", &missing_field,)
            .is_err()
    );
}
