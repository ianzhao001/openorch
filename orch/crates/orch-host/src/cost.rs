//! `orch cost`：成本账本 v1 报表（决策 12 内建项，design/07）。
//! 折叠账本已有事实：VerdictIssued.costUsd / CostSampled(usage,durationSecs) / GateExecuted.durationMs；
//! 不发明新数据——报表是账本的投影（「事件是事实，状态是投影」同律）。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use orch_core::read_ledger;

#[derive(Debug, Clone, PartialEq)]
pub struct VerifyModelUsage {
    pub model: String,
    pub usage: crate::pricing::TokenUsage,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitInfo {
    pub utilization: f64,
    pub status: Option<String>,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VerifyUsage {
    pub total_cost_usd: Option<f64>,
    pub per_model: Vec<VerifyModelUsage>,
    pub rate_limit: Option<RateLimitInfo>,
}

/// Consume the final result and rate-limit records from a verifier JSONL stream.
pub fn parse_verify_usage(jsonl: &str) -> VerifyUsage {
    let mut out = VerifyUsage::default();
    for line in jsonl.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("result") => {
                out.total_cost_usd = value.get("total_cost_usd").and_then(|v| v.as_f64());
                out.per_model.clear();
                if let Some(models) = value.get("modelUsage").and_then(|v| v.as_object()) {
                    for (model, data) in models {
                        let usage = crate::pricing::TokenUsage {
                            input: data
                                .get("inputTokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            output: data
                                .get("outputTokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            cache_read: data
                                .get("cacheReadInputTokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            cache_write: data
                                .get("cacheCreationInputTokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                        };
                        out.per_model.push(VerifyModelUsage {
                            model: model.clone(),
                            usage,
                            cost_usd: data.get("costUSD").and_then(|v| v.as_f64()),
                        });
                    }
                }
            }
            Some("rate_limit_event") => {
                if let Some(info) = value.get("rate_limit_info") {
                    out.rate_limit = Some(RateLimitInfo {
                        utilization: info
                            .get("utilization")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0),
                        status: info
                            .get("status")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        resets_at: info
                            .get("resetsAt")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug, Default)]
pub struct TaskCost {
    pub verifier_usd: f64,
    pub verify_runs: usize,
    pub gate_runs: usize,
    pub gate_ms: u64,
    pub agent_duration_secs: u64,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub events: usize,
    pub escalations: usize,
    /// 这棒是谁跑的：最后一条 ReportObserved 的 envDecl.model（r27/B52）；老轮账本无 §0 → None
    pub model: Option<String>,
    pub verifier_usage: Vec<VerifyModelUsage>,
    pub rate_limit: Option<RateLimitInfo>,
}

#[derive(Debug, Default)]
pub struct RoundCost {
    pub tasks: BTreeMap<String, TaskCost>,
    pub total_usd: f64,
    pub total_events: usize,
    pub total_tokens: u64,
    pub total_runs: usize,
}

/// 按 verifier 成本降序返回最多 `n` 个任务；同成本按 task id 升序确定性定序。
pub fn costliest_tasks(rc: &RoundCost, n: usize) -> Vec<(String, f64)> {
    let mut ranked: Vec<(String, f64)> = rc
        .tasks
        .iter()
        .map(|(task_id, cost)| (task_id.clone(), cost.verifier_usd))
        .collect();
    ranked.sort_by(|(left_id, left_usd), (right_id, right_usd)| {
        right_usd
            .total_cmp(left_usd)
            .then_with(|| left_id.cmp(right_id))
    });
    ranked.truncate(n);
    ranked
}

/// usage 形状各家不同（原样存档的设计），尽力提取常见 token 键；提不出就 None（诚实缺省，不填 0）
fn extract_tokens(usage: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    let pick = |keys: &[&str]| -> Option<u64> {
        for k in keys {
            // 顶层或一层嵌套（如 {"tokens":{"input":..}} / {"usage":{"input_tokens":..}}）
            if let Some(n) = usage.get(*k).and_then(|v| v.as_u64()) {
                return Some(n);
            }
            for parent in ["tokens", "usage"] {
                if let Some(n) = usage
                    .get(parent)
                    .and_then(|t| t.get(*k))
                    .and_then(|v| v.as_u64())
                {
                    return Some(n);
                }
            }
        }
        None
    };
    (
        pick(&["input_tokens", "input", "prompt_tokens"]),
        pick(&["output_tokens", "output", "completion_tokens"]),
    )
}

/// 该 task **最后一条** ReportObserved 的 `payload.envDecl.model`（返工重收以最新为准）。
/// 无 ReportObserved / 无 envDecl / model 非字符串或空串 → None（老轮账本无 §0，不得虚构）。
pub fn task_model(events: &[orch_core::EventRecord], task: &str) -> Option<String> {
    let ev = events
        .iter()
        .rev()
        .find(|ev| ev.kind == "ReportObserved" && ev.task_id.as_deref() == Some(task))?;
    let model = ev
        .payload
        .as_ref()?
        .get("envDecl")?
        .get("model")?
        .as_str()?;
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

pub fn cost_report(root: &Path, round: &str) -> Result<RoundCost> {
    let ledger = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let lr = read_ledger(&ledger).with_context(|| format!("读取账本失败: {}", ledger.display()))?;
    let mut rc = RoundCost::default();
    for ev in &lr.events {
        rc.total_events += 1;
        let Some(task_id) = &ev.task_id else { continue };
        let t = rc.tasks.entry(task_id.clone()).or_default();
        t.events += 1;
        let p = ev.payload.as_ref();
        match ev.kind.as_str() {
            "VerdictIssued" => {
                t.verify_runs += 1;
                rc.total_runs += 1;
                if let Some(usd) = p.and_then(|p| p.get("costUsd")).and_then(|v| v.as_f64()) {
                    t.verifier_usd += usd;
                    rc.total_usd += usd;
                }
                if let Some(models) = p
                    .and_then(|p| p.get("modelUsage"))
                    .and_then(|v| v.as_object())
                {
                    for (model, data) in models {
                        let entry = VerifyModelUsage {
                            model: model.clone(),
                            usage: crate::pricing::TokenUsage {
                                input: data
                                    .get("inputTokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                                output: data
                                    .get("outputTokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                                cache_read: data
                                    .get("cacheReadInputTokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                                cache_write: data
                                    .get("cacheCreationInputTokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                            },
                            cost_usd: data.get("costUSD").and_then(|v| v.as_f64()),
                        };
                        rc.total_tokens = rc
                            .total_tokens
                            .saturating_add(entry.usage.input)
                            .saturating_add(entry.usage.output)
                            .saturating_add(entry.usage.cache_read)
                            .saturating_add(entry.usage.cache_write);
                        t.verifier_usage.push(entry);
                    }
                }
                if let Some(rate) = p.and_then(|p| p.get("rateLimit")) {
                    t.rate_limit = Some(RateLimitInfo {
                        utilization: rate
                            .get("utilization")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0),
                        status: rate
                            .get("status")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        resets_at: rate
                            .get("resetsAt")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                    });
                }
            }
            "GateExecuted" => {
                t.gate_runs += 1;
                t.gate_ms += p
                    .and_then(|p| p.get("durationMs"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
            }
            "CostSampled" => {
                t.agent_duration_secs += p
                    .and_then(|p| p.get("durationSecs"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if let Some(usage) = p.and_then(|p| p.get("usage")) {
                    let (i, o) = extract_tokens(usage);
                    if i.is_some() {
                        t.tokens_in = Some(t.tokens_in.unwrap_or(0) + i.unwrap());
                    }
                    if o.is_some() {
                        t.tokens_out = Some(t.tokens_out.unwrap_or(0) + o.unwrap());
                    }
                    rc.total_tokens = rc
                        .total_tokens
                        .saturating_add(i.unwrap_or(0))
                        .saturating_add(o.unwrap_or(0));
                }
            }
            "EscalationRaised" => t.escalations += 1,
            _ => {}
        }
    }
    for (task_id, t) in rc.tasks.iter_mut() {
        t.model = task_model(&lr.events, task_id);
    }
    Ok(rc)
}

/// Current-round dimensions used by budget snapshot code: (USD, tokens, runs).
pub fn spent_dimensions(rc: &RoundCost) -> (f64, u64, usize) {
    (rc.total_usd, rc.total_tokens, rc.total_runs)
}

/// `orch cost` 表格渲染（含 model 列；None 显示 `-`，镜像 orch-cli 既有列风格）。
/// 说明：orch-cli/src/main.rs 与 mcp.rs 本轮冻结，打印面以本函数落位在 cost.rs。
pub fn render_table(round: &str, rc: &RoundCost) -> String {
    use std::fmt::Write;
    let mut out = format!("orch cost · round={round} · 事件 {} 条\n", rc.total_events);
    let _ = writeln!(
        out,
        "  {:<6} {:>10} {:>7} {:>9} {:>9} {:>12} {:>6} {:>5} {}",
        "任务", "verifier$", "verify", "门(次)", "门(ms)", "agent", "tokens", "升级", "model"
    );
    for (id, t) in &rc.tasks {
        let tokens = match (t.tokens_in, t.tokens_out) {
            (Some(i), Some(o)) => format!("{i}/{o}"),
            (Some(i), None) => format!("{i}/-"),
            _ => "-".into(),
        };
        let _ = writeln!(
            out,
            "  {:<6} {:>10} {:>7} {:>9} {:>9} {:>11}s {:>6} {:>5} {}",
            id,
            if t.verifier_usd > 0.0 {
                format!("{:.2}", t.verifier_usd)
            } else {
                "-".into()
            },
            t.verify_runs,
            t.gate_runs,
            t.gate_ms,
            t.agent_duration_secs,
            tokens,
            t.escalations,
            t.model.as_deref().unwrap_or("-"),
        );
    }
    let _ = writeln!(out, "  合计 verifier ${:.2}", rc.total_usd);
    let top_spenders = costliest_tasks(rc, 3);
    if top_spenders.is_empty() {
        let _ = writeln!(out, "  top 支出: -");
    } else {
        let rendered = top_spenders
            .iter()
            .map(|(task_id, verifier_usd)| format!("{task_id} ${verifier_usd:.2}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  top 支出: {rendered}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use orch_core::EventRecord;

    #[test]
    fn extracts_common_token_shapes() {
        let a = serde_json::json!({"input_tokens": 100, "output_tokens": 20});
        assert_eq!(extract_tokens(&a), (Some(100), Some(20)));
        let b = serde_json::json!({"tokens": {"input": 5, "output": 6}});
        assert_eq!(extract_tokens(&b), (Some(5), Some(6)));
        let c = serde_json::json!({"weird": true});
        assert_eq!(extract_tokens(&c), (None, None)); // 诚实缺省，不填 0
    }

    fn report_event(task: &str, payload: serde_json::Value) -> EventRecord {
        crate::ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some(task),
            Some("rT"),
            payload,
        )
    }

    /// cost_report 集成层面：model 字段由账本折叠填充（返工以最新为准；无 §0 的老事件 → None）
    #[test]
    fn cost_report_fills_model_from_latest_report() {
        let dir = std::env::temp_dir().join(format!("b52-cost-{}", ulid::Ulid::new()));
        let round_dir = dir.join("coordination/rounds/rT");
        std::fs::create_dir_all(&round_dir).unwrap();
        let events = vec![
            report_event(
                "BX",
                serde_json::json!({"reportPath": "reports/BX-REPORT.md",
                    "envDecl": {"model": "old-model", "depth": "d", "capture": "c"}}),
            ),
            report_event(
                "BX",
                serde_json::json!({"reportPath": "reports/BX-REPORT.md",
                    "envDecl": {"model": "new-model", "depth": "d", "capture": "c"}}),
            ),
            report_event(
                "BY",
                serde_json::json!({"reportPath": "reports/BY-REPORT.md"}),
            ),
        ];
        let mut text = String::new();
        for ev in &events {
            text.push_str(&serde_json::to_string(ev).unwrap());
            text.push('\n');
        }
        std::fs::write(round_dir.join("events.jsonl"), text).unwrap();

        let rc = cost_report(&dir, "rT").unwrap();
        assert_eq!(rc.tasks["BX"].model.as_deref(), Some("new-model"));
        assert_eq!(rc.tasks["BY"].model, None); // 老轮账本无 §0，不得虚构
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// envDecl.model 非字符串类型时行为钉死：数字/布尔/对象一律 None（不 format 冒充身份）
    #[test]
    fn task_model_rejects_non_string_model() {
        for bad in [
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::json!({"name": "x"}),
        ] {
            let events = vec![report_event(
                "BZ",
                serde_json::json!({"reportPath": "p", "envDecl": {"model": bad}}),
            )];
            assert_eq!(task_model(&events, "BZ"), None, "非字符串 model 必须 None");
        }
    }

    /// 打印面：model 列有值照显、None 显示 `-`
    #[test]
    fn render_table_shows_model_column() {
        let mut rc = RoundCost::default();
        let mut with = TaskCost::default();
        with.model = Some("kimi-k3".into());
        rc.tasks.insert("B52".into(), with);
        rc.tasks.insert("B51".into(), TaskCost::default());
        let table = render_table("r27", &rc);
        assert!(table
            .lines()
            .any(|l| l.contains("B52") && l.ends_with("kimi-k3")));
        assert!(table.lines().any(|l| l.contains("B51") && l.ends_with('-')));
        assert!(table.lines().nth(1).unwrap().ends_with("model"));
    }

    #[test]
    fn costliest_tasks_total_cmp_orders_special_values() {
        let mut rc = RoundCost::default();
        for (task_id, verifier_usd) in [
            ("negative-zero", -0.0),
            ("positive-zero", 0.0),
            ("infinity", f64::INFINITY),
            ("nan", f64::NAN),
        ] {
            rc.tasks.insert(
                task_id.into(),
                TaskCost {
                    verifier_usd,
                    ..Default::default()
                },
            );
        }

        let task_ids: Vec<String> = costliest_tasks(&rc, usize::MAX)
            .into_iter()
            .map(|(task_id, _)| task_id)
            .collect();
        assert_eq!(
            task_ids,
            vec!["nan", "infinity", "positive-zero", "negative-zero"]
        );
    }

    #[test]
    fn render_table_appends_at_most_three_top_spenders_after_total() {
        let mut rc = RoundCost {
            total_usd: 10.0,
            ..Default::default()
        };
        for (task_id, verifier_usd) in [("A", 4.0), ("B", 3.0), ("C", 2.0), ("D", 1.0)] {
            rc.tasks.insert(
                task_id.into(),
                TaskCost {
                    verifier_usd,
                    ..Default::default()
                },
            );
        }

        let table = render_table("rT", &rc);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines[lines.len() - 2], "  合计 verifier $10.00");
        assert_eq!(lines.last(), Some(&"  top 支出: A $4.00, B $3.00, C $2.00"));
    }
}
