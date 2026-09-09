//! B205 · H48-P1 lifecycle 批故障原子性契约。
//!
//! 当前 `append_ledger_and_wal` 直接 append tracked ledger，再 append WAL；WAL 失败会让命令
//! 返回 Err，但 ledger 已推进。更危险的是进程在 write_all/事件边界死亡时可能留下半行或只含
//! lifecycle 批前缀。本 seed 用**生产同一存储内核的受控 failpoint**锁死四条边界：
//!
//! M1. temp 写到一半就暴露到 canonical ledger ⇒ `partial_temp_never_becomes_visible` 红；
//! M2. ledger replace 后、WAL replace 前失败时重放会重复事件 ⇒
//!     `ledger_ahead_retry_reconciles_without_duplicates` 红；
//! M3. 同 eventId 不同字节被当成幂等 ⇒ `conflicting_event_id_is_refused` 红；
//! M4. 成功返回时 ledger/WAL 不逐字节相等 ⇒ `success_keeps_ledger_and_wal_identical` 红。
//!
//! 首红形态：compile。下面两项在 `orch_host::ledger` 中尚不存在，rustc 应报 E0432。
//! 实现不得把 failpoint 做成另一套假存储；它必须进入 `append` / `append_checked` 共用的真实
//! CoW/transaction 内核，只在指定阶段返回受控错误。

use std::fs;

use orch_host::ledger::{
    self, append_batch_with_fault_for_test, AtomicBatchFault,
};

const ROUND: &str = "rB205";

fn root(tag: &str) -> std::path::PathBuf {
    let root = orch_host::util::test_scratch_dir(&format!("b205-{tag}"));
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();
    root
}

fn event(id: &str, sequence: usize) -> orch_core::EventRecord {
    let mut event = ledger::event(
        "PlannerTurnCompleted",
        "runtime:orch",
        None,
        Some(ROUND),
        serde_json::json!({"sequence": sequence}),
    );
    event.event_id = id.to_string();
    event
}

fn paths(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    (
        root.join(format!("coordination/rounds/{ROUND}/events.jsonl")),
        root.join(format!("coordination/runtime/ledger-wal/{ROUND}.jsonl")),
    )
}

fn ids(bytes: &[u8]) -> Vec<String> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["eventId"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn partial_temp_never_becomes_visible() {
    let root = root("partial-temp");
    ledger::append(&root, ROUND, &[event("base", 0)]).unwrap();
    let (ledger_path, wal_path) = paths(&root);
    let before_ledger = fs::read(&ledger_path).unwrap();
    let before_wal = fs::read(&wal_path).unwrap();

    let batch = vec![event("batch-1", 1), event("batch-2", 2)];
    let error = append_batch_with_fault_for_test(
        &root,
        ROUND,
        &batch,
        AtomicBatchFault::DuringTempWrite { after_bytes: 17 },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("failpoint"));
    assert_eq!(fs::read(&ledger_path).unwrap(), before_ledger);
    assert_eq!(fs::read(&wal_path).unwrap(), before_wal);
    fs::remove_dir_all(root).ok();
}

#[test]
fn ledger_ahead_retry_reconciles_without_duplicates() {
    let root = root("ledger-ahead");
    ledger::append(&root, ROUND, &[event("base", 0)]).unwrap();
    let batch = vec![event("batch-1", 1), event("batch-2", 2)];

    let error = append_batch_with_fault_for_test(
        &root,
        ROUND,
        &batch,
        AtomicBatchFault::AfterLedgerReplace,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("failpoint"));

    // 允许 canonical ledger 已完整提交而 WAL 落后一批；不允许只出现 batch 前缀。
    let (ledger_path, wal_path) = paths(&root);
    assert_eq!(ids(&fs::read(&ledger_path).unwrap()), ["base", "batch-1", "batch-2"]);
    assert_eq!(ids(&fs::read(&wal_path).unwrap()), ["base"]);

    // 同一批重放必须识别 exact eventId+bytes，补齐 WAL 而不重复 ledger。
    ledger::append(&root, ROUND, &batch).unwrap();
    let ledger_bytes = fs::read(&ledger_path).unwrap();
    assert_eq!(ids(&ledger_bytes), ["base", "batch-1", "batch-2"]);
    assert_eq!(fs::read(&wal_path).unwrap(), ledger_bytes);
    fs::remove_dir_all(root).ok();
}

#[test]
fn conflicting_event_id_is_refused() {
    let root = root("conflict");
    ledger::append(&root, ROUND, &[event("same-id", 1)]).unwrap();
    let error = ledger::append(&root, ROUND, &[event("same-id", 999)]).unwrap_err();
    assert!(
        format!("{error:#}").contains("eventId") && format!("{error:#}").contains("conflict"),
        "同 eventId 不同字节必须响亮拒绝: {error:#}"
    );
    let (ledger_path, wal_path) = paths(&root);
    assert_eq!(ids(&fs::read(&ledger_path).unwrap()), ["same-id"]);
    assert_eq!(fs::read(&ledger_path).unwrap(), fs::read(&wal_path).unwrap());
    fs::remove_dir_all(root).ok();
}

#[test]
fn success_keeps_ledger_and_wal_identical() {
    let root = root("success");
    let batch = vec![event("batch-1", 1), event("batch-2", 2), event("batch-3", 3)];
    ledger::append(&root, ROUND, &batch).unwrap();
    let (ledger_path, wal_path) = paths(&root);
    assert_eq!(fs::read(&ledger_path).unwrap(), fs::read(&wal_path).unwrap());
    fs::remove_dir_all(root).ok();
}
