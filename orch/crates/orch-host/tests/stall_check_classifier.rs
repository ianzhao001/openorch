//! B247 · `orch stall-check` 的判定内核（H121①）。
//!
//! # 由来
//!
//! INC-001：三份产物在任务分支上躺了 **1h20m 无人收**。完成信号当时**全部免费可用**
//! （GO 的 `.ack`、分支上的 REPORT/BLOCKED、执行者进程已退出、账本零新事件），
//! 但系统与人都没有「它停了」这个信号。答案至今是**纪律**
//! （`coordination/scripts/stall-check.sh`，212 行手工先行版）而不是**机制**。
//!
//! # 本种子钉的是**判定表**，不是 IO
//!
//! 把 shell 版的确定性决策表移植为纯函数：输入是已经观测到的结构化事实，
//! 输出是判定 + `need_action` + 给 planner 的下一步提示。IO（读账本、探进程、
//! 看 wake 日志增长）由 CLI 层负责，**不在本种子的断言范围内**——
//! 这样判定表本身可以被确定性地证伪，不受机器负载影响。
//!
//! # 三条必须钉死的语义（都是血换来的）
//!
//! 1. **先判活动、再判产物——但只对「非当前 attempt 的产物」成立。**
//!    shell 的判据是 `art_matches_current`：执行者正在干活时，分支上**旧 attempt**
//!    的产物不构成「待收取」；但产物若属于**当前 attempt**，说明它已经交付，
//!    此时即便还有活动信号也是 `awaiting-collection`。
//!    **这两格必须分开断言**——只写「活动优先」会把已交付的当前 attempt 误判成还在干活。
//! 2. **`collect-inflight` 绝不可 kill**（H117）：收取持 1 小时 durable 租约，
//!    纯时间判活、不看 PID；掐进程不释放租约，反而制造必须等到期的恢复窗口。
//!    hint 必须带**完整的否定语义**——只断言含 `kill` 会让「立即 kill」也蒙混过关。
//! 3. **`planner-idle` 是轮级判定，不是 attempt 级**：shell 在逐任务循环**之外**算它
//!    （无本地 orch 进程 + 无 agent 进程 + 无活跃 wake 日志 + 账本静默 >600s）。
//!    它属于独立的 `classify_round`；塞进 `classify_attempt` 是不诚实的建模——
//!    单个 attempt 的入参里根本没有全轮信息。

use orch_host::stall::{
    classify_attempt, classify_round, ArtifactOnBranch, AttemptProjection, RoundInputs, StallInputs,
};

fn base(task: &str) -> StallInputs {
    StallInputs {
        task_id: task.to_string(),
        attempt_id: format!("{task}-A0001"),
        projection: AttemptProjection::Dispatched,
        artifact: ArtifactOnBranch::None,
        // shell 的 art_matches_current：分支产物是否属于**当前** attempt
        artifact_from_current_attempt: false,
        executor_alive: false,
        wake_log_growing: false,
    }
}

/// **INC-001 那一格**：当前 attempt 的 REPORT 已在分支上、执行者已退出 ⇒ 没人在收。
#[test]
fn a_current_attempt_report_with_a_dead_executor_is_awaiting_collection() {
    let verdict = classify_attempt(&StallInputs {
        artifact: ArtifactOnBranch::Report,
        artifact_from_current_attempt: true,
        executor_alive: false,
        wake_log_growing: false,
        ..base("B900")
    });

    assert_eq!(verdict.verdict, "awaiting-collection");
    assert!(
        verdict.need_action,
        "产物已就绪却无人收取，必须要求 planner 介入"
    );
}

/// **当前 attempt 的产物压过活动信号**：它已经交付了，哪怕日志还在动也该去收。
/// 这一格是 codex 卡面预审指出、我原种子写反了的那一格。
#[test]
fn a_current_attempt_artifact_outranks_activity() {
    let verdict = classify_attempt(&StallInputs {
        artifact: ArtifactOnBranch::Report,
        artifact_from_current_attempt: true,
        executor_alive: true,
        wake_log_growing: true,
        ..base("B901")
    });

    assert_eq!(
        verdict.verdict, "awaiting-collection",
        "当前 attempt 已交付 ⇒ 即使仍有活动也应去收，不能判成还在干活"
    );
    assert!(verdict.need_action);
}

/// **活动压过「旧 attempt」的产物**：分支上挂着的是上一个 attempt 的残留，
/// 而执行者正在为当前 attempt 干活——此时判「待收取」会把人从活干到一半叫停。
#[test]
fn activity_outranks_a_stale_prior_attempt_artifact() {
    let verdict = classify_attempt(&StallInputs {
        artifact: ArtifactOnBranch::Report,
        artifact_from_current_attempt: false,
        executor_alive: true,
        wake_log_growing: true,
        ..base("B902")
    });

    assert_eq!(
        verdict.verdict, "executor-working",
        "旧 attempt 的产物不构成待收取，执行者仍在为当前 attempt 工作"
    );
    assert!(!verdict.need_action, "执行者在正常工作时不得要求介入");
}

/// 收取在飞：需要 planner 知情，但提示必须**完整禁止** kill（H117）。
/// 只断言含 "kill" 不够——「立即 kill」也含 "kill"。
#[test]
fn collect_inflight_needs_attention_but_forbids_killing() {
    let verdict = classify_attempt(&StallInputs {
        projection: AttemptProjection::CollectInflight,
        ..base("B903")
    });

    assert_eq!(verdict.verdict, "collect-inflight");
    assert!(
        verdict.hint.contains("绝不可 kill"),
        "H117：hint 必须逐字含「绝不可 kill」这一完整否定语义，实际 hint={:?}",
        verdict.hint
    );
}

/// **活动条件是 OR，不是 AND**：进程活着但日志没动、日志在动但进程探不到，
/// 任一成立都算「在干活」。两格分开测——若两个信号都设成 true，
/// 一个错误的 AND 实现也能蒙混过去。
#[test]
fn either_activity_signal_alone_counts_as_working() {
    for (alive, growing) in [(true, false), (false, true)] {
        let verdict = classify_attempt(&StallInputs {
            artifact: ArtifactOnBranch::Report,
            artifact_from_current_attempt: false,
            executor_alive: alive,
            wake_log_growing: growing,
            ..base("B906")
        });
        assert_eq!(
            verdict.verdict, "executor-working",
            "活动条件必须是 OR：alive={alive} growing={growing}"
        );
    }
}

/// BLOCKED 是执行者的诚实终态：要进 planner 视野，且必须给出可执行的下一步。
#[test]
fn a_blocked_artifact_asks_the_planner_to_decide() {
    let verdict = classify_attempt(&StallInputs {
        artifact: ArtifactOnBranch::Blocked,
        artifact_from_current_attempt: true,
        executor_alive: false,
        wake_log_growing: false,
        ..base("B907")
    });

    assert!(verdict.need_action, "BLOCKED 必须进入 planner 视野");
    assert!(
        !verdict.hint.trim().is_empty(),
        "需要介入的判定必须给出可执行的下一步"
    );
}

/// 进程没了、分支上也没有任何产物 ⇒ 既不是在做也不是做完了，必须介入。
#[test]
fn a_dead_executor_with_no_artifact_needs_action() {
    let verdict = classify_attempt(&StallInputs {
        artifact: ArtifactOnBranch::None,
        executor_alive: false,
        wake_log_growing: false,
        ..base("B904")
    });

    assert_eq!(verdict.verdict, "executor-gone-no-artifact");
    assert!(verdict.need_action);
}

/// 已经 Recorded 的任务是终态，不该再报任何需要介入的东西。
#[test]
fn a_recorded_task_is_quiet() {
    let verdict = classify_attempt(&StallInputs {
        projection: AttemptProjection::Recorded,
        ..base("B905")
    });

    assert_eq!(verdict.verdict, "recorded");
    assert!(
        !verdict.need_action,
        "终态任务不得制造噪音，否则告警会被习惯性忽略"
    );
}

/// **轮级判定**（INC-002）：四个条件同时成立才算 planner 空转。
#[test]
fn planner_idle_is_a_round_level_verdict() {
    let idle = classify_round(&RoundInputs {
        local_orch_processes: 0,
        agent_processes: 0,
        live_wake_logs: 0,
        ledger_silence_secs: 900,
    });
    assert!(idle.planner_idle, "四条件齐备时必须报 planner-idle");
    assert!(idle.need_action);
}

/// **静默阈值是严格 `> 600`**：600 不报、601 才报。
/// 不钉这一格，实现成 `>= 600` 或 `> 60` 都能蒙混过去。
#[test]
fn the_silence_threshold_is_strictly_greater_than_600() {
    let quiet = |secs| {
        classify_round(&RoundInputs {
            local_orch_processes: 0,
            agent_processes: 0,
            live_wake_logs: 0,
            ledger_silence_secs: secs,
        })
        .planner_idle
    };
    assert!(!quiet(600), "恰好 600 秒不得报 planner-idle（阈值是严格大于）");
    assert!(quiet(601), "601 秒必须报 planner-idle");
}

/// 反向：只要还有任一活动信号，就不是 planner 空转——否则告警会在正常工作时乱响。
/// **同时断言 `need_action` 也为假**：只断言 `planner_idle=false` 而让 `need_action`
/// 悬空，等于允许实现照样报警。
#[test]
fn any_live_signal_clears_planner_idle() {
    for probe in [
        RoundInputs { local_orch_processes: 1, agent_processes: 0, live_wake_logs: 0, ledger_silence_secs: 9_999 },
        RoundInputs { local_orch_processes: 0, agent_processes: 1, live_wake_logs: 0, ledger_silence_secs: 9_999 },
        RoundInputs { local_orch_processes: 0, agent_processes: 0, live_wake_logs: 1, ledger_silence_secs: 9_999 },
        RoundInputs { local_orch_processes: 0, agent_processes: 0, live_wake_logs: 0, ledger_silence_secs: 60 },
    ] {
        let verdict = classify_round(&probe);
        assert!(
            !verdict.planner_idle,
            "存在活动信号或账本未静默时不得报 planner-idle：{probe:?}"
        );
        assert!(
            !verdict.need_action,
            "既然不是 planner 空转，就不得要求介入：{probe:?}"
        );
    }
}
