//! ═══ 红种子契约 · B72（approval 高危分类去重纯函数 · 压测棒）═══
//! 落位: orch/crates/orch-host/tests/approval_kinds.rs（逐字节复制）
//! 预期红（redForm: compile）：`approval::high_risk_kinds` 尚不存在 → error[E0425]。
//!
//! 契约（落在 orch_host::approval，勿动 lib.rs、不新增依赖）：
//!   - pub fn high_risk_kinds(cmds: &[String]) -> Vec<HighRiskAction>
//!       每条 cmd 过 `classify`，保留 Some 的，按变体去重**保首现顺序**。
//! 负向变异：M1 不去重 / M2 保留 None(空插) 或乱序 ⇒ 对应用例红。

use orch_host::approval::{high_risk_kinds, HighRiskAction};

fn v(cmds: &[&str]) -> Vec<String> {
    cmds.iter().map(|c| c.to_string()).collect()
}

#[test]
fn classifies_dedups_preserving_order() {
    let cmds = v(&[
        "git push origin main",
        "cargo test --workspace",
        "git push --force",
        "gh pr create --title x",
    ]);
    assert_eq!(
        high_risk_kinds(&cmds),
        vec![HighRiskAction::Push, HighRiskAction::Publish]
    );
}

#[test]
fn no_high_risk_is_empty() {
    assert!(high_risk_kinds(&v(&["cargo build", "ls -la"])).is_empty());
}
