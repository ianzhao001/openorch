//! B287 · H185 / H186 测试夹具 —— 冻结契约种子。
//!
//! **首红：compile `error[E0583]: file for module `round_state_independent_fixtures_support` not found`。**
//! support 只是零判别力管道 marker（`pub fn contract_loaded() {}`），落点是目录形态
//! `tests/round_state_independent_fixtures_support/mod.rs`，不新增 Cargo target。
//!
//! # 本卡钉什么
//!
//! `stale_binary_cli.rs` 的 fixture **克隆真实仓根**，因而继承主仓此刻的轮态；
//! 而 `assert_stale_read_allowed` 要求 `orch snapshot` **exit 0**，
//! `snapshot` 对未签核 / 已漂移的 IR 按设计 fail-closed。二者直接矛盾。
//!
//! 已实测的**两个**触发窗口，本种子都要盯：
//!
//! - **T1**：`round open` 之后、`plan sign-off` 之前（r74 实撞，签核后自动转绿）；
//! - **T2**：任何改动 `coordination/agents.yaml` / `coordination/tools/*.yaml` 的**提交**之后、
//!   下一次 `orch plan` 之前（r75 开轮期实撞并复算：r74 IR 记录 `18f1421f…`，
//!   main `6a77e2a4` 之后为 `f9294895…` ⇒ main 自那时起 `orch snapshot` 就是 exit 5）。
//!
//! ⚠️ **T2 带一个恶劣的盲区**：该 fixture 克隆的是**已提交**状态，
//! 所以「提交前跑全量门」**天然看不见 T2**——r74 就是这样漏掉的。
//! 修法必须让测试**不再依赖主仓轮态**，而不是靠「记得提交后再跑一次门」这种人肉纪律。
//!
//! # ⚠️ 诚实边界（写死在这里）
//!
//! - **绝不允许放宽 `orch snapshot` 的 fail-closed 语义**来迁就测试。
//!   它对未签核 / 已漂移 IR 拒绝是**正确行为**；被测的性质是「陈旧二进制仍允许只读命令」，
//!   不是「轮态」。本种子专门有一条断言守住这一点。
//! - **绝不允许**用 `#[ignore]` / `cfg` / 条件跳过 / 放宽断言把测试静音。
//! - **H186 未复现就不改测试**（R3 分支），本种子不对 H186 的结论作任何预设。
//!
//! # M 变异 ↔ 载体 一一对应
//!
//! | M | 注入 | 必红的载体 |
//! |---|---|---|
//! | M1 | 夹具改回以主仓轮态为输入 | `the_stale_fixture_is_round_state_independent` |
//! | M2 | 只覆盖 T1，不覆盖 T2 | `both_h185_triggers_have_a_regression` |
//! | M3 | 放宽 snapshot 对未签核 IR 的 fail-closed | `snapshot_still_fails_closed_on_an_unusable_ir` |
//! | M4 | 给目标用例挂 `#[ignore]` | `no_test_is_silenced` |
//! | M5 | 未复现却改了 verdict_seal_cli | `h186_is_not_touched_without_reproduction` |
//! | M6 | H186 harness 只跑一次就下结论 | `the_h186_harness_repeats_enough_times` |

#![allow(dead_code)]

mod round_state_independent_fixtures_support;

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn read_repo(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("读 {rel} 失败: {error}"))
}

/// 去掉 `//` 注释行后的源码。防止「把断言要求的字符串写进注释」这类假绿。
fn code_lines(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

const STALE: &str = "orch/crates/orch-cli/tests/stale_binary_cli.rs";
const H186_HARNESS: &str = "orch/scripts/h186-parallel-flake-attribution.sh";

// ------------------------------------------------- ① 夹具不再吃主仓轮态

#[test]
fn the_stale_fixture_is_round_state_independent() {
    let source = code_lines(&read_repo(STALE));

    assert!(
        source.contains("B287"),
        "{STALE} 必须带 B287 的显式标记，说明它已按本卡改造为轮态无关；\
         没有标记说明根本没动过"
    );
    assert!(
        !source.contains(".arg(\"clone\").arg(repo_root())")
            || source.contains("coordination/rounds"),
        "夹具要么不再整仓克隆，要么必须显式处置克隆进来的 coordination/rounds/** —— \
         二者必居其一。原状是「整仓克隆且原样继承轮态」，那正是 H185 的病根"
    );
}

/// **T1 与 T2 必须各有一个可执行回归。** 只修先发现的那一半不算修好。
#[test]
fn both_h185_triggers_have_a_regression() {
    let source = code_lines(&read_repo(STALE));

    let t1 = source.contains("unsigned") || source.contains("sign_off") || source.contains("signoff");
    let t2 = source.contains("registry") || source.contains("agent_registry_digest");

    assert!(
        t1,
        "缺 T1（已开轮、未 sign-off 窗口）的回归。r74 实撞过这一型"
    );
    assert!(
        t2,
        "缺 T2（registry 提交后、下一次 orch plan 之前）的回归。\
         r75 开轮期实撞过这一型，而且它有「提交前跑门看不见」的盲区，\
         **比 T1 更容易再犯**"
    );
}

// ------------------------------------------------- ② 产品语义不得被迁就

/// M3 的载体：`snapshot` 对不可用 IR 的 fail-closed 是**正确行为**，不得为了让测试绿而放宽。
#[test]
fn snapshot_still_fails_closed_on_an_unusable_ir() {
    let plan_src = read_repo("orch/crates/orch-host/src/plan.rs");
    assert!(
        plan_src.contains("尚未绑定 PlanSignedOff"),
        "require_active_round_ir 必须继续在 IR 未绑定 PlanSignedOff 时 bail —— \
         这是被测性质之外的产品语义，不得为迁就夹具而放宽"
    );
    assert!(
        plan_src.contains("agent_registry_digest"),
        "registry 摘要必须继续参与 IR 校验"
    );
}

/// M4 的载体。
#[test]
fn no_test_is_silenced() {
    let source = read_repo(STALE);
    for forbidden in ["#[ignore", "serial_test", "test_threads", "test-threads"] {
        assert!(
            !source.contains(forbidden),
            "{STALE} 不得用 {forbidden:?} 把问题静音。\
             把测试关掉不是修好它"
        );
    }
    assert!(
        source.contains("stale_snapshot_distinguishes_read_from_write"),
        "被测用例必须仍然存在——删掉它同样是静音"
    );
}

// ------------------------------------------------- ③ H186：先复现再谈修

/// M5 的载体：未复现就不许动那个测试。本种子只断言「没被动过」，
/// 不对 H186 的结论作任何预设——R1/R2 触发时由 planner 裁决后另立卡改。
#[test]
fn h186_is_not_touched_without_reproduction() {
    let root = repo_root();
    let target = root.join("orch/crates/orch-cli/tests/verdict_seal_cli.rs");
    assert!(target.is_file(), "verdict_seal_cli.rs 必须仍然存在");

    let source = read_repo("orch/crates/orch-cli/tests/verdict_seal_cli.rs");
    for forbidden in ["#[ignore", "serial_test", "test_threads", "test-threads"] {
        assert!(
            !source.contains(forbidden),
            "verdict_seal_cli.rs 不得被静音 ({forbidden:?})。\
             H186 只有 1 次他报红 / 2 次 planner 自测绿，样本不足以定性；\
             在没有复现的情况下改测试 = 用一次猜测替换一次取证"
        );
    }
}

#[test]
fn the_h186_harness_repeats_enough_times() {
    let path = repo_root().join(H186_HARNESS);
    assert!(
        path.is_file(),
        "H186 必须先有可重跑的归因 harness：{H186_HARNESS}。\
         一次性手跑不叫复现实验"
    );
    let source = fs::read_to_string(&path).expect("harness 必须可读");
    let code = code_lines(&source);
    assert!(
        code.contains("10") || code.contains("REPEAT") || code.contains("repeat"),
        "harness 必须支持 N ≥ 10 次重复并可参数化；\
         跑一次就下结论无法区分 flake 与真红"
    );
}

// ------------------------------------------------- ④ 管道 marker

#[test]
fn the_support_module_is_wired() {
    round_state_independent_fixtures_support::contract_loaded();
}
