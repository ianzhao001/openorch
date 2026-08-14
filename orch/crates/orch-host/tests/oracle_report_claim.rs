//! ═══ 红种子契约 · B65（O16：根治 O15——assertion-red §3 报数判据）═══
//! 落位: orch/crates/orch-host/tests/oracle_report_claim.rs（逐字节复制）
//! 预期红（redForm: compile）：`report_claim_ok` 尚不存在 → error[E0432]/E0425。
//!
//! 背景（O15，见 m0/dryrun-errata.md）：`replay_seed_red` 的 §3 叙事核验对 assertion-red 把
//! REPORT 声称计数与**整套 suite 聚合** `total_passed`（跨所有 test binary 求和，如 293/184）比对，
//! 而种子二进制自身真值是 `0 passed; N failed`——诚实 §3 被 E12 判「报数造假」，与 E9 verifier 的
//! 逐字忠实要求冲突无解。本棒把该判据抽成纯函数并改为**面向文件内计数**：assertion-red 只核
//! 「声称失败数==文件内失败数 ∧ 声称通过数==文件内通过数」，不再强求 §3 复述跨-binary 聚合 passed。
//! 与 O5 判据（文件内 passed≤阈值 ∧ 总红==文件红）对齐，让诚实 §3 同时过 E12 与 E9。
//!
//! 目标契约（落在 orch_host::oracle，模块已 pub 导出，勿动 lib.rs）：
//!  - pub fn report_claim_ok(claim: &SuiteCounts, measured: &Measured) -> Result<(), String>
//!      · measured.red_form == Some("compile")：直接 Ok（编译红无计数可比，数字比对不适用）；
//!      · 否则（assertion）：claim.failed == measured.file_failed
//!            ∧ claim.passed == measured.file_passed ⇒ Ok；否则 Err（文案含声称与文件内实测）。
//!      · **不得**再拿 claim 与 measured.total_failed / total_passed（跨-binary 聚合）比对。
//!
//! 负向变异下界（转绿后逐条自证）：
//!  M1 assertion 仍拿 total_passed 比对（回退 O15）
//!       ⇒ assertion_accepts_honest_file_internal_passed 红；
//!  M2 不核失败数 / 核错
//!       ⇒ assertion_rejects_wrong_failed_count 红；
//!  M3 compile-red 不豁免数字比对
//!       ⇒ compile_red_skips_numeric_compare 红。

use orch_host::oracle::{report_claim_ok, Measured, SuiteCounts};

fn suite(failed: usize, passed: usize) -> SuiteCounts {
    SuiteCounts { failed, passed, total: failed + passed }
}

fn measured(file_failed: usize, file_passed: usize, total_passed: usize, red_form: &str) -> Measured {
    Measured {
        file_failed,
        file_passed,
        total_failed: file_failed, // 全红局部在种子文件（baseline 不受扰）
        total_passed,
        total: file_failed + total_passed,
        failed_cases: Vec::new(),
        red_form: Some(red_form.to_string()),
    }
}

#[test]
fn assertion_accepts_honest_file_internal_passed() {
    // 诚实 §3：种子二进制 0 passed; 3 failed；整套 workspace 有 293 passed。
    // 修复后：只核文件内(3f/0p)，不管聚合 293 ⇒ Ok。
    let claim = suite(3, 0);
    let m = measured(3, 0, 293, "assertion");
    assert_eq!(report_claim_ok(&claim, &m), Ok(()));
}

#[test]
fn assertion_rejects_wrong_failed_count() {
    // 声称 2 失败但文件内实测 3 失败 ⇒ Err（真报数不符仍要拦）。
    let claim = suite(2, 0);
    let m = measured(3, 0, 184, "assertion");
    assert!(report_claim_ok(&claim, &m).is_err());
}

#[test]
fn compile_red_skips_numeric_compare() {
    // 编译红：数字比对不适用，直接 Ok（即使 claim 与文件内不符）。
    let claim = suite(0, 0);
    let m = measured(3, 0, 0, "compile");
    assert_eq!(report_claim_ok(&claim, &m), Ok(()));
}
