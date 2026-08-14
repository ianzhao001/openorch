//! ═══ 红种子契约 · B98 ═══
//! 预期红（redForm: compile）：quality state、事件构造与投影 API 尚不存在。
//! 变异清单：
//! M1 一次 PASS 即 eligible；M2 同 attempt PASS 重复计数；M3 quality FAIL 被忽略；
//! M4 quota/timeout 错扣质量；M5 integrity 只降 degraded；M6 PASS 自动解除隔离；
//! M7 任意 actor 可 manual reset；M8 probation 可进 production。

use std::collections::BTreeMap;

use orch_core::EventRecord;
use orch_host::quality::{
    assignment_allowed, project_quality, AssignmentMode, QualityPolicy, QualityState,
};

fn assessed(
    id: &str,
    actor: &str,
    task: &str,
    agent: &str,
    attempt: &str,
    assessment: &str,
) -> EventRecord {
    EventRecord {
        event_id: id.into(),
        ts: "2026-07-25T00:00:00Z".into(),
        actor: actor.into(),
        kind: "QualityAssessed".into(),
        task_id: Some(task.into()),
        round: Some("r44".into()),
        payload: Some(serde_json::json!({
            "agent": agent,
            "attemptId": attempt,
            "assessment": assessment,
            "reason": assessment,
        })),
        extra: serde_json::Map::new(),
    }
}

fn infra(id: &str, task: &str, agent: &str, attempt: &str, class: &str) -> EventRecord {
    EventRecord {
        event_id: id.into(),
        ts: "2026-07-25T00:00:00Z".into(),
        actor: "runtime:orch".into(),
        kind: "AttemptFailed".into(),
        task_id: Some(task.into()),
        round: Some("r44".into()),
        payload: Some(serde_json::json!({
            "agent": agent,
            "attemptId": attempt,
            "failureClass": class,
        })),
        extra: serde_json::Map::new(),
    }
}

fn initial(state: QualityState) -> BTreeMap<String, QualityState> {
    BTreeMap::from([("executor-mimo".into(), state)])
}

fn policy() -> QualityPolicy {
    QualityPolicy {
        probation_passes_required: 2,
    }
}

#[test]
fn probation_requires_two_distinct_consecutive_verified_attempts() {
    let first = project_quality(
        &[assessed(
            "q1",
            "verifier:claude-headless",
            "B99",
            "executor-mimo",
            "B99-A0001",
            "verified-pass",
        )],
        &initial(QualityState::Probation),
        &policy(),
    )
    .unwrap();
    let state = &first.agents["executor-mimo"];
    assert_eq!(state.state, QualityState::Probation);
    assert_eq!(state.consecutive_passes, 1);

    let second = project_quality(
        &[
            assessed(
                "q1",
                "verifier:claude-headless",
                "B99",
                "executor-mimo",
                "B99-A0001",
                "verified-pass",
            ),
            assessed(
                "q2",
                "verifier:claude-headless",
                "B101",
                "executor-mimo",
                "B101-A0001",
                "verified-pass",
            ),
        ],
        &initial(QualityState::Probation),
        &policy(),
    )
    .unwrap();
    assert_eq!(
        second.agents["executor-mimo"].state,
        QualityState::Eligible
    );
}

#[test]
fn duplicate_same_attempt_pass_is_idempotent_and_conflict_fails_closed() {
    let pass = assessed(
        "q1",
        "verifier:claude-headless",
        "B99",
        "executor-mimo",
        "B99-A0001",
        "verified-pass",
    );
    let projection = project_quality(
        &[pass.clone(), pass],
        &initial(QualityState::Probation),
        &policy(),
    )
    .unwrap();
    assert_eq!(projection.agents["executor-mimo"].consecutive_passes, 1);

    let conflict = project_quality(
        &[
            assessed(
                "q1",
                "verifier:claude-headless",
                "B99",
                "executor-mimo",
                "B99-A0001",
                "verified-pass",
            ),
            assessed(
                "q2",
                "verifier:claude-headless",
                "B99",
                "executor-mimo",
                "B99-A0001",
                "verified-fail",
            ),
        ],
        &initial(QualityState::Probation),
        &policy(),
    );
    assert!(conflict.is_err());
}

#[test]
fn verified_fail_degrades_and_breaks_probation_streak() {
    let projection = project_quality(
        &[
            assessed(
                "q1",
                "verifier:claude-headless",
                "B99",
                "executor-mimo",
                "B99-A0001",
                "verified-pass",
            ),
            assessed(
                "q2",
                "verifier:claude-headless",
                "B101",
                "executor-mimo",
                "B101-A0001",
                "verified-fail",
            ),
        ],
        &initial(QualityState::Probation),
        &policy(),
    )
    .unwrap();
    let state = &projection.agents["executor-mimo"];
    assert_eq!(state.state, QualityState::Degraded);
    assert_eq!(state.consecutive_passes, 0);

    let eligible_failed = project_quality(
        &[assessed(
            "q3",
            "verifier:claude-headless",
            "B103",
            "executor-mimo",
            "B103-A0001",
            "verified-fail",
        )],
        &initial(QualityState::Eligible),
        &policy(),
    )
    .unwrap();
    assert_eq!(
        eligible_failed.agents["executor-mimo"].state,
        QualityState::Degraded
    );
}

#[test]
fn infrastructure_failures_do_not_change_quality() {
    for class in ["quota", "rate-limited", "503", "auth", "timeout", "transport"] {
        let projection = project_quality(
            &[infra("f1", "B99", "executor-mimo", "B99-A0001", class)],
            &initial(QualityState::Eligible),
            &policy(),
        )
        .unwrap();
        assert_eq!(
            projection.agents["executor-mimo"].state,
            QualityState::Eligible,
            "class {class}"
        );
    }
}

#[test]
fn integrity_violation_quarantines_and_pass_cannot_auto_restore() {
    let projection = project_quality(
        &[
            assessed(
                "q1",
                "runtime:orch",
                "B99",
                "executor-mimo",
                "B99-A0001",
                "integrity-violation",
            ),
            assessed(
                "q2",
                "verifier:claude-headless",
                "B101",
                "executor-mimo",
                "B101-A0001",
                "verified-pass",
            ),
        ],
        &initial(QualityState::Eligible),
        &policy(),
    )
    .unwrap();
    assert_eq!(
        projection.agents["executor-mimo"].state,
        QualityState::Quarantined
    );
}

#[test]
fn only_user_or_planner_can_manual_reset_to_probation() {
    let unauthorized = project_quality(
        &[assessed(
            "q1",
            "executor:mimo",
            "B99",
            "executor-mimo",
            "B99-A0001",
            "manual-reset",
        )],
        &initial(QualityState::Quarantined),
        &policy(),
    );
    assert!(unauthorized.is_err());

    for actor in ["user", "planner:codex"] {
        let reset = project_quality(
            &[assessed(
                "q2",
                actor,
                "B99",
                "executor-mimo",
                "B99-A0002",
                "manual-reset",
            )],
            &initial(QualityState::Quarantined),
            &policy(),
        )
        .unwrap();
        assert_eq!(
            reset.agents["executor-mimo"].state,
            QualityState::Probation
        );
    }
}

#[test]
fn malformed_unknown_agent_and_zero_policy_fail_closed() {
    let unknown = project_quality(
        &[assessed(
            "q1",
            "verifier:claude-headless",
            "B99",
            "executor-unknown",
            "B99-A0001",
            "verified-pass",
        )],
        &initial(QualityState::Probation),
        &policy(),
    );
    assert!(unknown.is_err());
    assert!(project_quality(
        &[],
        &initial(QualityState::Probation),
        &QualityPolicy {
            probation_passes_required: 0,
        },
    )
    .is_err());
}

#[test]
fn assignment_modes_enforce_probation_and_isolation() {
    assert!(assignment_allowed(
        QualityState::Probation,
        AssignmentMode::Canary
    ));
    assert!(!assignment_allowed(
        QualityState::Probation,
        AssignmentMode::Production
    ));
    assert!(assignment_allowed(
        QualityState::Eligible,
        AssignmentMode::Production
    ));
    for state in [QualityState::Degraded, QualityState::Quarantined] {
        assert!(!assignment_allowed(state, AssignmentMode::Canary));
        assert!(!assignment_allowed(state, AssignmentMode::Production));
    }
}
