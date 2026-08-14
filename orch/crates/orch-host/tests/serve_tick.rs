//! ═══ 红种子契约 · B36 ═══（落位: orch/crates/orch-host/tests/serve_tick.rs，逐字节复制）
//! 预期红（redForm: compile）：serve::{TickInputs, InjectReason, Injection, decide_injections} 尚不存在
//!   （E0432 unresolved import + E0433/E0425），文件级编译红。
//! 背景：design/11 §3 daemon tick——机械分支(ready→verify / approved→merge)不注入主控；
//!   判断分支(inbox 新指令 / task FAIL / 全 recorded / 执行者判死判滞)各注入主控一条。
//!   注入去抖：同一决策点已在 pending，本 tick 不重复注入。
//! 变异清单（E9 下界）:
//!   M1 FAIL 分支不产注入 → ② 红；M2 pending 去抖失效(仍重复注入) → ③ 红；M3 无触发时误产注入 → ① 红
use orch_host::serve::{self, InjectReason, Injection, TickInputs};

#[test]
fn no_trigger_yields_no_injection() {
    // ① 机械态(ready/approved 由 runloop 处理,不入本函数)+ 无判断触发 → 零注入
    let inputs = TickInputs {
        new_inbox: vec![],
        fail_tasks: vec![],
        all_recorded: false,
        dead_or_stalled: vec![],
        pending: vec![],
    };
    assert_eq!(serve::decide_injections(&inputs), Vec::<Injection>::new());
}

#[test]
fn each_judgement_trigger_injects_once() {
    // ② 四个判断分支各产生恰一条注入,reason 齐全
    let inputs = TickInputs {
        new_inbox: vec!["1784800000-orch-x.md".to_string()],
        fail_tasks: vec!["B40".to_string()],
        all_recorded: true,
        dead_or_stalled: vec!["executor-claw".to_string()],
        pending: vec![],
    };
    let out = serve::decide_injections(&inputs);
    let reasons: Vec<InjectReason> = out.iter().map(|inj| inj.reason).collect();
    assert!(reasons.contains(&InjectReason::NewInstruction));
    assert!(reasons.contains(&InjectReason::TaskFailed));
    assert!(reasons.contains(&InjectReason::AllRecorded));
    assert!(reasons.contains(&InjectReason::AgentDown));
    assert_eq!(out.len(), 4);
}

#[test]
fn pending_reason_is_debounced() {
    // ③ 去抖:TaskFailed 已在 pending → 本 tick 不再为 FAIL 注入
    let inputs = TickInputs {
        new_inbox: vec![],
        fail_tasks: vec!["B40".to_string()],
        all_recorded: false,
        dead_or_stalled: vec![],
        pending: vec![InjectReason::TaskFailed],
    };
    assert!(serve::decide_injections(&inputs).is_empty());
}
