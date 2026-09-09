//! B199 · oracle v2 的编译红身份在「上游 `dependsOn` 卡落地」时会合法收缩，
//! 机检必须能把它与「种子被削弱」区分开（H71）。
//!
//! r60/B192 的现场：该卡 `dependsOn: [B197]`，其种子按设计消费 B197 交付的
//! `ManagedWakeOutcomeClass` / `ManagedWakeTerminationFacts`。种子基线在 B197 合入**前**
//! 切下，`error[E0432]` 未解析符号 **12 项**；B197 Recorded 后同一份种子在新树上只剩 **9 项**
//! ——全是 B192 自己要新增的 attach 面。oracle v2 做**符号集精确相等**比对，于是收取期硬失败：
//!
//! ```text
//! 机检 FAIL·compile oracle identity/high-water 不符：compile identity mismatch:
//!   baseline=[…12 symbols…] replay=[…9 symbols…]
//! ```
//!
//! **replay 是 baseline 的严格子集，缺的三项恰由已 Recorded 的上游提供**，
//! 「attach 面完全不存在」这个红的结构性理由一条未变——不是种子被削弱。
//! 但机检不区分两者，而同轮内的 `dependsOn` 链几乎必然触发它。
//!
//! 首红形态：**compile**。下面导入的两项在 `orch_host::oracle` 中尚不存在，
//! rustc 报 `error[E0432]: unresolved imports`。
//! 不得以建同名空壳、改本文件、或把该 test 排除出门的方式伪造红绿。

use orch_host::oracle::{classify_compile_identity_shift, CompileIdentityShift};

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// r60/B192 的真实符号集（截短但保持结构）。
fn b192_baseline() -> Vec<String> {
    v(&[
        "symbol:orch_host::wake::AttachMode",
        "symbol:orch_host::wake::AttachRefusal",
        "symbol:orch_host::wake::ManagedWakeOutcomeClass",
        "symbol:orch_host::wake::ManagedWakeTerminationFacts",
        "symbol:orch_host::wake::NudgeDelivery",
        "symbol:orch_host::wake::SessionDeathEvidence",
        "symbol:orch_host::wake::attach_action_id",
        "symbol:orch_host::wake::classify_managed_wake_outcome",
        "symbol:orch_host::wake::classify_opencode_session_receipt",
        "symbol:orch_host::wake::declare_managed_session_dead",
        "symbol:orch_host::wake::nudge_delivery_outcome",
        "symbol:orch_host::wake::plan_managed_wake_attach",
    ])
}

/// B197 Recorded 后由上游提供、因而从 replay 里消失的三项。
fn b197_provided() -> Vec<String> {
    v(&[
        "symbol:orch_host::wake::ManagedWakeOutcomeClass",
        "symbol:orch_host::wake::ManagedWakeTerminationFacts",
        "symbol:orch_host::wake::classify_managed_wake_outcome",
    ])
}

fn b192_replay() -> Vec<String> {
    b192_baseline()
        .into_iter()
        .filter(|s| !b197_provided().contains(s))
        .collect()
}

/// 完全相同的符号集仍然是最常见的情形，必须判 `Unchanged`（零行为变化）。
#[test]
fn identical_symbol_sets_are_unchanged() {
    let shift = classify_compile_identity_shift(&b192_baseline(), &b192_baseline(), &[]);
    assert_eq!(
        shift,
        CompileIdentityShift::Unchanged,
        "符号集未变时必须判 Unchanged，与上游列表无关"
    );
}

/// **H71 的核心**：收缩且差集**全部**由上游提供 → 合法收缩。
#[test]
fn shrink_fully_explained_by_upstream_is_legitimate() {
    let shift = classify_compile_identity_shift(&b192_baseline(), &b192_replay(), &b197_provided());
    match shift {
        CompileIdentityShift::LegitimateShrink { dropped } => {
            let mut dropped = dropped;
            dropped.sort();
            let mut want = b197_provided();
            want.sort();
            assert_eq!(dropped, want, "必须逐项报出被上游吸收的符号，供留证");
        }
        other => panic!("差集全部由上游提供时必须判合法收缩，实际 {other:?}"),
    }
}

/// 反向铁律：差集里只要有**一项**不是上游提供的，就必须判失配——
/// 那正是「种子被削弱」的形态，绝不能放行。
#[test]
fn shrink_not_fully_explained_by_upstream_is_a_mismatch() {
    let mut replay = b192_replay();
    // 再少一项，且这一项不在上游清单里
    replay.retain(|s| s != "symbol:orch_host::wake::attach_action_id");

    let shift = classify_compile_identity_shift(&b192_baseline(), &replay, &b197_provided());
    assert!(
        matches!(shift, CompileIdentityShift::Mismatch { .. }),
        "有未被上游解释的缺失符号时必须失配，实际 {shift:?}"
    );
}

/// 上游清单为空时，任何收缩都必须失配——不给「反正是收缩就放行」留口子。
#[test]
fn any_shrink_without_upstream_evidence_is_a_mismatch() {
    let shift = classify_compile_identity_shift(&b192_baseline(), &b192_replay(), &[]);
    assert!(
        matches!(shift, CompileIdentityShift::Mismatch { .. }),
        "无上游证据的收缩必须失配，实际 {shift:?}"
    );
}

/// 增长永远是失配：replay 出现 baseline 没有的符号，说明红的理由变了。
#[test]
fn growth_is_always_a_mismatch() {
    let mut replay = b192_replay();
    replay.push("symbol:orch_host::wake::something_new".to_string());

    let shift = classify_compile_identity_shift(&b192_baseline(), &replay, &b197_provided());
    assert!(
        matches!(shift, CompileIdentityShift::Mismatch { .. }),
        "replay 出现 baseline 没有的符号时必须失配，实际 {shift:?}"
    );
}

/// 收缩到空集必须失配：一个符号都不缺，说明种子根本不红了。
#[test]
fn shrinking_to_empty_is_a_mismatch() {
    let shift = classify_compile_identity_shift(&b192_baseline(), &[], &b192_baseline());
    assert!(
        matches!(shift, CompileIdentityShift::Mismatch { .. }),
        "replay 为空 = 种子已不红，必须失配（哪怕差集全在上游清单里），实际 {shift:?}"
    );
}
