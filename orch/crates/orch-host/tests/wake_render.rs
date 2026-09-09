//! ═══ 红种子契约 · B34 ═══（落位: orch/crates/orch-host/tests/wake_render.rs，逐字节复制）
//! 预期红（redForm: compile）：wake::WakeSpec / render_wake_argv 尚不存在（E0432/E0425，文件级编译红）。
//! 背景：design/10 §4——注入唤醒。注册表 coordination/agents.yaml 提供 wake.argv 模板
//!   （{session}/{message} 占位，语义沿 AdapterSpec 先例 adapter.rs:47-62）。
//! 变异清单（E9 下界）: M1 忽略 {message} 占位不替换 → ① 红；M2 缺 sessionId 不报错静默空串 → ② 红；M3 argv 顺序重排 → ④ 红
use orch_host::wake::{self, WakeSpec};

fn spec(session: &str, argv: &[&str]) -> WakeSpec {
    WakeSpec {
        injectable: true,
        session_id: session.to_string(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn placeholders_are_substituted() {
    // ① {session}/{message} 双占位替换
    let s = spec("ses_42", &["opencode", "run", "--session", "{session}", "{message}"]);
    let got = wake::render_wake_argv(&s, "GO 已派发").unwrap();
    assert_eq!(got, vec!["opencode", "run", "--session", "ses_42", "GO 已派发"]);
}

#[test]
fn empty_session_is_explicit_error() {
    // ② sessionId 未回填 → Err（文案含 sessionId 字样，绝不静默注入空串）
    let s = spec("", &["codex", "exec", "resume", "{session}", "{message}"]);
    let err = wake::render_wake_argv(&s, "hi").unwrap_err();
    assert!(err.contains("sessionId"));
}

#[test]
fn message_with_spaces_stays_single_arg() {
    // ③ 含空格/引号的消息保持单一 argv 元素（spawn argv 语义，无 shell 二次解释）
    let s = spec("t1", &["codex", "exec", "resume", "{session}", "{message}"]);
    let got = wake::render_wake_argv(&s, r#"NUDGE: 按 "内容" 行事"#).unwrap();
    assert_eq!(got.len(), 5);
    assert_eq!(got[4], r#"NUDGE: 按 "内容" 行事"#);
}

#[test]
fn template_order_preserved() {
    // ④ 模板顺序原样保留（flag 相对位置不许重排）
    let s = spec("s9", &["a", "--x", "{message}", "--session", "{session}", "--tail"]);
    let got = wake::render_wake_argv(&s, "m").unwrap();
    assert_eq!(got, vec!["a", "--x", "m", "--session", "s9", "--tail"]);
}
