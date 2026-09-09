//! Legacy consultation-gate fold plus schema-3 refusal side-effect discipline.
//!
//! The pure B165 fold remains historical evidence for rounds without an
//! explicit schema. Schema 3 bypasses that membership policy and is exercised
//! through the live CLI tests, where PlanSignedOff does not freeze consult.
//! M2. Treat a superseded sign-off as still binding after a replan bumped the
//!     revision. The replan window IS plan phase again and must admit — and a
//!     round that closed has no in-flight delivery left to protect.
//! M3. Let a refusal leave anything behind except one consultations log line:
//!     touching the protocol ledger, creating a consultation dir, or staying
//!     silent are all failures.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use orch_core::EventRecord;
use orch_host::consult::{consultation_admitted, consultation_gate, record_refusal, GateDecision};
use orch_host::plan::task_validated_payload;

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

static TEMP_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);
static EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(name: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = TEMP_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root.join("target/test-tmp").join(format!(
        "consult-gate-{name}-{}-{}-{seq}",
        std::process::id(),
        ulid::Ulid::new()
    ))
}

fn ev(kind: &str, actor: &str, payload: serde_json::Value) -> EventRecord {
    let seq = EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    serde_json::from_value(serde_json::json!({
        "eventId": format!("01SEEDGATE{seq:016}"),
        "ts": "2026-07-29T00:00:00Z",
        "actor": actor,
        "type": kind,
        "round": "r90",
        "payload": payload,
    }))
    .expect("event fixture is a valid EventRecord")
}

fn tv(revision: u32) -> EventRecord {
    ev(
        "TaskValidated",
        "runtime:orch",
        task_validated_payload(revision, DIGEST),
    )
}

fn so(revision: u32) -> EventRecord {
    ev(
        "PlanSignedOff",
        "user",
        serde_json::json!({"irRevision": revision, "validationDigest": DIGEST}),
    )
}

#[test]
fn legacy_signed_off_round_refuses_and_an_unsigned_plan_admits() {
    // M1: sign-off is the freeze switch; before it, consultation is open.
    match consultation_gate(&[tv(1), so(1)], "r90") {
        GateDecision::Refuse { reason } => {
            assert!(reason.contains('1'), "refusal names the revision: {reason}");
            assert!(
                reason.contains(&DIGEST[..8]),
                "refusal names the digest prefix: {reason}"
            );
        }
        GateDecision::Admit { .. } => panic!("a signed-off round must refuse consultation"),
    }
    assert!(matches!(
        consultation_gate(&[tv(1)], "r90"),
        GateDecision::Admit { .. }
    ));
    assert!(matches!(
        consultation_gate(&[], "r90"),
        GateDecision::Admit { .. }
    ));
}

#[test]
fn legacy_replanned_or_closed_round_admits_consultation_again() {
    // M2: a newer TaskValidated without its own sign-off reopens plan phase.
    assert!(matches!(
        consultation_gate(&[tv(1), so(1), tv(2)], "r90"),
        GateDecision::Admit { .. }
    ));
    let closed = [
        tv(1),
        so(1),
        ev("RoundClosed", "runtime:orch", serde_json::json!({})),
    ];
    assert!(matches!(
        consultation_gate(&closed, "r90"),
        GateDecision::Admit { .. }
    ));
    // Re-signing the new revision freezes it again.
    assert!(matches!(
        consultation_gate(&[tv(1), so(1), tv(2), so(2)], "r90"),
        GateDecision::Refuse { .. }
    ));
}

#[test]
fn legacy_refusal_leaves_one_log_line_and_nothing_else() {
    // M3: the only durable side effect of a refusal is a single log line.
    let root = temp_root("refusal");
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r90")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r90\n").unwrap();
    let ledger: String = [tv(1), so(1)]
        .iter()
        .map(|event| serde_json::to_string(event).unwrap() + "\n")
        .collect();
    fs::write(root.join("coordination/rounds/r90/events.jsonl"), &ledger).unwrap();

    let decision = consultation_admitted(&root).expect("gate evaluates on a well-formed root");
    assert!(matches!(decision, GateDecision::Refuse { .. }));
    record_refusal(&root, &decision).expect("refusal is recorded");

    let after = fs::read_to_string(root.join("coordination/rounds/r90/events.jsonl")).unwrap();
    assert_eq!(after, ledger, "the protocol ledger is never touched");

    let log = fs::read_to_string(root.join("coordination/consultations/log.jsonl")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one refusal line");
    let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(entry["kind"], "ConsultationRefused");
    // Redaction is applied on the way out; asserting the line is already its own
    // redacted form is the honest check (a redacted secret still contains "sk-").
    assert_eq!(
        orch_host::redact::redact_full(lines[0]),
        lines[0],
        "log lines must already be redacted"
    );

    let dirs = fs::read_dir(root.join("coordination/consultations"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().path().is_dir())
        .count();
    assert_eq!(dirs, 0, "a refusal never creates a consultation dir");
}
