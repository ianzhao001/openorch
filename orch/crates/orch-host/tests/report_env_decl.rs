//! ═══ 红种子契约 · B45 ═══（落位: orch/crates/orch-host/tests/report_env_decl.rs，逐字节复制）
//! 预期红（redForm: compile）：mech::report_env_decl / mech::env_decl_payload 尚不存在
//!   → error[E0425] cannot find function in module `mech`，文件级编译红。
//! 背景：r26 修订3（用户裁定）——派活须核对执行者实际 model/思考深度。探针实证五员三员自报失真
//!   （见 coordination/archive/probe-model-r26.md），故 REPORT 新增「§0 执行环境自报」必填段
//!   （MODEL=/DEPTH=/CAPTURE= 三行，CAPTURE=取证命令），机检层提取并入账。缺 §0 或缺行→机检 FAIL
//!   （failure_event stage="env-decl"）；tierf await-report 织入与 ReportObserved payload 扩展是
//!   IO 胶水，由卡+verifier 核，不在本纯函数种子内。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 缺 §0 段不返 None(放行) → `missing_section_returns_none` 红；
//!   M2 缺 MODEL 行仍返 Some → `missing_model_line_returns_none` 红；
//!   M3 payload 键名拼错/值窜位 → `env_decl_payload_shape` 红。
use orch_host::mech;

#[test]
fn parses_env_decl_section() {
    let report = "# B99 REPORT\n\n## §0 执行环境自报\nMODEL=glm-5.2\nDEPTH=standard\nCAPTURE=grep model ~/.config/opencode/opencode.json\n\n## §1 结论\n正文\n";
    let decl = mech::report_env_decl(report).expect("含完整 §0 段应解析成功");
    assert_eq!(decl.model, "glm-5.2");
    assert_eq!(decl.depth, "standard");
    assert_eq!(decl.capture, "grep model ~/.config/opencode/opencode.json");
}

#[test]
fn missing_section_returns_none() {
    assert!(
        mech::report_env_decl("# REPORT\n## §1 结论\n无自报段\n").is_none(),
        "缺 §0 段应返 None（机检 FAIL 的判据）"
    );
}

#[test]
fn missing_model_line_returns_none() {
    let report = "## §0 执行环境自报\nDEPTH=high\nCAPTURE=x\n";
    assert!(mech::report_env_decl(report).is_none(), "缺 MODEL 行应返 None");
}

#[test]
fn missing_capture_line_returns_none() {
    let report = "## §0 执行环境自报\nMODEL=a\nDEPTH=b\n";
    assert!(
        mech::report_env_decl(report).is_none(),
        "缺 CAPTURE(取证命令)行应返 None——自报不带取证命令视同未自报"
    );
}

#[test]
fn env_decl_payload_shape() {
    let decl = mech::report_env_decl("## §0 执行环境自报\nMODEL=a\nDEPTH=b\nCAPTURE=c\n")
        .expect("完整段应解析成功");
    let payload = mech::env_decl_payload(&decl);
    assert_eq!(payload["model"], "a");
    assert_eq!(payload["depth"], "b");
    assert_eq!(payload["capture"], "c");
}
