#![allow(dead_code)]
//! B265 seeded-red contract：让签名 AgentRegistry 真正签住 model/effort，
//! 并把请求侧真值投影进 `WakeIssued`（H160）。
//!
//! Expected red: **compile**。`registry::signed_invocation_binding` 与
//! `registry::SignedInvocationBinding` 尚不存在 ⇒ `error[E0432]: unresolved import`。
//!
//! ── 本卡的设计基础是被 fusion 推翻后重建的（见 r71 Q3 裁决书）──
//!
//! 问卷与 planner 独立答卷都以为「签名注册表不承载 model/effort」。**实测是错的**：
//!   * `registry.rs:86-101` 的 `AgentDefinition` 早已有 model/effort/expected_model/expected_effort；
//!   * `registry.rs:52-70` 的 `ToolDefinition` 早已有 modelSource / effortSemantic / observation；
//!   * `wake.rs:17037-17038` 早已把 `requestedModel`/`requestedEffort` 投影进 `WakeIssued`；
//!   * `plan.rs:88-105` 的 `IrSourceBindings` 早已有 `agentRegistryDigest` 字段。
//!
//! 机制全在位。**真实缺口是三条接线全断**：
//!   ① `plan.rs:1229-1239`：wake-only 注册表提前 `return Ok(())`，`digest_after`（:1272）
//!      永远走不到赋值点（:1278）⇒ `agentRegistryDigest` 恒为空字符串，
//!      又因 `skip_serializing_if = "String::is_empty"` 在 IR 里彻底隐身。
//!      **r70 `ROUND-IR.yaml` 的 sourceBindings 实测确实没有这一项**
//!      ⇒ 今天改 `agents.yaml` 里任何模型声明，`validationDigest` 一个字节都不变。
//!   ② `registry.rs:313`：`bail!("legacy agent {id} 不得声明 model/effort 字段")`，
//!      而 `coordination/agents.yaml` 的 `tool:` 数量为 **0** —— 九个 agent 全是 legacy 形态，
//!      声明模型**无门可入**。
//!   ③ `registry.rs:405-411`：`render_invocation` 的 legacy 分支把
//!      `requested_model` / `requested_effort` 硬编码为 `None`
//!      ⇒ r70 全部 **48 条** `WakeIssued` 的这两个字段都是 null。
//!
//! 后果就是 r70/B260：OpenCode 正式审查实际使用 GLM 而非用户指定的 qwen3.8-max，
//! 在 `mech.rs:809-814` 命中 `unconfigured()`（`verificationStatus="unconfigured"`、
//! `policy="advisory"`、`strictMismatch=false`）——**连「未对账」都报不出来，静默放过**。
//!
//! ── 本种子刻意不做的事 ──
//!
//! * **不用 struct literal 构造 `AgentDefinition` / `ToolDefinition`**：一律走 YAML 反序列化
//!   （沿用 B233/B234 的教训，避免把字段集钉进冻结文件，使「加公开字段」成为不可能——H74）。
//! * **不钉任何整段/前缀哈希**：r71 Q1 裁决 ⑤ 明确「整段哈希钉死是过度冻结的反模式」，
//!   本种子只钉窄不变量（具名符号、调用点、计数）。
//! * 源码窗口扫描一律放在普通 `#[test]` 里，**每条断言一个独立 `#[test]`**（r70 坑 16）；
//!   编译红由缺失的 `use` 提供，不靠 `const` 求值。
//!
//! ── Negative mutations that must turn the named case red ──
//!
//! M1. legacy 分支继续把 requested_model/effort 硬编码为 None
//!     -> `a_legacy_agent_declares_model_and_effort_into_the_signed_binding` 红。
//! M2. 允许声明 model 却不声明 observation（模型可声明、却无从核对）
//!     -> `declaring_a_model_without_an_observation_source_is_rejected` 红。
//! M3. `plan.rs` 继续对 wake-only 注册表提前 return
//!     -> `the_wake_only_early_return_no_longer_discards_the_registry_digest` 红。
//! M4. **接线变异**：谓词存在但 `render_invocation` 的 legacy 分支不调用它
//!     -> `render_invocation_consumes_the_signed_binding` 红。
//! M5. digest 赋值点被搬出该函数（用"移走"冒充"放开可达性"）
//!     -> `the_registry_digest_assignment_is_still_reachable` 红。
//!
//! ── 一条被 planner 主动删掉的断言（留证，防后人重蹈）──
//! 初稿曾写 `changing_only_the_declared_model_changes_the_registry_digest`
//! （两份只差 model 的注册表 digest 必须不同）。**复算发现它毫无判别力**：
//! `plan.rs:1039` 的 `agent_registry_digest` 本来就是对 `coordination/agents.yaml`
//! 的**文件字节**求哈希，改任何一个字符今天就会改变 digest ⇒ 该断言**当前即绿**，
//! 且 B234 的 `registry_digest_is_content_addressed` 已经覆盖同一性质。
//! 这正是 r70/B261 那型陷阱（`>=2` 在已有 3 处时毫无判别力）。已删除。
//! 「digest 真的进了签名 IR」这件事改由 requiredEvidence 的
//! `signed-ir-carries-the-registry-digest` 用**真实 `orch plan` 产物**证明——
//! 那比单测更强，因为它走的是生产路径。

use std::fs;
use std::path::PathBuf;

use orch_host::registry::{load_agent_definitions, signed_invocation_binding, SignedInvocationBinding};
use orch_host::util::test_scratch_dir;

// ── 源码窗口（窄不变量，不做整段哈希）────────────────────────────────────────
const REGISTRY_SRC: &str = include_str!("../src/registry.rs");
const PLAN_SRC: &str = include_str!("../src/plan.rs");

fn find(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("B265: missing source anchor {needle:?}"))
}

fn window<'a>(src: &'a str, start_anchor: &str, end_anchor: &str) -> &'a str {
    let start = find(src, start_anchor);
    let tail = &src[start..];
    let end = tail
        .get(start_anchor.len()..)
        .and_then(|rest| rest.find(end_anchor))
        .map(|offset| start_anchor.len() + offset)
        .unwrap_or(tail.len());
    &tail[..end]
}

// ── 夹具：一律 YAML，不用 struct literal ────────────────────────────────────

const AGENTS_WITH_BINDING: &str = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-probe:
    injectable: true
    sessionId: "fresh-session-per-wake"
    model: "qwen3.8-max"
    effort: "max"
    observation:
      source: "opencode-db"
      policy: "strict"
    wake:
      argv: ["opencode", "run", "{message}", "--model", "qwen3.8-max"]
"#;

const AGENTS_MODEL_WITHOUT_OBSERVATION: &str = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-probe:
    injectable: true
    sessionId: "fresh-session-per-wake"
    model: "qwen3.8-max"
    effort: "max"
    wake:
      argv: ["opencode", "run", "{message}", "--model", "qwen3.8-max"]
"#;

fn write_registry(tag: &str, agents_yaml: &str) -> PathBuf {
    let root = test_scratch_dir(tag);
    fs::create_dir_all(root.join("coordination")).expect("B265: 建 coordination 失败");
    fs::write(root.join("coordination/agents.yaml"), agents_yaml)
        .expect("B265: 写 agents.yaml 失败");
    root
}

// ── A1：声明进得去，且能被读出来（M1）──────────────────────────────────────

#[test]
fn a_legacy_agent_declares_model_and_effort_into_the_signed_binding() {
    let root = write_registry("b265-binding", AGENTS_WITH_BINDING);
    let defs = load_agent_definitions(&root).expect("B265: legacy agent 必须能声明 model/effort");
    let def = defs
        .get("executor-probe")
        .expect("B265: 夹具 agent 必须被载入");

    let binding: SignedInvocationBinding = signed_invocation_binding(def);

    assert_eq!(
        binding.requested_model.as_deref(),
        Some("qwen3.8-max"),
        "B265: 签名声明的 model 必须成为请求侧真值，而不是 None"
    );
    assert_eq!(
        binding.requested_effort.as_deref(),
        Some("max"),
        "B265: 签名声明的 effort 必须成为请求侧真值，而不是 None"
    );
    assert_eq!(
        binding.observation_source.as_deref(),
        Some("opencode-db"),
        "B265: 观测源必须随绑定一起进入签名面"
    );
    assert_eq!(
        binding.observation_policy.as_deref(),
        Some("strict"),
        "B265: 观测策略必须随绑定一起进入签名面"
    );
}

// ── A2：声明模型就必须同时声明怎么核对（M2）────────────────────────────────

#[test]
fn declaring_a_model_without_an_observation_source_is_rejected() {
    let root = write_registry("b265-no-observation", AGENTS_MODEL_WITHOUT_OBSERVATION);
    let outcome = load_agent_definitions(&root);
    assert!(
        outcome.is_err(),
        "B265: 声明 model 却不声明 observation 必须被拒——否则模型可声明、却无从核对，\
         正是 unobservable 退化成回退态的那个后门"
    );
}

// ── A3：接线变异（M4）——生产渲染路径必须真的消费该绑定 ────────────────────

#[test]
fn render_invocation_consumes_the_signed_binding() {
    let scope = window(REGISTRY_SRC, "pub fn render_invocation(", "\npub fn ");
    assert!(
        scope.contains("signed_invocation_binding("),
        "B265 M4: render_invocation 必须调用 signed_invocation_binding——\
         谓词写对了但生产路径不调它，正是 r64/H116 的形态"
    );
}

#[test]
fn render_invocation_no_longer_hardcodes_a_null_model_request() {
    let scope = window(REGISTRY_SRC, "pub fn render_invocation(", "\npub fn ");
    assert_eq!(
        scope.matches("requested_model: None,").count(),
        0,
        "B265 M1: legacy 分支不得再把 requested_model 硬编码为 None"
    );
}

// ── A5：wake-only 提前 return 必须消失（M3）───────────────────────────────

#[test]
fn the_wake_only_early_return_no_longer_discards_the_registry_digest() {
    let scope = window(
        PLAN_SRC,
        "fn bind_agent_registry_projection(",
        "\nfn ",
    );
    assert_eq!(
        scope
            .matches("Wake-only registries have no shared-domain")
            .count(),
        0,
        "B265 M3: wake-only 注册表的提前 return 必须移除，否则 agentRegistryDigest 恒为空、\
         在 IR 里因 skip_serializing_if 彻底隐身"
    );
}

#[test]
fn the_registry_digest_assignment_is_still_reachable() {
    let scope = window(
        PLAN_SRC,
        "fn bind_agent_registry_projection(",
        "\nfn ",
    );
    assert!(
        scope.contains("ir.source_bindings.agent_registry_digest = digest_after"),
        "B265: digest 赋值点必须仍在该函数内——本卡放开的是可达性，不是搬走赋值"
    );
}
