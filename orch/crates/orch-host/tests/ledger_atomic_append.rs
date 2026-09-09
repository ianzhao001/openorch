//! ═══ 红种子契约 · B87（ledger lock 内 read→判定→append）═══
//! 落位: orch/crates/orch-host/tests/ledger_atomic_append.rs（逐字节复制）
//! 预期红同棒：`ledger::append_checked` 尚不存在 → error[E0425]。
//!
//! 负向变异下界：
//! M1 锁前读取 → concurrent_check_append_writes_one_group 红；
//! M2 坏行仍 append → bad_ledger_refuses_append 红；
//! M3 空 decision 仍写占位行 → empty_decision_writes_nothing 红；
//! M4 未复用 ledger.lock → concurrent_check_append_writes_one_group 红。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use orch_core::read_ledger;
use orch_host::ledger;

// B137：pid 不足以避免同进程并发撞名（cargo test 多线程同纳秒刻度共用目录），
// 沿 src/binding.rs::b106_scratch_dir 范式叠加模块级单调计数器。
static TESTROOT_SEQ: AtomicU64 = AtomicU64::new(0);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
        let seq = TESTROOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = orch_root
            .join("target/test-tmp")
            .join(format!(
                "orch-r42-{label}-{}-{}-{seq}",
                std::process::id(),
                ulid::Ulid::new()
            ));
        fs::create_dir_all(path.join("coordination/rounds/r42")).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn ledger(&self) -> PathBuf {
        self.0.join("coordination/rounds/r42/events.jsonl")
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn concurrent_check_append_writes_one_group() {
    let root = TestRoot::new("atomic");
    fs::write(root.ledger(), "").unwrap();
    let root = Arc::new(root);
    let barrier = Arc::new(Barrier::new(8));
    let mut workers = Vec::new();

    for _ in 0..8 {
        let root = Arc::clone(&root);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            ledger::append_checked(root.path(), "r42", |events| {
                if events.iter().any(|event| {
                    event.kind == "AttemptBlocked" && event.task_id.as_deref() == Some("B87")
                }) {
                    return Ok(Vec::new());
                }
                Ok(vec![
                    ledger::event(
                        "EscalationRaised",
                        "runtime:orch",
                        Some("B87"),
                        Some("r42"),
                        serde_json::json!({"stage": "executor-blocked"}),
                    ),
                    ledger::event(
                        "AttemptBlocked",
                        "runtime:orch",
                        Some("B87"),
                        Some("r42"),
                        serde_json::json!({}),
                    ),
                ])
            })
            .unwrap()
        }));
    }

    let written: usize = workers.into_iter().map(|w| w.join().unwrap()).sum();
    assert_eq!(written, 2, "八个竞争者中只能有一组两事件落账");
    let ledger = read_ledger(&root.ledger()).unwrap();
    assert!(ledger.bad_lines.is_empty());
    assert_eq!(
        ledger
            .events
            .iter()
            .filter(|event| event.kind == "AttemptBlocked")
            .count(),
        1
    );
}

#[test]
fn bad_ledger_refuses_append() {
    let root = TestRoot::new("bad");
    fs::write(root.ledger(), "{not-json}\n").unwrap();
    let before = fs::read(root.ledger()).unwrap();
    let result = ledger::append_checked(root.path(), "r42", |_| {
        Ok(vec![ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some("B87"),
            Some("r42"),
            serde_json::json!({}),
        )])
    });
    assert!(result.is_err());
    assert_eq!(fs::read(root.ledger()).unwrap(), before);
}

#[test]
fn empty_decision_writes_nothing() {
    let root = TestRoot::new("empty");
    fs::write(root.ledger(), "").unwrap();
    assert_eq!(
        ledger::append_checked(root.path(), "r42", |_| Ok(Vec::new())).unwrap(),
        0
    );
    assert_eq!(fs::read(root.ledger()).unwrap(), b"");
}
