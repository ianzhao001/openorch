use std::path::PathBuf;
use std::process::Command;

use orch_host::wake::{
    backend_receipt_from_log, backend_receipt_kind_from_argv, identity_fence_decision,
    wake_request_message_sha256, ActiveWake, BackendReceiptExpectation, BackendReceiptKind,
    WakeFenceDecision, WakeIntent,
};
use sha2::{Digest, Sha256};

const CONTINUATION: &str = "implementation:r58:B174:B174-A0002:executor-desktop";

fn live_log(name: &str) -> Option<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            env!("CARGO_MANIFEST_DIR"),
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    let path = git_dir
        .parent()?
        .join("coordination/runtime/logs")
        .join(name);
    path.is_file().then_some(path)
}

#[test]
fn request_digest_is_stable_but_continuation_scoped() {
    let first = wake_request_message_sha256("same resolved message", CONTINUATION).unwrap();
    let again = wake_request_message_sha256("same resolved message", CONTINUATION).unwrap();
    let other = wake_request_message_sha256(
        "same resolved message",
        "implementation:r58:B175:B175-A0001:executor-desktop",
    )
    .unwrap();
    assert_eq!(first, again);
    assert_ne!(first, other);
}

#[test]
fn configured_provider_is_frozen_from_argv_not_log_shape() {
    assert_eq!(
        backend_receipt_kind_from_argv(&["codex".into(), "exec".into()]).unwrap(),
        BackendReceiptKind::Codex
    );
    assert_eq!(
        backend_receipt_kind_from_argv(&["opencode".into(), "run".into()]).unwrap(),
        BackendReceiptKind::OpenCode
    );
    assert_eq!(
        backend_receipt_kind_from_argv(&[
            "sh".into(),
            "coordination/scripts/wake-multica.sh".into(),
        ])
        .unwrap(),
        BackendReceiptKind::SmartClaw
    );
    assert!(backend_receipt_kind_from_argv(&["agy".into()]).is_err());
}

#[test]
fn exact_identity_is_idempotent_for_ten_replays_and_wrapper_exit_never_releases() {
    let digest = wake_request_message_sha256("message", CONTINUATION).unwrap();
    for wrapper_exited in [false, true] {
        let active = ActiveWake::new(
            "executor-desktop",
            CONTINUATION,
            "wake-one",
            &digest,
            true,
            wrapper_exited,
        )
        .unwrap();
        for _ in 0..10 {
            assert_eq!(
                identity_fence_decision(
                    std::slice::from_ref(&active),
                    "executor-desktop",
                    &WakeIntent::continuation(CONTINUATION).unwrap(),
                    &digest,
                )
                .unwrap(),
                WakeFenceDecision::Idempotent {
                    wake_id: "wake-one".to_string(),
                    backend_accepted: true,
                }
            );
        }
        assert!(identity_fence_decision(
            &[active],
            "executor-desktop",
            &WakeIntent::unscoped(),
            &digest,
        )
        .is_err());
    }
}

#[test]
fn frozen_live_claw_and_opencode_corpora_use_the_provider_bound_parser() {
    let cases = [
        (
            "wake-executor-claw-1785354284394478000-38805-0.jsonl",
            21_564usize,
            "6419539c5db9a7c6af135b92febd3a36caa4c865005ee0efcfd99324f89fb6cc",
            BackendReceiptExpectation::new(
                BackendReceiptKind::SmartClaw,
                "1785354284",
                "review:r58:B174:B174-A0002:primary:executor-claw",
                &"a".repeat(64),
                Some("orch-wake-1785354284"),
            )
            .unwrap(),
        ),
        (
            "wake-executor-opencode-1785365921884948000-58299-0.jsonl",
            284_612usize,
            "48084626e1ffaec40eedab3ea80b3ba7dcd6e46004df1e5604cb5dd6b2d14912",
            BackendReceiptExpectation::new(
                BackendReceiptKind::OpenCode,
                "019fb019-f94f-4896-8b85-ac0597fbfdfa",
                "review:r58:B176:B176-A0002:secondary:executor-opencode",
                &"b".repeat(64),
                None,
            )
            .unwrap(),
        ),
    ];
    for (basename, length, digest, expectation) in cases {
        let Some(path) = live_log(basename) else {
            eprintln!("optional live B179 corpus unavailable: {basename}");
            continue;
        };
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len(), length, "frozen corpus length drifted");
        assert_eq!(hex::encode(Sha256::digest(&bytes)), digest);
        let receipt = backend_receipt_from_log(&expectation, &bytes, true)
            .unwrap()
            .expect("the live provider frame must commit a receipt");
        assert_eq!(receipt.kind, expectation.expected_kind);
        assert_eq!(receipt.probe_offset, 0);
        assert_eq!(receipt.probe_end, length as u64);
    }
}

#[test]
fn frozen_zero_byte_phantom_is_pending_while_open_and_loud_at_eof() {
    let basename = "wake-executor-claw-1785354574194279000-21535-0.jsonl";
    let Some(path) = live_log(basename) else {
        eprintln!("optional live B179 zero-byte corpus unavailable: {basename}");
        return;
    };
    let bytes = std::fs::read(path).unwrap();
    assert!(bytes.is_empty());
    let expectation = BackendReceiptExpectation::new(
        BackendReceiptKind::SmartClaw,
        "019faf6c-d7ff-45c4-892f-fe50c9502521",
        "manual:r58:019faf6c-d7ff-45c4-892f-fe50c9502521:executor-claw",
        &"c".repeat(64),
        Some("orch-wake-019faf6c-d7ff-45c4-892f-fe50c9502521"),
    )
    .unwrap();
    assert!(backend_receipt_from_log(&expectation, &bytes, false)
        .unwrap()
        .is_none());
    assert!(backend_receipt_from_log(&expectation, &bytes, true).is_err());
}
