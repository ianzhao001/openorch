#![allow(dead_code)]
//! B267 seeded-red contract：把**观察期限**与**执行期限**拆成两件事（H163）。
//!
//! Expected red: **compile**。`tierf::await_expiry_disposition` 与
//! `tierf::AwaitExpiryDisposition` 尚不存在 ⇒ `error[E0432]: unresolved import`。
//!
//! ── 实撞（r70/B263-A0003，烧掉一个仍在正常推进的 attempt）──
//!
//! `WakeIssued.runtimeLimit.effectiveSecs = 21600`，PID / 日志 / WIP 在 30 分钟内持续进展。
//! planner 把 `orch await-report B263 --timeout-secs 1800` 当作**可续开的观察窗口**；
//! 恰好 1800 秒后 runtime 依次落 `EscalationRaised`、`AttemptTimedOut(timeoutSecs=1800)`、
//! `ActionRejected`，把**只消耗了八分之一 runtime 预算**的活跃 attempt canonical terminal。
//!
//! ── 缺陷的机械形态（planner 已逐行复核）──
//!
//! `tierf.rs` 里 `runtime_limit` / `runtimeLimit` 的命中数是 **0**——
//! **观察路径完全不知道该 attempt 已认证的运行期限存在**。
//! 超时块（`if liveness::attempt_current_elapsed(attempt_clock, start) >= Duration::from_secs(timeout_secs)`
//! 起、至 `// liveness probe` 止，窗口 4029 字节）拿 CLI 传进来的 `timeout_secs`
//! **直接**产出 canonical `AttemptTimedOut`，中间没有任何一层把它与
//! `WakeIssued.runtimeLimit.effectiveSecs` 对照。
//!
//! ── 修法的唯一原则 ──
//!
//! **只有绑定 `WakeIssued.runtimeLimit` 的 runtime deadline 才能 canonical terminal 一个 attempt。**
//! observer 到期只是「我不看了」，不是「它死了」。
//!
//! 无已认证 runtime limit 时**同样不得** canonical terminal——因为此时我们
//! **没有任何权威**去宣告该 attempt 死亡。这是刻意的 fail-closed 方向：
//! 宁可让 planner 多看一眼，也不要再烧掉一个还在干活的 attempt。
//!
//! ── 本种子刻意不做的事 ──
//!
//! 不构造 `EventRecord` 结构体字面量、不钉整段哈希（Q1 裁决 ⑤）；
//! 判定逻辑做成纯函数以便独立测试；源码窗口扫描放普通 `#[test]`、每条断言一个（坑 16）。
//!
//! ── Negative mutations that must turn the named case red ──
//!
//! M1. observer 到期即判 runtime deadline（回到 r70 的形态）
//!     -> `an_observer_window_shorter_than_the_runtime_limit_is_not_terminal` 红。
//! M2. 无已认证 runtime limit 时回落为 canonical terminal
//!     -> `without_an_authenticated_runtime_limit_expiry_is_never_terminal` 红。
//! M3. 真到 runtime deadline 却仍判非终态（另一个方向的错误）
//!     -> `reaching_the_authenticated_runtime_limit_is_terminal` 红。
//! M4. **接线变异**：谓词存在但超时块不调用它
//!     -> `the_await_timeout_block_consults_the_disposition` 红。
//! M5. 超时块继续对 runtime limit 一无所知（不读取已认证事实）
//!     -> `the_await_timeout_block_reads_the_authenticated_runtime_limit` 红。
//! M6. r70/B263-A0003 的确切参数被判成终态（回归本体）
//!     -> `the_b263_a0003_regression_stays_non_terminal` 红。

use orch_host::tierf::{await_expiry_disposition, AwaitExpiryDisposition};

const TIERF_SRC: &str = include_str!("../src/tierf.rs");

const TIMEOUT_BLOCK_START: &str =
    "if liveness::attempt_current_elapsed(attempt_clock, start)";
const TIMEOUT_BLOCK_END: &str = "\n        // liveness probe";

fn timeout_block() -> &'static str {
    let start = TIERF_SRC
        .find(TIMEOUT_BLOCK_START)
        .expect("B267: missing source anchor for the await-timeout block");
    let end = TIERF_SRC[start..]
        .find(TIMEOUT_BLOCK_END)
        .expect("B267: missing source anchor for the end of the await-timeout block");
    &TIERF_SRC[start..start + end]
}

// ── 行为：观察期限短于执行期限时，到期不是终态（M1）───────────────────────

#[test]
fn an_observer_window_shorter_than_the_runtime_limit_is_not_terminal() {
    let disposition = await_expiry_disposition(1_800, 1_800, Some(21_600));
    assert!(
        matches!(disposition, AwaitExpiryDisposition::ObserverExpired { .. }),
        "B267 M1: observer 到期只是「我不看了」，不是「它死了」；实得 {disposition:?}"
    );
}

// ── 行为：无已认证期限时永不终态（M2）─────────────────────────────────────

#[test]
fn without_an_authenticated_runtime_limit_expiry_is_never_terminal() {
    let disposition = await_expiry_disposition(9_999, 1_800, None);
    assert!(
        matches!(disposition, AwaitExpiryDisposition::ObserverExpired { .. }),
        "B267 M2: 没有已认证的 runtime limit 就没有宣告死亡的权威——\
         必须 fail-closed 到非终态；实得 {disposition:?}"
    );
}

// ── 行为：真到执行期限才是终态（M3）───────────────────────────────────────

#[test]
fn reaching_the_authenticated_runtime_limit_is_terminal() {
    let disposition = await_expiry_disposition(21_600, 1_800, Some(21_600));
    assert!(
        matches!(
            disposition,
            AwaitExpiryDisposition::RuntimeDeadlineElapsed { .. }
        ),
        "B267 M3: 绑定 WakeIssued.runtimeLimit 的 deadline 到了，才允许 canonical terminal；\
         实得 {disposition:?}"
    );
}

// ── 回归本体：r70/B263-A0003 的确切参数（M6）──────────────────────────────

#[test]
fn the_b263_a0003_regression_stays_non_terminal() {
    // 实撞参数：observer 1800s、已认证 runtimeLimit 21600s、恰在 1800s 到期。
    let disposition = await_expiry_disposition(1_800, 1_800, Some(21_600));
    match disposition {
        AwaitExpiryDisposition::ObserverExpired {
            observer_secs,
            runtime_limit_secs,
        } => {
            assert_eq!(observer_secs, 1_800, "B267: 必须如实回报观察窗口");
            assert_eq!(
                runtime_limit_secs, 21_600,
                "B267: 必须如实回报被保护的执行期限——运维要靠这两个数字判断该不该续看"
            );
        }
        other => panic!(
            "B267 M6: r70/B263-A0003 烧掉了一个只消耗八分之一预算的活跃 attempt，\
             这条回归必须永远非终态；实得 {other:?}"
        ),
    }
}

// ── 接线变异（M4）：生产超时块必须真的调用该谓词 ──────────────────────────

#[test]
fn the_await_timeout_block_consults_the_disposition() {
    assert!(
        timeout_block().contains("await_expiry_disposition("),
        "B267 M4: 谓词写对了但生产超时块不调它，正是 r64/H116 的形态"
    );
}

// ── 接线变异（M5）：生产超时块必须读到已认证的执行期限 ────────────────────

#[test]
fn the_await_timeout_block_reads_the_authenticated_runtime_limit() {
    assert!(
        timeout_block().contains("runtime_limit"),
        "B267 M5: 今天 tierf.rs 全文对 runtime_limit 的命中数是 0——\
         观察路径对已认证的执行期限一无所知，这就是 H163 的根"
    );
}
