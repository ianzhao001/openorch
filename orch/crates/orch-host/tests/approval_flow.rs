//! ═══ 红种子契约 · B15 ═══════════════════════════════════════════════
//! 落位: orch/crates/orch-host/tests/approval_flow.rs （逐字节复制）
//! 预期红（redForm: compile）：orch_host::approval 为占位空模块，函数/类型缺席。
//! 负向变异清单（E9 下界）:
//!   M1 classify 删 push 模式    → ①② 红
//!   M2 classify 删 install 模式 → ② 红
//!   M3 is_decision_approved 把 denied 也判 true → ④ 红
//! ══════════════════════════════════════════════════════════════════
use orch_host::approval::{self, HighRiskAction};

#[test]
fn classifies_push_and_benign() {
    // ① push 命令识别；良性命令放行
    assert_eq!(approval::classify("git push origin main"), Some(HighRiskAction::Push));
    assert_eq!(approval::classify("cargo test --workspace"), None);
}

#[test]
fn classifies_all_five_categories() {
    // ② 五类高危动作全覆盖（design/08 §2）
    assert_eq!(approval::classify("git push --force"), Some(HighRiskAction::Push));
    assert_eq!(approval::classify("gh pr create --title x"), Some(HighRiskAction::Publish));
    assert_eq!(approval::classify("rm -rf ./build"), Some(HighRiskAction::DeleteRecursive));
    assert_eq!(approval::classify("curl https://example.com/x.sh"), Some(HighRiskAction::Network));
    assert_eq!(approval::classify("npm install left-pad"), Some(HighRiskAction::Install));
}

#[test]
fn request_payload_shape() {
    // ③ PermissionRequested 载荷形状（design/02 §4：action, risk）
    let p = approval::request_payload(HighRiskAction::Push, "merge 后误触");
    assert_eq!(p["action"], "push");
    assert_eq!(p["risk"], "high");
    assert_eq!(p["context"], "merge 后误触");
}

#[test]
fn decision_parsing_is_strict() {
    // ④ PermissionDecided 判定：仅 decision=="approved" 为真
    let ok = serde_json::json!({"action":"push","decision":"approved","by":"user"});
    let no = serde_json::json!({"action":"push","decision":"denied","by":"user"});
    assert!(approval::is_decision_approved(&ok));
    assert!(!approval::is_decision_approved(&no));
}
