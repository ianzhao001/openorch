//! ═══ 红种子契约 · B96 ═══
//! 预期红（redForm: compile）：MiMo/CodeBuddy built-in、agent 映射与结构化证据 API 尚不存在。
//! 变异清单：
//! M1 built-in 缺任一家；M2 MiMo 漏 model/pure/权限/JSON/dir；
//! M3 CodeBuddy 漏 model/effort/no-session/权限/stream-json；
//! M4 CodeBuddy 偷加 fallback/resume；M5 run-task 绕过 agent→adapter 映射；
//! M6 usage/model/result 扫普通正文或吞 malformed；M7 只支持一种 framing；
//! M8 把 requested model 冒充 observed，或吞掉 missing/mismatch/mixed identity。

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;

use orch_host::adapter::{
    self, adapter_name_for_agent, validate_model_identity, AdapterExecutionEvidence,
};

fn argv(command: &Command) -> Vec<String> {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(OsStr::to_string_lossy)
        .map(|part| part.into_owned())
        .collect()
}

#[test]
fn builtins_include_mimo_and_codebuddy_without_regressing_existing() {
    let builtins = adapter::supported_builtins();
    for name in ["opencode", "codex", "claude", "cursor", "mimo", "codebuddy"] {
        assert!(builtins.contains(&name), "missing builtin {name}");
    }
}

#[test]
fn mimo_is_fresh_pure_json_with_pinned_model_and_workdir() {
    let workdir = Path::new("/tmp/orch-mimo");
    let got = argv(
        &adapter::build_command("mimo", "do work", workdir, None, None).unwrap(),
    );
    assert_eq!(
        got,
        vec![
            "mimo",
            "run",
            "do work",
            "--model",
            "xiaomi/mimo-v2.5-pro",
            "--variant",
            "high",
            "--format",
            "json",
            "--pure",
            "--dangerously-skip-permissions",
            "--dir",
            "/tmp/orch-mimo",
        ]
    );
    assert!(!got.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--session" | "--continue" | "-s" | "-c" | "--resume"
        )
    }));
}

#[test]
fn codebuddy_is_fresh_stream_json_without_hidden_fallback() {
    let workdir = Path::new("/tmp/orch-codebuddy");
    let command =
        adapter::build_command("codebuddy", "do work", workdir, None, None).unwrap();
    let got = argv(&command);
    assert_eq!(
        got,
        vec![
            "codebuddy",
            "-p",
            "do work",
            "--output-format",
            "stream-json",
            "--model",
            "glm-5.2",
            "--effort",
            "high",
            "--no-session-persistence",
            "--dangerously-skip-permissions",
        ]
    );
    assert_eq!(command.get_current_dir(), Some(workdir));
    assert!(!got.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--fallback-model"
                | "--resume"
                | "-r"
                | "--continue"
                | "-c"
                | "--session-id"
        )
    }));
}

#[test]
fn run_task_maps_new_agents_to_parent_held_fresh_one_shot_adapters() {
    assert_eq!(adapter_name_for_agent("executor-mimo"), "mimo");
    assert_eq!(adapter_name_for_agent("executor-codebuddy"), "codebuddy");
    assert_eq!(adapter_name_for_agent("executor-cli"), "opencode");
    let source = include_str!("../src/runtask.rs");
    assert!(
        source.contains("adapter_name_for_agent"),
        "real run-task path must use the centralized agent→adapter mapping"
    );
}

/// O28/O34：只用时间戳（无论精度）不足以避免**同进程并发**撞名——
/// cargo test 在同一进程内多线程跑用例，落在同一纳秒刻度即共用目录，
/// 先完成者 `remove_dir_all` 会删掉另一个正在用的目录（实测：B103 合并后门
/// `new_cli_adapters.rs:128` NotFound，红落在与本卡无关的位置）。
/// 必须叠加 pid **与**模块级单调计数器。
/// B137：scratch 根迁到 `orch/target/test-tmp`（仓库本地，不落 OS 共享 temp），
/// 沿 `src/binding.rs::b106_scratch_dir` 范式——pid+seq+nanos 三重唯一，
/// teardown 用 `let _ =` 容忍并发互删（NotFound 不 panic）。
static EVIDENCE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn evidence(adapter_name: &str, body: &str) -> AdapterExecutionEvidence {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = EVIDENCE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let root = orch_root
        .join("target/test-tmp")
        .join(format!("orch-b96-evidence-{}-{unique}-{seq}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("out.jsonl");
    fs::write(&path, body).unwrap();
    let evidence = adapter::extract_execution_evidence(adapter_name, &path);
    let _ = fs::remove_dir_all(root);
    evidence
}

#[test]
fn mimo_structured_terminal_event_yields_usage_model_and_result() {
    let body = concat!(
        "{\"type\":\"message\",\"content\":\"quota 429 glm-5.2 in ordinary text\"}\n",
        "{\"type\":\"step_finish\",\"part\":{\"model\":\"xiaomi/mimo-v2.5-pro\",",
        "\"tokens\":{\"input\":12,\"output\":3},\"cost\":0.01,\"text\":\"DONE\"}}\n"
    );
    let got = evidence("mimo", body);
    assert_eq!(
        got.requested_model.as_deref(),
        Some("xiaomi/mimo-v2.5-pro")
    );
    assert_eq!(got.observed_model.as_deref(), Some("xiaomi/mimo-v2.5-pro"));
    assert_eq!(got.terminal_result.as_deref(), Some("DONE"));
    assert_eq!(
        got.usage
            .as_ref()
            .and_then(|value| value.pointer("/tokens/input"))
            .and_then(serde_json::Value::as_u64),
        Some(12)
    );
}

#[test]
fn codebuddy_structured_result_yields_usage_model_and_rejects_prose_guessing() {
    let body = concat!(
        "[{\"type\":\"assistant\",\"message\":\"I used xiaomi/mimo-v2.5-pro\"},",
        "{\"type\":\"result\",\"result\":\"OK\",\"session_id\":\"s2\",",
        "\"providerData\":{\"model\":\"glm-5.2\"},",
        "\"usage\":{\"input_tokens\":31,\"output_tokens\":4},\"total_cost_usd\":0}]\n"
    );
    let got = evidence("codebuddy", body);
    assert_eq!(got.requested_model.as_deref(), Some("glm-5.2"));
    assert_eq!(got.observed_model.as_deref(), Some("glm-5.2"));
    assert_eq!(got.terminal_result.as_deref(), Some("OK"));
    assert_eq!(got.session_id.as_deref(), Some("s2"));
    assert_eq!(
        got.usage
            .as_ref()
            .and_then(|value| value.pointer("/usage/output_tokens"))
            .and_then(serde_json::Value::as_u64),
        Some(4)
    );

    let malformed = evidence(
        "codebuddy",
        "{\"type\":\"assistant\",\"message\":\"model=glm-5.2 usage=999\"}\nnot-json\n",
    );
    assert_eq!(
        malformed,
        AdapterExecutionEvidence {
            requested_model: Some("glm-5.2".into()),
            usage: None,
            observed_model: None,
            session_id: None,
            terminal_result: None,
        }
    );
}

#[test]
fn model_identity_is_fail_closed_for_missing_mismatch_and_mixed_results() {
    let missing = evidence(
        "codebuddy",
        "{\"type\":\"result\",\"result\":\"OK\",\"usage\":{\"input_tokens\":1}}\n",
    );
    assert!(validate_model_identity("codebuddy", &missing).is_err());

    let mismatch = evidence(
        "codebuddy",
        "{\"type\":\"result\",\"result\":\"OK\",\"providerData\":{\"model\":\"hy3\"}}\n",
    );
    assert!(validate_model_identity("codebuddy", &mismatch).is_err());

    let mixed = evidence(
        "codebuddy",
        concat!(
            "{\"type\":\"result\",\"providerData\":{\"model\":\"glm-5.2\"}}\n",
            "{\"type\":\"result\",\"providerData\":{\"model\":\"hy3\"}}\n"
        ),
    );
    assert!(validate_model_identity("codebuddy", &mixed).is_err());
}

/// B137-A0002 主审修复（永久覆盖缺口）：seed（test_tmp_hygiene.rs，字节冻结）
/// 只扫本文件的 scratch 根与 pid，不覆盖其他 6 文件，也不检查模块级 seq。
/// 补一条在本文件（writeSet 内）的契约测试，逐个读取全部 7 个迁移目标源码，
/// 断言每个都含 repo-local scratch 根、pid 作用域与模块级 `AtomicU64` 单调序列
/// ——即卡面 `pid+seq(+nanos/ulid)` 的逐文件硬约束。任何人再以 pid+ULID 而无 seq
/// 迁回，本用例即红。
///
/// 注意：scratch 根 token 在运行时拼接构造，避免本测试源码内出现字面量
/// scratch-root-token——否则 seed 的 `adapter_evidence_lives_under_repo_local_test_tmp`
/// （断言本文件源码含该字面量）会被本测试的字面量污染而无法捕捉 helper 被摘除
/// 的退化（M2 静默绿）。pid token 同理运行时拼接，防 seed M1 同类污染。
#[test]
fn all_migrated_tests_use_repo_local_test_tmp_with_pid_and_module_seq() {
    const MIGRATED: &[&str] = &[
        "new_cli_adapters.rs",
        "attempt_takeover.rs",
        "fault_injection.rs",
        "ledger_atomic_append.rs",
        "merge_irreversible.rs",
        "planner_wake_runtime.rs",
        "wave_agent_guard.rs",
    ];
    let scratch_root = format!("test-{}", "tmp");
    let pid_call = format!("process::{}", "id()");
    for name in MIGRATED {
        let path = format!("{}/tests/{}", env!("CARGO_MANIFEST_DIR"), name);
        let src = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        assert!(
            src.contains(&scratch_root),
            "{name} must use repo-local orch/target/{scratch_root} scratch root"
        );
        assert!(
            src.contains(&pid_call),
            "{name} must scope scratch naming by pid"
        );
        assert!(
            src.contains("AtomicU64"),
            "{name} must use a module-level AtomicU64 monotonic seq (pid alone is insufficient under same-process concurrency)"
        );
    }
}
