//! orch mcp serve 内核（r26/B44）：手写极简纯同步 JSON-RPC（R4 调研定级「暂缓 rmcp,换方案」）。
//! 纯函数面：请求解析 / 路由 / 应答构造；只读工具面 status/cost/doctor——写操作面保留 HITL
//! 边界不暴露（R4 §4 裁定）。stdio 循环薄壳在 orch-cli。
//! （planner 预置占位：lib.rs 声明先行入库，B44 在本文件内实现，勿动 lib.rs——frozenPaths。）

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use orch_core::{doctor, fold, read_ledger, CheckStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// MCP 规范在本切片实现时的稳定协议版本；initialize 固定声明该版本，避免依赖协商状态。
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RpcRequest {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    pub annotations: Value,
}

pub fn parse_request(line: &str) -> Option<RpcRequest> {
    let request = serde_json::from_str::<RpcRequest>(line).ok()?;
    if request.method.is_empty() {
        return None;
    }
    Some(request)
}

pub fn tools_manifest() -> Vec<ToolSpec> {
    [
        (
            "orch_status",
            "Read the current round and task projection without changing repository state.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        (
            "orch_cost",
            "Read the cost projection for a round; defaults to the current round.",
            json!({
                "type": "object",
                "properties": {
                    "round": {
                        "type": "string",
                        "description": "Optional round id; defaults to CURRENT-ROUND."
                    }
                },
                "additionalProperties": false
            }),
        ),
        (
            "orch_doctor",
            "Read coordination layout health checks without repairing or writing files.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
    ]
    .into_iter()
    .map(|(name, description, input_schema)| ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
        annotations: json!({
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        }),
    })
    .collect()
}

fn success_response(request: &RpcRequest, result: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": &request.id,
        "result": result,
    }))
    .expect("JSON-RPC success response is serializable")
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    }))
    .expect("JSON-RPC error response is serializable")
}

pub fn parse_error_response() -> String {
    error_response(&Value::Null, -32700, "Parse error")
}

pub fn respond_route_error(request: &RpcRequest) -> String {
    error_response(&request.id, -32601, "Method not found")
}

pub fn initialize_response(request: &RpcRequest) -> String {
    success_response(
        request,
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": "orch",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

pub fn tools_list_response(request: &RpcRequest) -> String {
    success_response(request, json!({"tools": tools_manifest()}))
}

fn status_text(root: &Path) -> Result<String> {
    let round = crate::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path)
        .with_context(|| format!("读取账本失败: {}", ledger_path.display()))?;
    let projection = fold(&ledger.events);
    let mut output = format!(
        "orch status · round={round} · events={} · bad_lines={} · plan_signed_off={} · round_closed={}",
        projection.total_events,
        ledger.bad_lines.len(),
        projection.plan_signed_off,
        projection.round_closed
    );
    for (task_id, task) in &projection.tasks {
        write!(
            output,
            "\n{task_id}\t{}\tevents={}\tlast={}",
            task.state
                .map(|state| state.to_string())
                .unwrap_or_else(|| "?".to_string()),
            task.event_count,
            task.last_ts
        )
        .expect("writing to String cannot fail");
    }
    Ok(output)
}

fn cost_text(root: &Path, arguments: &Value) -> Result<String> {
    let round = match arguments.get("round").and_then(Value::as_str) {
        Some(round) if !round.is_empty() => round.to_string(),
        _ => crate::current_round(root)?,
    };
    let report = crate::cost::cost_report(root, &round)?;
    let mut output = format!(
        "orch cost · round={round} · events={} · verifier_usd={:.2}",
        report.total_events, report.total_usd
    );
    for (task_id, task) in &report.tasks {
        write!(
            output,
            "\n{task_id}\tverifier_usd={:.2}\tverify_runs={}\tgate_runs={}\tgate_ms={}\tagent_secs={}\ttokens={}/{}\tescalations={}",
            task.verifier_usd,
            task.verify_runs,
            task.gate_runs,
            task.gate_ms,
            task.agent_duration_secs,
            task.tokens_in
                .map(|tokens| tokens.to_string())
                .unwrap_or_else(|| "-".to_string()),
            task.tokens_out
                .map(|tokens| tokens.to_string())
                .unwrap_or_else(|| "-".to_string()),
            task.escalations
        )
        .expect("writing to String cannot fail");
    }
    Ok(output)
}

fn doctor_text(root: &Path) -> String {
    let mut output = format!("orch doctor · root={}", root.display());
    for check in doctor(root) {
        let status = match check.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        };
        write!(output, "\n{status}\t{}\t{}", check.name, check.detail)
            .expect("writing to String cannot fail");
    }
    output
}

fn tool_result_response(request: &RpcRequest, text: String, is_error: bool) -> String {
    success_response(
        request,
        json!({
            "content": [{
                "type": "text",
                "text": text
            }],
            "isError": is_error
        }),
    )
}

pub fn tools_call_response(root: &Path, request: &RpcRequest) -> String {
    let Some(name) = request.params.get("name").and_then(Value::as_str) else {
        return error_response(&request.id, -32602, "Invalid params: missing tool name");
    };
    let arguments = request.params.get("arguments").unwrap_or(&Value::Null);
    let outcome = match name {
        "orch_status" => status_text(root),
        "orch_cost" => cost_text(root, arguments),
        "orch_doctor" => Ok(doctor_text(root)),
        _ => {
            return error_response(
                &request.id,
                -32602,
                &format!("Invalid params: unknown tool {name}"),
            );
        }
    };
    match outcome {
        Ok(text) => tool_result_response(request, text, false),
        Err(error) => tool_result_response(request, format!("{error:#}"), true),
    }
}

/// 路由一个已解析请求。MCP initialized notification 不带应答，其余请求均返回单行 JSON。
pub fn route_request(root: &Path, request: &RpcRequest) -> Option<String> {
    match request.method.as_str() {
        "initialize" => Some(initialize_response(request)),
        "notifications/initialized" => None,
        "ping" => Some(success_response(request, json!({}))),
        "tools/list" => Some(tools_list_response(request)),
        "tools/call" => Some(tools_call_response(root, request)),
        _ => Some(respond_route_error(request)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_tool_call_is_a_json_rpc_error_with_request_id() {
        let request = parse_request(
            r#"{"jsonrpc":"2.0","id":"call-1","method":"tools/call","params":{"name":"orch_merge","arguments":{}}}"#,
        )
        .unwrap();

        let response = tools_call_response(Path::new("."), &request);
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["id"], "call-1");
        assert_eq!(value["error"]["code"], -32602);
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    #[test]
    fn initialize_then_tools_list_are_valid_json() {
        let initialize =
            parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
        let initialized: Value = serde_json::from_str(&initialize_response(&initialize)).unwrap();
        assert_eq!(initialized["jsonrpc"], "2.0");
        assert_eq!(
            initialized["result"]["protocolVersion"],
            MCP_PROTOCOL_VERSION
        );

        let list = parse_request(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        let listed: Value = serde_json::from_str(&tools_list_response(&list)).unwrap();
        assert_eq!(listed["id"], 2);
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn missing_params_default_to_null() {
        let request = parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
        assert_eq!(request.params, Value::Null);
    }
}
