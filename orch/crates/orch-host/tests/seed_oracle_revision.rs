//! ═══ 红种子契约 · R44S ═══
//! 预期红（redForm: compile）：seed oracle revision 判定 API 尚不存在。
//! 变异清单：
//! M1 仍只按 taskId 永久拒重；M2 相同 SHA 也允许重复；M3 只比首个 seed；
//! M4 忽略 target/重复 target；M5 malformed 历史 payload 被当作可修订；M6 不取最后事件。

use orch_core::EventRecord;
use orch_host::round::{
    classify_seed_verification, SeedVerificationDecision,
};

fn verified(id: &str, task: &str, seeds: serde_json::Value) -> EventRecord {
    EventRecord {
        event_id: id.into(),
        ts: "2026-07-25T00:00:00Z".into(),
        actor: "planner".into(),
        kind: "SeedOracleVerified".into(),
        task_id: Some(task.into()),
        round: Some("r44".into()),
        payload: Some(serde_json::json!({"seeds": seeds, "measured": {"redForm":"compile"}})),
        extra: serde_json::Map::new(),
    }
}

fn current(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(target, sha)| ((*target).into(), (*sha).into()))
        .collect()
}

#[test]
fn first_verification_is_allowed() {
    assert_eq!(
        classify_seed_verification(&[], "B95", &current(&[("a.rs", "sha1")])).unwrap(),
        SeedVerificationDecision::First
    );
}

#[test]
fn identical_latest_seed_set_is_idempotently_rejected() {
    let events = vec![verified(
        "v1",
        "B95",
        serde_json::json!([
            {"target":"b.rs","sha256":"sha2"},
            {"target":"a.rs","sha256":"sha1"}
        ]),
    )];
    assert_eq!(
        classify_seed_verification(
            &events,
            "B95",
            &current(&[("a.rs", "sha1"), ("b.rs", "sha2")])
        )
        .unwrap(),
        SeedVerificationDecision::AlreadyVerified {
            event_id: "v1".into()
        }
    );
}

#[test]
fn changed_sha_or_target_allows_revision_and_names_latest_event() {
    let events = vec![
        verified(
            "old-other",
            "OTHER",
            serde_json::json!([{"target":"a.rs","sha256":"other"}]),
        ),
        verified(
            "v1",
            "B95",
            serde_json::json!([{"target":"a.rs","sha256":"sha1"}]),
        ),
    ];
    assert_eq!(
        classify_seed_verification(&events, "B95", &current(&[("a.rs", "sha2")]))
            .unwrap(),
        SeedVerificationDecision::Revision {
            supersedes_event_id: "v1".into()
        }
    );
    assert_eq!(
        classify_seed_verification(&events, "B95", &current(&[("renamed.rs", "sha1")]))
            .unwrap(),
        SeedVerificationDecision::Revision {
            supersedes_event_id: "v1".into()
        }
    );
}

#[test]
fn last_task_scoped_verification_wins() {
    let events = vec![
        verified(
            "v1",
            "B95",
            serde_json::json!([{"target":"a.rs","sha256":"old"}]),
        ),
        verified(
            "v2",
            "B95",
            serde_json::json!([{"target":"a.rs","sha256":"new"}]),
        ),
    ];
    assert_eq!(
        classify_seed_verification(&events, "B95", &current(&[("a.rs", "new")]))
            .unwrap(),
        SeedVerificationDecision::AlreadyVerified {
            event_id: "v2".into()
        }
    );
}

#[test]
fn malformed_or_duplicate_seed_sets_fail_closed() {
    let missing = vec![EventRecord {
        event_id: "bad".into(),
        ts: "2026-07-25T00:00:00Z".into(),
        actor: "planner".into(),
        kind: "SeedOracleVerified".into(),
        task_id: Some("B95".into()),
        round: Some("r44".into()),
        payload: Some(serde_json::json!({"measured":{}})),
        extra: serde_json::Map::new(),
    }];
    assert!(classify_seed_verification(&missing, "B95", &current(&[("a.rs", "x")]))
        .is_err());
    assert!(classify_seed_verification(
        &[],
        "B95",
        &current(&[("a.rs", "x"), ("a.rs", "y")])
    )
    .is_err());
}
