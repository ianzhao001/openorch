//! Maintenance freeze for the legacy run/step and run-wave entry points.
//! New policy belongs in `serve`; changing either whitelist is an explicit
//! compatibility decision, not an incidental feature edit.

use std::collections::BTreeSet;

fn crate_paths(source: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut rest = source;
    while let Some(offset) = rest.find("crate::") {
        rest = &rest[offset..];
        let end = rest
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == ':'))
            .unwrap_or(rest.len());
        if &rest[..end] != "crate::" {
            paths.insert(rest[..end].to_string());
        }
        rest = &rest[end..];
    }
    paths
}

#[test]
fn run_and_step_keep_the_frozen_decision_surface() {
    let source = include_str!("../src/runloop.rs");
    assert!(source.contains("Maintenance-only entry point"));
    assert_eq!(
        crate_paths(source),
        BTreeSet::from([
            "crate::current_round".to_string(),
            "crate::plan::require_active_round_ir".to_string(),
            "crate::verify::validate_root_merge_authorization".to_string(),
        ])
    );
    for decision in [
        "next_actions_for_mode(",
        "infer_inflight_with_ttl(",
        "throttle_expired(",
        "verify::run_verify(",
        "close::run_merge(",
    ] {
        assert!(
            source.contains(decision),
            "legacy run/step decision disappeared: {decision}"
        );
    }
    for new_policy_module in [
        "crate::scheduler::",
        "crate::wave::",
        "crate::tierf::",
        "crate::budget::",
        "crate::collect::",
        "crate::serve::",
        "crate::liveness::",
    ] {
        assert!(
            !source.contains(new_policy_module),
            "new policy call leaked into maintenance-only run/step: {new_policy_module}"
        );
    }
}

#[test]
fn run_wave_keeps_the_frozen_runtime_surface() {
    let source = include_str!("../src/wave.rs");
    assert_eq!(
        crate_paths(source),
        BTreeSet::from([
            "crate::close::run_merge".to_string(),
            "crate::current_round".to_string(),
            "crate::ledger::append".to_string(),
            "crate::ledger::append_checked".to_string(),
            "crate::ledger::event".to_string(),
            "crate::liveness::LivenessOpts".to_string(),
            "crate::liveness::LivenessOpts::default".to_string(),
            "crate::plan::IrLiveness".to_string(),
            "crate::plan::require_active_round_ir".to_string(),
            "crate::preset::plan_waves".to_string(),
            // B147（依赖 DAG 阻塞判定）与 B150（本冻结门）同轮合并的语义并集：
            // write_sets_overlap_glob 是已审已 verdict 的 run-wave 判定面合法扩展。
            "crate::preset::write_sets_overlap_glob".to_string(),
            "crate::runloop::INFLIGHT_TTL_SECS".to_string(),
            "crate::runloop::infer_inflight_with_ttl".to_string(),
            "crate::tierf::AwaitOutcome".to_string(),
            "crate::tierf::run_await".to_string(),
            "crate::tierf::run_dispatch".to_string(),
            "crate::verify::run_verify".to_string(),
            "crate::verify::validate_root_merge_authorization".to_string(),
        ])
    );
}
