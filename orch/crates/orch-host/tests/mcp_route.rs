//! ═══ 红种子契约 · B44 ═══（落位: orch/crates/orch-host/tests/mcp_route.rs，逐字节复制）
//! 预期红（redForm: compile）：`orch_host::mcp` 模块已由 planner 预置空占位（lib.rs 已声明），
//!   但 parse_request/tools_manifest/respond_route_error/initialize_response 均不存在
//!   → error[E0425]/E0599 cannot find function in module `mcp`，文件级编译红。
//! 背景：R4 调研定级「暂缓 rmcp,换方案=手写极简纯同步 JSON-RPC」。本棒为 orch mcp serve 首切片：
//!   纯函数内核（解析/路由/应答构造）+ 只读工具面（status/cost/doctor）。**写操作面（dispatch/merge/
//!   round close）保留 HITL 边界，严禁暴露为无头 MCP tool（R4 §4 裁定）**。stdio 循环是 IO 薄壳，
//!   由卡+verifier 核，不在本纯函数种子内。
//! 变异清单（E9 下界，负向自证逐条注入应各自变红）：
//!   M1 manifest 混入写工具(orch_merge) → `manifest_excludes_write_operations` 红；
//!   M2 parse_request 对缺 method 的行不返 None → `malformed_line_returns_none` 红；
//!   M3 未知 method 不回 -32601 → `unknown_method_yields_method_not_found` 红。
use orch_host::mcp;

#[test]
fn parse_request_extracts_id_and_method() {
    let req = mcp::parse_request(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#)
        .expect("合法 JSON-RPC 请求应解析成功");
    assert_eq!(req.method, "tools/list");
    assert_eq!(req.id, serde_json::json!(7));
}

#[test]
fn malformed_line_returns_none() {
    assert!(mcp::parse_request("not json at all").is_none(), "非 JSON 行应返 None");
    assert!(
        mcp::parse_request(r#"{"jsonrpc":"2.0","id":1}"#).is_none(),
        "缺 method 字段应返 None"
    );
}

#[test]
fn tools_manifest_is_readonly_surface_only() {
    let names: Vec<String> = mcp::tools_manifest().into_iter().map(|t| t.name).collect();
    assert_eq!(
        names,
        vec!["orch_status", "orch_cost", "orch_doctor"],
        "首切片只读工具面固定为 status/cost/doctor 三件"
    );
}

#[test]
fn manifest_excludes_write_operations() {
    // R4 §4 裁定:写操作面保留 HITL,无头 MCP tool 一律不暴露
    let names: Vec<String> = mcp::tools_manifest().into_iter().map(|t| t.name).collect();
    for forbidden in ["orch_dispatch", "orch_merge", "orch_round_close", "orch_nudge"] {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "写操作 {forbidden} 不得暴露为 MCP tool"
        );
    }
}

#[test]
fn unknown_method_yields_method_not_found() {
    let req = mcp::parse_request(r#"{"jsonrpc":"2.0","id":3,"method":"nope","params":{}}"#)
        .expect("形状合法应解析成功");
    let resp = mcp::respond_route_error(&req);
    assert!(resp.contains("-32601"), "未知 method 应回 JSON-RPC -32601: {resp}");
    assert!(resp.contains("\"id\":3"), "应答必须回带请求 id: {resp}");
}

#[test]
fn initialize_response_shape() {
    let req = mcp::parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .expect("initialize 请求应解析成功");
    let resp = mcp::initialize_response(&req);
    assert!(resp.contains("protocolVersion"), "initialize 应答须含 protocolVersion: {resp}");
    assert!(resp.contains("\"id\":1"), "应答必须回带请求 id: {resp}");
}
