//! ═══ 红种子契约 · B39 ═══（落位: orch/crates/orch-host/tests/serve_daemon.rs，逐字节复制）
//! 预期红（redForm: compile）：serve::derive_tick_inputs 尚不存在（E0425 cannot find function），文件级编译红。
//! 背景：design/11 §3 daemon tick——每 tick 从账本投影+inbox 目录派生 TickInputs,再喂 decide_injections。
//!   本棒做「派生」纯函数(compute_tick_inputs 的可测内核)：任务态→fail_tasks/all_recorded,其余透传。
//!   注:B36 已交付 TickInputs 结构 + decide_injections;serve_tick(IO 执行注入)另在实现侧,种子只钉派生纯函数。
//! 变异清单（E9 下界）:
//!   M1 fail_tasks 认错状态(非 changes_requested) → ① 红；M2 空任务集误判 all_recorded → ② 红；
//!   M3 透传字段(new_inbox/dead/pending)丢失 → ③ 红
use orch_host::serve::{self, InjectReason};

#[test]
fn fail_tasks_come_from_changes_requested_state() {
    // ① FAIL 后任务态=changes_requested → 计入 fail_tasks;recorded 不计
    let states = vec![
        ("B39".to_string(), "changes_requested".to_string()),
        ("B40".to_string(), "recorded".to_string()),
    ];
    let ti = serve::derive_tick_inputs(&states, vec![], vec![], vec![], false);
    assert_eq!(ti.fail_tasks, vec!["B39".to_string()]);
    assert!(!ti.all_recorded);
}

#[test]
fn all_recorded_needs_nonempty_and_every_task_recorded() {
    // ② 全 recorded 才算;混态不算;空任务集不算(开轮瞬间不得误注入收轮)
    let all_rec = vec![
        ("B39".to_string(), "recorded".to_string()),
        ("B40".to_string(), "recorded".to_string()),
    ];
    assert!(serve::derive_tick_inputs(&all_rec, vec![], vec![], vec![], false).all_recorded);
    let mixed = vec![
        ("B39".to_string(), "recorded".to_string()),
        ("B40".to_string(), "ready_for_verification".to_string()),
    ];
    assert!(!serve::derive_tick_inputs(&mixed, vec![], vec![], vec![], false).all_recorded);
    assert!(!serve::derive_tick_inputs(&[], vec![], vec![], vec![], false).all_recorded);
}

#[test]
fn passthrough_fields_feed_decide_injections() {
    // ③ new_inbox/dead_or_stalled/pending 原样透传;与 decide 联动:pending=TaskFailed 去抖 FAIL,仍出 NewInstruction
    let states = vec![("B39".to_string(), "changes_requested".to_string())];
    let ti = serve::derive_tick_inputs(
        &states,
        vec!["1784800000-x.md".to_string()],
        vec!["executor-claw".to_string()],
        vec![InjectReason::TaskFailed],
        false,
    );
    assert_eq!(ti.new_inbox, vec!["1784800000-x.md".to_string()]);
    assert_eq!(ti.dead_or_stalled, vec!["executor-claw".to_string()]);
    assert_eq!(ti.pending, vec![InjectReason::TaskFailed]);
    let injs = serve::decide_injections(&ti);
    assert!(injs.iter().any(|i| i.reason == InjectReason::NewInstruction));
    assert!(!injs.iter().any(|i| i.reason == InjectReason::TaskFailed));
}
