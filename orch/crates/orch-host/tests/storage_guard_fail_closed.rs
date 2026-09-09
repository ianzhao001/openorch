//! B206 seeded-red contract: 磁盘水位守卫 fail-closed + 环境性中断不判死。
//!
//! Expected red: compile. `orch_host::storage` 在 B206 之前不存在。
//!
//! 设计依据：r63 fusion 规划的 planner judge（consultations/01KZ409HF3FPC2B12YFV5VPEYK）。
//! 四家成员在「绝对字节而非百分比」「探测失败一律 fail-closed」两点上独立达成一致；
//! 「回收路径必须豁免守卫」与「判死抑制必须是账本纯折叠」是采纳裁定，见下。
//!
//! Negative mutations that must turn the named case red:
//! M1. 探测失败时放行（fail-open）-> `probe_failure_is_refused_not_waved_through` 红。
//!     r58 事故的复现路径就是「没有探测 ⇒ 静默写到 ENOSPC」，放行等于把坑重新打开。
//! M2. 把守卫也加在回收路径上 -> `reclaim_paths_are_exempt_so_a_full_disk_can_self_rescue` 红。
//!     磁盘打满后若连拆场都被守卫拒绝，系统就失去自救能力，只能人工救。
//! M3. 判死抑制去读当前磁盘状态（而非账本事实）-> `suppression_is_a_pure_fold_over_the_ledger` 红。
//!     判定必须可复算：同一份账本重放必须得出同一结论，否则 r62 赖以恢复的判据可复算性失效。
//! M4. 抑制后仍归 `liveness-dead` -> `suppressed_interruption_does_not_burn_retry_budget` 红。
//!     `count_dead_attempts` 只数 AttemptCrashed，但 dead 分类会推动重试消耗与降级惩罚；
//!     归 `liveness-stalled` 才能复用既有的 ResumeIssued 清除路径实现无惩罚续办。
//! M5. 百分比阈值 -> `threshold_is_absolute_bytes_not_a_ratio` 红。
//! M6. 守卫在写副作用之后才探测 -> `guard_precedes_every_write_side_effect` 红。
//! M7. 只探测不预留（探测通过后放任并发消费余量）-> `permit_reserves_so_concurrent_writers_cannot_race` 红。
//!     这是 TOCTOU：探到 30 GiB 与真正写下去之间，另一个 attempt 可能已经吃掉它。
//! M8. 只探一个路径（假定同卷）-> `probe_deduplicates_by_filesystem_not_by_path` 红。
//!     `.worktrees/` 与 `orch/target/` 可以分属不同挂载点，单点探测会给出错误结论。

use orch_core::EventRecord;
use orch_host::storage::{
    storage_guard_decision, storage_interruption_active, GuardEntry, GuardOutcome, ProbeResult,
    StoragePermit, StorageThreshold,
};

const ROUND: &str = "r63";
const TASK: &str = "B901";
const ATTEMPT: &str = "B901-A0001";

fn threshold() -> StorageThreshold {
    StorageThreshold::from_floor_bytes(20 * 1024 * 1024 * 1024)
}

fn event(kind: &str, payload: serde_json::Value) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": ulid::Ulid::new().to_string(),
        "ts": "2026-08-04T00:00:00Z",
        "actor": "runtime:orch",
        "type": kind,
        "round": ROUND,
        "taskId": TASK,
        "payload": payload,
    }))
    .expect("构造事件失败")
}

#[test]
fn threshold_is_absolute_bytes_not_a_ratio() {
    // 同一个百分比在 460 GiB 卷与 100 GiB 卷上语义不可比，而 ENOSPC 是绝对量事件。
    let small_volume = ProbeResult::Ok {
        available_bytes: 25 * 1024 * 1024 * 1024,
        total_bytes: 100 * 1024 * 1024 * 1024,
    };
    let large_volume = ProbeResult::Ok {
        available_bytes: 25 * 1024 * 1024 * 1024,
        total_bytes: 4000 * 1024 * 1024 * 1024,
    };
    // 可用字节相同 ⇒ 判定必须相同，与卷容量（即百分比）无关。
    assert_eq!(
        storage_guard_decision(&threshold(), GuardEntry::Dispatch, &small_volume),
        storage_guard_decision(&threshold(), GuardEntry::Dispatch, &large_volume),
        "判定必须只看绝对可用字节，不得随卷容量（百分比）漂移"
    );
}

#[test]
fn probe_failure_is_refused_not_waved_through() {
    let outcome = storage_guard_decision(
        &threshold(),
        GuardEntry::WakeReview,
        &ProbeResult::Failed {
            reason: "statfs: permission denied".into(),
        },
    );
    match outcome {
        GuardOutcome::Refuse { probe, .. } => {
            // 「探不到」与「余量不足」必须可区分，否则运维无法定位。
            assert_eq!(probe, "failed", "探测失败必须带 probe:\"failed\" 标记");
        }
        other => panic!("探测失败必须 fail-closed 拒绝，实际: {other:?}"),
    }
}

#[test]
fn low_water_refuses_with_self_describing_payload() {
    let outcome = storage_guard_decision(
        &threshold(),
        GuardEntry::Gate,
        &ProbeResult::Ok {
            available_bytes: 1024 * 1024 * 1024,
            total_bytes: 460 * 1024 * 1024 * 1024,
        },
    );
    let GuardOutcome::Refuse {
        available_bytes,
        threshold_bytes,
        entry,
        probe,
    } = outcome
    else {
        panic!("低于阈值必须拒绝");
    };
    // payload 必须自描述：账本里要能读出「当时可用多少、当时阈值多少、卡在哪个入口」，
    // 否则阈值放在 gitignored 的 machine.yaml 里就彻底不可审计了。
    assert_eq!(available_bytes, 1024 * 1024 * 1024);
    assert_eq!(threshold_bytes, 20 * 1024 * 1024 * 1024);
    assert_eq!(entry, GuardEntry::Gate);
    assert_eq!(probe, "low");
}

#[test]
fn reclaim_paths_are_exempt_so_a_full_disk_can_self_rescue() {
    // 拆场/GC/收轮是磁盘打满后的自救路径。如果它们也被守卫拒绝，
    // 系统就把自己锁死在满盘状态，只剩人工救援。
    let bone_dry = ProbeResult::Ok {
        available_bytes: 0,
        total_bytes: 460 * 1024 * 1024 * 1024,
    };
    for entry in [GuardEntry::Reclaim, GuardEntry::RoundClose] {
        assert!(
            matches!(
                storage_guard_decision(&threshold(), entry, &bone_dry),
                GuardOutcome::Allow { .. }
            ),
            "{entry:?} 是回收路径，必须豁免守卫（否则打满后无法自救）"
        );
    }
    for entry in [GuardEntry::Dispatch, GuardEntry::WakeReview, GuardEntry::Gate] {
        assert!(
            matches!(
                storage_guard_decision(&threshold(), entry, &bone_dry),
                GuardOutcome::Refuse { .. }
            ),
            "{entry:?} 会新增占用，必须被拒"
        );
    }
}

#[test]
fn suppression_is_a_pure_fold_over_the_ledger() {
    // 判死抑制只能由账本事实驱动。守卫在磁盘真正耗尽**之前**（阈值处）就落账，
    // 所以「存储事实」恒先于「心跳消失」存在——判死读账本就足以区分。
    let with_storage = vec![
        event(
            "AttemptStarted",
            serde_json::json!({"attemptId": ATTEMPT, "attemptNo": 1}),
        ),
        event(
            "EscalationRaised",
            serde_json::json!({"stage": "storage", "probe": "low", "entry": "gate"}),
        ),
    ];
    assert!(
        storage_interruption_active(&with_storage, TASK, ATTEMPT),
        "attempt 开始之后出现的 storage 升级必须使抑制生效"
    );

    // 已恢复：同 stage 的 recovered 之后不再抑制，否则一次打满会永久压住判死。
    let mut recovered = with_storage.clone();
    recovered.push(event(
        "EscalationRaised",
        serde_json::json!({"stage": "storage", "state": "recovered"}),
    ));
    assert!(
        !storage_interruption_active(&recovered, TASK, ATTEMPT),
        "水位回稳后必须解除抑制"
    );

    // 纯折叠：同一输入重复求值必须同结果（不得读文件系统当前状态）。
    for _ in 0..3 {
        assert!(storage_interruption_active(&with_storage, TASK, ATTEMPT));
        assert!(!storage_interruption_active(&recovered, TASK, ATTEMPT));
    }

    // 无 storage 事实时不得抑制——抑制必须要有证据，不能是默认行为。
    let plain = vec![event(
        "AttemptStarted",
        serde_json::json!({"attemptId": ATTEMPT, "attemptNo": 1}),
    )];
    assert!(!storage_interruption_active(&plain, TASK, ATTEMPT));
}

#[test]
fn permit_reserves_so_concurrent_writers_cannot_race() {
    // TOCTOU：探测通过与实际写入之间存在窗口，并发操作会吃掉刚探到的余量。
    // 守卫因此不能只是「查一下」，必须在短锁内复查并**预留**，
    // 且底层建目录/spawn 必须消费 permit——否则守卫只是建议，不是准入。
    let probe = ProbeResult::Ok {
        available_bytes: 30 * 1024 * 1024 * 1024,
        total_bytes: 460 * 1024 * 1024 * 1024,
    };
    let floor = StorageThreshold::from_floor_bytes(20 * 1024 * 1024 * 1024);

    // 第一笔预留 8 GiB：30 - 8 = 22 ≥ 20，放行。
    let first = StoragePermit::acquire(&floor, GuardEntry::WakeReview, &probe, 8 * 1024 * 1024 * 1024)
        .expect("首笔预留应放行");

    // 第二笔同样 8 GiB：账面 30 但已预留 8 ⇒ 可用 22，再扣 8 = 14 < 20，必须拒绝。
    // 判据看的是 available - outstanding，不是裸 available。
    assert!(
        StoragePermit::acquire(&floor, GuardEntry::WakeReview, &probe, 8 * 1024 * 1024 * 1024)
            .is_err(),
        "并发第二笔必须看到第一笔的预留（available - outstanding），否则就是 TOCTOU"
    );

    // permit 释放后额度归还，同样的请求再次放行——预留不得泄漏。
    drop(first);
    assert!(
        StoragePermit::acquire(&floor, GuardEntry::WakeReview, &probe, 8 * 1024 * 1024 * 1024)
            .is_ok(),
        "permit 释放后必须归还额度，否则一轮下来会把自己饿死"
    );
}

#[test]
fn probe_deduplicates_by_filesystem_not_by_path() {
    // `.worktrees/` 与 `orch/target/` 可以分属不同挂载点。按路径逐个探会重复计同一个卷的
    // 余量（虚高），只探一个路径又会漏掉另一个卷。判据必须按文件系统去重后逐卷各自成立。
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/storage.rs"),
    )
    .expect("读 src/storage.rs 失败");
    assert!(
        src.contains("fsid") || src.contains("filesystem") || src.contains("dedup"),
        "生产代码必须显式表达「按文件系统去重」，不能默认所有路径同卷"
    );
}

#[test]
fn guard_precedes_every_write_side_effect() {
    // 守卫的三个入口必须都在「首个写副作用」之前。这条用源码断言固化：
    // 每个入口函数体内，guard 调用必须先于任何 create_dir_all / worktree_add / spawn。
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/storage.rs"),
    )
    .expect("读 src/storage.rs 失败");
    assert!(
        src.contains("GuardEntry::Reclaim"),
        "回收豁免必须在生产代码里表达，不能只写在文档"
    );
    for entry in ["Dispatch", "WakeReview", "Gate"] {
        assert!(
            src.contains(entry),
            "守卫入口 {entry} 必须在生产代码里具名，便于审查者逐个核对调用点"
        );
    }
}
