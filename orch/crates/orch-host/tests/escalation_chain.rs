//! B147 retained dependency barrier; automatic candidate/review chains retired.
use orch_host::plan::{dependency_dispatch_admission, DependencyAdmission};

#[test]
fn explicit_dependencies_hold_dependents_until_prerequisites_record() {
    let dependencies = vec!["A".to_string()];
    assert_eq!(
        dependency_dispatch_admission(&dependencies, &[], None, &|_| false),
        DependencyAdmission::BlockedByDependency {
            blockers: vec!["A".to_string()]
        }
    );
    assert_eq!(
        dependency_dispatch_admission(
            &dependencies,
            &[("A".into(), "merge-A".into())],
            None,
            &|_| true
        ),
        DependencyAdmission::Admitted
    );
    assert!(matches!(
        dependency_dispatch_admission(
            &dependencies,
            &[("A".into(), "merge-A".into())],
            Some("stale-base"),
            &|_| false
        ),
        DependencyAdmission::ForwardBaselineRequired { .. }
    ));
}
