//! B197 · 受管会话终态入账 + 「零产物终止」的机械判别（H63 改判后重定义）。
//!
//! H63 原定性是「受管 opencode 审查会话在 81 秒**被 TERM**，发送方未定位」，并据此
//! 长期使用 `--deadline-secs 0` 规避。r60 开轮前的全样本复核推翻了它：受管期 6 条审查
//! wake 的 supervisor 终态里，`hardDeadlineReached` 无一为真、`completionReason` 全是
//! `natural-exit`、`cancelReason` 全为空；而 `signals: ['TERM']` **在四次成功交付里同样出现**
//! ——它只是自然退出后对残留组成员的常规收束。真正的判别量是 **`terminalSeen`**。
//!
//! 那次误判的代价是整整一轮：r58 因此 forced 收轮。误判之所以可能，是因为这些事实**只躺在
//! `coordination/runtime/supervisors/*.status.json` 里，从不进账本**——planner 只能手工翻
//! JSON，并且翻错了。本卡把这些事实变成账本上的一等事实，并把判据写成代码。
//!
//! 首红形态：**compile**。下面导入的项在 `orch_host::wake` / `orch_core` 中尚不存在，
//! rustc 报 `error[E0432]: unresolved imports`。
//! 不得以建同名空壳、改本文件、或把该 test 排除出门的方式伪造红绿。

use orch_core::{known_event_kinds, EventRecord};
use orch_host::wake::{
    classify_managed_wake_outcome, pair_review_requests_with_wakes, ManagedWakeOutcomeClass,
    ManagedWakeTerminationFacts, ReviewWakePairing,
};

fn facts(
    wake_id: &str,
    completion_reason: &str,
    terminal_seen: bool,
    hard_deadline_reached: bool,
    signals: &[&str],
) -> ManagedWakeTerminationFacts {
    ManagedWakeTerminationFacts {
        wake_id: wake_id.to_string(),
        agent: "executor-opencode".to_string(),
        completion_reason: Some(completion_reason.to_string()),
        terminal_seen,
        exited_naturally: false,
        hard_deadline_reached,
        cancel_request_id: None,
        signals: signals.iter().map(|s| s.to_string()).collect(),
        managed_scope_terminated: true,
        log_bytes_read: 200_000,
    }
}

/// **这是本卡的核心回归**：把 r58/r59 六条真实现场逐条钉进代码。
/// 四条成功、两条零产物，`signals` 全是 `['TERM']`——所以 `signals` 永远不能用来判死。
#[test]
fn the_six_historical_review_sessions_classify_by_terminal_seen_only() {
    // (标签, deadlineSecs, terminalSeen, 期望分类)
    let cases: [(&str, u64, bool, ManagedWakeOutcomeClass); 6] = [
        ("r58/B179", 1800, false, ManagedWakeOutcomeClass::TruncatedNoTerminal),
        ("r58/B180", 1800, true, ManagedWakeOutcomeClass::DeliveredTerminal),
        ("r58/B181", 1800, false, ManagedWakeOutcomeClass::TruncatedNoTerminal),
        ("r59/B181", 0, true, ManagedWakeOutcomeClass::DeliveredTerminal),
        ("r59/B183", 0, true, ManagedWakeOutcomeClass::DeliveredTerminal),
        ("r59/B184", 0, true, ManagedWakeOutcomeClass::DeliveredTerminal),
    ];

    for (label, _deadline, terminal_seen, expected) in cases {
        // 六条现场无一例外：completionReason=natural-exit、hardDeadlineReached=false、
        // signals=['TERM']。唯一的差异就是 terminalSeen。
        let observed = classify_managed_wake_outcome(&facts(
            label,
            "natural-exit",
            terminal_seen,
            false,
            &["TERM"],
        ));
        assert_eq!(
            observed, expected,
            "{label}: 分类必须只由 terminalSeen 决定，实际 {observed:?}"
        );
    }
}

/// 反向锁：带 TERM 但拿到终帧的会话，**绝不允许**被判成被杀/截断。
/// 这正是 H63 误判的那一步——如果这条断言不在，同样的误判会再来一次。
#[test]
fn signals_alone_never_imply_a_kill() {
    let delivered_with_term =
        classify_managed_wake_outcome(&facts("term-but-ok", "natural-exit", true, false, &["TERM"]));
    assert_eq!(
        delivered_with_term,
        ManagedWakeOutcomeClass::DeliveredTerminal,
        "TERM 只是自然退出后的常规组收束，不构成被杀的证据"
    );

    let delivered_with_term_kill = classify_managed_wake_outcome(&facts(
        "term-kill-but-ok",
        "natural-exit",
        true,
        false,
        &["TERM", "KILL"],
    ));
    assert_eq!(
        delivered_with_term_kill,
        ManagedWakeOutcomeClass::DeliveredTerminal,
        "收束升级到 KILL 同样不改变「provider 已自然完成」这个事实"
    );
}

/// 真正的「被截止时间停掉」必须由 `hardDeadlineReached` 证明，不得靠时长或信号推断。
#[test]
fn hard_deadline_class_requires_the_deadline_flag() {
    let stopped = classify_managed_wake_outcome(&facts(
        "deadline",
        "hard-deadline",
        false,
        true,
        &["TERM", "KILL"],
    ));
    assert_eq!(
        stopped,
        ManagedWakeOutcomeClass::StoppedByHardDeadline,
        "hardDeadlineReached=true 才是硬截止"
    );

    let not_stopped =
        classify_managed_wake_outcome(&facts("no-flag", "natural-exit", false, false, &["TERM"]));
    assert_ne!(
        not_stopped,
        ManagedWakeOutcomeClass::StoppedByHardDeadline,
        "没有 hardDeadlineReached 就不得推断为硬截止——这是 H63 的原始错误"
    );
}

/// 认证 cancel 必须由 requestId 证明；没有 requestId 就不是人为取消。
#[test]
fn cancel_class_requires_an_authenticated_request_id() {
    let mut cancelled = facts("cancel", "manual-cancel", false, false, &["TERM"]);
    cancelled.cancel_request_id = Some("req-01".to_string());
    assert_eq!(
        classify_managed_wake_outcome(&cancelled),
        ManagedWakeOutcomeClass::StoppedByAuthenticatedCancel
    );

    let mut unattributed = cancelled.clone();
    unattributed.cancel_request_id = None;
    assert_ne!(
        classify_managed_wake_outcome(&unattributed),
        ManagedWakeOutcomeClass::StoppedByAuthenticatedCancel,
        "没有认证 requestId 不得记成人为取消"
    );
}

/// 终态必须成为**账本事实**，而不是只躺在 runtime JSON 里。
#[test]
fn managed_wake_terminated_is_a_registered_event_kind() {
    let kinds = known_event_kinds();
    assert!(
        kinds.iter().any(|kind| *kind == "ManagedWakeTerminated"),
        "ManagedWakeTerminated 必须登记进事件词表，否则 status 会把它报成未知类型"
    );
}

/// 审查请求与 wake 的绑定：H63 复核时 planner 只能靠「同一秒、同一 agent 的相邻事件」
/// 猜配对，第一次就猜错了。绑定必须是显式的；对**历史**的无绑定事件，
/// 必须诚实地返回「无法配对」，**不得靠相邻性猜**。
#[test]
fn review_wake_pairing_is_explicit_and_never_guesses() {
    let bound: Vec<EventRecord> = serde_json::from_str(
        r#"[
          {"eventId":"1","ts":"2026-08-02T00:00:00Z","actor":"root","type":"WakeIssued",
           "taskId":"B197","payload":{"agent":"executor-opencode","wakeId":"w-1"}},
          {"eventId":"2","ts":"2026-08-02T00:00:00Z","actor":"root","type":"ReviewRequested",
           "taskId":"B197","payload":{"agent":"executor-opencode","role":"secondary",
                                       "attemptId":"B197-A0001","wakeId":"w-1"}}
        ]"#,
    )
    .expect("fixture 应可解析");
    let pairs = pair_review_requests_with_wakes(&bound);
    assert_eq!(pairs.len(), 1, "恰好一条配对");
    match &pairs[0] {
        ReviewWakePairing::Bound { wake_id, .. } => assert_eq!(wake_id, "w-1"),
        other => panic!("显式携带 wakeId 时必须判为 Bound，实际 {other:?}"),
    }

    let legacy: Vec<EventRecord> = serde_json::from_str(
        r#"[
          {"eventId":"1","ts":"2026-08-02T00:00:00Z","actor":"root","type":"WakeIssued",
           "taskId":null,"payload":{"agent":"executor-opencode","wakeId":"w-legacy"}},
          {"eventId":"2","ts":"2026-08-02T00:00:00Z","actor":"root","type":"ReviewRequested",
           "taskId":"B181","payload":{"agent":"executor-opencode","role":"secondary",
                                       "attemptId":"B181-A0001","deadlineSecs":1800}}
        ]"#,
    )
    .expect("fixture 应可解析");
    let legacy_pairs = pair_review_requests_with_wakes(&legacy);
    assert_eq!(legacy_pairs.len(), 1);
    assert!(
        matches!(legacy_pairs[0], ReviewWakePairing::LegacyUnpaired { .. }),
        "历史事件没有 wakeId 时必须诚实报「无法配对」，不得按同秒相邻猜成 Bound：{:?}",
        legacy_pairs[0]
    );
}
