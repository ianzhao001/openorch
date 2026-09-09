use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use orch_host::harness::{
    classify_terminal_observation, CapabilitySource, HarnessId, TerminalObservation, TerminalRecord,
};

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(1);
static ZCODE_WRAPPER_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    PrettyTerminalObject,
    CompactTerminalObject,
    ProgressLinesThenExitZero,
    ZeroFrameEof,
    ToolFramesThenKilled,
    DeadlineExceeded,
    AuthenticatedCancel,
    TerminalWithoutUsage,
}

#[derive(Debug)]
pub struct WrapperOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("CARGO_MANIFEST_DIR must be inside the repository")
        .to_path_buf()
}

pub fn scratch_root(tag: &str) -> PathBuf {
    let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock must follow the Unix epoch")
        .as_nanos();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-tmp")
        .join(format!(
            "b295-{tag}-{}-{sequence}-{nanos}",
            std::process::id()
        ));
    fs::create_dir_all(&root).expect("create repository-local B295 scratch root");
    root
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable fixture");
    let mut permissions = fs::metadata(path)
        .expect("inspect executable fixture")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make fixture executable");
}

fn pretty_provider() -> &'static str {
    r#"#!/bin/sh
printf '%s\n' '{' \
  '  "sessionId": "stub-pretty-session",' \
  '  "response": "B295 pretty final answer",' \
  '  "usage": {' \
  '    "inputTokens": 12,' \
  '    "outputTokens": 5,' \
  '    "reasoningTokens": 3,' \
  '    "cacheReadTokens": 2,' \
  '    "totalTokens": 22' \
  '  },' \
  '  "projection": {"contextWindow": 1000000},' \
  '  "traceId": "trace-b295-pretty"' \
  '}'
"#
}

fn compact_provider() -> &'static str {
    r#"#!/bin/sh
printf '%s\n' '{"sessionId":"stub-compact-session","response":"B295 compact final answer","usage":{"inputTokens":3,"outputTokens":2,"reasoningTokens":1,"totalTokens":6},"projection":{"contextWindow":1000000},"traceId":"trace-b295-compact"}'
"#
}

fn zcode_config() -> serde_json::Value {
    serde_json::json!({
        "model": "fixture-provider/fixture-model",
        "provider": {
            "fixture-provider": {
                "models": {
                    "fixture-model": {
                        "reasoning": {
                            "levels": ["max"],
                            "defaultLevel": "max",
                            "providerOptionsByLevel": {"max": {}}
                        }
                    }
                }
            }
        }
    })
}

pub fn run_zcode_wrapper(root: &Path, shape: Shape) -> WrapperOutcome {
    // The real Python wrapper launches a second short-lived provider process.
    // macOS intermittently starves one of three simultaneous nested launches;
    // serialize only this fixture boundary while keeping scratch identities
    // parallel-unique and every production assertion unchanged.
    let _guard = ZCODE_WRAPPER_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let provider_body = match shape {
        Shape::PrettyTerminalObject => pretty_provider(),
        Shape::CompactTerminalObject => compact_provider(),
        other => panic!("shape {other:?} is not a zcode terminal-object fixture"),
    };
    let provider = root.join("zcode-provider-stub");
    write_executable(&provider, provider_body);
    let rollout = root.join("rollout");
    fs::create_dir_all(&rollout).expect("create zcode rollout fixture");
    let config = root.join("config.json");
    fs::write(
        &config,
        serde_json::to_vec_pretty(&zcode_config()).expect("serialize zcode config"),
    )
    .expect("write zcode config");
    let review_output = root.join("review.md");

    let mut command = Command::new("sh");
    command
        .arg(repo_root().join("orch/scripts/wake-zcode-stream.sh"))
        .arg("B295 terminal envelope fixture")
        .env("ORCH_ZCODE_ROLLOUT_DIR", &rollout)
        .env("ORCH_ZCODE_CONFIG", &config)
        .env("ORCH_HARNESS_ID", "zcode")
        .env("ORCH_HARNESS_ACTION_ID", "fixture-action-b295")
        .env("ORCH_HARNESS_WAKE_ID", "fixture-wake-b295")
        .env("ORCH_HARNESS_ROUND", "r77")
        .env("ORCH_HARNESS_TASK_ID", "B295")
        .env("ORCH_HARNESS_ATTEMPT_ID", "B295-A0001")
        .env("ORCH_HARNESS_ROLE", "nongate")
        .env("ORCH_HARNESS_CWD", root)
        .env("ORCH_HARNESS_FIXED_HEAD", "0".repeat(40))
        .env("ORCH_HARNESS_PROVIDER", "fixture-provider")
        .env("ORCH_HARNESS_MODEL", "fixture-model")
        .env("ORCH_HARNESS_EFFORT", "max")
        .env("ORCH_HARNESS_PROVIDER_BIN", &provider)
        .env("ORCH_HARNESS_REVIEW_OUTPUT_PATH", &review_output)
        .env("ORCH_HARNESS_ORCH_BIN", "/bin/true")
        // This is fixture scheduling headroom, not a weaker production
        // deadline: the real wrapper still has to emit its exact terminal
        // object and exit zero, while a saturated full workspace gate may
        // delay the nested Python + provider spawn well beyond five seconds.
        .env("ORCH_HARNESS_DEADLINE_SECS", "60")
        .current_dir(root);
    wrapper_outcome(command.output().expect("run real zcode wrapper"))
}

fn wrapper_outcome(output: Output) -> WrapperOutcome {
    WrapperOutcome {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8(output.stdout).expect("wrapper stdout must be UTF-8"),
        stderr: String::from_utf8(output.stderr).expect("wrapper stderr must be UTF-8"),
    }
}

pub fn parse_terminal_frame(stdout: &str) -> serde_json::Value {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| {
            value.get("type").and_then(serde_json::Value::as_str) == Some("zcode.terminal")
        })
        .expect("zcode wrapper must emit one terminal frame")
}

fn observation_script(shape: Shape) -> (&'static str, bool, bool, Option<&'static str>) {
    match shape {
        Shape::ProgressLinesThenExitZero => (
            "#!/bin/sh\nprintf '%s\\n' 'waiting for provider'\nexit 0\n",
            false,
            false,
            None,
        ),
        Shape::ZeroFrameEof => ("#!/bin/sh\nexit 0\n", false, false, None),
        Shape::ToolFramesThenKilled => (
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"tool_result\",\"output\":\"progress\"}'\nkill -TERM $$\n",
            false,
            false,
            None,
        ),
        Shape::DeadlineExceeded => ("#!/bin/sh\nexit 72\n", false, false, None),
        Shape::AuthenticatedCancel => ("#!/bin/sh\nexit 0\n", false, true, None),
        Shape::TerminalWithoutUsage => (
            "#!/bin/sh\nprintf '%s\\n' 'B295 stable final answer'\nexit 0\n",
            true,
            false,
            Some("B295 stable final answer"),
        ),
        other => panic!("shape {other:?} is not a classifier fixture"),
    }
}

pub fn classify(root: &Path, shape: Shape) -> TerminalRecord {
    let (body, turn_ended, authenticated_cancel, final_text) = observation_script(shape);
    let provider = root.join("terminal-observation-stub");
    write_executable(&provider, body);
    let output = Command::new(&provider)
        .current_dir(root)
        .output()
        .expect("run terminal observation stub");
    let stdout = String::from_utf8(output.stdout).expect("stub stdout must be UTF-8");
    let exact_reason = match shape {
        Shape::ProgressLinesThenExitZero => "provider exited zero without a final answer",
        Shape::ZeroFrameEof => "provider exited zero after zero-frame EOF",
        Shape::ToolFramesThenKilled => "provider terminated after activity without a final answer",
        Shape::DeadlineExceeded => "hard-deadline",
        Shape::AuthenticatedCancel => "authenticated-cancel",
        Shape::TerminalWithoutUsage => "exact-terminal",
        _ => unreachable!("classifier shapes were checked above"),
    };
    classify_terminal_observation(TerminalObservation {
        capability: CapabilitySource::Derived,
        exit_code: output.status.code(),
        exact_reason: exact_reason.to_string(),
        turn_ended,
        final_text: final_text.map(str::to_string),
        final_text_sha256: None,
        output_path: None,
        output_sha256: None,
        usage: None,
        usage_absent_reason: Some("fixture provider emitted no usage object".to_string()),
        managed_scope_terminated: true,
        activity_seen: !stdout.trim().is_empty(),
        authenticated_cancel,
    })
    .expect("production terminal classifier must accept fixture facts")
}

pub fn write_descriptor(root: &Path, agent: &str, harness: &str, terminal: &str) {
    fs::write(
        root.join("descriptor.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "agent": agent,
            "harness": harness,
            "terminal": terminal,
        }))
        .expect("serialize descriptor fixture"),
    )
    .expect("write descriptor fixture");
}

pub fn terminal_records_for(root: &Path, agent: &str) -> Vec<TerminalRecord> {
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("descriptor.json")).expect("read descriptor fixture"),
    )
    .expect("parse descriptor fixture");
    assert_eq!(
        value.get("agent").and_then(serde_json::Value::as_str),
        Some(agent),
        "descriptor agent must match"
    );
    HarnessId::parse(
        value
            .get("harness")
            .and_then(serde_json::Value::as_str)
            .expect("descriptor harness"),
    )
    .expect("descriptor harness must use the production closed vocabulary");
    let capability = match value
        .get("terminal")
        .and_then(serde_json::Value::as_str)
        .expect("descriptor terminal")
    {
        "native" => CapabilitySource::Native,
        "derived" => CapabilitySource::Derived,
        "absent" => CapabilitySource::Absent,
        other => panic!("unknown descriptor terminal capability {other:?}"),
    };
    let observation = if capability == CapabilitySource::Absent {
        TerminalObservation {
            capability,
            exit_code: None,
            exact_reason: "harness descriptor declares terminal capability absent".to_string(),
            turn_ended: false,
            final_text: None,
            final_text_sha256: None,
            output_path: None,
            output_sha256: None,
            usage: None,
            usage_absent_reason: Some("descriptor exposes no usage source".to_string()),
            managed_scope_terminated: false,
            activity_seen: false,
            authenticated_cancel: false,
        }
    } else {
        TerminalObservation {
            capability,
            exit_code: Some(0),
            exact_reason: "exact-terminal".to_string(),
            turn_ended: true,
            final_text: Some("descriptor-bound final answer".to_string()),
            final_text_sha256: None,
            output_path: None,
            output_sha256: None,
            usage: None,
            usage_absent_reason: Some("fixture terminal omitted usage".to_string()),
            managed_scope_terminated: true,
            activity_seen: true,
            authenticated_cancel: false,
        }
    };
    vec![classify_terminal_observation(observation)
        .expect("production terminal classifier must consume descriptor capability")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use orch_host::wake::{
        classify_managed_terminal, durable_identity_kind, DurableIdentityKind, ManagedTerminal,
    };

    #[test]
    fn wrapper_terminal_frame_is_consumed_by_the_managed_supervisor_classifier() {
        let root = scratch_root("supervisor-terminal-frame");
        let outcome = run_zcode_wrapper(&root, Shape::PrettyTerminalObject);
        let terminal_count = outcome
            .stdout
            .lines()
            .filter(|line| classify_managed_terminal("sh", line) == ManagedTerminal::OpenCodeStop)
            .count();
        assert_eq!(
            terminal_count, 1,
            "wrapper must expose exactly one managed terminal frame"
        );
    }

    #[test]
    fn terminal_serialization_keeps_usage_as_explicit_null() {
        let root = scratch_root("explicit-null");
        let record = classify(&root, Shape::TerminalWithoutUsage);
        let value = serde_json::to_value(record).expect("serialize terminal record");
        assert!(value.get("usage").is_some_and(serde_json::Value::is_null));
        assert!(value
            .get("usageAbsentReason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reason| !reason.trim().is_empty()));
    }

    #[test]
    fn dsh_stream_wrapper_uses_managed_process_custody_for_terminal_reconciliation() {
        let argv = vec![
            "sh".to_string(),
            "orch/scripts/wake-dsh-stream.sh".to_string(),
            "message".to_string(),
        ];
        assert_eq!(
            durable_identity_kind(&argv).expect("DSH wrapper topology"),
            DurableIdentityKind::ManagedPidGroup
        );
    }
}
