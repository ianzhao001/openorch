//! B198 · liveness 的「停滞」判据不得把「正在跑全量门」误判为死（H70），
//! 且 `await-report` 的缺省超时必须随卡面 `budgets.wallMinutes` 缩放（H68）。
//!
//! 两条在 r60 各咬了一次，且**卡越重越容易中**：
//! - **H70**：`liveness.rs` 工作相位的 idle 只由 `ack_age` 与 `last_activity_age`
//!   （**worktree 文件活动**）取最小；而执行者跑 `cargo test --workspace`（本仓一次
//!   130–150 秒，加 check/build 与复跑轻松超过 10 分钟）期间**根本不写工作树**。
//!   r60/B192 在 ack 后 **621 秒**就被判滞并落 `AttemptTimedOut`，而同刻 wake 日志显示
//!   它正在收全量门结果、准备写 REPORT。
//! - **H68**：`await-report` 缺省 1800s 与卡面 `budgets.wallMinutes` 互不知情。
//!   r60/B193（240 分钟的卡）在超时后 **19 秒**交卷，同样被误落 `AttemptTimedOut`。
//!
//! 首红形态：**compile**。下面导入的三项在 `orch_host` 中尚不存在，
//! rustc 报 `error[E0432]: unresolved imports`。
//! 本文件刻意**不**直接构造 `ProbeSnapshot` 字面量——那会额外产生 E0560 并污染
//! oracle 的精确身份；新字段一律经构造函数注入。
//! 不得以建同名空壳、改本文件、或把该 test 排除出门的方式伪造红绿。

use std::time::Duration;

use orch_host::liveness::{
    judge, probe_working, working_idle_age, Judgement, LivenessOpts, ProbeSnapshot,
};
use orch_host::tierf::default_await_timeout_secs;

fn opts() -> LivenessOpts {
    LivenessOpts {
        stall: Duration::from_secs(600),
        ..LivenessOpts::default()
    }
}

fn secs(v: u64) -> Option<Duration> {
    Some(Duration::from_secs(v))
}

/// 工作相位的 idle 必须是**所有活性信号**取最小——新增的 provider 输出信号必须参与。
#[test]
fn working_idle_is_the_minimum_of_every_activity_signal() {
    assert_eq!(
        working_idle_age(secs(900), secs(800), secs(5)),
        secs(5),
        "provider 仍在输出时 idle 必须取它——否则长门必被误判"
    );
    assert_eq!(
        working_idle_age(secs(900), secs(800), None),
        secs(800),
        "无 provider 输出信号时退回原语义（ack 与 worktree 取最小）"
    );
    assert_eq!(
        working_idle_age(secs(900), None, None),
        secs(900),
        "只有 ack 时取 ack"
    );
    assert_eq!(
        working_idle_age(None, None, None),
        None,
        "三者皆无 = 无法判定，必须返回 None 让调用方保守处理，不得当成 0"
    );
}

/// **H70 的核心回归**：worktree 早已静默、但 provider 仍在输出 → 不得判滞。
#[test]
fn a_long_gate_run_is_not_stalled_while_the_provider_still_writes() {
    // r60/B192 的现场：ack 后 900s，worktree 900s 无写入，但 provider 3 秒前还在输出。
    let snapshot: ProbeSnapshot = probe_working(
        Duration::from_secs(1_200),
        secs(900),
        secs(900),
        secs(3),
    );
    let verdict = judge(&snapshot, &opts());
    assert!(
        matches!(verdict, Judgement::Healthy(_)),
        "provider 3 秒前还在输出，绝不能判滞：{verdict:?}"
    );
}

/// 反向锁：所有信号都陈旧时**仍必须判滞**——本卡不是把判据关掉。
#[test]
fn everything_stale_still_stalls() {
    let snapshot = probe_working(
        Duration::from_secs(3_000),
        secs(2_000),
        secs(2_000),
        secs(2_000),
    );
    let verdict = judge(&snapshot, &opts());
    assert!(
        matches!(verdict, Judgement::StalledCandidate(_)),
        "三个信号全部超过 stall 阈值时必须判滞，否则判据形同虚设：{verdict:?}"
    );
}

/// provider 输出信号缺失时，行为必须与本卡之前**逐字相同**（不引入新的误判方向）。
#[test]
fn absent_provider_signal_preserves_the_previous_semantics() {
    let stalled = probe_working(Duration::from_secs(3_000), secs(1_000), secs(1_000), None);
    assert!(matches!(
        judge(&stalled, &opts()),
        Judgement::StalledCandidate(_)
    ));

    let healthy = probe_working(Duration::from_secs(3_000), secs(1_000), secs(10), None);
    assert!(matches!(judge(&healthy, &opts()), Judgement::Healthy(_)));
}

/// **H68**：`await-report` 的缺省超时必须由卡面 `budgets.wallMinutes` 推导，
/// 且**严格覆盖**它——否则重卡永远在交卷前被判超时。
#[test]
fn default_await_timeout_covers_the_card_budget() {
    for wall_minutes in [30_u64, 120, 180, 240, 600] {
        let got = default_await_timeout_secs(wall_minutes);
        assert!(
            got >= wall_minutes * 60,
            "wallMinutes={wall_minutes} 的缺省超时 {got}s 必须覆盖卡面预算 {}s",
            wall_minutes * 60
        );
    }

    assert!(
        default_await_timeout_secs(240) > default_await_timeout_secs(30),
        "缺省超时必须随卡面预算单调增长，不得是常量"
    );
    // r60/B193 的现场：240 分钟的卡，旧缺省 1800s 远不够。
    assert!(
        default_await_timeout_secs(240) > 1800,
        "240 分钟的卡不得再落回 1800s 缺省——这正是 H68 的现场"
    );
    assert!(
        default_await_timeout_secs(0) > 0,
        "卡面未声明预算时也必须给出一个正的缺省，不能是 0"
    );
}
