//! B226 seeded-red contract: 清理接线一族——sweep 有生产调用者、close 出三态报告、
//! 日志按「已收轮」粒度归档（H111 + 日志轮转）。
//!
//! Expected red: compile. `orch_host::logrotate` 模块与 `orch_host::sites::reclaim_report`
//! 在 B226 之前不存在。
//!
//! 铁则（backlog 逐字）：**只轮转账本已终态的日志——在飞 attempt 的日志是判死证据。**
//! v1 收窄到轮粒度：只有已含 RoundClosed 的轮，其账本引用的日志才进归档集。
//!
//! Negative mutations that must turn the named case red:
//! M1. 未收轮的轮（或当前轮）的日志被放进归档集
//!     -> `open_round_logs_never_enter_the_archive_set` 红。
//! M2. legacy 探活重链文件（wake-<agent>.log，无时间戳后缀）被归档
//!     -> `legacy_handshake_relink_files_are_never_archived` 红。
//! M3. 归档映射不确定（同输入两次调用产出不同集合）
//!     -> `archive_set_is_deterministic` 红。
//! M4. close 路径没有接 sweep_test_scratch_root / 三态报告（能力又一次交付了没人调）
//!     -> `close_path_wires_sweep_and_reclaim_report` 红（读真实源码，裁定⑬范式）。

use orch_core::EventRecord;
use orch_host::logrotate::archive_set_for_round;
use orch_host::sites::reclaim_report;

fn wake_issued(round: &str, event_id: &str, log_path: &str) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": event_id,
        "ts": "2026-08-04T00:00:00Z",
        "actor": "runtime:orch",
        "type": "WakeIssued",
        "round": round,
        "taskId": "B904",
        "payload": {
            "wakeId": format!("WAKE-{event_id}"),
            "agent": "executor-pi",
            "backendState": "legacy-untracked",
            "logPath": log_path,
        },
    }))
    .expect("构造事件失败")
}

fn round_closed(round: &str, event_id: &str) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": event_id,
        "ts": "2026-08-04T00:00:00Z",
        "actor": "runtime:orch",
        "type": "RoundClosed",
        "round": round,
        "payload": {"forced": false},
    }))
    .expect("构造事件失败")
}

fn fixture() -> Vec<EventRecord> {
    vec![
        // r90：已收轮 ⇒ 其日志可归档。
        wake_issued("r90", "EV-W-OLD", "coordination/runtime/logs/wake-executor-pi-111-1-1.jsonl"),
        round_closed("r90", "EV-RC-90"),
        // r91：未收轮 ⇒ 一个字节都不能动。
        wake_issued("r91", "EV-W-OPEN", "coordination/runtime/logs/wake-executor-pi-222-2-2.jsonl"),
    ]
}

#[test]
fn closed_round_logs_enter_the_archive_set() {
    let set = archive_set_for_round(&fixture(), "r90");
    assert!(
        set.iter().any(|p| p.to_string_lossy().contains("wake-executor-pi-111-1-1.jsonl")),
        "已收轮的账本引用日志必须进归档集"
    );
}

#[test]
fn open_round_logs_never_enter_the_archive_set() {
    // M1：对未收轮的轮请求归档 ⇒ 空集（在飞日志是判死证据）。
    let set = archive_set_for_round(&fixture(), "r91");
    assert!(set.is_empty(), "未收轮的轮必须返回空归档集，得到 {set:?}");
}

#[test]
fn legacy_handshake_relink_files_are_never_archived() {
    // M2：wake-<agent>.log（无时间戳后缀）是 handshake 依赖的重链文件，永不轮转。
    let mut events = fixture();
    events.push(wake_issued("r90", "EV-W-LEGACY", "coordination/runtime/logs/wake-executor-pi.log"));
    let set = archive_set_for_round(&events, "r90");
    assert!(
        !set.iter().any(|p| p.to_string_lossy().ends_with("wake-executor-pi.log")),
        "legacy 探活重链文件绝不进归档集"
    );
}

#[test]
fn archive_set_is_deterministic() {
    // M3：同输入两次调用，集合逐字相等（历史报告的引用可追溯性依赖确定性映射）。
    let events = fixture();
    assert_eq!(
        archive_set_for_round(&events, "r90"),
        archive_set_for_round(&events, "r90"),
        "归档映射必须确定性"
    );
}

#[test]
fn close_path_wires_sweep_and_reclaim_report() {
    // M4：读真实生产源码（storage.rs:743 范式）——close 路径必须真的调用
    // sweep_test_scratch_root 与 reclaim_report，否则又是「交付了没人调」。
    let cli_main = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../orch-cli/src/main.rs"),
    )
    .expect("读 orch-cli/src/main.rs");
    let close_region = cli_main
        .split("RoundCmd::Close")
        .nth(1)
        .or_else(|| cli_main.split("fn cmd_round").nth(1))
        .expect("orch-cli 必须含 round close 处理段");
    let head = &close_region[..close_region.len().min(8000)];
    assert!(
        head.contains("sweep_test_scratch_root"),
        "round close 必须接线 scratch 回收（H111 的『谁调它』半句）"
    );
    assert!(
        head.contains("reclaim_report"),
        "round close 必须输出 gc 三态报告（把 O1 口头纪律变成有清单的动作）"
    );
    // 防呆：报告函数保持只读——sites.rs 中 reclaim_report 定义段禁止出现删除性调用。
    let sites_rs = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/sites.rs"),
    )
    .expect("读 sites.rs");
    let report_fn = sites_rs
        .split("pub fn reclaim_report")
        .nth(1)
        .expect("sites.rs 必须含 reclaim_report");
    let report_head = &report_fn[..report_fn.len().min(3000)];
    for banned in ["remove_dir_all", "worktree remove", "fs::remove"] {
        assert!(
            !report_head.contains(banned),
            "reclaim_report 必须只读（发现删除性调用 {banned}）"
        );
    }
    let _ = reclaim_report; // 编译期锚定：函数必须存在且可引用。
}
