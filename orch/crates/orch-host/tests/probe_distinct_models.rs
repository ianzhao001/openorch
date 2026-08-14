//! ═══ 红种子契约 · B71（probe 归一化去重纯函数 · 压测棒）═══
//! 落位: orch/crates/orch-host/tests/probe_distinct_models.rs（逐字节复制）
//! 预期红（redForm: compile）：`probe::distinct_models` 尚不存在 → error[E0425]。
//!
//! 契约（落在 orch_host::probe，勿动 lib.rs、不新增依赖）：
//!   - pub fn distinct_models(decls: &[ProbeDecl]) -> Vec<String>
//!       每个 decl.model 过 `normalize_model_id`（去 provider 前缀），去重**保首现顺序**。
//! 负向变异：M1 不归一化 / M2 不去重或乱序 ⇒ 对应用例红。

use orch_host::probe::{distinct_models, ProbeDecl};

fn d(model: &str) -> ProbeDecl {
    ProbeDecl { model: model.to_string(), depth: "high".to_string() }
}

#[test]
fn normalizes_and_dedups_preserving_order() {
    let decls = [d("z-ai/glm-5.2"), d("dewu-ep/glm-5.2"), d("openai/gpt-5")];
    assert_eq!(distinct_models(&decls), vec!["glm-5.2".to_string(), "gpt-5".to_string()]);
}

#[test]
fn empty_is_empty() {
    assert!(distinct_models(&[]).is_empty());
}
