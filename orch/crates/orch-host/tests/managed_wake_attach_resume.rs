//! B192 · 会话死亡后的 attach/resume 正解，并吸收 H64（continuation fence 封死一切续办入口）。
//!
//! H64 的结构性矛盾：`resume` 的语义**就是**「原会话已死、用新现场重建」，
//! 它却要求请求摘要与原请求逐字相同——于是会话一死，wake / nudge / resume 全被 fence 拒，
//! dispatch 幂等空转，retry-dead 交回 planner，唯一兜底是 `await-report` 空等 30 分钟。
//! r58 正是因此 forced 收轮。
//!
//! 本卡的解法**不是放宽 fence**，而是给它一条**凭证据的窄豁免**：只有当「会话已死」被
//! immutable terminal status 证明时，resume 才免于摘要比对。
//! **断言不算证据，沉默更不算**——这是 H63 误判的教训。
//!
//! ## 本文件的导入面刻意只含本卡要新增的符号（H71 教训）
//!
//! r60 那次重跑失败的直接原因是：本种子当时导入了上游 B197 将要交付的
//! `ManagedWakeOutcomeClass` / `ManagedWakeTerminationFacts` / `classify_managed_wake_outcome`，
//! 于是 B197 一合入，oracle 的编译红身份就从 12 个符号收缩为 9 个，机检硬失败。
//! **现在改为从 immutable terminal status 的 JSON 直接构造死亡证据**，
//! 本文件不再引用任何上游符号，基线不会被上游合入改变。
//!
//! 首红形态：**compile**，`error[E0432]: unresolved imports`。
//! 不得以建同名空壳、改本文件、或把该 test 排除出门的方式伪造红绿。

use orch_host::wake::{
    attach_action_id, classify_opencode_session_receipt, declare_managed_session_dead,
    plan_managed_wake_attach, session_death_evidence_from_status,
    AttachMode, AttachRefusal,
};

/// r58/B181 那条零产物终止的真实终态形状（字段名与 supervisor status.json 一致）。
const TRUNCATED_TERMINAL: &str = r#"{
  "wakeId": "w-source",
  "completionReason": "natural-exit",
  "terminalSeen": false,
  "exitedNaturally": false,
  "exitStatus": 0,
  "cancelReason": null,
  "signals": ["TERM"],
  "hardDeadlineReached": false,
  "managedScopeTerminated": true,
  "logBytesRead": 166121
}"#;

/// 同一会话尚未收束时的形状：终态未落定，`managedScopeTerminated` 为假。
const NOT_YET_TERMINAL: &str = r#"{
  "wakeId": "w-source",
  "completionReason": null,
  "terminalSeen": false,
  "exitedNaturally": false,
  "exitStatus": null,
  "cancelReason": null,
  "signals": [],
  "hardDeadlineReached": false,
  "managedScopeTerminated": false,
  "logBytesRead": 4096
}"#;

/// action id 必须是幂等的身份：同输入同 id（重试不铸新动作），
/// 换 mode 必须换 id（resume 与 fork 是两次可审计、分别计费的动作，不得互相冒充）。
#[test]
fn attach_action_id_is_stable_and_mode_scoped() {
    let a = attach_action_id("r61", "B192-A0001", "secondary", "w-source", AttachMode::Resume);
    let b = attach_action_id("r61", "B192-A0001", "secondary", "w-source", AttachMode::Resume);
    let forked = attach_action_id("r61", "B192-A0001", "secondary", "w-source", AttachMode::Fork);
    let other = attach_action_id("r61", "B192-A0002", "secondary", "w-source", AttachMode::Resume);

    assert_eq!(a, b, "同一次 attach 重试必须复用同一 actionId");
    assert_ne!(a, forked, "resume 与 fork 必须是不同的动作");
    assert_ne!(a, other, "不同 attempt 不得共用 actionId");
    assert!(!a.is_empty(), "actionId 不得为空");
}

/// session receipt 只认「顶层 sessionID 的完整独立 JSON 对象」。
/// 这是恢复能力的根：认错一个 id，continuation 就会接到别人的会话上。
#[test]
fn session_receipt_only_accepts_an_exact_top_level_id() {
    let good = r#"{"type":"step_start","sessionID":"ses_abc123"}"#;
    assert_eq!(
        classify_opencode_session_receipt(good).as_deref(),
        Some("ses_abc123"),
        "顶层 sessionID 的完整帧必须被接受"
    );

    for bad in [
        r#"{"type":"tool","output":{"sessionID":"ses_nested"}}"#, // 嵌套
        r#"{"part":{"sessionID":"ses_partonly"}}"#,               // part-only
        r#"{"type":"step_start","sessionID":"ses_a","part":{"sessionID":"ses_b"}}"#, // 冲突
        r#"{"type":"step_start","sessionID":"ses_with\nnewline"}"#, // 控制字符
        r#"{"type":"step_start","sessionID":""}"#,                // 空
        r#"{"type":"step_start","sessionID":"../escape"}"#,       // 路径分隔符
        r#"{"type":"step_start","sessionID":"ses_trunc""#,        // 未收完
    ] {
        assert_eq!(
            classify_opencode_session_receipt(bad),
            None,
            "非法 receipt 必须拒绝，实际被接受: {bad}"
        );
    }
}

/// 死亡证据**只能**由已落盘的 immutable terminal status 构造，
/// 且要求 `managedScopeTerminated == true`。沉默、无输出、进程不在都不算。
#[test]
fn death_evidence_requires_an_immutable_terminal_status() {
    let evidence = session_death_evidence_from_status(TRUNCATED_TERMINAL)
        .expect("零产物终止是合法的死亡证据");
    assert!(
        evidence.exempts_request_digest(),
        "凭 immutable terminal status 的死亡证据，resume 必须免于摘要比对（H64 正解）"
    );

    assert!(
        session_death_evidence_from_status(NOT_YET_TERMINAL).is_none(),
        "未落终态时不得构造死亡证据——沉默不是死亡"
    );
    assert!(
        session_death_evidence_from_status("{}").is_none(),
        "字段缺失时必须返回 None，不得用缺省值补齐"
    );
    assert!(
        session_death_evidence_from_status("not json").is_none(),
        "无法解析时必须返回 None，不得 panic 或猜测"
    );
}

/// attach 只消费 immutable terminal status：源会话还活着、或终态未落定，
/// 都必须 fail-closed，零新 provider spawn。
#[test]
fn attach_refuses_until_the_source_is_provably_terminal() {
    let live = plan_managed_wake_attach("w-source", AttachMode::Resume, None);
    assert!(
        matches!(live, Err(AttachRefusal::SourceNotProvenTerminal)),
        "拿不到终态就必须拒绝，实际 {live:?}"
    );

    let half = session_death_evidence_from_status(NOT_YET_TERMINAL);
    assert!(half.is_none(), "前置：未收束的会话构造不出证据");

    let evidence = session_death_evidence_from_status(TRUNCATED_TERMINAL).expect("证据");
    let ready = plan_managed_wake_attach("w-source", AttachMode::Resume, Some(&evidence))
        .expect("终态齐备时必须给出计划");
    assert_eq!(ready.mode, AttachMode::Resume);
    assert_eq!(ready.source_wake_id, "w-source");
    assert!(
        !ready.continuation_wake_id.is_empty(),
        "计划必须预分配 continuationWakeId，使 crash-after-spawn 可对账"
    );
}

/// 「宣告会话死亡」必须是显式、可审计、且凭证据的入口，不是一个开关。
#[test]
fn declaring_a_session_dead_requires_terminal_evidence() {
    assert!(
        declare_managed_session_dead("w-source", None).is_err(),
        "无证据的宣告必须被拒"
    );

    let evidence = session_death_evidence_from_status(TRUNCATED_TERMINAL).expect("证据");
    let declared =
        declare_managed_session_dead("w-source", Some(&evidence)).expect("有终态证据时必须允许宣告");
    assert_eq!(declared.wake_id, "w-source");
    assert!(
        declared.evidence.exempts_request_digest(),
        "宣告的产物必须能直接被 resume 消费，否则续办入口仍然是断的"
    );
}
