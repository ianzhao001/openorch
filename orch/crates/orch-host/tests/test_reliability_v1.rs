//! Maintenance scope guard. Runtime proof is separately required by the card:
//! default/selfhost lib compilation, polluted Pi subprocess, durable-pin reader.
//! Mutations: remove the selfhost test boundary, remove Pi isolation, restore
//! the vacuous OR scan. Each must independently make this test fail.
use std::{fs, path::Path};
#[test]
fn maintenance_preserves_feature_environment_and_behavioral_test_boundaries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let consult = fs::read_to_string(root.join("crates/orch-host/src/consult.rs")).unwrap();
    assert!(consult.contains("#[cfg(feature = \"selfhost\")]\n    #[test]\n    fn refusal_only_appends_the_consult_log"), "selfhost writer test must be feature scoped");
    let support = fs::read_to_string(root.join("crates/orch-host/tests/harness_invocation_envelope_support/mod.rs")).unwrap();
    assert!(support.contains(".env_remove(\"ORCH_PI_PROJECT_ROOT\")"), "Pi fixture must isolate project root");
    let pin = fs::read_to_string(root.join("crates/orch-host/tests/audited_agent_pin_amendment.rs")).unwrap();
    assert!(!pin.contains("!source.contains(\"load_agent_definitions\") ||"), "vacuous OR is not pin evidence");
    assert!(pin.contains("backend_receipt_expectation_for_wake("), "exercise durable reader");
}
