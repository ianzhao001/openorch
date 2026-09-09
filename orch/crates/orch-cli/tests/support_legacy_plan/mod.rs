use std::fs;
use std::path::Path;

pub fn materialize(root: &Path) -> anyhow::Result<orch_host::plan::PlanOutcome> {
    let round = orch_host::current_round(root)?;
    let artifact = orch_host::plan::build_legacy_plan_artifact_readonly(root, &round)?;
    if let Some(bytes) = artifact.ir_bytes.as_deref() {
        fs::create_dir_all(
            artifact
                .outcome
                .ir_path
                .parent()
                .expect("legacy ROUND-IR parent"),
        )?;
        fs::write(&artifact.outcome.ir_path, bytes)?;
    }
    if let Some(event) = artifact.validation_event.as_ref() {
        orch_host::ledger::append(root, &round, std::slice::from_ref(event))?;
    }
    Ok(artifact.outcome)
}
