//! ═══ 红种子契约 · B35 ═══（落位: orch/crates/orch-host/tests/poke_render.rs，逐字节复制）
//! 预期红（redForm: compile）：wake::render_poke / set_session 尚不存在（E0425，文件级编译红）。
//! 背景：design/10 §4 之③④——不可注入执行者的 POKE 粘贴词渲染 + 会话切换命令的纯函数层。
//! 变异清单（E9 下界）: M1 render_poke 忽略 {round} 占位 → ① 红；M2 set_session 对 in-flight 不产警告 → ③ 红；M3 set_session 未知 agent 静默成功 → ④ 红
use orch_host::wake;

#[test]
fn poke_substitutes_round() {
    // ① pokeHint 的 {round} 占位替换
    let got = wake::render_poke("r20 已派发,继续协作:重跑 sh coordination/scripts/wait-dispatch.sh executor-claw".replace("r20", "r{round}").as_str(), "r21");
    assert!(got.contains("r21 已派发"));
    assert!(!got.contains("{round}"));
}

#[test]
fn poke_without_placeholder_passthrough() {
    // ② 无占位模板原样直通
    let got = wake::render_poke("固定文案无占位", "r21");
    assert_eq!(got, "固定文案无占位");
}

#[test]
fn set_session_warns_on_inflight() {
    // ③ in-flight 时切换会话 → Ok 但携带警告文案(不禁止,用户有最终权)
    let out = wake::set_session_decision(true);
    assert!(out.warning.is_some());
    assert!(out.warning.unwrap().contains("in-flight"));
    let quiet = wake::set_session_decision(false);
    assert!(quiet.warning.is_none());
}

#[test]
fn unknown_agent_is_error_shape() {
    // ④ set_session 的注册表查找语义:未知 agent 必须显式报错(纯函数层用查找结果建模)
    let known = ["executor-desktop".to_string(), "executor-opencode".to_string()];
    assert!(wake::validate_agent(&known, "executor-desktop").is_ok());
    let err = wake::validate_agent(&known, "executor-nobody").unwrap_err();
    assert!(err.contains("executor-nobody"));
}
