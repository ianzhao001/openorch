//! B286 · DSH 非门席通道健康与终态可判定 —— 冻结契约种子。
//!
//! **首红：compile `error[E0583]: file for module `dsh_session_health_support` not found`。**
//! support 只是零判别力管道 marker（`pub fn contract_loaded() {}`），落点是目录形态
//! `tests/dsh_session_health_support/mod.rs`，不新增 Cargo target。
//!
//! # 本卡钉什么
//!
//! r74/B283-A0001 的 DSH 非门会话到裁定时点无 `turn/end`。用户说「已死」，
//! 而机械取证显示它**仍在活跃生成**（16 秒内事件 17,127→17,508、最后事件年龄 0 秒、
//! `tool/call`×39、daemon 仍在监听）。
//! **分歧的根源不是谁记错了，而是当时没有任何机械判据能区分这两者。**
//!
//! 本种子钉三件事：
//!
//! 1. 四态（`converged` / `generating` / `stalled` / `dead`）机械可分；
//! 2. **`dead` 必须有独立于事件流的证据**——「没有 `turn/end`」推不出「已死」；
//! 3. 嵌套深度读对：`session.history` 的事件是 `{"event":{"type":…}}`，
//!    r74 planner 的等待器读顶层 `type`（恒为 `None`）⇒ 即使正常收敛也永不触发。
//!
//! # ⚠️ 诚实边界（写死在这里）
//!
//! - **不解释 DSH 为什么不收敛。** 只交付「能把不收敛与已死区分开」的判据。
//! - **`contextWindow` 缺失时必须输出 `null`。** r74 至今没能取得 flash 的真实上下文窗口
//!   （`session.models` 目录里没有该字段，`request/header.config` 里也没出现）。
//!   用目录值、缺省值或猜测值填充 = 虚报。
//! - **不连 daemon、不调模型、不联网。** 全部夹具是本地合成 JSON。
//!
//! # M 变异 ↔ 载体 一一对应
//!
//! | M | 注入 | 必红的载体 |
//! |---|---|---|
//! | M1 | 读取深度改回只读顶层 `type` | `nested_and_flat_shapes_agree` |
//! | M2 | 由「无 turn/end」直接推出 dead | `dead_requires_evidence_outside_the_event_stream` |
//! | M3 | `--daemon-alive unknown` 也给 dead | 同上 |
//! | M4 | 合并 generating 与 stalled | `the_four_states_are_distinguished` |
//! | M5 | contextWindow 缺失时填缺省值 | `a_missing_context_window_is_reported_as_null` |
//! | M6 | 分类器改为直接连 daemon | `the_classifier_is_offline_and_model_free` |

#![allow(dead_code)]

mod dsh_session_health_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

// ---------------------------------------------------------------- 公共工具

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn classifier() -> PathBuf {
    repo_root().join("orch/scripts/dsh-session-health.py")
}

fn temp_root(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "orch-b286-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

/// 跑分类器。`now_ms` 显式传入，使断言与真实时钟无关（可重放）。
fn classify(history: &Value, now_ms: u64, daemon_alive: &str) -> Value {
    let root = temp_root("run");
    let path = root.join("history.json");
    fs::write(&path, serde_json::to_vec(history).unwrap()).unwrap();

    let output = Command::new("python3")
        .arg(classifier())
        .arg("--history")
        .arg(&path)
        .arg("--now-ms")
        .arg(now_ms.to_string())
        .arg("--daemon-alive")
        .arg(daemon_alive)
        .output()
        .expect("分类器必须可执行");
    assert!(
        output.status.success(),
        "分类器非零退出: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "分类器必须输出单行 JSON: {error}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    fs::remove_dir_all(root).unwrap();
    parsed
}

fn nested(events: Vec<Value>) -> Value {
    serde_json::json!({
        "events": events.into_iter().map(|e| serde_json::json!({"event": e})).collect::<Vec<_>>(),
        "hasMore": false
    })
}

fn ev(kind: &str, seq: u64, time_ms: u64) -> Value {
    serde_json::json!({"type": kind, "seq": seq, "time": time_ms, "data": {}})
}

/// r74 实撞形态：`request/header` 携带有效三元组，但**没有** `contextWindow`。
fn header_event(seq: u64, time_ms: u64, with_context_window: bool) -> Value {
    let mut config = serde_json::json!({
        "provider": "opencode-go",
        "model": "deepseek-v4-flash",
        "reasoningEffort": "max"
    });
    if with_context_window {
        config["contextWindow"] = serde_json::json!(1_000_000u64);
    }
    serde_json::json!({
        "type": "request/header", "seq": seq, "time": time_ms,
        "data": {"header": {"config": config}}
    })
}

const NOW: u64 = 1_787_200_000_000;

fn converged_history() -> Value {
    nested(vec![
        header_event(0, NOW - 600_000, false),
        ev("turn/start", 1, NOW - 590_000),
        ev("assistant/chunk", 2, NOW - 500_000),
        ev("turn/end", 3, NOW - 490_000),
    ])
}

/// 无 `turn/end`，但事件流仍在推进（最后一条 2 秒前）。r74 实撞的正是这一态。
fn generating_history() -> Value {
    nested(vec![
        header_event(0, NOW - 600_000, false),
        ev("turn/start", 1, NOW - 590_000),
        ev("tool/call", 2, NOW - 10_000),
        ev("tool/result", 3, NOW - 5_000),
        ev("assistant/chunk", 4, NOW - 2_000),
    ])
}

/// 无 `turn/end`，且事件流久无推进。
fn quiet_history() -> Value {
    nested(vec![
        header_event(0, NOW - 3_600_000, false),
        ev("turn/start", 1, NOW - 3_590_000),
        ev("assistant/chunk", 2, NOW - 3_000_000),
    ])
}

fn state_of(value: &Value) -> String {
    value["state"].as_str().expect("输出必须含 state 字符串").to_string()
}

// ------------------------------------------------- ① 四态可分

#[test]
fn the_four_states_are_distinguished() {
    let converged = state_of(&classify(&converged_history(), NOW, "yes"));
    let generating = state_of(&classify(&generating_history(), NOW, "yes"));
    let stalled = state_of(&classify(&quiet_history(), NOW, "yes"));
    let dead = state_of(&classify(&quiet_history(), NOW, "no"));

    assert_eq!(converged, "converged", "有 turn/end 必须判 converged");
    assert_eq!(generating, "generating", "无 turn/end 但事件仍在推进 ⇒ generating");
    assert_eq!(stalled, "stalled", "无 turn/end、久无推进、daemon 存活 ⇒ stalled");
    assert_eq!(dead, "dead", "daemon 明确不存活 ⇒ dead");

    let all = [&converged, &generating, &stalled, &dead];
    for (i, a) in all.iter().enumerate() {
        for b in all.iter().skip(i + 1) {
            assert_ne!(
                a, b,
                "四态必须两两不同；合并任意两态都会让 r74 那次描述分歧重演"
            );
        }
    }
}

// ------------------------------------------------- ② dead 需要事件流之外的证据

#[test]
fn dead_requires_evidence_outside_the_event_stream() {
    // 同一份「无 turn/end 且久无推进」的历史，只改 daemon 存活输入。
    for alive in ["yes", "unknown"] {
        let state = state_of(&classify(&quiet_history(), NOW, alive));
        assert_ne!(
            state, "dead",
            "daemon-alive={alive} 时不得判 dead。「没有 turn/end」**推不出**「已死」——\
             这正是 r74 那次描述分歧的全部内容；unknown 最坏只能给到 stalled"
        );
    }
    assert_eq!(
        state_of(&classify(&quiet_history(), NOW, "no")),
        "dead",
        "只有 daemon 明确不存活时才允许 dead"
    );
}

// ------------------------------------------------- ③ 嵌套深度

#[test]
fn nested_and_flat_shapes_agree() {
    let nested_history = generating_history();
    // 同语义的扁平形状。
    let flat = serde_json::json!({
        "events": nested_history["events"]
            .as_array().unwrap().iter()
            .map(|wrapper| wrapper["event"].clone())
            .collect::<Vec<_>>(),
        "hasMore": false
    });

    let from_nested = classify(&nested_history, NOW, "yes");
    let from_flat = classify(&flat, NOW, "yes");

    assert_eq!(
        state_of(&from_nested),
        state_of(&from_flat),
        "嵌套与扁平的同语义输入必须得到相同 state。\
         r74 planner 的等待器读顶层 type（恒为 None）⇒ 即使 DSH 正常收敛也永不触发，\
         那是一次真实的监视失效"
    );
    for key in ["eventCount", "toolCalls", "toolResults", "turnStarted", "turnEnded"] {
        assert_eq!(
            from_nested[key], from_flat[key],
            "字段 {key} 在两种形状下必须一致：nested={} flat={}",
            from_nested[key], from_flat[key]
        );
    }
    assert_eq!(
        from_nested["toolCalls"], serde_json::json!(1),
        "计数必须真的读到嵌套里的事件，而不是恒零"
    );
}

// ------------------------------------------------- ④ 模型与上下文窗口

#[test]
fn the_effective_model_is_extracted() {
    let out = classify(&converged_history(), NOW, "yes");
    assert_eq!(out["provider"], serde_json::json!("opencode-go"));
    assert_eq!(out["model"], serde_json::json!("deepseek-v4-flash"));
    assert_eq!(out["effort"], serde_json::json!("max"));
}

/// M5 的载体。r74 至今未取得 flash 的真实 contextWindow；缺失时**必须**输出 null。
#[test]
fn a_missing_context_window_is_reported_as_null() {
    let out = classify(&converged_history(), NOW, "yes");
    assert!(
        out["contextWindow"].is_null(),
        "会话记录里没有 contextWindow 时必须输出 null，不得用目录值/缺省值/猜测值填充——\
         那是虚报。实际: {}",
        out["contextWindow"]
    );

    let with_window = nested(vec![
        header_event(0, NOW - 600_000, true),
        ev("turn/start", 1, NOW - 590_000),
        ev("turn/end", 2, NOW - 500_000),
    ]);
    assert_eq!(
        classify(&with_window, NOW, "yes")["contextWindow"],
        serde_json::json!(1_000_000u64),
        "记录里**有** contextWindow 时必须如实输出"
    );
}

// ------------------------------------------------- ⑤ 离线、无模型

#[test]
fn the_classifier_is_offline_and_model_free() {
    let source = fs::read_to_string(classifier()).expect("分类器源码必须可读");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    for forbidden in ["http://", "https://", "127.0.0.1", "urllib", "requests", "socket"] {
        assert!(
            !code.contains(forbidden),
            "分类器必须是纯离线只读的，不得含 {forbidden}。\
             它的输入是一份已经取好的 session.history，不是 daemon 连接"
        );
    }
}

// ------------------------------------------------- ⑥ 管道 marker

#[test]
fn the_support_module_is_wired() {
    dsh_session_health_support::contract_loaded();
}
