use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use orch_host::runloop::{next_actions_for_mode, Action, TaskSnapshot};
use orch_host::wave::{run_wave_with_mode, TaskWaveOutcome, WaveDriver, WaveStep, WaveTaskPhase};

#[derive(Default)]
struct Driver {
    phases: HashMap<String, WaveTaskPhase>,
    log: Mutex<Vec<String>>,
    root_authorized: bool,
}

impl WaveDriver for Driver {
    fn phase(&self, task: &str) -> WaveTaskPhase {
        self.phases.get(task).cloned().unwrap_or(WaveTaskPhase::New)
    }

    fn already_recorded(&self, _task: &str) -> bool {
        false
    }

    fn dispatch(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("dispatch:{task}"));
        true
    }

    fn drive_to_collect(&self, task: &str) -> TaskWaveOutcome {
        self.log.lock().unwrap().push(format!("collect:{task}"));
        TaskWaveOutcome::Collected
    }

    fn verify(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("verify:{task}"));
        true
    }

    fn merge(&self, task: &str) -> bool {
        self.log.lock().unwrap().push(format!("merge:{task}"));
        true
    }

    fn root_merge_authorized(&self, _task: &str) -> bool {
        self.root_authorized
    }
}

fn step(task: &str) -> WaveStep {
    WaveStep {
        tasks: vec![task.into()],
        merge_order: vec![task.into()],
    }
}

#[test]
fn wave_collects_then_awaits_root_without_verify_blocker_or_merge() {
    let driver = Driver::default();
    let outcome = run_wave_with_mode(&[step("B")], &driver, true);
    assert_eq!(outcome.awaiting_root, vec!["B"]);
    assert_eq!(outcome.wave.blocked_wave, None);
    assert!(outcome.wave.merged.is_empty());
    assert_eq!(*driver.log.lock().unwrap(), vec!["dispatch:B", "collect:B"]);
}

#[test]
fn approved_root_pass_is_merged_on_the_next_entry_without_verify() {
    let mut driver = Driver::default();
    driver.phases.insert("B".into(), WaveTaskPhase::Approved);
    driver.root_authorized = true;
    let outcome = run_wave_with_mode(&[step("B")], &driver, true);
    assert_eq!(outcome.wave.merged, vec!["B"]);
    assert!(outcome.awaiting_root.is_empty());
    assert_eq!(*driver.log.lock().unwrap(), vec!["merge:B"]);
}

#[test]
fn actor_agnostic_approved_projection_without_root_authorization_stays_at_barrier() {
    let mut driver = Driver::default();
    driver.phases.insert("B".into(), WaveTaskPhase::Approved);
    let outcome = run_wave_with_mode(&[step("B")], &driver, true);
    assert_eq!(outcome.awaiting_root, vec!["B"]);
    assert!(outcome.wave.merged.is_empty());
    assert_eq!(*driver.log.lock().unwrap(), Vec::<String>::new());
}

#[test]
fn runloop_root_mode_projects_ready_to_awaiting_and_approved_to_merge() {
    let tasks = vec![
        TaskSnapshot {
            id: "A".into(),
            state: "ready_for_verification".into(),
        },
        TaskSnapshot {
            id: "B".into(),
            state: "approved".into(),
        },
    ];
    let (actions, awaiting) = next_actions_for_mode(&tasks, &[], true);
    assert_eq!(awaiting, vec!["A"]);
    assert_eq!(actions, vec![Action::Merge { task: "B".into() }]);
}

#[test]
fn direct_verify_rejects_before_log_creation_or_spawn() {
    let root: PathBuf = orch_host::util::test_scratch_dir("b130-direct-verify");
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r48")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r48\n").unwrap();
    fs::write(
        root.join("coordination/rounds/r48/ROUND-IR.yaml"),
        r#"round: r48
revision: 1
policy: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff, autoMergeOnPass: true}
budgets: {}
verification: {mode: root-manual-fixed-head, adapter: root-manual}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling: {allowedAgents: [], capacities: {}}
tasks: []
"#,
    )
    .unwrap();

    let error = orch_host::verify::run_verify(&root, "B", "sentinel-do-not-spawn", 1)
        .err()
        .expect("direct verify must reject")
        .to_string();
    assert!(!error.is_empty());
    assert!(!root.join("coordination/runtime/logs").exists());
}
