//! B290 · 冻结契约：supersession 除了「恰好一个 quoted SHA-256 literal swap」之外，
//! 还必须能表达**结构化 / 多行的断言演进**；该形状必须 **closed**——
//! 既要「从 old + 声明的编辑单元逐字节重建出 new」，**也要**约束编辑**范围**。
//!
//! 来源：`SESSION-HANDOFF-r75.md`「B288 没直接解锁 B284/H184」与 backlog H182 / H184 / H190。
//!
//! 现场根因（复核到卡面冻结时的 HEAD）：`oracle.rs:1296`
//! `validate_declaration_against_snapshots` 在**任何授权分支之前**无条件走
//! `single_hash_literal_swap`（`oracle.rs:1320`）。⇒ H190 与 B284/H184 都表达不出来。
//!
//! ## r76 计划审核加固的两条（本契约的核心）
//!
//! 1. **「重建相等」是必要不充分**。杀手反例（两家独立给出同一条）：
//!    `edit = { oldRange: 0..old.len(), replacement: new }` 能逐字节重建**任意** new。
//!    ⇒ 必须同时钉：旧窗口摘要必验、单元严格有序不重叠、差异全覆盖、禁止整文件 catch-all。
//! 2. **判定必须来自生产入口**。原稿把所有场景经 `support::evaluate(Scenario)` 间接，
//!    执行者可以按枚举返回罐头 `Ok`/`Err`，`oracle.rs` / `card.rs` 一行不改也全绿。
//!    ⇒ 本契约改为：**support 只造 fixture 树与声明，判定一律直调**
//!    `orch_host::oracle::*` 与 `orch_host::verify::*` 的 `pub` 入口。
//!
//! 本契约钉的是**窄不变量**，**不钉 `oracle.rs` 的整段字节**——整段哈希钉死正是本卡要来救的
//! 那三个死结的成因。每条断言一个独立 `#[test]`。

#![allow(dead_code)]

mod structured_contract_evolution_support;

use structured_contract_evolution_support as support;

/// 直调生产：结构化/字面量两种形状都走同一个 `pub` 校验入口。
fn validate(fx: &support::Fixture) -> Result<(), String> {
    orch_host::oracle::validate_frozen_contract_declaration(
        &fx.root,
        &fx.declaration,
        &fx.prior_main,
        &fx.effective_main,
    )
    .map_err(|error| format!("{error:#}"))
}

/// 直调生产：planner-adjudicated 授权分支。
fn validate_adjudicated(fx: &support::Fixture) -> Result<(), String> {
    orch_host::oracle::validate_planner_adjudicated_supersession(
        &fx.root,
        &fx.declaration,
        &fx.prior_main,
        &fx.effective_main,
    )
    .map_err(|error| format!("{error:#}"))
}

// ── ① 结构化形状：闭合重建 ───────────────────────────────────────────────────

#[test]
fn a_structured_evolution_reconstructs_the_new_contract_byte_for_byte() {
    let fx = support::fixture(support::Shape::StructuredHappy);
    validate(&fx).expect("按声明顺序应用编辑单元后逐字节等于 new 的结构化演进必须被接受");
}

#[test]
fn a_multi_line_evolution_is_expressible_at_all() {
    // 这条是本卡存在的理由：H190 / B284 都不是单 hex 互换。
    let fx = support::fixture(support::Shape::StructuredMultiLine);
    assert!(
        support::edit_unit_count(&fx) >= 2,
        "多行样本至少要有两个编辑单元，否则它退化成单点替换、证明不了本卡的能力"
    );
    validate(&fx).expect("跨多行、增删混合的演进必须可被声明并通过校验");
}

#[test]
fn an_unexplained_byte_difference_is_refused() {
    let fx = support::fixture(support::Shape::StructuredUnexplainedByte);
    let error = validate(&fx).expect_err("new 里存在未被任何编辑单元解释的字节时必须 fail-closed");
    assert!(
        error.contains("unexplained byte offset="),
        "拒绝文案必须用稳定标记指出首个未解释字节位置，否则出事时无从定位；实际文案：{error}"
    );
}

// ── ② 范围有界：r76 计划审核抓到的核心缺口 ───────────────────────────────────

#[test]
fn a_whole_file_single_unit_is_refused() {
    let fx = support::fixture(support::Shape::StructuredWholeFileSingleUnit);
    validate(&fx).expect_err(
        "单个覆盖 0..old.len() 的 catch-all 单元能逐字节重建任意 new，\
         实质是任意重写，必须拒绝——这是本卡最重要的一条负例",
    );
}

#[test]
fn overlapping_units_are_refused() {
    let fx = support::fixture(support::Shape::StructuredOverlappingUnits);
    validate(&fx).expect_err("单元区间必须严格递增且互不重叠，否则未解释字节可藏在重叠缝里");
}

#[test]
fn a_unit_with_a_mismatched_old_window_digest_is_refused() {
    let fx = support::fixture(support::Shape::StructuredMismatchedOldWindow);
    validate(&fx).expect_err(
        "单元声明的旧窗口摘要必须等于 old_text 对应区间的实测 sha256；\
         不验它就等于允许「声明错区间、apply 照样覆盖」",
    );
}

#[test]
fn unordered_units_are_refused() {
    let fx = support::fixture(support::Shape::StructuredUnorderedUnits);
    validate(&fx).expect_err("单元必须按起点严格递增，乱序声明必须拒绝");
}

// ── ③ 旧的单 literal 路径逐字不变 ────────────────────────────────────────────

#[test]
fn the_single_literal_swap_helper_keeps_its_exact_semantics() {
    let old_hash = "a".repeat(64);
    let new_hash = "b".repeat(64);
    let before = format!("const PIN: &str = \"{old_hash}\";\n");
    let after = format!("const PIN: &str = \"{new_hash}\";\n");
    assert_eq!(
        orch_host::oracle::single_hash_literal_swap(&before, &after),
        Some((old_hash.clone(), new_hash.clone())),
        "恰好一个 quoted SHA-256 字面量互换必须继续被识别"
    );
    assert!(
        orch_host::oracle::single_hash_literal_swap(&before, &before).is_none(),
        "零差异不是一次 swap"
    );
    let two = format!("const A: &str = \"{new_hash}\";\nconst B: &str = \"{new_hash}\";\n");
    assert!(
        orch_host::oracle::single_hash_literal_swap(&before, &two).is_none(),
        "多处差异不得被当成单 literal swap —— 放宽这条等于给旧路径开后门"
    );
}

#[test]
fn the_literal_shape_still_validates_end_to_end() {
    let fx = support::fixture(support::Shape::LiteralHappy);
    validate(&fx).expect("既有的单 literal 形状必须逐字保持可用");
}

// ── ④ 两个形状互斥，不是退化链 ───────────────────────────────────────────────

#[test]
fn declaring_both_shapes_is_refused() {
    let fx = support::fixture(support::Shape::BothShapesDeclared);
    validate(&fx).expect_err("同时声明两种形状必须被拒——closed variant 不允许二义");
}

#[test]
fn declaring_neither_shape_is_refused() {
    let fx = support::fixture(support::Shape::NeitherShapeDeclared);
    validate(&fx).expect_err("两种形状都不声明必须被拒，不得有隐式缺省");
}

#[test]
fn a_failed_literal_shape_never_falls_back_to_structured() {
    let fx = support::fixture(support::Shape::LiteralWithMultiLineDiff);
    let error = validate(&fx).expect_err("声明为 literal 形状却给出多行差异时必须直接失败");
    assert!(
        error.contains("literal shape"),
        "失败必须用稳定标记归因到 literal 形状本身，不得出现「literal 失败就自动按结构化再试一次」\
         的退化链；实际文案：{error}"
    );
}

// ── ⑤ 授权面与既有约束一条都不放宽 ───────────────────────────────────────────

#[test]
fn a_structured_evolution_still_requires_retained_coverage() {
    let fx = support::fixture(support::Shape::StructuredMissingRetainedCoverage);
    validate(&fx).expect_err(
        "结构化形状下 removedAssertions[].retainedCoverage 仍必须非空且存在于 new 文本中",
    );
}

#[test]
fn a_structured_evolution_still_requires_user_authorization() {
    let fx = support::fixture(support::Shape::StructuredMissingUserAuthorization);
    validate_adjudicated(&fx).expect_err("新形状不得成为绕过 B288 授权层的第三条路");
}

#[test]
fn a_structured_evolution_still_binds_the_whole_file_digests() {
    let fx = support::fixture(support::Shape::StructuredWrongWholeFileDigest);
    validate(&fx).expect_err("old/new 整文件 SHA 的绑定必须逐字保留");
}

// ── ⑥ 真实 emit → durable event → replay ─────────────────────────────────────

#[test]
fn a_structured_declaration_survives_a_real_emit_replay_round_trip() {
    for shape in [
        support::Shape::StructuredHappy,
        support::Shape::LiteralHappy,
    ] {
        let scene = support::emit_scene(shape);
        let emitted = orch_host::verify::build_frozen_contract_superseded_events(
            &scene.root,
            &scene.round,
            &scene.task_id,
            &scene.existing_events,
            &scene.authorization,
            &scene.effective_main_sha,
            &scene.recorded,
        )
        .unwrap_or_else(|error| {
            panic!("{shape:?} 必须能经真实 emit 生产 durable event：{error:#}")
        });
        assert_eq!(emitted.len(), 1, "每个样本只声明一个 supersession");

        let mut events = scene.existing_events.clone();
        events.push(scene.recorded.clone());
        let position = events.len();
        events.extend(emitted);
        let payload = orch_host::verify::validate_frozen_contract_supersession_delta_for_replay(
            &scene.root,
            &scene.effective_main_sha,
            &events,
            position,
        )
        .unwrap_or_else(|error| panic!("{shape:?} 的 durable 事件必须能被真实 replay：{error:#}"));

        let replayed = serde_json::to_value(payload.signed_declaration())
            .expect("replay 出的声明必须可序列化");
        let original = serde_json::to_value(&scene.declaration).expect("原声明必须可序列化");
        assert_eq!(
            replayed, original,
            "{shape:?}: replay 重建的声明必须与发射时结构同构——\
             把结构化声明在 replay 时降级成 literal 形状必须在这里红。\
             注意：这条走的是真实 emit→event→replay，不是两次调用同一函数比返回值。"
        );
    }
}

#[test]
fn the_support_module_is_wired() {
    assert_eq!(
        support::CONTRACT_ID,
        "B290",
        "support 模块必须由本卡交付并自报契约身份"
    );
}
