//! B292 · 冻结契约：非门席位必须留下与 durable runtime 事实绑定的四态收据；
//! 缺席由真实 `orch verdict` / `orch seal` 点名报警，但永远不改变动作结果。
//!
//! 来源：H187。r75/B285-A0001 审查期，planner 宣布已发起 agy/dsh，实际两席都没发；
//! 用户追问后才发现。收据若只证明“文件存在”，仍可在根本没启动模型时伪造，因此本契约
//! 同时要求 matching `WakeIssued` / `WorkspaceLeased` durable 事实。
//!
//! ## r76 计划审核加固
//!
//! 1. 原稿所有结果（包括 `exit_code`）由 support 自报，可返回罐头值洗绿。
//!    本契约固定 spawn 真实 `orch` 二进制，命令名与参数骨架都写在冻结字节里。
//! 2. 原稿只比较“有/无收据的退出码相等”；一致失败或提前 success 都能通过。
//!    本契约增加绝对成功锚点：真实退出码为 0，且账本真实追加 `VerdictIssued` / `TaskRecorded`。
//! 3. 同 attempt 已坐 formal review 的 agent 必须从非门期望集合剔除，避免 B291 把 dsh 自审。
//!
//! 每条断言一个独立 `#[test]`。

#![allow(dead_code)]

mod nongate_attempt_receipts_support;

use std::process::{Command, Output};

use nongate_attempt_receipts_support as support;
use orch_core::read_ledger;

const NONGATE_SEATS: [&str; 2] = ["executor-antigravity", "executor-dsh"];
const RECEIPT_STATES: [&str; 4] = ["answered", "failed", "timedOut", "empty"];
const WARNING_MARKER: &str = "[orch] nongate receipt warning:";

fn run_orch(scene: &support::Scene, args: &[&str]) -> Output {
    let mut command = Command::new(support::orch_binary());
    support::configure_fixture_command(&mut command, scene);
    command
        .current_dir(&scene.root)
        .args(args)
        .output()
        .expect("spawn 真实 orch 进程失败；本契约不接受 support 自报结果")
}

fn run_real_verdict(scene: &support::Scene) -> Output {
    run_orch(
        scene,
        &[
            "verdict",
            &scene.task_id,
            "--attempt",
            &scene.attempt_id,
            "--expected-head",
            &scene.expected_head,
            "--expected-main",
            &scene.expected_main,
            "--verdict",
            "pass",
        ],
    )
}

fn run_real_seal(scene: &support::Scene) -> Output {
    run_orch(
        scene,
        &[
            "seal",
            &scene.task_id,
            "--attempt",
            &scene.attempt_id,
            "--expected-head",
            &scene.expected_head,
        ],
    )
}

fn combined_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn event_count(scene: &support::Scene, kind: &str) -> usize {
    let path = scene
        .root
        .join(format!("coordination/rounds/{}/events.jsonl", scene.round));
    let ledger = read_ledger(&path).expect("fixture ledger 必须可读");
    assert!(ledger.bad_lines.is_empty(), "fixture ledger 不得含坏行");
    ledger
        .events
        .iter()
        .filter(|event| {
            event.kind == kind && event.task_id.as_deref() == Some(scene.task_id.as_str())
        })
        .count()
}

fn warning_names(text: &str, agent: &str) -> bool {
    text.lines()
        .any(|line| line.contains(WARNING_MARKER) && line.contains(agent))
}

fn assert_real_verdict_success(scene: &support::Scene, output: &Output) {
    assert!(
        output.status.success(),
        "真实 verdict 必须 exit 0：{}",
        combined_text(output)
    );
    assert_eq!(
        event_count(scene, "VerdictIssued"),
        1,
        "exit 0 还不够：verdict 必须真实追加唯一 VerdictIssued，防提前 success"
    );
}

fn assert_real_seal_success(scene: &support::Scene, output: &Output) {
    assert!(
        output.status.success(),
        "真实 seal 必须 exit 0：{}",
        combined_text(output)
    );
    assert_eq!(
        event_count(scene, "TaskRecorded"),
        1,
        "exit 0 还不够：seal 必须真实走完 merge→合后门→TaskRecorded"
    );
}

#[test]
fn missing_both_receipts_is_named_but_real_verdict_still_succeeds() {
    let scene = support::verdict_scene("b292-missing-both");
    let output = run_real_verdict(&scene);
    assert_real_verdict_success(&scene, &output);
    let text = combined_text(&output);
    for seat in NONGATE_SEATS {
        assert!(
            warning_names(&text, seat),
            "缺席警告必须点名 {seat}；输出：{text}"
        );
    }
}

#[test]
fn a_partially_attempted_pair_still_names_only_the_missing_seat() {
    let scene = support::verdict_scene("b292-missing-one");
    support::write_bound_receipt(&scene, "executor-dsh", "answered");
    let output = run_real_verdict(&scene);
    assert_real_verdict_success(&scene, &output);
    let text = combined_text(&output);
    assert!(
        text.contains("executor-antigravity"),
        "必须点名缺席 agy：{text}"
    );
    assert!(
        !warning_names(&text, "executor-dsh"),
        "已留证 dsh 不得被误报：{text}"
    );
}

#[test]
fn the_warning_carries_attempt_and_fixed_head() {
    let scene = support::verdict_scene("b292-identity");
    let output = run_real_verdict(&scene);
    assert_real_verdict_success(&scene, &output);
    let text = combined_text(&output);
    assert!(
        text.contains(&scene.attempt_id) && text.contains(&scene.expected_head),
        "警告必须绑定 attemptId 与 fixed HEAD；输出：{text}"
    );
}

#[test]
fn all_four_receipt_states_count_as_attempted() {
    for state in RECEIPT_STATES {
        let scene = support::verdict_scene(&format!("b292-state-{state}"));
        for seat in NONGATE_SEATS {
            support::write_bound_receipt(&scene, seat, state);
        }
        let output = run_real_verdict(&scene);
        assert_real_verdict_success(&scene, &output);
        let text = combined_text(&output);
        for seat in NONGATE_SEATS {
            assert!(
                !warning_names(&text, seat),
                "状态 {state} 已有绑定收据，不得仍报 {seat} 缺席：{text}"
            );
        }
    }
}

#[test]
fn a_receipt_without_a_terminal_reason_is_incomplete() {
    let scene = support::verdict_scene("b292-no-reason");
    support::write_bound_receipt_without_terminal_reason(&scene, "executor-dsh", "timedOut");
    let output = run_real_verdict(&scene);
    assert_real_verdict_success(&scene, &output);
    assert!(
        warning_names(&combined_text(&output), "executor-dsh"),
        "非 answered 收据缺 exact terminal reason 必须视为缺席"
    );
}

#[test]
fn a_well_formed_but_unbound_receipt_is_still_missing() {
    let scene = support::verdict_scene("b292-forged-receipt");
    support::write_unbound_receipt(&scene, "executor-dsh", "answered");
    let output = run_real_verdict(&scene);
    assert_real_verdict_success(&scene, &output);
    assert!(
        warning_names(&combined_text(&output), "executor-dsh"),
        "格式正确但没有 matching WakeIssued/WorkspaceLeased 的文件不得冒充已尝试"
    );
}

#[test]
fn a_cross_attempt_runtime_fact_cannot_bind_the_receipt() {
    let scene = support::verdict_scene("b292-wrong-attempt");
    support::write_receipt_bound_to_other_attempt(&scene, "executor-dsh", "answered");
    let output = run_real_verdict(&scene);
    assert_real_verdict_success(&scene, &output);
    assert!(
        warning_names(&combined_text(&output), "executor-dsh"),
        "另一 attempt 的 durable event 不得被借来给当前收据背书"
    );
}

#[test]
fn a_formal_reviewer_is_excluded_from_same_attempt_nongate_expectations() {
    let scene = support::verdict_scene_with_formal_reviewer("b292-formal-overlap", "executor-dsh");
    support::write_bound_receipt(&scene, "executor-antigravity", "answered");
    let output = run_real_verdict(&scene);
    assert_real_verdict_success(&scene, &output);
    let text = combined_text(&output);
    assert!(
        !warning_names(&text, "executor-dsh"),
        "同 attempt 已坐 formal secondary 的 dsh 必须从非门期望集合剔除：{text}"
    );
}

#[test]
fn missing_receipts_do_not_change_real_verdict_success_or_effect() {
    let with = support::verdict_scene("b292-verdict-with");
    for seat in NONGATE_SEATS {
        support::write_bound_receipt(&with, seat, "answered");
    }
    let without = support::verdict_scene("b292-verdict-without");
    let with_output = run_real_verdict(&with);
    let without_output = run_real_verdict(&without);
    assert_real_verdict_success(&with, &with_output);
    assert_real_verdict_success(&without, &without_output);
    assert_eq!(
        with_output.status.code(),
        without_output.status.code(),
        "缺收据只能增加警告，不得改变真实 verdict 退出码"
    );
}

#[test]
fn missing_receipts_do_not_change_real_seal_success_or_effect() {
    let with = support::seal_scene("b292-seal-with");
    for seat in NONGATE_SEATS {
        support::write_bound_receipt(&with, seat, "answered");
    }
    let without = support::seal_scene("b292-seal-without");
    let with_output = run_real_seal(&with);
    let without_output = run_real_seal(&without);
    assert_real_seal_success(&with, &with_output);
    assert_real_seal_success(&without, &without_output);
    assert_eq!(
        with_output.status.code(),
        without_output.status.code(),
        "缺收据只能增加警告，不得改变真实 seal 退出码"
    );
}

#[test]
fn the_guide_documents_the_full_receipt_contract() {
    let guide = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
    for marker in [
        "nongate receipt 四态",
        "绑定 WakeIssued 与 WorkspaceLeased",
        "不改变 verdict 或 seal 的退出码",
        "排除同 attempt formal reviewer",
    ] {
        assert!(
            guide.contains(marker),
            "输出契约变化必须同卡写进指南，且不能用既有泛词凑绿（缺 {marker:?}）"
        );
    }
}

#[test]
fn the_support_module_is_wired() {
    assert_eq!(support::CONTRACT_ID, "B292");
}
