#![allow(dead_code)]
//! B271 seeded-red contract：门夹具登记表的 birth mismatch 必须**跳过并留痕**，
//! 而不是中止整个收割（H146）。**这是 r70/B258 的后继卡：新 task ID、新 seed target。**
//!
//! Expected red: **compile**。`gate::reap_gate_fixture_registry_reporting` 与
//! `gate::GateRegistryReaping` 尚不存在 ⇒ `error[E0432]: unresolved import`。
//!
//! ── 🔴 B258 为什么死掉，以及本种子如何逐条避开 ──
//!
//! r70/B258 以 canonical `AttemptBlocked`（`blockerClass=frozen-seed-unsatisfiable`）终结，
//! 烧掉一整棒。**两个独立的冻结合同缺陷，都是 planner 的种子设计错误**：
//!
//! **缺陷 1**：seed 调用 `Command::process_group(0)` 却没在该 integration-test crate 引入
//! `std::os::unix::process::CommandExt` ⇒ 产品 API 补齐后仍**稳定 compile-red E0599**
//! （依赖 crate 无法向另一个 crate 的 lexical scope 注入 trait）。
//! **本种子的规避**：**根本不 spawn 任何进程**——全部判据要么是纯行为（喂一份登记表文件），
//! 要么是源码窗口扫描。因此**不需要** `CommandExt`，也就不会重演该 E0599。
//! （刻意不加那条 `use`：未被使用的 import 会产生 warning，而快门要求 `cargo check` 0 新增 warning。）
//!
//! **缺陷 2**：B258 的新 seed 要求 `run_gate_command` 窗口内旧调用
//! `reap_gate_fixture_registry(` **恰为 0**；而已落位并冻结的
//! `gate_fixture_registry_reaping.rs:192` 要求**同一窗口内至少 2 次**。两条断言互斥。
//! **本种子的规避**：
//!   ① **完全不断言**那个计数（断言 `>=2` 与旧合同重复，零判别力）；
//!   ② planner 已机械核对一条更隐蔽的兼容性：
//!      `"reap_gate_fixture_registry("` **不是** `"reap_gate_fixture_registry_reporting("` 的子串
//!      ⇒ 若实现把两处调用**改名**为 reporting 变体，旧冻结测试当场红。
//!      故本卡的合同是：**旧函数保名、保签名、保两处调用点**，reporting 变体**新增**，由旧函数委派。
//!      本种子的接线断言因此指向**旧函数的定义窗口**，而不是 `run_gate_command` 窗口。
//!
//! ── 缺陷本体（H146）──
//!
//! `reap_gate_fixture_registry_with_control` 的收割循环里：
//! ```text
//! reap_gate_process_group_kernel(pgid, recorded_birth, ReapPolicy::RegisteredFixture, control)?;
//! ```
//! 那个 `?` 会**中止整个循环**——一条 birth mismatch（PID 已被无关进程复用）
//! 就让登记表里**其余所有条目都不再被收割**，夹具进程继续泄漏。
//!
//! 而同一个循环对 torn / empty / malformed 三类坏行**早就是「跳过 + 计数 + 汇总」**。
//! ⇒ birth mismatch 只是**少了一个同族的跳过分支**，不是需要新机制。
//!
//! ── 留痕必须是返回值，不能只有 stderr（r69 成因 3）──
//!
//! 今天 `skipped` 只是个**局部变量**：打完 `eprintln!` 就被丢弃，调用方拿不到。
//! r69 的教训是「拒收只写 journal 等于没写」——本卡要求把跳过分类**放进返回值**。
//!
//! ── Negative mutations that must turn the named case red ──
//!
//! M1. birth mismatch 仍用 `?` 中止整个收割
//!     -> `the_reaping_loop_skips_a_birth_mismatch_instead_of_aborting` 红。
//! M2. 跳过了但不进返回值（只 eprintln）
//!     -> `malformed_lines_are_counted_in_the_returned_report` 红。
//! M3. **接线变异**：reporting 变体存在但旧函数不委派给它
//!     -> `the_legacy_entry_point_delegates_to_the_reporting_variant` 红。
//! M4. 登记表不存在时报错而不是返回空报告
//!     -> `a_missing_registry_is_an_empty_report_not_an_error` 红。
//! M5. 重复收割不再幂等
//!     -> `reaping_twice_stays_idempotent` 红。
//! M6. 把跳过做成静默丢弃：坏行既不计数也不出现在报告里
//!     -> `every_skipped_line_is_visible_in_the_report` 红。

use std::fs;

use orch_host::gate::{reap_gate_fixture_registry_reporting, GateRegistryReaping};
use orch_host::util::test_scratch_dir;

const GATE_SRC: &str = include_str!("../src/gate.rs");

fn window(anchor: &str) -> &'static str {
    let start = GATE_SRC
        .find(anchor)
        .unwrap_or_else(|| panic!("B271: missing source anchor {anchor:?}"));
    let tail = &GATE_SRC[start + anchor.len()..];
    let end = tail
        .find("\nfn ")
        .or_else(|| tail.find("\npub fn "))
        .unwrap_or(tail.len());
    &GATE_SRC[start..start + anchor.len() + end]
}

// ── 行为：坏行必须进返回值（M2 / M6）────────────────────────────────────

#[test]
fn malformed_lines_are_counted_in_the_returned_report() {
    let root = test_scratch_dir("b271-malformed");
    let registry = root.join("registry.tsv");
    // 一条字段数不对、一条空行、一条 pid=0 —— 三类既有的坏行
    fs::write(&registry, "not-a-line\n\n0\t0\tbirth\n").expect("B271: 写登记表失败");

    let report: GateRegistryReaping =
        reap_gate_fixture_registry_reporting(&registry).expect("B271: 收割不得因坏行整体失败");

    assert!(
        report.skipped_malformed >= 3,
        "B271 M2: 今天 skipped 只是个被丢弃的局部变量——\
         「拒收只写 journal 等于没写」（r69 成因 3）。实得 {}",
        report.skipped_malformed
    );
}

#[test]
fn every_skipped_line_is_visible_in_the_report() {
    let root = test_scratch_dir("b271-visible");
    let registry = root.join("registry.tsv");
    fs::write(&registry, "garbage\n").expect("B271: 写登记表失败");

    let report =
        reap_gate_fixture_registry_reporting(&registry).expect("B271: 收割不得因坏行整体失败");
    assert!(
        report.processed.is_empty(),
        "B271: 坏行不得被当成已处理"
    );
    assert!(
        report.skipped_malformed > 0,
        "B271 M6: 静默丢弃就是又一次把归因成本转嫁给人"
    );
}

// ── 行为：缺文件 / 幂等（M4 / M5）───────────────────────────────────────

#[test]
fn a_missing_registry_is_an_empty_report_not_an_error() {
    let root = test_scratch_dir("b271-missing");
    let report = reap_gate_fixture_registry_reporting(&root.join("nope.tsv"))
        .expect("B271 M4: 登记表不存在是正常情况，不是错误");
    assert!(report.processed.is_empty());
    assert_eq!(report.skipped_malformed, 0);
}

#[test]
fn reaping_twice_stays_idempotent() {
    let root = test_scratch_dir("b271-idempotent");
    let registry = root.join("registry.tsv");
    fs::write(&registry, "garbage\n").expect("B271: 写登记表失败");

    let first = reap_gate_fixture_registry_reporting(&registry).expect("B271: 首次收割");
    let again = reap_gate_fixture_registry_reporting(&registry).expect("B271 M5: 重复收割必须幂等成功");
    assert_eq!(first.processed, again.processed);
}

// ── 源码：mismatch 必须是跳过分支，不是 `?` 中止（M1）───────────────────

#[test]
fn the_reaping_loop_skips_a_birth_mismatch_instead_of_aborting() {
    let scope = window("fn reap_gate_fixture_registry_with_control(");
    assert!(
        scope.contains("skipped_birth_mismatch"),
        "B271 M1: 今天那条 `reap_gate_process_group_kernel(...)?` 的 `?` 会中止整个循环——\
         一条 PID 复用就让登记表里其余所有夹具都不再被收割。\
         同一个循环对 torn/empty/malformed 早已是「跳过 + 计数 + 汇总」，\
         birth mismatch 只是少了一个同族分支"
    );
}

// ── 接线变异（M3）：旧入口必须委派，而不是被改名 ─────────────────────────

#[test]
fn the_legacy_entry_point_delegates_to_the_reporting_variant() {
    let scope = window("pub fn reap_gate_fixture_registry(");
    assert!(
        scope.contains("reap_gate_fixture_registry_reporting("),
        "B271 M3: 旧函数必须**保名、保签名、保 run_gate_command 里的两处调用点**并委派——\
         把调用点改名会当场打破已冻结的 gate_fixture_registry_reaping.rs:192（要求同窗口 >=2），\
         那正是 B258 死掉的第二个原因"
    );
}
