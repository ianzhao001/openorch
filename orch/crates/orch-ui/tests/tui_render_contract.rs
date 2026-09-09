//! B103 契约种子 · 只读 TUI 渲染契约（r45）
//!
//! 落位：`orch/crates/orch-ui/tests/tui_render_contract.rs`（逐字节搬运，禁止修改）
//! 预期红形态：**assertion**（B99 合并后 crate 存在但渲染层未实现；
//! seed-verified 必须在 B99 合入 main 之后再跑）
//!
//! ## 契约
//!
//! `render(&OrchSnapshot, width, height) -> ratatui::buffer::Buffer` —— **纯函数、不碰终端**，
//! 因此可用内存 buffer 断言（与 `ratatui::backend::TestBackend` 同源思路）。
//!
//!   1. `critical` 告警的可视样式必须与 `info` 可区分（不能只靠文字顺序）
//!   2. 快照 `source == Frontend`（前端自算）或已陈旧时，必须在画面上显式标注
//!   3. 活动流渲染必须是 redact 后的文本（纵深防御：即便上游漏网也不得把密文画到屏幕）
//!   4. 极窄/极矮尺寸必须截断而不 panic
//!   5. **纯只读**：按键动作集合里不得存在任何会触发写操作的动作
//!
//! ## 负向变异清单（REPORT §5 逐条自证）
//! 1. critical 与 info 用同一样式 → `critical_alerts_are_visually_distinct` 红
//! 2. 去掉陈旧/前端自算标记 → `frontend_snapshot_is_marked` 红
//! 3. 直接渲染未 redact 的活动原文 → `activity_is_rendered_redacted` 红
//! 4. 窄宽度越界 panic → `narrow_viewport_truncates_without_panic` 红
//! 5. 新增任何写操作按键动作 → `key_bindings_are_read_only` 红

use orch_host::snapshot::{
    ActivityLine, Alert, OrchSnapshot, Severity, SnapshotSource,
};
use orch_ui::{key_bindings, render};

fn alert(severity: Severity, subject: &str, message: &str) -> Alert {
    Alert {
        severity,
        kind: "liveness".to_string(),
        subject: subject.to_string(),
        message: message.to_string(),
        suggested_command: Some(format!("orch retry-dead {subject}")),
    }
}

fn base(source: SnapshotSource) -> OrchSnapshot {
    let mut snap = OrchSnapshot::empty("r45", "2026-07-26T00:00:00Z");
    snap.source = source;
    snap
}

fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    buf.content()
        .iter()
        .map(|cell| cell.symbol().to_string())
        .collect::<Vec<_>>()
        .concat()
}

#[test]
fn critical_alerts_are_visually_distinct() {
    let mut snap = base(SnapshotSource::Daemon);
    snap.alerts = vec![
        alert(Severity::Critical, "executor-desktop", "DEADEXEC"),
        alert(Severity::Info, "executor-opencode", "infoline"),
    ];
    let buf = render(&snap, 120, 40);

    let style_of = |needle: char| {
        buf.content()
            .iter()
            .find(|cell| cell.symbol().starts_with(needle))
            .map(|cell| cell.style())
    };
    let critical_style = style_of('D').expect("critical 告警文本必须上屏");
    let info_style = style_of('i').expect("info 告警文本必须上屏");
    assert_ne!(
        critical_style, info_style,
        "critical 必须与 info 视觉可区分（样式不同），不能只靠排序"
    );
}

#[test]
fn frontend_snapshot_is_marked() {
    let snap = base(SnapshotSource::Frontend);
    let text = buffer_text(&render(&snap, 120, 40));
    assert!(
        text.contains("frontend") || text.contains("自算") || text.contains("陈旧"),
        "前端自算/陈旧快照必须显式标注，避免把投影当真值：\n{text}"
    );
}

#[test]
fn activity_is_rendered_redacted() {
    let mut snap = base(SnapshotSource::Daemon);
    snap.activity = vec![ActivityLine {
        ts: "2026-07-26T00:00:01Z".to_string(),
        agent: "executor-opencode".to_string(),
        kind: "message".to_string(),
        summary: "token sk-livesecret leaked".to_string(),
    }];
    let text = buffer_text(&render(&snap, 120, 40));
    assert!(
        !text.contains("sk-livesecret"),
        "活动流必须以 redact 后的文本渲染，密文不得上屏：\n{text}"
    );
}

#[test]
fn narrow_viewport_truncates_without_panic() {
    let mut snap = base(SnapshotSource::Daemon);
    snap.alerts = vec![alert(Severity::Critical, "executor-desktop", "DEADEXEC")];
    for (w, h) in [(20u16, 6u16), (1, 1), (200, 3)] {
        let buf = render(&snap, w, h);
        assert_eq!(buf.area().width, w, "渲染宽度必须与请求一致（截断而非越界）");
        assert_eq!(buf.area().height, h, "渲染高度必须与请求一致");
    }
}

#[test]
fn key_bindings_are_read_only() {
    let bindings = key_bindings();
    assert!(bindings.len() >= 2, "至少要有刷新与退出两个按键");
    for (key, action) in bindings {
        assert!(
            action.is_read_only(),
            "TUI 是纯只读面板：按键 {key:?} 绑定了非只读动作 {action:?}"
        );
    }
}
