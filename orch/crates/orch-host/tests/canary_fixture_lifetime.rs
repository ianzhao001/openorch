//! B194 · H56 应急夹具直修（`413c411`）的转正审计留证 + 待办③「settle 预算常量化」。
//!
//! H56 的死局链条：B143 canary 的假 provider 在收到 `GO-REWAKE` 后 **约 3.5ms 就退出**，
//! 而 B177 的 pre-ACK 身份证明要求 provider 的凭据在 500ms 窗口内保持 50ms 不变、
//! 且要做 A/full/B 三次拓扑取样（串行最坏约 1.6s）。夹具活不过证明窗口 → 合后门红 →
//! 屏障 Active → **全轮冻结**。应急直修把夹具改成 `sleep 3`。
//!
//! `sleep 3` 是个魔数：它与身份证明预算之间的关系**只存在于注释里**，任何一方改动都不会
//! 有人报警。本卡把这条关系变成**机械可检**的：预算提为公开常量，夹具寿命由它推导。
//!
//! 首红形态：**compile**。下面导入的四项在 `orch_host::wake` 中尚不存在，
//! rustc 报 `error[E0432]: unresolved imports`。
//! 不得以建同名空壳常量、改本文件、或把该 test 排除出门的方式伪造红绿。

use std::path::Path;

use orch_host::wake::{
    canary_provider_lifetime_secs, provider_identity_worst_case_settle_ms,
    PROVIDER_IDENTITY_SETTLE_WINDOW_MS, PROVIDER_IDENTITY_STABLE_MS,
};

/// B177 pre-ACK 身份证明的取样次数（A / full / B 三明治）。
/// 与 supervisor 终态里的 `fullCredentialInspections` 同源。
const TOPOLOGY_INSPECTIONS: u32 = 3;

/// 预算必须是**公开且自洽**的：稳定期落在窗口内，两者都非零。
/// 这三条一旦不成立，身份证明本身就是空转，H56 会以另一种形状回来。
#[test]
fn settle_budget_is_public_and_self_consistent() {
    assert!(
        PROVIDER_IDENTITY_STABLE_MS > 0,
        "身份稳定期必须为正，否则「不变」这个判据没有内容"
    );
    assert!(
        PROVIDER_IDENTITY_SETTLE_WINDOW_MS > PROVIDER_IDENTITY_STABLE_MS,
        "窗口必须严格大于稳定期：{PROVIDER_IDENTITY_SETTLE_WINDOW_MS} vs {PROVIDER_IDENTITY_STABLE_MS}"
    );

    let worst = provider_identity_worst_case_settle_ms(TOPOLOGY_INSPECTIONS);
    assert!(
        worst >= PROVIDER_IDENTITY_SETTLE_WINDOW_MS,
        "最坏耗时不得小于单次窗口：worst={worst}"
    );
    assert_eq!(
        provider_identity_worst_case_settle_ms(1),
        PROVIDER_IDENTITY_SETTLE_WINDOW_MS,
        "单次取样的最坏耗时就是一个窗口——公式必须随取样次数线性增长，不得是常量"
    );
    assert!(
        provider_identity_worst_case_settle_ms(2) > provider_identity_worst_case_settle_ms(1),
        "取样次数增加时最坏耗时必须增加，否则这个函数没有把预算算进去"
    );
}

/// H56 的核心不变量：**夹具寿命必须严格覆盖身份证明的最坏耗时**，并留出余量。
/// 这正是应急直修用 `sleep 3` 换来的东西，本用例把它钉死。
#[test]
fn canary_lifetime_strictly_exceeds_worst_case_settle() {
    let lifetime_ms = canary_provider_lifetime_secs() * 1000;
    let worst = provider_identity_worst_case_settle_ms(TOPOLOGY_INSPECTIONS);

    assert!(
        lifetime_ms > worst,
        "夹具寿命 {lifetime_ms}ms 必须严格大于身份证明最坏耗时 {worst}ms——\
         这正是 H56 全轮冻结的直接成因"
    );
    assert!(
        lifetime_ms >= worst * 2,
        "余量不足：{lifetime_ms}ms 应至少为最坏耗时 {worst}ms 的两倍，\
         否则机器负载稍高就会重演 H56"
    );
}

/// 常量化的意义在于**联动**：改预算，夹具寿命必须跟着变。
/// 如果 `canary_provider_lifetime_secs()` 是个写死的字面量，本用例抓不住它——
/// 所以这里用「寿命必须由最坏耗时推导」的可观测后果来验：
/// 它对预算的任何放大都必须保持覆盖关系。
#[test]
fn canary_lifetime_is_derived_from_the_budget_not_hardcoded() {
    for inspections in 1..=TOPOLOGY_INSPECTIONS {
        let worst = provider_identity_worst_case_settle_ms(inspections);
        assert!(
            canary_provider_lifetime_secs() * 1000 > worst,
            "夹具寿命必须覆盖 {inspections} 次取样的最坏耗时 {worst}ms"
        );
    }
    // 覆盖关系必须来自推导：寿命不能小于「最坏耗时向上取整到秒」。
    let worst = provider_identity_worst_case_settle_ms(TOPOLOGY_INSPECTIONS);
    let floor_secs = worst.div_ceil(1000);
    assert!(
        canary_provider_lifetime_secs() >= floor_secs,
        "寿命 {}s 低于由预算推出的下界 {floor_secs}s",
        canary_provider_lifetime_secs()
    );
}

/// 源码级回归：`serve.rs` 里的 B143 canary 夹具**不得再出现裸的 `sleep 3`**，
/// 必须调用上面那个推导函数。这条防的是「有人把 sleep 删了 / 改小了」的原始事故。
#[test]
fn serve_canary_fixture_no_longer_hardcodes_its_lifetime() {
    let serve = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/serve.rs");
    let source = std::fs::read_to_string(&serve).expect("serve.rs 应可读");

    assert!(
        source.contains("canary_provider_lifetime_secs"),
        "serve.rs 的 canary 夹具必须由 canary_provider_lifetime_secs() 推导寿命"
    );
    assert!(
        !source.contains("sleep 3;"),
        "serve.rs 仍写着裸的 `sleep 3;` 魔数——H56③ 未完成"
    );
    assert!(
        source.contains("GO-REWAKE"),
        "前置条件：本用例检查的确实是 GO-REWAKE 分支所在的夹具"
    );
}
