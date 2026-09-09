//! ═══ 红种子契约 · B93（AgentProfile + candidate chain validation）═══
//! 落位: orch/crates/orch-host/tests/agent_profile_validation.rs（逐字节复制）
//! 预期红（redForm: compile）：agent_profile 契约符号尚不存在 → error[E0432]。
//!
//! 负向变异下界：
//! M1 空 quotaDomain/零 capacity 仍通过 → malformed_profiles_are_rejected 红；
//! M2 重复 profile id 后者覆盖前者 → duplicate_profile_ids_are_rejected 红；
//! M3 未知/重复 candidate 静默跳过 → unknown_and_duplicate_candidates_are_rejected 红；
//! M4 忽略 capability → missing_capability_rejects_chain 红；
//! M5 忽略 qualityClass → unsupported_quality_rejects_chain 红；
//! M6 排序候选链 → valid_chain_preserves_declared_order 红。

use orch_host::agent_profile::{
    validate_candidate_chain, validate_profiles, AgentProfile, QualityClass, RouteRequirement,
};

fn profile(
    id: &str,
    quota: &str,
    capacity: usize,
    capabilities: &[&str],
    quality_class: QualityClass,
) -> AgentProfile {
    AgentProfile {
        id: id.into(),
        capabilities: capabilities.iter().map(|value| (*value).to_string()).collect(),
        quota_domain: quota.into(),
        max_concurrent: capacity,
        quality_class,
    }
}

fn requirement(quality_class: QualityClass, capabilities: &[&str]) -> RouteRequirement {
    RouteRequirement {
        quality_class,
        required_capabilities: capabilities
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
    }
}

#[test]
fn malformed_profiles_are_rejected() {
    let profiles = vec![
        profile("empty-quota", " ", 1, &["rust"], QualityClass::Standard),
        profile("zero-cap", "zhipu", 0, &["rust"], QualityClass::Standard),
    ];
    let errors = validate_profiles(&profiles).unwrap_err();
    assert!(errors.iter().any(|error| error.contains("empty-quota")));
    assert!(errors.iter().any(|error| error.contains("zero-cap")));
}

#[test]
fn duplicate_profile_ids_are_rejected() {
    let profiles = vec![
        profile("same", "q1", 1, &["rust"], QualityClass::Standard),
        profile("same", "q2", 1, &["rust"], QualityClass::Standard),
    ];
    assert!(validate_profiles(&profiles).is_err());
}

#[test]
fn unknown_and_duplicate_candidates_are_rejected() {
    let profiles = vec![profile(
        "codex",
        "openai",
        1,
        &["rust"],
        QualityClass::Critical,
    )];
    let req = requirement(QualityClass::Critical, &["rust"]);
    assert!(
        validate_candidate_chain(&["missing".into()], &profiles, &req).is_err()
    );
    assert!(
        validate_candidate_chain(&["codex".into(), "codex".into()], &profiles, &req).is_err()
    );
}

#[test]
fn missing_capability_rejects_chain() {
    let profiles = vec![profile(
        "codex",
        "openai",
        1,
        &["rust"],
        QualityClass::Critical,
    )];
    let req = requirement(QualityClass::Critical, &["rust", "git"]);
    assert!(validate_candidate_chain(&["codex".into()], &profiles, &req).is_err());
}

#[test]
fn unsupported_quality_rejects_chain() {
    let profiles = vec![profile(
        "agy",
        "google",
        1,
        &["research"],
        QualityClass::Light,
    )];
    let req = requirement(QualityClass::Critical, &["research"]);
    assert!(validate_candidate_chain(&["agy".into()], &profiles, &req).is_err());
}

#[test]
fn valid_chain_preserves_declared_order() {
    let profiles = vec![
        profile(
            "codex",
            "openai",
            1,
            &["rust", "git"],
            QualityClass::Critical,
        ),
        profile(
            "opencode",
            "zhipu",
            1,
            &["rust", "git"],
            QualityClass::Critical,
        ),
    ];
    // A higher-quality agent may serve a lower-quality route; candidate order is
    // still the planner's declared order, not an implicit quality sort.
    let req = requirement(QualityClass::Standard, &["rust"]);
    assert_eq!(
        validate_candidate_chain(
            &["opencode".into(), "codex".into()],
            &profiles,
            &req
        )
        .unwrap(),
        vec!["opencode".to_string(), "codex".to_string()]
    );
}
