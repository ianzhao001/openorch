#![allow(dead_code)]
//! B270 seeded-red contract：受限的 frozen-contract supersession（H154），
//! 并先给「已落位冻结 seed 不得修改」一个**机械后盾**。
//!
//! Expected red: **compile**。`oracle::single_hash_literal_swap` 与
//! `oracle::frozen_contract_supersession_authorized` 尚不存在 ⇒ `error[E0432]`。
//!
//! ── 本卡由 r71 Q1 的五方咨询定稿（裁决书见 `planning/q1-fusion-adjudication.md`）──
//!
//! 成员 opencode / zcode / pi / codex 四家 + SmartClaw 场外一票 + planner 独立答卷。
//! **planner 自己的初稿方案被推翻了两次**，最终采纳 SmartClaw 的**方案 E**。
//!
//! ── 🔴 咨询挖出的、问卷根本没问到的 P0 ──
//!
//! pi（发现 2）与 zcode（攻击面 1）**独立收敛**，planner 逐行复核确认：
//! **协议铁律「已落位冻结 seed 不得修改」在 runtime 层完全无人守卫。**
//!
//!   * `plan.rs:2536` `source_file_bytes(root, &seed.src, …)` —— **只校验 `seed.src`**；
//!   * `oracle.rs:287-311` `validate_seed_paths` —— 只查 target ∈ writeSet、∉ frozenPaths、
//!     不重复/不互为祖先，**零字节校验**；
//!   * 全仓 `SeedRelocated` 只被**写入**（`mech.rs:16` 注释、`collect.rs:737`），
//!     **没有任何代码读它做守卫**。
//!
//! ⇒ 今天不需要任何新机制，一张新卡只要把某个已落位的冻结测试放进 `writeSet`，
//!    就能把 `assert_eq!(hash, "f1bcbc…")` 改成 `assert!(true)`，门照样绿。
//!
//! ── 为什么校验必须**非追溯**（planner 的全量对账，见 `planning/r71-landed-seed-drift-audit.md`）──
//!
//! 243 组 `(seed.src, seed.target)` 中 **203 逐字节一致、36 漂移**。
//! 追溯式硬校验会**当场打红 36 个文件** —— 那就是第二张 B257。
//! 因此本卡采用**基线化 + 显式披露**：把当前分支头的落位摘要记为新锚点，此后偏离即 fail-closed。
//! （**明确不据 36 条断言曾发生洗绿**：粗筛是逐行 exact 匹配，跨行 assert / rustfmt 重排版 /
//! 消息串改写都会误判。语义定性是独立工作，登 backlog。）
//!
//! ── 方案 E（SmartClaw 提出、planner 采纳）为什么胜过前四方一致的「外部 skip」──
//!
//! B253 的哈希断言与**其余 26 条窄断言共处同一个可执行 `#[test]`**（共 27 条）。
//! 任何「跳过该 test」的方案（`#[ignore]` / 门层 skip / `--skip`）都会**团灭**那 26 条；
//! 而该测试自己在 `:128-133` 就**明令禁止** `#[ignore]`/`serial_test`/`Mutex`/`test_threads`。
//! 方案 E 只置换那**一处 64 位十六进制字面量**，26 条窄断言**全部继续执行**，
//! 且不需要改门命令或 `PROJECT-BINDING.yaml`（pi 指出的「执行者裸 cargo 永久红」陷阱因此消失）。
//!
//! ── 四条被咨询挖出的攻击（本种子逐条钉死）──
//!
//! A1（claw）**冻结-退役两步洗绿**：先故意立一张过度冻结的种子卡（现有闸只扫已落位测试、
//!    **不扫本轮 `seeds/`**，当时的 `check-frozen-prefix-conflicts.sh:52`（B328 已退役）），下轮再「合法地」以
//!    不可满足为由退役它 ⇒ supersession 让过度冻结从错误变成有用的第一步。
//! A2（pi S2）**先改后退役**：先篡改冻结文件，再声明退役、payload 填**篡改后**的 sha
//!    ⇒ 若只验「当前字节 == payload」，篡改被官方记录合法化。
//! A3（zcode 攻击面 3）**伪造退役事件**：若允许 `actor` 不是 `runtime:orch`，
//!    或只要求 `PlanSignedOff` 而不要求超越合同已 `TaskRecorded`，
//!    就能在替代合同尚未实现/验证时先把旧合同退役。
//! A4（claw 攻击 4）**置换夹带**：置换 commit 在同一 hunk 里顺手改一条窄断言串
//!    ⇒ 若只查「有一个 64-hex 变了」而非「**只有** 64 字符窗口内字节变」，夹带通过。
//!
//! ── Negative mutations that must turn the named case red ──
//!
//! M1. 置换校验只看「有 hex 变了」，不查窗口外零字节（A4）
//!     -> `a_swap_that_smuggles_another_edit_is_refused` 红。
//! M2. 允许一次置换多个字面量
//!     -> `changing_two_hash_literals_at_once_is_refused` 红。
//! M3. 退役 payload 的 old sha 不与**当前落位字节**核对（A2 前半）
//!     -> `a_declared_old_digest_that_disagrees_with_the_landed_bytes_is_refused` 红。
//! M4. 退役 payload 的 old sha 不与**历史 SeedRelocated** 核对（A2 后半：先改后退役）
//!     -> `a_declared_old_digest_that_disagrees_with_the_original_relocation_is_refused` 红。
//! M5. 允许 `actor` 不是 `runtime:orch`（A3 前半）
//!     -> `only_the_runtime_may_author_a_supersession` 红。
//! M6. 超越合同尚未 `TaskRecorded` 就允许退役（A3 后半）
//!     -> `a_supersession_before_its_replacement_is_recorded_is_refused` 红。
//! 原 M7 的永久 landed 接线在 B328 退役；当前 seed 与后继 target 演进由
//! `recorded_seed_evolution_v1` 通过真实机检验证，历史源/事件/提交只读。
//! M8. 把闸修成全拦：合法置换也被拒
//!     -> `a_clean_single_literal_swap_is_accepted` 红。
//! M9. 把闸修成全拦：条件齐备的退役也被拒
//!     -> `a_fully_anchored_supersession_is_authorized` 红。

use orch_host::oracle::{frozen_contract_supersession_authorized, single_hash_literal_swap};

// Fixed hexadecimal fixture values exercise historical digest authorization only.
fn hex_of(seed: &str) -> String {
    seed.repeat(64 / seed.len())
}

fn old_hex() -> String {
    hex_of("f1bcbc72")
}

fn new_hex() -> String {
    hex_of("01234567")
}

fn other_hex() -> String {
    hex_of("fedcba98")
}

fn contract(hex: &str, marker: &str) -> String {
    format!(
        "assert_eq!(digest, \"{hex}\", \"B253: prefix\");\nassert!(tests.contains(\"{marker}\"));\n"
    )
}

// ── 方案 E 的 diff 形状机检 ────────────────────────────────────────────────

#[test]
fn a_clean_single_literal_swap_is_accepted() {
    let before = contract(&old_hex(), "isolated_fusion_repo");
    let after = contract(&new_hex(), "isolated_fusion_repo");
    let swap = single_hash_literal_swap(&before, &after)
        .expect("B270 M8: 干净的单字面量置换必须被接受");
    assert_eq!(swap, (old_hex(), new_hex()));
}

#[test]
fn a_swap_that_smuggles_another_edit_is_refused() {
    let before = contract(&old_hex(), "isolated_fusion_repo");
    // 同一次置换里顺手把窄断言的标记串也改了 —— claw 攻击 4
    let after = contract(&new_hex(), "isolated_fusion_repoX");
    assert!(
        single_hash_literal_swap(&before, &after).is_none(),
        "B270 M1: 判据必须是「**只有** 64 字符窗口内字节变」，\
         而不是「有一个 64-hex 变了」——否则夹带一路通过"
    );
}

#[test]
fn changing_two_hash_literals_at_once_is_refused() {
    let before = format!("{}{}", contract(&old_hex(), "a"), contract(&other_hex(), "b"));
    let after = format!("{}{}", contract(&new_hex(), "a"), contract(&new_hex(), "b"));
    assert!(
        single_hash_literal_swap(&before, &after).is_none(),
        "B270 M2: 一次退役只对应一处置换；批量置换等于批量豁免"
    );
}

// ── 退役授权：四个锚点缺一不可 ────────────────────────────────────────────

#[test]
fn a_fully_anchored_supersession_is_authorized() {
    let outcome = frozen_contract_supersession_authorized(
        "runtime:orch",
        true,
        Some(&old_hex()),
        &old_hex(),
        &old_hex(),
    );
    assert!(
        outcome.is_ok(),
        "B270 M9: 条件齐备时必须放行，否则等于把欠账永久钉死；实得 {outcome:?}"
    );
}

#[test]
fn only_the_runtime_may_author_a_supersession() {
    let outcome =
        frozen_contract_supersession_authorized(
            "planner",
            true,
            Some(&old_hex()),
            &old_hex(),
            &old_hex(),
        );
    assert!(
        outcome.is_err(),
        "B270 M5: 退役必须与 SiteRetired 同型严格授权——planner 不得直写"
    );
}

#[test]
fn a_supersession_before_its_replacement_is_recorded_is_refused() {
    let outcome = frozen_contract_supersession_authorized(
        "runtime:orch",
        false,
        Some(&old_hex()),
        &old_hex(),
        &old_hex(),
    );
    assert!(
        outcome.is_err(),
        "B270 M6: 超越合同尚未 TaskRecorded 就退役旧合同 = 保护净损失、且可以一轮拖一轮"
    );
}

#[test]
fn a_declared_old_digest_that_disagrees_with_the_landed_bytes_is_refused() {
    let outcome = frozen_contract_supersession_authorized(
        "runtime:orch",
        true,
        Some(&old_hex()),
        &old_hex(),
        &other_hex(), // 当前落位字节已经不是声明的那份
    );
    assert!(
        outcome.is_err(),
        "B270 M3: 声明值必须与**当前落位字节**一致，否则退役的是一份想象中的合同"
    );
}

#[test]
fn a_declared_old_digest_that_disagrees_with_the_original_relocation_is_refused() {
    let outcome = frozen_contract_supersession_authorized(
        "runtime:orch",
        true,
        Some(&other_hex()), // 历史 SeedRelocated 记的是另一份
        &old_hex(),
        &old_hex(),
    );
    assert!(
        outcome.is_err(),
        "B270 M4: 没有历史锚，攻击者可以「先篡改、再退役」，用官方记录把篡改合法化（pi S2）"
    );
}

// B328: retired permanent-landed source-text assertion M7. The historical authorization
// checks above remain; current seed immutability is exercised through real candidate checks
// in recorded_seed_evolution_v1 rather than a source-text substring.
