//! ═══ 红种子契约 · B48 ═══（落位: orch/crates/orch-host/tests/probe_parse.rs，逐字节复制）
//! 预期红（redForm: compile）：`orch_host::probe` 模块已由 planner 预置空占位（lib.rs 已声明），
//!   但 parse_probe_line 及其返回结构不存在 → error[E0425]/E0599 cannot find in module `probe`，
//!   文件级编译红。
//! 背景：r26 修订3——全员 model/思考深度探针 SOP（见 coordination/archive/probe-model-r26.md，五员三员
//!   自报失真）。探针回答约定单行 "MODEL=<id> DEPTH=<档> [CWD=...]"。本棒实现解析纯函数：
//!   parse_probe_line(&str) -> Option<ProbeDecl{model,depth}>——MODEL/DEPTH 两键缺一不可，
//!   值为空视同缺失，多余键（CWD 等）容忍忽略，首尾空白容忍。后续轮把它接进 wake 探针自动化。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 model/depth 两值窜位 → `parses_model_and_depth` 红；
//!   M2 缺 DEPTH 仍返 Some → `missing_depth_returns_none` 红；
//!   M3 空值当有效值收下 → `empty_value_is_none` 红；
//!   M4 不容忍多余键(见 CWD 即 None) → `tolerates_extra_fields_and_whitespace` 红。
use orch_host::probe;

#[test]
fn parses_model_and_depth() {
    let p = probe::parse_probe_line("MODEL=glm-5.2 DEPTH=high").expect("双键齐全应解析成功");
    assert_eq!(p.model, "glm-5.2");
    assert_eq!(p.depth, "high");
}

#[test]
fn tolerates_extra_fields_and_whitespace() {
    let p = probe::parse_probe_line("  MODEL=gpt-5.6-sol   DEPTH=xhigh CWD=/tmp/x  ")
        .expect("多余键与首尾空白应容忍");
    assert_eq!(p.model, "gpt-5.6-sol");
    assert_eq!(p.depth, "xhigh");
}

#[test]
fn missing_model_returns_none() {
    assert!(probe::parse_probe_line("DEPTH=high").is_none(), "缺 MODEL 应返 None");
}

#[test]
fn missing_depth_returns_none() {
    assert!(probe::parse_probe_line("MODEL=x").is_none(), "缺 DEPTH 应返 None");
}

#[test]
fn empty_value_is_none() {
    assert!(
        probe::parse_probe_line("MODEL= DEPTH=high").is_none(),
        "空值视同缺失（探针答非所问不得混过）"
    );
}
