//! Read-only merge accounting contracts retained after legacy writers retire.

use orch_host::close::{
    cleanup_disposition, merge_executed_event, merge_failure_disposition,
    post_merge_gate_failed_event, task_recorded_event, CleanupDisposition, MergeFailure,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REPO: AtomicU64 = AtomicU64::new(0);

#[test]
fn committed_no_ff_merge_is_irreversible_even_when_authorization_shape_is_wrong() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors().nth(2).unwrap().join("target/test-tmp")
        .join(format!("b323-irreversible-{}-{}", std::process::id(),
            NEXT_REPO.fetch_add(1, Ordering::Relaxed)));
    assert!(!root.exists());
    fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git").arg("-c").arg("core.fsmonitor=false")
            .arg("-C").arg(&root).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "irreversible fixture"]);
    git(&["config", "user.email", "irreversible@example.invalid"]);
    fs::write(root.join("feature.txt"), "base\n").unwrap();
    git(&["add", "feature.txt"]);
    git(&["commit", "-q", "-m", "base"]);
    let expected_main = git(&["rev-parse", "HEAD"]);
    git(&["switch", "-q", "-c", "task"]);
    fs::write(root.join("feature.txt"), "task change\n").unwrap();
    git(&["add", "feature.txt"]);
    git(&["commit", "-q", "-m", "task"]);
    let task_head = git(&["rev-parse", "HEAD"]);
    git(&["switch", "-q", "main"]);
    git(&["merge", "--no-ff", "--no-verify", "-q", "task", "-m", "irreversible merge"]);
    let merge = git(&["rev-parse", "HEAD"]);
    assert_ne!(merge, expected_main);
    orch_host::verify::validate_merge_commit_shape(&root, &merge, &expected_main, &task_head, &[]).unwrap();
    let wrong_authority = orch_host::verify::validate_merge_commit_shape(
        &root, &merge, &expected_main, &expected_main, &[]);
    assert!(wrong_authority.is_err(), "wrong second-parent authorization was admitted");
    assert_eq!(merge_failure_disposition(true, wrong_authority.is_ok()), MergeFailure::BoundaryViolation);
    // A post-effect refusal classifies the already-created commit. It must not
    // rewind Git or erase the task's content while reporting the violation.
    assert_eq!(git(&["rev-parse", "main"]), merge);
    assert_eq!(fs::read_to_string(root.join("feature.txt")).unwrap(), "task change\n");
    fs::remove_dir_all(root).unwrap();
}

fn payload<'a>(event: &'a orch_core::EventRecord, key: &str) -> Option<&'a serde_json::Value> {
    event.payload.as_ref()?.get(key)
}

#[test]
fn committed_merge_accounting_event_shapes_remain_stable() {
    let merged = merge_executed_event("B88", "r42", "abc1234");
    assert_eq!(merged.kind, "MergeExecuted");
    assert_eq!(merged.actor, "reviewer:orch-runtime");
    assert_eq!(
        payload(&merged, "mergeSha").and_then(serde_json::Value::as_str),
        Some("abc1234")
    );

    let failed = post_merge_gate_failed_event("B88", "r42", "abc1234", "postGate", 7);
    assert_eq!(failed.kind, "EscalationRaised");
    assert_eq!(
        payload(&failed, "stage").and_then(serde_json::Value::as_str),
        Some("post-merge-gate")
    );
    assert_eq!(
        payload(&failed, "gate").and_then(serde_json::Value::as_str),
        Some("postGate")
    );
    assert_eq!(
        payload(&failed, "exit").and_then(serde_json::Value::as_i64),
        Some(7)
    );
    assert_eq!(task_recorded_event("B88", "r42").kind, "TaskRecorded");
}

#[test]
fn cleanup_uses_the_durable_local_vs_remote_route_only() {
    assert_eq!(
        cleanup_disposition(true),
        CleanupDisposition::DeferredTierF,
        "remote implementers retain their externally-owned site"
    );
    assert_eq!(
        cleanup_disposition(false),
        CleanupDisposition::AttemptBestEffort,
        "local implementation has no provider process to await"
    );
}

#[test]
fn irreversible_merge_failure_classification_remains_pure() {
    assert_eq!(
        merge_failure_disposition(false, false),
        MergeFailure::ConflictEscalation
    );
    assert_eq!(
        merge_failure_disposition(true, false),
        MergeFailure::BoundaryViolation
    );
    assert_eq!(merge_failure_disposition(true, true), MergeFailure::None);
    assert_eq!(merge_failure_disposition(false, true), MergeFailure::None);
}
