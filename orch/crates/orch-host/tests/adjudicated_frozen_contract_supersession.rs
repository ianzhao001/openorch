//! B288 · 让落地冻结契约可以被 planner 裁定地合法演进 —— 冻结契约种子。
//!
//! **首红：compile `error[E0583]: file for module `adjudicated_frozen_contract_supersession_support` not found`。**
//! support 只是零判别力管道 marker（`pub fn contract_loaded() {}`），落点是目录形态
//! `tests/adjudicated_frozen_contract_supersession_support/mod.rs`，不新增 Cargo target。
//!
//! # 本卡钉什么
//!
//! 同一个结构性堵点已经烧过三次：r70/B257 烧掉一整棒 attempt；r74 的 B284 被判「无合法
//! 绿解」移出该轮；r75 开轮期又试了一次 seed 路径，被 `orch plan` 当场硬拒。
//!
//! 三条路径已逐条复核到函数：
//!
//! - `oracle.rs::validate_seed_paths`（`plan.rs:2478` 与 `round.rs:405` 调用，
//!   `plan.rs` 注释明写是**授权边界**）：只遍历 `c.meta.seeds`，
//!   seed 的 src 字节 ≠ 已落位字节 ⇒ 硬拒，**无 supersession 逃生**；
//! - `oracle.rs::validate_landed_seed_digests_exact`（自 `mech.rs`）：对 **candidate diff**
//!   生效，改了冻结 target 而无签名 supersession ⇒ 拒；**有**合法声明则放行；
//! - `card.rs::validate_frozen_contract_supersessions`：**无条件**要求 `blockedAttempt`
//!   与 `replacement.taskRecordedEventId` 两个真实事件锚点。
//!
//! ⇒ **writeSet 直接编辑本来就是被授权的通道**（seed 是用来创建的，不是用来改的），
//! 但钥匙被锁在「必须先真的撞死一棒 attempt」里——那是为**事后恢复**设计的，
//! 不是为**事前裁定**设计的。本种子钉的就是那把事前裁定的钥匙。
//!
//! # ⚠️ 诚实边界（写死在这里）
//!
//! - **新变体不是「planner 想删就删」。** 它去掉两个事件锚点，就必须补上一组**等价强度**
//!   的证据：已提交且摘要绑定的裁定文书、逐条 `removedAssertions`（每条必须说明
//!   **哪一条更窄的断言仍然表达了原意图**）、以及可追溯到用户原话的授权记录。
//!   `retainedCoverage` 是核心，不是形式——没有它，这个变体就退化成后门。
//! - **既有 recovery 变体一个字节不放宽。** 本种子专门有一条断言守住它。
//! - **不动 `oracle.rs`**：无声明的冻结改动仍然硬拒。
//!
//! # M 变异 ↔ 载体 一一对应
//!
//! | M | 注入 | 必红的载体 |
//! |---|---|---|
//! | M1 | 放行空 `removedAssertions` | `every_removed_assertion_names_its_retained_coverage` |
//! | M2 | 允许缺 `retainedCoverage` | 同上 |
//! | M3 | 缺 `adjudication` 或不校验其摘要 | `an_adjudication_must_be_committed_and_digest_bound` |
//! | M4 | `userAuthorization.actor` 允许非 user | `a_planner_adjudicated_supersession_needs_no_blocked_attempt` |
//! | M5 | 顺手放宽 recovery 变体的锚点要求 | `the_recovery_variant_keeps_working` |
//! | M6 | 允许同时声明两种授权 | `the_two_authorization_variants_are_mutually_exclusive` |
//! | M7 | 允许「裸 supersession」 | 同上 |
//! | M8 | 删掉 `oracle.rs` 的无声明拒绝路径 | `an_undeclared_frozen_edit_is_still_refused` |

#![allow(dead_code)]

mod adjudicated_frozen_contract_supersession_support;

use std::fs;
use std::path::{Path, PathBuf};

use orch_host::card::{self, CardMeta};

// ---------------------------------------------------------------- 公共工具

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

const OLD_FILE: &str = "ea403bd7f55dd85d5df8fbb381f7fb8faae2626aa06f0928f00a0143376a0097";
const NEW_FILE: &str = "45e7a5ac49314267d4a592127c0dfc90b204b5ed40d54d4c41581d33193c7861";
const OLD_LIT: &str = "f1bcbc72087f8e45ddccdb3f0e876f9d9818ae08bd6a6b83b3ca6eb4f9b8df4f";
const NEW_LIT: &str = "1edf194359e845e18b800a38a0c1997356c65624bd3aacab3eaa0001d47e050e";
const ADJ_SHA: &str = "9f2b1c4d5e6a7b8c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e";
const RELOC_EVENT: &str = "01KZQH53Q27EG05H528T6Y6CAF";
const BLOCKED_EVENT: &str = "01M0BAKZJ6VD4GFWH1FT3BHFGW";
const RECORDED_EVENT: &str = "01M0CM4BFQ1CZTWJFR92AZSG0V";
const TARGET: &str = "orch/crates/orch-host/tests/fusion_fixture_repository_isolation_v2.rs";

/// 两个变体共用的躯干。`authorization_block` 是唯一变量。
fn meta_yaml(authorization_block: &str) -> String {
    format!(
        r#"taskId: B288
frozenContractSupersessions:
  - target: {TARGET}
    initiator: B288
    originalSeedRelocated:
      eventId: {RELOC_EVENT}
      sha256: {OLD_FILE}
    effectiveAnchor:
      kind: seed-relocated
      eventId: {RELOC_EVENT}
      sha256: {OLD_FILE}
    oldFileSha256: {OLD_FILE}
    newFileSha256: {NEW_FILE}
    oldLiteralSha256: {OLD_LIT}
    newLiteralSha256: {NEW_LIT}
    subjectPrefix:
      path: orch/crates/orch-host/src/fusion.rs
      bytes: 26950
    dispositions:
      - oldAssertion: "B253: fusion production prefix must remain byte-identical to the signed baseline"
        exemptionReason: "整体前缀哈希超出该契约自述意图，意图已由 run_fusion 窗口断言表达"
    reviews: []
{authorization_block}
"#
    )
}

/// 既有 recovery 授权（两个真实事件锚点）。
const RECOVERY_AUTH: &str = r#"    blockedAttempt:
      round: r73
      taskId: B279
      attemptId: B279-A0002
      eventId: 01M0BAKZJ6VD4GFWH1FT3BHFGW
    replacement:
      round: r74
      taskId: B283
      taskRecordedEventId: 01M0CM4BFQ1CZTWJFR92AZSG0V"#;

fn parse(meta_yaml_text: &str) -> Result<CardMeta, String> {
    serde_yaml::from_str::<CardMeta>(meta_yaml_text).map_err(|error| error.to_string())
}

/// 解析 + 校验，两步任一失败都算「被拒」。
fn accepted(meta_yaml_text: &str) -> Result<(), String> {
    let meta = parse(meta_yaml_text)?;
    card::validate_frozen_contract_supersessions(&meta).map_err(|error| error.to_string())
}

fn planner_auth(
    with_adjudication: bool,
    removed: &str,
    actor: &str,
) -> String {
    let adjudication = if with_adjudication {
        format!(
            "      adjudication:\n        path: coordination/rounds/r75/planning/r75-narrow-b253.md\n        sha256: {ADJ_SHA}\n"
        )
    } else {
        String::new()
    };
    format!(
        "    authorization:\n      kind: planner-adjudicated\n{adjudication}      userAuthorization:\n        actor: {actor}\n        date: \"2026-08-19\"\n        quote: \"加 B288 先修演进通道\"\n      removedAssertions:\n{removed}"
    )
}

const GOOD_REMOVED: &str = r#"        - assertion: "B253: fusion production prefix must remain byte-identical to the signed baseline"
          reason: "整体前缀哈希超出契约自述意图，实际效果是任何卡都不许碰 fusion.rs"
          retainedCoverage: "run_fusion 窗口断言：post-start provision_consult_sites 恰一次且早于 std::thread::scope"
"#;

// ------------------------------------------------- ① 新变体：不需要 blockedAttempt

#[test]
fn a_planner_adjudicated_supersession_needs_no_blocked_attempt() {
    let yaml = meta_yaml(&planner_auth(true, GOOD_REMOVED, "user"));
    assert!(
        !yaml.contains("blockedAttempt"),
        "本用例的输入必须确实不含 blockedAttempt，否则它证明不了任何事"
    );
    accepted(&yaml).unwrap_or_else(|error| {
        panic!(
            "字段齐全的 planner-adjudicated 声明必须被接受（这正是本卡要开的口子）: {error}"
        )
    });

    // actor 必须是 user：裁定要能追溯到用户原话，不能是 planner 自授权。
    let planner_self = meta_yaml(&planner_auth(true, GOOD_REMOVED, "planner"));
    assert!(
        accepted(&planner_self).is_err(),
        "userAuthorization.actor 必须是 user。planner 自授权 = 这个变体退化成后门"
    );
}

// ------------------------------------------------- ② 既有 recovery 变体不得被动

#[test]
fn the_recovery_variant_keeps_working() {
    accepted(&meta_yaml(RECOVERY_AUTH)).unwrap_or_else(|error| {
        panic!("既有 recovery 形态（两个真实事件锚点）必须继续被接受: {error}")
    });

    // 缺 replacement ⇒ 仍然必须被拒。
    let missing_replacement = RECOVERY_AUTH
        .split("    replacement:")
        .next()
        .unwrap()
        .trim_end()
        .to_string();
    assert!(
        accepted(&meta_yaml(&missing_replacement)).is_err(),
        "recovery 变体缺 replacement 锚点仍然必须被拒——本卡不放宽它"
    );
}

// ------------------------------------------------- ③ 裁定文书必须存在且摘要绑定

#[test]
fn an_adjudication_must_be_committed_and_digest_bound() {
    assert!(
        accepted(&meta_yaml(&planner_auth(false, GOOD_REMOVED, "user"))).is_err(),
        "planner-adjudicated 缺 adjudication ⇒ 必须拒绝。\
         去掉两个事件锚点就必须补上等价强度的证据，不能两头都省"
    );

    let bad_sha = meta_yaml(&planner_auth(true, GOOD_REMOVED, "user"))
        .replace(ADJ_SHA, "not-a-canonical-sha256");
    assert!(
        accepted(&bad_sha).is_err(),
        "adjudication.sha256 非 canonical ⇒ 必须拒绝"
    );

    let bad_path = meta_yaml(&planner_auth(true, GOOD_REMOVED, "user"))
        .replace("coordination/rounds/r75/planning/r75-narrow-b253.md", "../escape.md");
    assert!(
        accepted(&bad_path).is_err(),
        "adjudication.path 非规范仓库相对路径 ⇒ 必须拒绝"
    );
}

// ------------------------------------------------- ④ removedAssertions 的实质要求

#[test]
fn every_removed_assertion_names_its_retained_coverage() {
    // 空列表
    assert!(
        accepted(&meta_yaml(&planner_auth(true, "        []\n", "user"))).is_err(),
        "removedAssertions 为空 ⇒ 必须拒绝：没说删了什么，就不是裁定"
    );

    // 缺 retainedCoverage —— 本卡的核心
    let no_coverage = r#"        - assertion: "B253: fusion production prefix must remain byte-identical to the signed baseline"
          reason: "太宽了"
"#;
    assert!(
        accepted(&meta_yaml(&planner_auth(true, no_coverage, "user"))).is_err(),
        "缺 retainedCoverage ⇒ 必须拒绝。\
         它把「我觉得这条太宽」变成「原契约的意图由**这一条**继续守着」——\
         没有它，这个变体就是 planner 想删就删"
    );

    // 重复 assertion
    let duplicated = format!("{GOOD_REMOVED}{GOOD_REMOVED}");
    assert!(
        accepted(&meta_yaml(&planner_auth(true, &duplicated, "user"))).is_err(),
        "removedAssertions 里 assertion 重复 ⇒ 必须拒绝"
    );
}

// ------------------------------------------------- ⑤ 两种授权互斥且必居其一

#[test]
fn the_two_authorization_variants_are_mutually_exclusive() {
    let both = format!("{RECOVERY_AUTH}\n{}", planner_auth(true, GOOD_REMOVED, "user"));
    assert!(
        accepted(&meta_yaml(&both)).is_err(),
        "同时声明 recovery 与 planner-adjudicated ⇒ 必须拒绝；\
         两套证据混用会让「按哪套判」变得不确定"
    );

    assert!(
        accepted(&meta_yaml("")).is_err(),
        "两种授权都不声明的「裸 supersession」⇒ 必须拒绝"
    );
}

// ------------------------------------------------- ⑥ 接线：读源码，不自调

/// M8 的载体：`oracle.rs` 对**无声明**的冻结改动的硬拒必须原样保留。
/// 本卡只加一条合法出口，不拆守门人。
#[test]
fn an_undeclared_frozen_edit_is_still_refused() {
    let oracle = fs::read_to_string(repo_root().join("orch/crates/orch-host/src/oracle.rs"))
        .expect("oracle.rs 必须可读");
    assert!(
        oracle.contains("task 分支修改了已落位冻结 seed 但无签名 supersession"),
        "无声明就改冻结 seed 的拒绝路径必须原样保留——本卡加的是出口，不是拆门"
    );
    assert!(
        oracle.contains("新 seed 试图覆盖已落位 target"),
        "seed 覆盖已落位 target 的硬拒也必须保留：\
         seed 是用来创建的，改动走 writeSet 直接编辑 + 声明"
    );
}

// ------------------------------------------------- ⑦ 管道 marker

#[test]
fn the_support_module_is_wired() {
    adjudicated_frozen_contract_supersession_support::contract_loaded();
}
