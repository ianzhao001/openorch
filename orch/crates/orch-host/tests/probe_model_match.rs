//! ═══ 红种子契约 · B51 ═══（落位: orch/crates/orch-host/tests/probe_model_match.rs，逐字节复制）
//! 预期红（redForm: compile）：`probe::normalize_model_id` / `probe::model_matches` 尚不存在 →
//!   error[E0425]，文件级编译红（probe.rs 已有 B48 交付的 parse_probe_line，本棒在同文件扩展）。
//! 背景：r26 model 探针实录（coordination/archive/probe-model-r26.md）——五员三员自报失真，且同一模型
//!   在不同通道写法不一（"z-ai/glm-5.2" / "dewu-ep/glm-5.2" / "glm-5.2" / "GLM-5.2"）。
//!   探针 SOP 自动化需要归一化比对：
//!   `pub fn normalize_model_id(raw: &str) -> String`——trim → 去 provider 前缀（最后一个 '/' 前
//!     全部丢弃）→ 转小写；
//!   `pub fn model_matches(declared: &str, expected: &str) -> bool`——两侧归一化后精确相等；
//!     任一侧归一化后为空 → false（缺值不得混过，镜像 parse_probe_line 的空值语义）。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 不去 provider 前缀 → `provider_prefix_is_stripped` 红；
//!   M2 大小写敏感比对 → `case_insensitive_match` 红；
//!   M3 空值当匹配 → `empty_side_never_matches` 红；
//!   M4 归一化后仍不同却判真(恒真) → `different_models_do_not_match` 红。
use orch_host::probe;

#[test]
fn provider_prefix_is_stripped() {
    // M1：通道各自的 provider 前缀不参与身份比对
    assert_eq!(probe::normalize_model_id("z-ai/glm-5.2"), "glm-5.2");
    assert_eq!(probe::normalize_model_id("dewu-ep/glm-5.2"), "glm-5.2");
    assert!(probe::model_matches("z-ai/glm-5.2", "glm-5.2"));
}

#[test]
fn case_insensitive_match() {
    // M2：大小写不构成身份差异
    assert!(probe::model_matches("GLM-5.2", "glm-5.2"));
}

#[test]
fn whitespace_is_trimmed() {
    assert!(probe::model_matches("  glm-5.2  ", "glm-5.2"));
}

#[test]
fn different_models_do_not_match() {
    // M4：kimi-k3 冒充 claude 这类失真必须判假（r26 C 案例）
    assert!(!probe::model_matches("claude", "kimi-k3"));
    assert!(!probe::model_matches("qwen-max", "glm-5.2"));
}

#[test]
fn empty_side_never_matches() {
    // M3：空值/纯前缀不得混过
    assert!(!probe::model_matches("", "glm-5.2"));
    assert!(!probe::model_matches("z-ai/", "glm-5.2"));
    assert!(!probe::model_matches("glm-5.2", ""));
}
