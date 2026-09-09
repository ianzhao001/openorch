use std::fs;
use std::io::Write;
use std::path::Path;

#[allow(dead_code)]
pub fn open(root: &Path, round: &str, purpose: &str) {
    for sub in [
        "tasks", "seeds", "reports", "reviews", "evidence", "dispatch",
    ] {
        fs::create_dir_all(root.join(format!("coordination/rounds/{round}/{sub}"))).unwrap();
    }
    let opened = orch_host::ledger::event(
        "RoundOpened",
        "runtime:orch",
        None,
        Some(round),
        serde_json::json!({"purpose": purpose}),
    );
    orch_host::ledger::append(root, round, &[opened]).unwrap();
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{round}\n"),
    )
    .unwrap();
    let mut board = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("coordination/BOARD.md"))
        .unwrap();
    writeln!(board, "\n## {round}（{purpose}）\n").unwrap();
}

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
