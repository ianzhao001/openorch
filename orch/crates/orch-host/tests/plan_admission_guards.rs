//! B308 · plan admission guards immutable contract
//!
//! 首红：compile，必须包含缺少 `plan::PLAN_ADMISSION_GUARDS_V1` 的 `error[E0432]`；
//! 同时缺少本卡要交付的两个 production helper：
//! `oracle::collect_landed_write_set_conflicts` 与
//! `binding::validate_rust_check_all_targets`（`error[E0425]`）。
//! 三个缺符号全部由 B308 同卡交付、零上游依赖；合法 rustc code 集合 = E0432 + E0425。
//!
//! M1 恢复 landed first-bail → `r79_two...` 红。
//! M2 不匹配 broad glob → `broad_glob...` 红。
//! M3 任意 supersession 都豁免 → `only_exact...` 红。
//! M4 readonly 针对移动 main 重算 → `authorization_only...` 红。
//! M5 plan 捕获后再次读 main ref → exact-OID requiredEvidence/生产 fixture 红。
//! M6 接受 `--` 后的 all-targets → `all_targets_after...` 红。
//! M7 非 Rust 也强制 → `non_rust...` 红。
//! M8 archived storage 不验 actor/round/payload → 三条负控至少一条红。
//!
//! 本文件永久冻结；补充测试放本卡其它 writeSet 落点。

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

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

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git 启动失败");
    assert!(
        out.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn landed_conflicts(write_set: &[String], superseded: &[&str]) -> Vec<String> {
    let root = outer_root();
    let main = git(root, &["rev-parse", "refs/heads/main^{commit}"]);
    let superseded = superseded
        .iter()
        .map(|value| (*value).to_string())
        .collect::<BTreeSet<_>>();
    orch_host::oracle::collect_landed_write_set_conflicts(root, &main, write_set, &superseded)
        .unwrap()
}

#[test]
fn r79_two_landed_targets_are_reported_together_and_sorted() {
    let first = "orch/crates/orch-core/tests/review_event_projection.rs";
    let second = "orch/crates/orch-host/tests/plan_compile.rs";
    let conflicts = landed_conflicts(&[second.into(), first.into()], &[]);
    assert!(conflicts.iter().any(|target| target == first));
    assert!(conflicts.iter().any(|target| target == second));
    let mut sorted = conflicts.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(conflicts, sorted, "冲突必须严格排序、去重、一次报全");
}

#[test]
fn broad_glob_intersects_every_landed_target_under_the_prefix() {
    let prefix = "orch/crates/orch-host/tests/**".to_string();
    let conflicts = landed_conflicts(&[prefix], &[]);
    assert!(conflicts
        .iter()
        .any(|target| target == "orch/crates/orch-host/tests/plan_compile.rs"));
    assert!(
        conflicts.len() > 1,
        "broad glob 必须展开出不止一个 landed target"
    );
    assert!(conflicts.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn only_an_exact_supersession_target_is_exempted() {
    let first = "orch/crates/orch-core/tests/review_event_projection.rs";
    let second = "orch/crates/orch-host/tests/plan_compile.rs";
    let conflicts = landed_conflicts(&[first.into(), second.into()], &[first]);
    assert!(!conflicts.iter().any(|target| target == first));
    assert!(conflicts.iter().any(|target| target == second));

    let parent_is_not_exact =
        landed_conflicts(&[first.into()], &["orch/crates/orch-core/tests/**"]);
    assert_eq!(parent_is_not_exact, vec![first.to_string()]);
}

#[test]
fn landed_guard_is_authorization_only_not_readonly_replay() {
    let source = include_str!("../src/plan.rs");
    let production = source.split_once("fn run_plan_locked(").unwrap().1;
    assert!(production.contains("collect_landed_write_set_conflicts"));
    let readonly = source
        .split_once("fn compare_candidate_with_persisted_for_dispatch(")
        .unwrap()
        .1
        .split_once("pub fn load_round_ir(")
        .unwrap()
        .0;
    assert!(
        !readonly.contains("collect_landed_write_set_conflicts"),
        "readonly replay 不得针对移动后的 main 重新分类已签卡面"
    );
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
