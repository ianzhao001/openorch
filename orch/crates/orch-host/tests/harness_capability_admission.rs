#![allow(dead_code)]
//! B293 冻结契约 · 单一 harness 身份 + 能力描述符 + fail-closed 准入
//!
//! 本文件一旦落位即**永久冻结**（AGENTS.md 铁律 10）：不可扩展、修改、增删用例或重命名。
//! 补充测试请放到 writeSet 内的其它落点。
//!
//! ## 判据纪律
//!
//! - 每条断言一个独立 `#[test]`，失败点可单独定位（RUNBOOK 坑 16）。
//! - 判定一律来自**真实生产 `pub` 入口**。`support` 只允许造 scratch 现场、写合法/非法输入、
//!   读回结果，**不得**自报「拒绝了没有」这类结论（r76 F3：support 罐头值可洗绿）。
//! - **接线断言故意读源码，不自调**——沿用 B265 落位种子
//!   `signed_invocation_binding.rs:180-195` 的既有惯用法：谓词自己调自己永远能证明
//!   「有调用者」，只有对生产函数体开窗口才能证明它**真的被接进了派发路径**。
//! - 现场一律落**仓内**（`orch/target/test-tmp/**`），禁 `/tmp`（RUNBOOK 坑 7）。
//! - `support::scratch_root` 必须对**同一进程内并行用例**也保证唯一（H190：pid+纳秒会碰撞）。
//!
//! ## 检查点为什么在 wake.rs 而不在 registry.rs
//!
//! r77 计划审核（四家独立命中）查明：把 fail-closed 加进
//! `registry::render_legacy_invocation` **无合法绿解**——B283 的**已落位冻结种子**
//! `legacy_review_managed_custody.rs:300-329` 在一个**没有 `harnesses.yaml`** 的 scratch 根上
//! 调 `render_invocation(...).expect("渲染必须成功")`，而该文件不在本卡 writeSet 内、不可修。
//!
//! 但 `registry` 那两个渲染函数的**全部生产调用者**只有 `wake.rs:12482` 与 `wake.rs:12487`，
//! 二者都在 `render_registered_invocation` 内。⇒ 把检查放在该函数入口，
//! **同时覆盖 legacy 与 modern 两条分支**，是唯一真实派发路径，且
//! `registry` 的签名与行为逐字不变 ⇒ B283/B265 冻结种子全绿。
//!
//! ## 负向变异清单（卡面 §5）
//!
//! - M1 描述符缺失时回落「全能力可用」   → `a_missing_descriptor_file_is_refused`
//! - M2 不认识的 `pinTransport` 当 `env`  → `an_unknown_pin_transport_value_is_refused`
//! - M3 删掉 wake 派发入口里的准入调用    → `the_wake_dispatch_entry_calls_the_admission_check`
//! - M4 准入从 `bail!` 降级成警告后继续   → `an_uncarriable_pin_is_refused`
//! - M5 `ProviderKind` 恢复独立变体表      → `provider_kind_delegates_to_harness_id_in_source`
//! - M6 改 `ProviderKind::as_str` 既有字面量 → `existing_wire_strings_are_byte_identical`
//! - M7 删掉新增公开条目的 `///`           → `every_public_item_in_harness_rs_carries_its_own_doc`
//! - M8 新增一条绕过检查的渲染分支          → `there_is_exactly_one_legacy_render_construction`

mod harness_capability_admission_support;

use harness_capability_admission_support as support;

use orch_host::adapter;
use orch_host::chanhealth::ProviderKind;
use orch_host::harness::{self, HarnessId, PinTransport};
use orch_host::registry;

const HARNESS_SRC: &str = include_str!("../src/harness.rs");
const WAKE_SRC: &str = include_str!("../src/wake.rs");
const REGISTRY_SRC: &str = include_str!("../src/registry.rs");
const CHANHEALTH_SRC: &str = include_str!("../src/chanhealth.rs");

/// 从源码里截出 `start` 起、到下一个 `end` 之前的窗口（B265 同款）。
fn window<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let from = src
        .find(start)
        .unwrap_or_else(|| panic!("源码里找不到窗口起点: {start}"));
    let rest = &src[from + start.len()..];
    let to = rest.find(end).unwrap_or(rest.len());
    &rest[..to]
}

// ───────────────────────── ① 身份收敛 ─────────────────────────

/// `HarnessId` 必须覆盖全部在册通道 + consult-only 身份。
/// 少一条就说明又留了一套私有词表。
#[test]
fn harness_id_covers_every_registered_identity() {
    let mut names: Vec<&str> = HarnessId::ALL.iter().map(|id| id.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "agy",
            "claude",
            "codebuddy",
            "codex",
            "cursor",
            "dclaw",
            "dsh",
            "mimo",
            "opencode",
            "pi",
            "smartclaw",
            "zcode",
        ],
        "HarnessId 必须同时覆盖九条 wake 通道与三个 consult-only 身份"
    );
}

/// 每个 `ProviderKind` 变体都必须能由某个 `HarnessId` 表达，且往返一致。
#[test]
fn every_provider_kind_variant_round_trips_through_harness_id() {
    for kind in [
        ProviderKind::Codex,
        ProviderKind::OpenCode,
        ProviderKind::SmartClaw,
        ProviderKind::Agy,
        ProviderKind::Dclaw,
    ] {
        let id = HarnessId::parse(kind.as_str()).unwrap_or_else(|error| {
            panic!("ProviderKind {} 无法由 HarnessId 表达: {error}", kind.as_str())
        });
        assert_eq!(id.as_str(), kind.as_str(), "wire 表示必须逐字往返");
    }
}

/// **M5 的真载体**：「派生」是结构属性——相同字符串的独立表与派生实现行为完全一致，
/// 所以只能读源码。`chanhealth.rs` 不得再自持一份变体字面量表。
#[test]
fn provider_kind_delegates_to_harness_id_in_source() {
    assert!(
        CHANHEALTH_SRC.contains("HarnessId"),
        "chanhealth.rs 必须引用 HarnessId —— 否则 ProviderKind 只是第四套独立词表"
    );
    let scope = window(CHANHEALTH_SRC, "impl ProviderKind", "\n}\n");
    for literal in ["\"codex\"", "\"opencode\"", "\"smartclaw\"", "\"agy\"", "\"dclaw\""] {
        assert!(
            !scope.contains(literal),
            "impl ProviderKind 内不得再出现独立的 wire 字面量 {literal}；必须委托给 HarnessId"
        );
    }
}

/// 既有 wire 字面量一个字节都不能改：它们进过快照与告警文本。
#[test]
fn existing_wire_strings_are_byte_identical() {
    assert_eq!(ProviderKind::Codex.as_str(), "codex");
    assert_eq!(ProviderKind::OpenCode.as_str(), "opencode");
    assert_eq!(ProviderKind::SmartClaw.as_str(), "smartclaw");
    assert_eq!(ProviderKind::Agy.as_str(), "agy");
    assert_eq!(ProviderKind::Dclaw.as_str(), "dclaw");
}

/// consult 内置表由 `HarnessId` 的子集派生，且六个既有字面量不变。
#[test]
fn builtin_adapters_stay_byte_identical_and_derive_from_harness_ids() {
    let mut adapters: Vec<&str> = adapter::supported_builtins();
    adapters.sort_unstable();
    assert_eq!(
        adapters,
        vec!["claude", "codebuddy", "codex", "cursor", "mimo", "opencode"],
        "BUILTIN_ADAPTERS 的既有字面量不得改动"
    );
    let mut derived: Vec<&str> = HarnessId::ALL
        .iter()
        .filter(|id| id.is_consult_builtin())
        .map(|id| id.as_str())
        .collect();
    derived.sort_unstable();
    assert_eq!(derived, adapters, "内置 adapter 表必须由 HarnessId 的子集派生");
}

/// 第三套词表也必须收编：`BackendReceiptKind` 的五个变体逐一由 `HarnessId` 表达。
#[test]
fn backend_receipt_kind_is_collapsed_too() {
    let scope = window(WAKE_SRC, "impl BackendReceiptKind", "\n}\n");
    assert!(
        scope.contains("HarnessId"),
        "impl BackendReceiptKind 必须委托 HarnessId，否则第三套词表仍然独立"
    );
    for name in ["codex", "opencode", "smartclaw", "pi", "zcode"] {
        HarnessId::parse(name)
            .unwrap_or_else(|error| panic!("BackendReceiptKind 的 {name} 必须可由 HarnessId 表达: {error}"));
    }
}

// ───────────────────────── ② 描述符 fail-closed ─────────────────────────

/// 描述符与注册表的键集合必须**严格相等**，且**不设任何实现侧特例**。
#[test]
fn descriptor_and_registry_key_sets_must_match_exactly() {
    let root = support::scratch_root("key_sets");
    support::write_agents(&root, &["alpha", "beta"]);
    support::write_harnesses(&root, &[("alpha", "pi", "env"), ("beta", "dsh", "none")]);
    let registry = harness::load_harness_registry(&root).expect("键集合相等时必须加载成功");
    let mut ids = registry.agent_ids();
    ids.sort();
    assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);

    let extra = support::scratch_root("key_sets_extra");
    support::write_agents(&extra, &["alpha"]);
    support::write_harnesses(&extra, &[("alpha", "pi", "env"), ("ghost", "dsh", "none")]);
    let error =
        harness::load_harness_registry(&extra).expect_err("描述符里有注册表没有的 agent 必须拒");
    assert!(
        format!("{error:#}").contains("ghost"),
        "错误必须点名多出来的条目"
    );
}

/// 不得靠硬编码 agent 名把某个键排除在相等域之外。
#[test]
fn no_agent_name_is_special_cased_in_source() {
    for forbidden in ["\"planner\"", "!= \"planner\"", "== \"planner\""] {
        assert!(
            !HARNESS_SRC.contains(forbidden),
            "harness.rs 不得对 {forbidden} 做实现侧特例——相等域必须是注册表全集"
        );
    }
}

/// 描述符文件缺失 ⇒ `Err`，**不是**「当作全能力可用」。
#[test]
fn a_missing_descriptor_file_is_refused() {
    let root = support::scratch_root("missing_file");
    support::write_agents(&root, &["alpha"]);
    let error =
        harness::load_harness_registry(&root).expect_err("描述符缺失必须拒，不得给任何缺省");
    assert!(
        format!("{error:#}").contains(harness::HARNESS_REGISTRY_RELPATH),
        "错误必须点名缺失的文件"
    );
}

/// 注册表里有、描述符里没有的 agent ⇒ `Err`。
#[test]
fn a_missing_agent_entry_is_refused() {
    let root = support::scratch_root("missing_entry");
    support::write_agents(&root, &["alpha", "beta"]);
    support::write_harnesses(&root, &[("alpha", "pi", "env")]);
    let error = harness::load_harness_registry(&root).expect_err("缺条目必须拒");
    assert!(format!("{error:#}").contains("beta"), "错误必须点名缺失的 agent");
}

/// 枚举取值不认识 ⇒ `Err`，不得回落任何缺省。
#[test]
fn an_unknown_pin_transport_value_is_refused() {
    let root = support::scratch_root("unknown_transport");
    support::write_agents(&root, &["alpha"]);
    support::write_harnesses(&root, &[("alpha", "pi", "envv")]);
    let error = harness::load_harness_registry(&root).expect_err("不认识的取值必须拒");
    assert!(format!("{error:#}").contains("envv"), "错误必须点名不认识的取值");
}

/// 不认识的 harness 名同样拒。
#[test]
fn an_unknown_harness_name_is_refused() {
    let root = support::scratch_root("unknown_harness");
    support::write_agents(&root, &["alpha"]);
    support::write_harnesses(&root, &[("alpha", "piglet", "env")]);
    let error = harness::load_harness_registry(&root).expect_err("不认识的 harness 必须拒");
    assert!(format!("{error:#}").contains("piglet"), "错误必须点名不认识的 harness");
}

// ───────────────────────── ③ 准入判据与接线 ─────────────────────────

/// `pinTransport: none` 的 harness 上声明 pin ⇒ 拒（H194 的机械收口）。
#[test]
fn an_uncarriable_pin_is_refused() {
    let root = support::scratch_root("uncarriable_pin");
    support::write_agents_with_pin(&root, "alpha", Some("deepseek-v4-pro"));
    support::write_harnesses(&root, &[("alpha", "dsh", "none")]);
    let defs = registry::load_agent_definitions(&root).expect("注册表本身应可解析");
    let registry = harness::load_harness_registry(&root).expect("描述符应可加载");
    let error = harness::assert_pin_is_transportable(&registry, "alpha", defs.get("alpha").unwrap())
        .expect_err("承载不了的 pin 必须被拒");
    let text = format!("{error:#}");
    for wanted in ["alpha", "dsh", "none"] {
        assert!(text.contains(wanted), "错误必须点名 {wanted}：{text}");
    }
}

/// 反向：可承载时不得误伤。只证明「拒绝路径能拒」不算——那可用恒真 `bail!` 洗绿。
#[test]
fn a_transportable_pin_is_accepted() {
    let root = support::scratch_root("carriable_pin");
    support::write_agents_with_pin(&root, "alpha", Some("muse-spark-1.2-contributor"));
    support::write_harnesses(&root, &[("alpha", "pi", "env")]);
    let defs = registry::load_agent_definitions(&root).expect("注册表本身应可解析");
    let registry = harness::load_harness_registry(&root).expect("描述符应可加载");
    harness::assert_pin_is_transportable(&registry, "alpha", defs.get("alpha").unwrap())
        .expect("可承载的 pin 不得被拒");
}

/// 未声明 pin 的 agent 在 `none` 下照常通过——检查不得误伤。
#[test]
fn an_absent_pin_is_not_refused_on_a_none_transport() {
    let root = support::scratch_root("absent_pin");
    support::write_agents_with_pin(&root, "alpha", None);
    support::write_harnesses(&root, &[("alpha", "dsh", "none")]);
    let defs = registry::load_agent_definitions(&root).expect("注册表本身应可解析");
    let registry = harness::load_harness_registry(&root).expect("描述符应可加载");
    harness::assert_pin_is_transportable(&registry, "alpha", defs.get("alpha").unwrap())
        .expect("没有声明 pin 时不得拒绝");
}

/// `PinTransport::carries_pin` 是准入判据的唯一来源，四个取值语义固定。
#[test]
fn pin_transport_carrying_semantics_are_fixed() {
    assert!(PinTransport::ArgvLiteral.carries_pin());
    assert!(PinTransport::Env.carries_pin());
    assert!(PinTransport::EnvVerify.carries_pin());
    assert!(!PinTransport::None.carries_pin());
}

/// **M3 的真载体（接线）**：准入必须被接进**真实派发入口**，而不是躺在那里没人调。
/// 沿用 B265 的源码窗口惯用法。
#[test]
fn the_wake_dispatch_entry_calls_the_admission_check() {
    let scope = window(WAKE_SRC, "fn render_registered_invocation(", "\nfn ");
    assert!(
        scope.contains("assert_pin_is_transportable"),
        "render_registered_invocation 必须调用准入检查——它是 legacy 与 modern 两条渲染分支的\
         唯一真实生产入口；不调用就等于交付一条没人走的路（H111 同型）"
    );
    let legacy_at = scope
        .find("render_legacy_invocation")
        .expect("render_registered_invocation 内必须仍保留 legacy 渲染分支");
    let check_at = scope
        .find("assert_pin_is_transportable")
        .expect("已在上一条断言");
    assert!(
        check_at < legacy_at,
        "准入必须发生在 legacy 渲染**之前**，否则拒绝晚于 RenderedInvocation 构造"
    );
}

/// **M8 的真载体**：不留第二条绕过检查的渲染路径。按构造点计数，防改名绕过。
#[test]
fn there_is_exactly_one_legacy_render_construction() {
    let production = &REGISTRY_SRC[..REGISTRY_SRC
        .find("#[cfg(test)]")
        .unwrap_or(REGISTRY_SRC.len())];
    assert_eq!(
        production.matches("RenderedInvocation {").count(),
        2,
        "registry.rs 生产段内构造 RenderedInvocation 的位置必须恰好两处\
         （legacy 一处、modern 一处）；新增并列变体即红"
    );
    let wake_production = &WAKE_SRC[..WAKE_SRC.find("#[cfg(test)]").unwrap_or(WAKE_SRC.len())];
    assert_eq!(
        wake_production.matches("RenderedInvocation {").count(),
        0,
        "wake.rs 生产段内不得自行构造 RenderedInvocation——那是一条绕过准入的旁路"
    );
}

// ───────────────────────── ④ 摘要与文档 ─────────────────────────

/// 摘要必须对**确切字节**取 SHA-256：只改一处注释也要变。
#[test]
fn the_descriptor_digest_is_over_exact_file_bytes() {
    let root = support::scratch_root("digest");
    support::write_agents(&root, &["alpha"]);
    support::write_harnesses(&root, &[("alpha", "pi", "env")]);
    let before = harness::registry_digest(&root).expect("摘要必须可计算");
    assert_eq!(before.len(), 64, "摘要必须是 64 位十六进制");
    assert_eq!(before, before.to_lowercase(), "摘要必须是小写");
    support::append_comment_line(&root);
    let after = harness::registry_digest(&root).expect("摘要必须可计算");
    assert_ne!(before, after, "只改注释也必须改变摘要");
}

/// 铁律 12 的承重点：`#![deny(missing_docs)]` 在本仓**不存在**（DEFERRED-LEDGER D-01），
/// 所以「公开条目有说明」这件事只能由本用例扛。**覆盖公开字段与枚举变体**。
#[test]
fn every_public_item_in_harness_rs_carries_its_own_doc() {
    let lines: Vec<&str> = HARNESS_SRC.lines().collect();
    let mut documented = 0usize;
    let mut seen_docs: Vec<String> = Vec::new();
    let mut in_public_block = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("pub struct ") || trimmed.starts_with("pub enum ") {
            in_public_block = trimmed.ends_with('{');
        } else if trimmed == "}" {
            in_public_block = false;
        }
        let is_item = trimmed.starts_with("pub fn ")
            || trimmed.starts_with("pub struct ")
            || trimmed.starts_with("pub enum ")
            || trimmed.starts_with("pub const ")
            || trimmed.starts_with("pub trait ")
            || trimmed.starts_with("pub type ");
        // 公开结构体字段（`pub name: T,`）与枚举变体（`Name,` / `Name(..)`）同样承重。
        let is_member = in_public_block
            && !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("#[")
            && (trimmed.starts_with("pub ")
                || trimmed
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase()));
        if !is_item && !is_member {
            continue;
        }
        let mut cursor = index;
        let mut doc: Option<String> = None;
        while cursor > 0 {
            cursor -= 1;
            let above = lines[cursor].trim_start();
            if above.starts_with("#[") || above.is_empty() {
                continue;
            }
            if let Some(rest) = above.strip_prefix("///") {
                let body = rest.trim();
                if !body.is_empty() {
                    doc = Some(body.to_string());
                }
            }
            break;
        }
        let doc = doc.unwrap_or_else(|| panic!("公开条目/成员缺少非空 /// 说明: {trimmed}"));
        assert!(
            !seen_docs.contains(&doc),
            "占位式重复说明按弱化断言处理（铁律 4 同类）: {doc}"
        );
        seen_docs.push(doc);
        documented += 1;
    }
    assert!(
        documented >= 12,
        "harness.rs 至少要交付十二个有说明的公开条目/成员，实际 {documented}"
    );
}
