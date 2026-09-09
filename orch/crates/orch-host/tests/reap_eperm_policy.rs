//! B248 · 门收割的 EPERM 分层（**H134a 护栏，不核销 H134**）。
//!
//! # 缺陷
//!
//! `gate.rs::reap_errno_tolerated(errno) = errno == 3 || errno == 1`
//! （ESRCH=3、EPERM=1）被**两条语义完全不同的路径共用**：
//!
//! - `reap_gate_process_group()` 的**直接**调用（`run_gate_command` 收尾）——
//!   这里遇 EPERM 大概率是"这个组不归本门管"，跳过是对的。
//! - `reap_gate_fixture_registry()` 的**表内**调用——registry 里的每个 pgid 都是
//!   本门自己登记的。这里遇 EPERM 意味着**一次真实的收割失败被静默吞掉**，
//!   而它恰恰是 H106 孤儿子形态的放大面：吞掉之后孤儿继续存活并打红后续的门。
//!
//! # ⚠️ 本卡的诚实边界（卡面与 REPORT 都必须复述）
//!
//! r67 那八次门红**一次都没有命中 EPERM 分支**——B243 的留痕计数为 0，
//! 这是**反证**，已排除 EPERM 是那八次的成因。真正的根因（pid 复用防线时序）
//! **本卡根本不动**。
//!
//! ⇒ 本卡是**加固**，不是治病。它只能叫「H134a EPERM 分层护栏」，
//! **不得用来核销 H134**，也**不得**把「连跑 N 次零红」写进任何验收条目——
//! 负载不可控、不可证伪，且本卡不动那条根因。

use orch_host::gate::{reap_errno_tolerated, reap_errno_tolerated_for, ReapPolicy};

const ESRCH: i32 = 3;
const EPERM: i32 = 1;
const EINVAL: i32 = 22;

/// 直接 reap 路径的语义**一字不变**：ESRCH 与 EPERM 都容忍。
/// B227 的冻结契约要求 `ESRCH` 字面量仍在 `gate.rs` 全文，这条同时守住旧行为。
#[test]
fn the_direct_reap_path_keeps_tolerating_eperm() {
    assert!(
        reap_errno_tolerated_for(ReapPolicy::DirectGroup, ESRCH),
        "组可能在收尾前就没了，ESRCH 必须继续容忍"
    );
    assert!(
        reap_errno_tolerated_for(ReapPolicy::DirectGroup, EPERM),
        "直接路径遇 EPERM 通常是『该组不归本门管』，保持跳过"
    );
}

/// **本卡的核心**：表内组的 EPERM 必须**硬失败**。
/// registry 里的每个 pgid 都是本门自己登记的，无权 kill 只能是真实故障。
#[test]
fn the_registry_path_hard_fails_on_eperm() {
    assert!(
        !reap_errno_tolerated_for(ReapPolicy::RegisteredFixture, EPERM),
        "表内组是本门自己登记的，EPERM 是真实收割失败，不得静默吞掉"
    );
}

/// 分层不得误伤 ESRCH：表内组也可能在收割前自然退出，那是正常的。
#[test]
fn the_registry_path_still_tolerates_esrch() {
    assert!(
        reap_errno_tolerated_for(ReapPolicy::RegisteredFixture, ESRCH),
        "进程自然退出不是故障，两条路径都必须继续容忍 ESRCH"
    );
}

/// 未知 errno 在**两条**路径上都必须硬失败——分层只区分 EPERM，不放宽其余。
#[test]
fn unknown_errnos_hard_fail_on_both_paths() {
    assert!(!reap_errno_tolerated_for(ReapPolicy::DirectGroup, EINVAL));
    assert!(!reap_errno_tolerated_for(ReapPolicy::RegisteredFixture, EINVAL));
}

/// 旧的无参判据必须继续存在且语义不变——它是直接路径的既有调用面，
/// B227/B239/B243 的冻结种子都还在用它。删掉或改语义都会打红那些冻结文件。
#[test]
fn the_legacy_predicate_is_preserved_as_the_direct_policy() {
    assert!(reap_errno_tolerated(ESRCH));
    assert!(reap_errno_tolerated(EPERM));
    assert!(!reap_errno_tolerated(EINVAL));
    for errno in [ESRCH, EPERM, EINVAL] {
        assert_eq!(
            reap_errno_tolerated(errno),
            reap_errno_tolerated_for(ReapPolicy::DirectGroup, errno),
            "旧判据必须逐值等价于新的 DirectGroup 策略，errno={errno}"
        );
    }
}
