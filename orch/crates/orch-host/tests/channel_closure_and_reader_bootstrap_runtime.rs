#![allow(dead_code)]

mod support_dsh;

use std::path::{Path, PathBuf};

use orch_host::wake::{backend_receipt_from_log, BackendReceiptExpectation, BackendReceiptKind};
use support_dsh::{run_wake_dsh, DshSession, FakeDsh};

const WAKE_ID: &str = "11111111-2222-4333-8444-555555555555";
const CONTINUATION: &str = "review:r82:B311:B311-A0001:nongate:executor-dsh";
const MESSAGE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repository layout")
        .to_path_buf()
}

fn expectation() -> BackendReceiptExpectation {
    BackendReceiptExpectation::new(
        BackendReceiptKind::Dsh,
        WAKE_ID,
        CONTINUATION,
        MESSAGE_SHA,
        None,
    )
    .unwrap()
    .with_signed_pin("one-dewu-dsh", "deepseek-v4-pro", "max")
    .unwrap()
}

#[test]
fn dsh_receipt_binds_the_selected_session_and_exact_signed_header_pin() {
    let log = concat!(
        "turn.started\n",
        "{\"type\":\"dsh.session\",\"sessionId\":\"session-b311\",",
        "\"provider\":\"one-dewu-dsh\",\"model\":\"deepseek-v4-pro\",",
        "\"effort\":\"max\"}\n"
    );
    let receipt = backend_receipt_from_log(&expectation(), log.as_bytes(), true)
        .unwrap()
        .expect("exact DSH header receipt");
    assert_eq!(receipt.kind, BackendReceiptKind::Dsh);
    assert_eq!(receipt.wake_id, WAKE_ID);
    assert_eq!(receipt.request_session_id, None);
    assert_eq!(receipt.observed_session_id.as_deref(), Some("session-b311"));

    for (key, value) in [
        ("provider", "wrong-provider"),
        ("model", "wrong-model"),
        ("effort", "wrong-effort"),
    ] {
        let mutated = log.replacen(
            &format!(
                "\"{key}\":\"{}\"",
                match key {
                    "provider" => "one-dewu-dsh",
                    "model" => "deepseek-v4-pro",
                    _ => "max",
                }
            ),
            &format!("\"{key}\":\"{value}\""),
            1,
        );
        assert!(backend_receipt_from_log(&expectation(), mutated.as_bytes(), true).is_err());
    }
}

#[test]
fn context_only_or_closed_receiptless_stream_never_accepts_dsh() {
    let context = b"{\"type\":\"request/context\",\"data\":{\"provider\":\"one-dewu-dsh\",\"model\":\"deepseek-v4-pro\",\"contextWindow\":1000000}}\n";
    assert!(backend_receipt_from_log(&expectation(), context, false)
        .unwrap()
        .is_none());
    assert!(backend_receipt_from_log(&expectation(), context, true).is_err());
}

#[test]
fn legacy_direct_wrapper_keeps_projection_but_never_fabricates_dsh_session() {
    let fake = FakeDsh::new("b311-legacy-no-receipt")
        .with_session(DshSession::with_tool_result_text("B311-LEGACY-TOOL"));
    let script = repo_root().join("orch/scripts/wake-dsh-stream.sh");
    let out = run_wake_dsh(&script, &fake, &fake.message_with_worktree_line());
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("B311-LEGACY-TOOL"));
    assert!(!out.stdout.contains("dsh.session"));
}

#[test]
fn canonical_verifier_and_supervisor_keep_dsh_in_the_closed_identity_branches() {
    let wake =
        std::fs::read_to_string(repo_root().join("orch/crates/orch-host/src/wake.rs")).unwrap();
    let verify =
        std::fs::read_to_string(repo_root().join("orch/crates/orch-host/src/verify.rs")).unwrap();
    for marker in [
        "exact_managed_wrapper_arg(wrapper, DSH_STREAM_SCRIPT_REL)",
        "BackendReceiptKind::Dsh if frame_type == \"dsh.session\"",

    ] {
        assert!(wake.contains(marker), "wake production lacks {marker}");
    }
    let managed = std::fs::read_to_string(repo_root().join("orch/crates/orch-host/src/channel/managed.rs")).unwrap();
    for marker in [".envs(&spec.env)", "scrub_managed_dsh_overrides(&mut command, &spec.argv)"] {
        assert!(managed.contains(marker), "shared managed production lacks {marker}");
    }
    assert!(verify.contains("\"agy\" | \"dsh\""));
}
