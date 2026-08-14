//! ═══ 红种子契约 · B95 ═══
//! 预期红（redForm: compile）：typed blocker、WaveDriver blocker hook 与落盘函数尚不存在。
//! 变异清单：
//! M1 verify 失败后 break，阻断 clean peer；M2 merge 失败后仍继续不可逆动作；
//! M3 collect Failed/DeadOrStalled 分类互换或不记 blocker；M4 healthy peer 也记 blocker；
//! M5 事件缺 stage/class/attemptKey 或误用 AttemptBlocked；M6 随机 inbox/去掉幂等；
//! M7 坏账本仍写事件/inbox；M8 新 DispatchIssued 后错误沿用旧 attempt 去重。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use orch_host::wave::{
    record_wave_blocker, run_wave_with, TaskWaveBlocker, TaskWaveOutcome, WaveBlockerClass,
    WaveDriver, WaveTaskPhase, WaveStep,
};

fn step(ids: &[&str]) -> WaveStep {
    WaveStep {
        tasks: ids.iter().map(|id| (*id).to_string()).collect(),
        merge_order: ids.iter().map(|id| (*id).to_string()).collect(),
    }
}

#[derive(Default)]
struct Driver {
    phases: HashMap<String, WaveTaskPhase>,
    collect: HashMap<String, TaskWaveOutcome>,
    dispatch_fail: HashSet<String>,
    verify_fail: HashSet<String>,
    merge_fail: HashSet<String>,
    blockers: Mutex<Vec<TaskWaveBlocker>>,
    calls: Mutex<Vec<String>>,
}

impl WaveDriver for Driver {
    fn phase(&self, task: &str) -> WaveTaskPhase {
        self.phases
            .get(task)
            .cloned()
            .unwrap_or(WaveTaskPhase::New)
    }
    fn already_recorded(&self, _task: &str) -> bool {
        false
    }
    fn dispatch(&self, task: &str) -> bool {
        self.calls.lock().unwrap().push(format!("dispatch:{task}"));
        !self.dispatch_fail.contains(task)
    }
    fn drive_to_collect(&self, task: &str) -> TaskWaveOutcome {
        self.collect
            .get(task)
            .cloned()
            .unwrap_or(TaskWaveOutcome::Collected)
    }
    fn verify(&self, task: &str) -> bool {
        self.calls.lock().unwrap().push(format!("verify:{task}"));
        !self.verify_fail.contains(task)
    }
    fn merge(&self, task: &str) -> bool {
        self.calls.lock().unwrap().push(format!("merge:{task}"));
        !self.merge_fail.contains(task)
    }
    fn record_blocker(&self, blocker: &TaskWaveBlocker) -> bool {
        self.blockers.lock().unwrap().push(blocker.clone());
        true
    }
}

#[test]
fn collect_failure_blocks_only_that_task_and_clean_peer_finishes() {
    let mut d = Driver::default();
    d.collect.insert("A".into(), TaskWaveOutcome::Failed);
    let out = run_wave_with(&[step(&["A", "B"])], &d);
    assert_eq!(out.merged, vec!["B"]);
    assert_eq!(out.blocked_wave, Some(0));
    assert_eq!(
        *d.blockers.lock().unwrap(),
        vec![TaskWaveBlocker {
            task_id: "A".into(),
            class: WaveBlockerClass::CollectFailed,
        }]
    );
}

#[test]
fn verify_failure_does_not_prevent_later_clean_peer() {
    let mut d = Driver::default();
    d.verify_fail.insert("A".into());
    let out = run_wave_with(&[step(&["A", "B"])], &d);
    assert_eq!(out.merged, vec!["B"]);
    assert_eq!(out.blocked_wave, Some(0));
    assert_eq!(
        *d.blockers.lock().unwrap(),
        vec![TaskWaveBlocker {
            task_id: "A".into(),
            class: WaveBlockerClass::VerifyFailed,
        }]
    );
    let calls = d.calls.lock().unwrap();
    assert!(calls.contains(&"verify:B".to_string()));
    assert!(calls.contains(&"merge:B".to_string()));
}

#[test]
fn merge_failure_stops_later_irreversible_actions() {
    let mut d = Driver::default();
    d.merge_fail.insert("A".into());
    let out = run_wave_with(&[step(&["A", "B"])], &d);
    assert!(out.merged.is_empty());
    assert_eq!(out.blocked_wave, Some(0));
    let calls = d.calls.lock().unwrap();
    assert!(!calls.contains(&"verify:B".to_string()));
    assert!(!calls.contains(&"merge:B".to_string()));
    assert_eq!(
        *d.blockers.lock().unwrap(),
        vec![TaskWaveBlocker {
            task_id: "A".into(),
            class: WaveBlockerClass::MergeFailed,
        }]
    );
}

#[test]
fn dispatch_dead_and_needs_operator_have_exact_classes() {
    let mut d = Driver::default();
    d.dispatch_fail.insert("D".into());
    d.collect
        .insert("S".into(), TaskWaveOutcome::DeadOrStalled);
    d.phases
        .insert("N".into(), WaveTaskPhase::NeedsOperator);
    let out = run_wave_with(&[step(&["D", "S", "N", "H"])], &d);
    assert_eq!(out.merged, vec!["H"]);
    assert_eq!(out.blocked_wave, Some(0));
    assert_eq!(
        *d.blockers.lock().unwrap(),
        vec![
            TaskWaveBlocker {
                task_id: "N".into(),
                class: WaveBlockerClass::NeedsOperator,
            },
            TaskWaveBlocker {
                task_id: "D".into(),
                class: WaveBlockerClass::DispatchFailed,
            },
            TaskWaveBlocker {
                task_id: "S".into(),
                class: WaveBlockerClass::DeadOrStalled,
            },
        ]
    );
}

fn event(id: &str, kind: &str, task: &str, round: &str) -> orch_core::EventRecord {
    orch_core::EventRecord {
        event_id: id.into(),
        ts: "2026-07-25T00:00:00Z".into(),
        actor: "runtime:test".into(),
        kind: kind.into(),
        task_id: Some(task.into()),
        round: Some(round.into()),
        payload: Some(serde_json::json!({"agent":"executor-desktop"})),
        extra: serde_json::Map::new(),
    }
}

fn write_ledger(root: &Path, events: &[orch_core::EventRecord]) {
    let dir = root.join("coordination/rounds/r44");
    fs::create_dir_all(&dir).unwrap();
    let body = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.join("events.jsonl"), format!("{body}\n")).unwrap();
}

fn temp_root(tag: &str) -> PathBuf {
    orch_host::util::test_scratch_dir(&format!("b95-{tag}"))
}

#[test]
fn blocker_record_is_attempt_scoped_and_idempotent() {
    let root = temp_root("idempotent");
    write_ledger(&root, &[event("d1", "DispatchIssued", "A", "r44")]);
    let blocker = TaskWaveBlocker {
        task_id: "A".into(),
        class: WaveBlockerClass::VerifyFailed,
    };
    let first = record_wave_blocker(&root, "r44", &blocker).unwrap();
    let second = record_wave_blocker(&root, "r44", &blocker).unwrap();
    assert_eq!(first, second);

    let ledger = orch_core::read_ledger(
        &root.join("coordination/rounds/r44/events.jsonl"),
    )
    .unwrap();
    let blockers = ledger
        .events
        .iter()
        .filter(|event| {
            event.kind == "EscalationRaised"
                && event.payload.as_ref().and_then(|p| p.get("stage"))
                    == Some(&serde_json::json!("wave-task-blocker"))
        })
        .collect::<Vec<_>>();
    assert_eq!(blockers.len(), 1);
    assert_eq!(
        blockers[0]
            .payload
            .as_ref()
            .and_then(|p| p.get("attemptKey"))
            .and_then(serde_json::Value::as_str),
        Some("d1")
    );
    assert!(root.join(&first).is_file());

    let mut events = ledger.events;
    events.push(event("d2", "DispatchIssued", "A", "r44"));
    write_ledger(&root, &events);
    let third = record_wave_blocker(&root, "r44", &blocker).unwrap();
    assert_ne!(first, third);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bad_ledger_fails_closed_before_inbox_or_append() {
    let root = temp_root("bad-ledger");
    let dir = root.join("coordination/rounds/r44");
    fs::create_dir_all(&dir).unwrap();
    let ledger_path = dir.join("events.jsonl");
    fs::write(&ledger_path, "{not-json}\n").unwrap();
    let before = fs::read(&ledger_path).unwrap();
    let result = record_wave_blocker(
        &root,
        "r44",
        &TaskWaveBlocker {
            task_id: "A".into(),
            class: WaveBlockerClass::CollectFailed,
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read(&ledger_path).unwrap(), before);
    assert!(!root.join("coordination/inbox").exists());
    fs::remove_dir_all(root).unwrap();
}
