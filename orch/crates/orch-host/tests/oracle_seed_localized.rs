//! ═══ 红种子契约 · B60（O5 种子局部性纯判据）═══
//! 落位: orch/crates/orch-host/tests/oracle_seed_localized.rs（逐字节复制）
//! 预期红（redForm: compile）：`seed_red_localized` 尚不存在 → error[E0432]。
//!
//! 背景：O5「种子红必须无空洞位且不扰基线」判据当前内联在 `preverify` 的 IO 流程里。
//! 本棒把该**纯算术判据**抽成可单测的独立函数（additive seam，`preverify` 不动），
//! 复用既有 `oracle::{SuiteCounts, FileCounts}`；不改任何既有函数/测试、不新增依赖。
//!
//! 目标契约（落在 orch_host::oracle，模块已 pub 导出，勿动 lib.rs）：
//!  - pub fn seed_red_localized(suite: &SuiteCounts, seed_file: &FileCounts,
//!        allow_file_passed: usize) -> Result<(), String>
//!      两条判据，全过 ⇒ Ok(())，否则 Err（文案指明是哪一条失败）：
//!      · 无空洞位：seed 文件内 passed = seed_file.tests - seed_file.failed，须 <= allow_file_passed；
//!      · 基线不受扰：suite.failed == seed_file.failed（全部红都局部在种子文件，无附带红）。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 不查空洞位（忽略 allow_file_passed）⇒ rejects_hollow_seed_unless_allowed 红；
//!  M2 不查基线（忽略 suite.failed 与 seed_file.failed 关系）⇒ rejects_collateral_red 红；
//!  M3 干净种子被误拒 ⇒ accepts_clean_localized_seed 红。

use orch_host::oracle::{seed_red_localized, FileCounts, SuiteCounts};

#[test]
fn accepts_clean_localized_seed() {
    // 3 红全在种子文件、0 passed、无附带红
    let suite = SuiteCounts { failed: 3, passed: 0, total: 3 };
    let seed = FileCounts { tests: 3, failed: 3 };
    assert!(seed_red_localized(&suite, &seed, 0).is_ok());
}

#[test]
fn rejects_collateral_red() {
    // 种子文件 3 红，但整套 4 红 ⇒ 有 1 条附带红，基线被扰
    let suite = SuiteCounts { failed: 4, passed: 0, total: 4 };
    let seed = FileCounts { tests: 3, failed: 3 };
    assert!(seed_red_localized(&suite, &seed, 0).is_err());
}

#[test]
fn rejects_hollow_seed_unless_allowed() {
    // 种子文件 4 例 3 红 ⇒ 文件内 1 passed（空洞位）；基线未扰（suite.failed==seed.failed==3）
    let suite = SuiteCounts { failed: 3, passed: 1, total: 4 };
    let seed = FileCounts { tests: 4, failed: 3 };
    assert!(seed_red_localized(&suite, &seed, 0).is_err()); // allow 0 ⇒ 空洞位被拒
    assert!(seed_red_localized(&suite, &seed, 1).is_ok()); // allow 1 ⇒ 放行
}
