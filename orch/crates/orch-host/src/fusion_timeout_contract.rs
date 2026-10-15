//! B370: timeout is an adapter fact, independent of a caller's elapsed clock.
//! Mutations (each must fail): discard the typed marker in adapter::run;
//! treat the marker as an ordinary error; lose marker identity through context;
//! infer timeout from prose; route production fusion errors around classification.
//! Public adapter::run docs are reviewed separately and removed as a negative check.
use super::*;
use crate::adapter::AdapterTimeout;

fn failed_member() -> MemberOutcome {
    MemberOutcome::new(0, "contract-member", MemberStatus::Failed)
}

#[test]
fn typed_timeout_does_not_wait_for_an_outer_deadline() {
    let started = Instant::now();
    let outer_deadline = Duration::from_secs(3600);
    let mut member = failed_member();
    apply_adapter_error(&mut member, anyhow::Error::new(AdapterTimeout));
    assert!(started.elapsed() < outer_deadline);
    assert_eq!(member.status, MemberStatus::TimedOut);
    assert_eq!(member.failure_class, Some(FailureClass::Timeout));
}

#[test]
fn timeout_identity_survives_multiple_context_layers() {
    let error = anyhow::Error::new(AdapterTimeout)
        .context("member exceeded its deadline; log: member.jsonl")
        .context("adapter invocation failed");
    let mut member = failed_member();
    apply_adapter_error(&mut member, error);
    assert_eq!(member.status, MemberStatus::TimedOut);
    assert_eq!(member.failure_class, Some(FailureClass::Timeout));
    let reason = member.reason.unwrap();
    assert!(reason.contains("adapter invocation failed"));
    assert!(reason.contains("member.jsonl"));
}

#[test]
fn ordinary_errors_and_timeout_prose_are_not_timeout_facts() {
    for message in ["spawn failed", "超时（>1s）已 kill；日志: member.jsonl", "adapter deadline expired"] {
        let mut member = failed_member();
        apply_adapter_error(&mut member, anyhow::anyhow!(message).context("slow outer operation"));
        assert_eq!(member.status, MemberStatus::Failed, "{message}");
        assert_eq!(member.failure_class, Some(FailureClass::Unknown), "{message}");
        assert!(member.reason.unwrap().contains(message));
    }
}

#[test]
fn real_adapter_zero_deadline_preserves_timeout_identity_and_log_context() {
    let root = crate::util::test_scratch_dir("typed-adapter-timeout");
    let specs = root.join("coordination/adapters");
    fs::create_dir_all(&specs).unwrap();
    // Spawn sleep directly so killing the owned child also closes both pipes.
    let spec = serde_json::json!({"launch": {"argv": ["/bin/sleep", "30"], "cwd_is_workdir": true}});
    fs::write(specs.join("contract-timeout.yaml"), serde_yaml::to_string(&spec).unwrap()).unwrap();
    let logs = root.join("logs");
    let error = adapter::run("contract-timeout", "unused", &root, &logs, "member", Duration::ZERO, None, Some(&root))
        .err().expect("live child with zero deadline must time out");
    assert!(error.downcast_ref::<AdapterTimeout>().is_some(), "{error:#}");
    let message = format!("{error:#}");
    assert!(message.contains("contract-timeout 超时"), "{message}");
    assert!(message.contains(logs.join("member.jsonl").to_str().unwrap()), "{message}");
    assert!(logs.join("member.jsonl").is_file());
    assert!(logs.join("member.stderr.log").is_file());
    let mut member = failed_member();
    apply_adapter_error(&mut member, error.context("fusion member"));
    assert_eq!(member.status, MemberStatus::TimedOut);
    assert_eq!(member.failure_class, Some(FailureClass::Timeout));
}
