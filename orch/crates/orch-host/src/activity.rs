//! 活动流归一化（r45/B100）：把各家执行者 CLI 的 stream-json 归一成结构化活动事件，
//! 供快照内核与面板消费。
//!
//! 形状依据 `coordination/scripts/watch_sessions.py` 已实证的解析分支：
//!   · codex     ：`item.completed`/`item.started`（item.type ∈ agent_message/command_execution/file_change）、
//!                 `thread.started`/`turn.started`（相位）、`turn.completed`（usage 结算）
//!   · opencode  ：`text` / `tool_use` / `step_start` / `step_finish`（tokens 结算）
//!   · claude/agy：`assistant`（message.content[] 首个 text/tool_use 块）、`system`（相位）、
//!                 `result`（total_cost_usd 结算）
//!
//! 纪律：
//! - 未知形状**显式**标 `ActivityKind::Unknown` 并保留 `raw_ref`（文件+行号），绝不静默丢弃；
//! - `summary` 一律过 `redact::redact_full`（密文不进结构体），原文只留 `raw_ref`，
//!   面板展开时才按 raw_ref 回读；
//! - 纯 std + 既有依赖（serde_json/redact），不新增依赖。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::redact;

/// 归一化后的活动类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ActivityKind {
    /// 模型说话（文本消息）
    Message,
    /// 工具/命令调用
    Tool,
    /// 文件变更
    File,
    /// 用量结算行
    Token,
    /// turn/step 开始或结束（相位边界）
    Phase,
    /// 未识别形状（保留原文引用，绝不静默丢弃）
    Unknown,
}

/// 原文引用：来源文件 + 行号（1 起）。原文本身不进结构体。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RawRef {
    pub path: String,
    pub line: u64,
}

/// 结构化活动事件。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ActivityEvent {
    /// 源行自带时间戳（有则取之，无则 None；不由本层凭空造）
    pub ts: Option<String>,
    pub agent: String,
    pub kind: ActivityKind,
    /// 人读摘要：已过 `redact::redact_full`，并按字符截断
    pub summary: String,
    pub raw_ref: RawRef,
}

/// summary 截断上限（按字符，对齐 watch_sessions.py 的 160-200 截断先例）
const SUMMARY_MAX_CHARS: usize = 200;

/// summary 收尾：先全量过打码组合网，再按字符截断（截断不会还原密文）。
fn finish_summary(raw: &str) -> String {
    let redacted = redact::redact_full(raw);
    redacted.chars().take(SUMMARY_MAX_CHARS).collect()
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

fn json_u64(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// 源行自带时间戳：claude `timestamp` / 通用 `ts`（字符串或数值原样转出）。
fn extract_ts(value: &serde_json::Value) -> Option<String> {
    if let Some(ts) = json_str(value, "timestamp") {
        return Some(ts.to_string());
    }
    value
        .get("ts")
        .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
}

/// 归一化单行 stream-json。任何输入都产出一个事件（未知形状 → Unknown + raw_ref）。
pub fn parse_activity_line(
    agent: &str,
    source_path: &str,
    line_no: u64,
    line: &str,
) -> ActivityEvent {
    let mut event = ActivityEvent {
        ts: None,
        agent: agent.to_string(),
        kind: ActivityKind::Unknown,
        summary: String::new(),
        raw_ref: RawRef {
            path: source_path.to_string(),
            line: line_no,
        },
    };
    let trimmed = line.trim();
    // 非 JSON 行（SSE 前缀/噪声/截断行）：显式 Unknown，摘要留打码后的原文片段
    if !trimmed.starts_with('{') {
        event.summary = finish_summary(trimmed);
        return event;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        event.summary = finish_summary(trimmed);
        return event;
    };
    event.ts = extract_ts(&value);
    let Some(kind_tag) = json_str(&value, "type") else {
        event.summary = finish_summary(trimmed);
        return event;
    };

    let classified: Option<(ActivityKind, String)> = match kind_tag {
        // ── codex ──
        "item.completed" | "item.started" => {
            let item = value.get("item").unwrap_or(&serde_json::Value::Null);
            match json_str(item, "type") {
                Some("agent_message") => Some((
                    ActivityKind::Message,
                    json_str(item, "text").unwrap_or("").to_string(),
                )),
                Some("command_execution") => Some((
                    ActivityKind::Tool,
                    format!("$ {}", json_str(item, "command").unwrap_or("")),
                )),
                Some("file_change") => {
                    let first = item
                        .get("changes")
                        .and_then(|c| c.as_array())
                        .and_then(|c| c.first())
                        .unwrap_or(&serde_json::Value::Null);
                    Some((
                        ActivityKind::File,
                        format!(
                            "{} {}",
                            json_str(first, "kind").unwrap_or(""),
                            json_str(first, "path").unwrap_or("")
                        ),
                    ))
                }
                _ => None,
            }
        }
        "thread.started" | "turn.started" => Some((ActivityKind::Phase, kind_tag.to_string())),
        "turn.completed" => {
            let usage = value.get("usage").unwrap_or(&serde_json::Value::Null);
            Some((
                ActivityKind::Token,
                format!(
                    "turn completed: in={} cached={} out={}",
                    json_u64(usage, "input_tokens"),
                    json_u64(usage, "cached_input_tokens"),
                    json_u64(usage, "output_tokens")
                ),
            ))
        }
        // ── opencode ──
        "text" => Some((
            ActivityKind::Message,
            value
                .pointer("/part/text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        )),
        "tool_use" => Some((
            ActivityKind::Tool,
            format!(
                "tool {}",
                value
                    .pointer("/part/tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ),
        )),
        "step_start" => Some((ActivityKind::Phase, kind_tag.to_string())),
        "step_finish" => {
            // 用量结算行，绝不能判成 Message（变异清单第 5 条下界）
            let tokens = value.pointer("/part/tokens").unwrap_or(&serde_json::Value::Null);
            Some((
                ActivityKind::Token,
                format!(
                    "step finish: in={} out={} cache={}",
                    json_u64(tokens, "input"),
                    json_u64(tokens, "output"),
                    tokens
                        .get("cache")
                        .map(|c| json_u64(c, "read"))
                        .unwrap_or(0)
                ),
            ))
        }
        // ── claude / agy ──
        "assistant" => {
            let blocks = value
                .pointer("/message/content")
                .and_then(|c| c.as_array());
            blocks.and_then(|blocks| {
                for block in blocks {
                    match json_str(block, "type") {
                        Some("text") => {
                            if let Some(text) =
                                json_str(block, "text").filter(|t| !t.is_empty())
                            {
                                return Some((ActivityKind::Message, text.to_string()));
                            }
                        }
                        Some("tool_use") => {
                            let name = json_str(block, "name").unwrap_or("");
                            let input = block
                                .get("input")
                                .map(|i| i.to_string())
                                .unwrap_or_default();
                            return Some((ActivityKind::Tool, format!("{name} {input}")));
                        }
                        _ => {}
                    }
                }
                None
            })
        }
        "system" => Some((
            ActivityKind::Phase,
            format!(
                "system {}",
                json_str(&value, "subtype").unwrap_or("")
            ),
        )),
        "result" => Some((
            ActivityKind::Token,
            format!(
                "result: cost=${:.4}",
                value
                    .get("total_cost_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            ),
        )),
        _ => None,
    };

    match classified {
        Some((kind, summary_raw)) => {
            event.kind = kind;
            // 已识别形状但载荷为空：回退到原文片段，避免「识别了却无可读摘要」
            let summary_raw = if summary_raw.trim().is_empty() {
                trimmed
            } else {
                &summary_raw
            };
            event.summary = finish_summary(summary_raw);
        }
        None => {
            // 未知形状：显式 Unknown + 打码后的原文片段 + raw_ref，绝不静默丢弃
            event.summary = finish_summary(trimmed);
        }
    }
    event
}

/// 整份日志归一化：逐行产出事件（行号 1 起，空行跳过）。
pub fn parse_activity_log(agent: &str, source_path: &str, text: &str) -> Vec<ActivityEvent> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| parse_activity_line(agent, source_path, (index + 1) as u64, line))
        .collect()
}

/// 进程内单调计数器（errata O28：只用挂钟纳秒在同 tick 连调会撞名，
/// 唯一性必须叠加 pid 与进程内计数器）。
static WAKE_LOG_SEQ: AtomicU64 = AtomicU64::new(0);

/// 每次注入的独立 wake 日志路径：`coordination/runtime/logs/wake-<agent>-<ts>-<pid>-<seq>.jsonl`。
/// - ts：挂钟纳秒（定宽 19 位，字典序≈时序，单调可排序）；
/// - pid + 进程内 AtomicU64：同进程并发/同 tick 不撞名；
/// - 纯路径构造，不做任何 IO（调用方负责建目录与 create_new）。
pub fn wake_log_path(root: &Path, agent: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let seq = WAKE_LOG_SEQ.fetch_add(1, Ordering::Relaxed);
    root.join("coordination/runtime/logs")
        .join(format!("wake-{agent}-{nanos}-{pid}-{seq}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "logs/test.jsonl";

    #[test]
    fn codex_file_change_is_file_kind() {
        let event = parse_activity_line(
            "a",
            SRC,
            1,
            r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"kind":"update","path":"src/x.rs"}]}}"#,
        );
        assert_eq!(event.kind, ActivityKind::File);
        assert!(event.summary.contains("src/x.rs"));
    }

    #[test]
    fn turn_started_is_phase() {
        let event = parse_activity_line("a", SRC, 1, r#"{"type":"turn.started"}"#);
        assert_eq!(event.kind, ActivityKind::Phase);
    }

    #[test]
    fn claude_tool_use_block_is_tool() {
        let event = parse_activity_line(
            "a",
            SRC,
            1,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"cmd":"ls"}}]}}"#,
        );
        assert_eq!(event.kind, ActivityKind::Tool);
        assert!(event.summary.contains("Bash"));
    }

    #[test]
    fn malformed_json_is_unknown_not_dropped() {
        let event = parse_activity_line("a", SRC, 9, "{not json");
        assert_eq!(event.kind, ActivityKind::Unknown);
        assert_eq!(event.raw_ref.line, 9);
        assert!(!event.summary.is_empty());
    }

    #[test]
    fn whole_log_line_numbers_are_one_based() {
        let events = parse_activity_log(
            "a",
            SRC,
            "{\"type\":\"turn.started\"}\n\n{\"type\":\"result\",\"total_cost_usd\":0.1}\n",
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].raw_ref.line, 1);
        assert_eq!(events[1].raw_ref.line, 3);
        assert_eq!(events[1].kind, ActivityKind::Token);
    }
}
