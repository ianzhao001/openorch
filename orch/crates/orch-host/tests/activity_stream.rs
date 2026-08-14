//! B100 契约种子 · wake 流落盘修复 + 活动流归一化（r45）
//!
//! 落位：`orch/crates/orch-host/tests/activity_stream.rs`（逐字节搬运，禁止修改）
//! 预期红形态：**compile**（`orch_host::activity::*` 的项尚不存在 → E0432/E0425）
//!
//! ## 契约
//!
//! ### 1. 归一化（纯函数）
//! `parse_activity_line(agent, source_path, line_no, line) -> ActivityEvent`
//!   - 四家 stream-json 形状必须各自识别（形状取自 `coordination/scripts/watch_sessions.py`
//!     已实证的解析分支）：
//!     · codex   ：`{"type":"item.completed","item":{"type":"agent_message","text":…}}` → Message
//!                 `{"type":"item.completed","item":{"type":"command_execution","command":…}}` → Tool
//!                 `{"type":"turn.completed","usage":{…}}` → Token
//!     · opencode：`{"type":"text","part":{"text":…}}` → Message
//!                 `{"type":"tool_use","part":{"tool":…}}` → Tool
//!                 `{"type":"step_finish","part":{"tokens":{…}}}` → Token
//!     · claude/agy：`{"type":"assistant","message":{"content":[{"type":"text",…}]}}` → Message
//!                 `{"type":"result","total_cost_usd":…}` → Token
//!   - 无法识别的形状 → `ActivityKind::Unknown`，**必须保留 raw_ref，不得静默丢弃**
//!   - `summary` **必须过 `redact::redact_full`**（密文不得进入结构体）
//!   - `raw_ref` 记录来源文件与行号，原文本身不进结构体（面板展开时才回读）
//!
//! ### 2. wake 落盘唯一性
//! `wake_log_path(root, agent) -> PathBuf`
//!   - 同一 agent 连续两次调用**必须给出不同路径**（每次注入独立文件，历史不被覆盖）
//!   - 路径必须落在 `coordination/runtime/logs/` 下且文件名含 agent
//!   - 唯一性必须同时含 pid 与进程内单调计数器（errata O28：只用挂钟纳秒会撞名）
//!
//! ## 负向变异清单（REPORT §5 逐条自证）
//! 1. 落盘改回截断/固定路径 → `wake_log_paths_are_unique_per_injection` 红
//! 2. 唯一性只用时间戳（去掉计数器）→ 同上用例红（同 tick 连调必撞名）
//! 3. 跳过 redact → `summary_is_redacted` 红
//! 4. 未知形状静默丢弃（返回 None / 空 summary）→ `unknown_shape_is_preserved` 红
//! 5. opencode 的 `step_finish` 被判成 Message → `recognizes_four_vendor_shapes` 红

use orch_host::activity::{parse_activity_line, wake_log_path, ActivityKind};

const SRC: &str = "coordination/runtime/logs/wake-executor-opencode-1.jsonl";

fn kind_of(line: &str) -> ActivityKind {
    parse_activity_line("executor-opencode", SRC, 1, line).kind
}

#[test]
fn recognizes_four_vendor_shapes() {
    // codex
    assert_eq!(
        kind_of(r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#),
        ActivityKind::Message
    );
    assert_eq!(
        kind_of(r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo test"}}"#),
        ActivityKind::Tool
    );
    assert_eq!(
        kind_of(r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#),
        ActivityKind::Token
    );
    // opencode
    assert_eq!(kind_of(r#"{"type":"text","part":{"text":"hello"}}"#), ActivityKind::Message);
    assert_eq!(kind_of(r#"{"type":"tool_use","part":{"tool":"bash"}}"#), ActivityKind::Tool);
    assert_eq!(
        kind_of(r#"{"type":"step_finish","part":{"tokens":{"input":1,"output":2}}}"#),
        ActivityKind::Token,
        "opencode 的 step_finish 是用量结算行，不能判成 Message"
    );
    // claude / agy
    assert_eq!(
        kind_of(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#),
        ActivityKind::Message
    );
    assert_eq!(
        kind_of(r#"{"type":"result","total_cost_usd":1.5}"#),
        ActivityKind::Token
    );
}

#[test]
fn unknown_shape_is_preserved() {
    let event = parse_activity_line("executor-claw2", SRC, 42, "data: {\"type\":\"reasoning\"}");
    assert_eq!(
        event.kind,
        ActivityKind::Unknown,
        "未识别形状必须显式标 Unknown，不得静默丢弃"
    );
    assert_eq!(event.raw_ref.line, 42, "必须保留原文行号以便展开回读");
    assert_eq!(event.raw_ref.path, SRC, "必须保留来源文件");
}

#[test]
fn summary_is_redacted() {
    let line = r#"{"type":"text","part":{"text":"key sk-livesecret and Bearer abcdef"}}"#;
    let event = parse_activity_line("executor-opencode", SRC, 7, line);
    assert!(
        !event.summary.contains("sk-livesecret"),
        "summary 必须过 redact_full，密文不得进入结构体：{}",
        event.summary
    );
    assert!(
        !event.summary.contains("Bearer abcdef"),
        "Bearer token 必须被打码：{}",
        event.summary
    );
}

#[test]
fn activity_carries_agent_identity() {
    let event = parse_activity_line("executor-desktop", SRC, 3, r#"{"type":"text","part":{"text":"x"}}"#);
    assert_eq!(event.agent, "executor-desktop");
}

#[test]
fn wake_log_paths_are_unique_per_injection() {
    let root = std::path::Path::new("/tmp/orch-b100-fixture");
    let first = wake_log_path(root, "executor-opencode");
    let second = wake_log_path(root, "executor-opencode");
    assert_ne!(
        first, second,
        "每次注入必须落独立文件，历史不得被覆盖（wake.rs 旧实现用 File::create 截断）"
    );
    for path in [&first, &second] {
        let text = path.to_string_lossy();
        assert!(
            text.contains("coordination/runtime/logs"),
            "wake 日志必须落在 runtime/logs 下: {text}"
        );
        assert!(
            text.contains("executor-opencode"),
            "文件名必须含 agent 以便面板归集: {text}"
        );
    }
}
