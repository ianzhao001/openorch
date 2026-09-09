//! B224 seeded-red contract: 回收臂在真实运行模式下够得着（H108+H109）。
//!
//! Expected red: compile. `SITE_RETIRED_EVENT_KIND` 与 `retire_task_sites` 在 B224 之前不存在。
//!
//! 设计依据：r64 fusion judge（consultations/01KZ5XQ31SKXAF6GC1WDMXK3Q0/judge.md）+
//! 密封独答（commit 8cd6418）。**本种子钉住三条来之不易的判据**：
//! ① 终点事件必须锚定授权事件（retireEventId → TaskRecorded/RoundClosed，反伪造，
//!    先例 = WorkspaceReleased.terminationEventId，sites.rs:731/:335）；
//! ② ReviewDelivered / VerdictIssued / TaskRecorded **本身**都不构成释放——r63 fusion
//!    证伪了「交付即可拆」的 4:5 多数共识，r64 judge 又确认 verdict 后存在合法 late 交付
//!    （wake.rs post_verdict late-N 路径）；释放只能来自显式 SiteRetired 事实；
//! ③ 生产者守卫：managed 且未 terminal 的 wake 绝不发 SiteRetired（托管终态只能来自
//!    前置和解，不得拿断言覆盖传感器）——r64 fusion 成员 pi 对独答的修正，judge 采纳 D3。
//!
//! Negative mutations that must turn the named case red:
//! M1. 把 ReviewDelivered/VerdictIssued/TaskRecorded 当作隐式释放条件
//!     -> `task_events_alone_never_release_a_site` 红。
//! M2. fold 不校验 retireEventId 指向真实存在的 TaskRecorded/RoundClosed 事件
//!     -> `dangling_or_wrong_type_retire_anchor_stays_active` 红。
//! M3. 「恰一条」放宽为「至少一条」
//!     -> `duplicate_site_retired_is_ambiguous_and_stays_active` 红。
//! M4. fold 不按 (siteId, generation) 精确匹配
//!     -> `cross_generation_retire_never_releases_newer_lease` 红。
//! M5. 生产者对 managed-pending 的现场也发 SiteRetired
//!     -> `producer_skips_sites_of_managed_wakes_without_terminal_evidence` 红。
//! M6. 三处前台入口丢掉前置和解（或和解被塞进 reap 内部形成环）
//!     -> `production_reconcile_precedes_reap_at_all_foreground_entries` 红。

use orch_core::EventRecord;
use orch_host::sites::{
    retire_task_sites, site_identity, LeaseState, SiteRole, SITE_RETIRED_EVENT_KIND,
};

const ROUND: &str = "r64";
const TASK: &str = "B902";
const ATTEMPT: &str = "B902-A0001";
const HEAD: &str = "0123456789012345678901234567890123456789";

fn event_with_id(event_id: &str, kind: &str, payload: serde_json::Value) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": event_id,
        "ts": "2026-08-04T00:00:00Z",
        "actor": "runtime:orch",
        "type": kind,
        "round": ROUND,
        "taskId": TASK,
        "payload": payload,
    }))
    .expect("构造事件失败")
}

fn lease(role: &str, agent: &str, generation: u32) -> EventRecord {
    event_with_id(
        &format!("EV-LEASE-{role}-{agent}-{generation}"),
        "WorkspaceLeased",
        serde_json::json!({
            "siteId": format!("{TASK}-{role}-{agent}-g{generation:02}"),
            "generation": generation,
            "attemptId": ATTEMPT,
            "role": role,
            "agent": agent,
            "reviewedHead": HEAD,
            "paths": {
                "worktree": format!(".worktrees/{TASK}-{role}-{agent}-g{generation:02}"),
                "target": format!("orch/target/review-{ATTEMPT}-{role}-{agent}-g{generation:02}"),
            },
        }),
    )
}

fn task_recorded(event_id: &str) -> EventRecord {
    event_with_id(
        event_id,
        "TaskRecorded",
        serde_json::json!({"attemptId": ATTEMPT}),
    )
}

fn retired(role: &str, agent: &str, generation: u32, anchor: &str, suffix: &str) -> EventRecord {
    event_with_id(
        &format!("EV-RETIRE-{role}-{agent}-{generation}-{suffix}"),
        SITE_RETIRED_EVENT_KIND,
        serde_json::json!({
            "siteId": format!("{TASK}-{role}-{agent}-g{generation:02}"),
            "generation": generation,
            "taskId": TASK,
            "attemptId": ATTEMPT,
            "role": role,
            "agent": agent,
            "trigger": "task-recorded",
            "retireEventId": anchor,
        }),
    )
}

fn state(events: &[EventRecord], role: SiteRole, agent: &str, generation: u32) -> LeaseState {
    LeaseState::of(events, &site_identity(TASK, role, agent), generation)
}

#[test]
fn site_retired_with_valid_anchor_releases_the_lease() {
    let events = vec![
        lease("secondary", "executor-zcode", 1),
        task_recorded("EV-TR-1"),
        retired("secondary", "executor-zcode", 1, "EV-TR-1", "a"),
    ];
    match state(&events, SiteRole::Secondary, "executor-zcode", 1) {
        LeaseState::Released {
            completion_receipt, ..
        } => {
            assert!(
                completion_receipt.contains("SiteRetired"),
                "回执必须标明 SiteRetired 来源，得到 {completion_receipt}"
            );
        }
        other => panic!("合法 SiteRetired 必须释放租约，得到 {other:?}"),
    }
}

#[test]
fn task_events_alone_never_release_a_site() {
    // M1：ReviewDelivered / VerdictIssued / TaskRecorded 全部在场，但无 SiteRetired。
    let events = vec![
        lease("secondary", "executor-agy", 1),
        event_with_id(
            "EV-RD-1",
            "ReviewDelivered",
            serde_json::json!({"attemptId": ATTEMPT}),
        ),
        event_with_id(
            "EV-VI-1",
            "VerdictIssued",
            serde_json::json!({"attemptId": ATTEMPT}),
        ),
        task_recorded("EV-TR-1"),
    ];
    assert!(
        matches!(
            state(&events, SiteRole::Secondary, "executor-agy", 1),
            LeaseState::Active { .. }
        ),
        "任务级事件本身绝不构成释放（r63 fusion 证伪的多数共识）"
    );
}

#[test]
fn dangling_or_wrong_type_retire_anchor_stays_active() {
    // M2a：retireEventId 指向不存在的事件。
    let dangling = vec![
        lease("secondary", "executor-zcode", 1),
        retired("secondary", "executor-zcode", 1, "EV-DOES-NOT-EXIST", "a"),
    ];
    assert!(
        matches!(
            state(&dangling, SiteRole::Secondary, "executor-zcode", 1),
            LeaseState::Active { .. }
        ),
        "悬空 retireEventId 必须判活跃"
    );
    // M2b：retireEventId 指向的事件类型不是 TaskRecorded/RoundClosed。
    let wrong_type = vec![
        lease("secondary", "executor-zcode", 1),
        event_with_id(
            "EV-RD-1",
            "ReviewDelivered",
            serde_json::json!({"attemptId": ATTEMPT}),
        ),
        retired("secondary", "executor-zcode", 1, "EV-RD-1", "a"),
    ];
    assert!(
        matches!(
            state(&wrong_type, SiteRole::Secondary, "executor-zcode", 1),
            LeaseState::Active { .. }
        ),
        "锚定到非授权类型事件必须判活跃"
    );
}

#[test]
fn duplicate_site_retired_is_ambiguous_and_stays_active() {
    // M3：同 (siteId, generation) 两条 SiteRetired ⇒ 绑定歧义 ⇒ fail-closed。
    let events = vec![
        lease("secondary", "executor-zcode", 1),
        task_recorded("EV-TR-1"),
        retired("secondary", "executor-zcode", 1, "EV-TR-1", "a"),
        retired("secondary", "executor-zcode", 1, "EV-TR-1", "b"),
    ];
    assert!(
        matches!(
            state(&events, SiteRole::Secondary, "executor-zcode", 1),
            LeaseState::Active { .. }
        ),
        "重复 SiteRetired 必须判活跃（证据歧义）"
    );
}

#[test]
fn cross_generation_retire_never_releases_newer_lease() {
    // M4：g01 的退役事实绝不能打穿 g02 的租约。
    let events = vec![
        lease("secondary", "executor-zcode", 1),
        task_recorded("EV-TR-1"),
        retired("secondary", "executor-zcode", 1, "EV-TR-1", "a"),
        lease("secondary", "executor-zcode", 2),
    ];
    assert!(
        matches!(
            state(&events, SiteRole::Secondary, "executor-zcode", 1),
            LeaseState::Released { .. }
        ),
        "g01 应按其退役事实释放"
    );
    assert!(
        matches!(
            state(&events, SiteRole::Secondary, "executor-zcode", 2),
            LeaseState::Active { .. }
        ),
        "g02 必须不受 g01 退役事实影响"
    );
}

#[test]
fn producer_skips_sites_of_managed_wakes_without_terminal_evidence() {
    // M5（judge 采纳 D3）：绑定 managed-pending wake 的现场不得被 retire。
    let managed_wake = event_with_id(
        "EV-WAKE-MANAGED",
        "WakeIssued",
        serde_json::json!({
            "wakeId": "WAKE-MANAGED-1",
            "agent": "executor-opencode",
            "backendState": "pending",
            "controlWakeId": "WAKE-MANAGED-1",
        }),
    );
    let mut lease_managed = lease("secondary", "executor-opencode", 1);
    if let Some(payload) = lease_managed.payload.as_mut() {
        payload["wakeId"] = serde_json::json!("WAKE-MANAGED-1");
    }
    let unmanaged_lease = lease("primary", "executor-agy", 1);
    let events = vec![managed_wake, lease_managed, unmanaged_lease];

    let produced = retire_task_sites(&events, TASK, "EV-TR-ANCHOR");
    assert!(
        !produced.iter().any(|e| {
            e.payload
                .as_ref()
                .and_then(|p| p.get("agent"))
                .and_then(|a| a.as_str())
                == Some("executor-opencode")
        }),
        "managed 且未 terminal 的现场绝不发 SiteRetired（终态只能来自前置和解）"
    );
    assert!(
        produced.iter().any(|e| {
            e.kind == SITE_RETIRED_EVENT_KIND
                && e.payload
                    .as_ref()
                    .and_then(|p| p.get("agent"))
                    .and_then(|a| a.as_str())
                    == Some("executor-agy")
        }),
        "非托管 leased-未 released 现场必须被生产 SiteRetired"
    );
}

#[test]
fn production_reconcile_precedes_reap_at_all_foreground_entries() {
    let root = orch_host::util::test_scratch_dir("maintenance-migrated-contract");
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&root)
        .status()
        .unwrap()
        .success());
    let ledger = root.join("coordination/rounds/rClosed/events.jsonl");
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    let event = serde_json::json!({"eventId":"closed","ts":"2026-09-08T00:00:00Z","actor":"runtime:orch","type":"RoundClosed","round":"rClosed","payload":{}});
    let bytes = serde_json::to_vec(&event).unwrap();
    std::fs::write(&ledger, &bytes).unwrap();
    // Closed maintenance cannot run the old reconcile/append_checked side effects.
    orch_host::reclaim::maintain_storage(&root, false).unwrap();
    assert_eq!(std::fs::read(&ledger).unwrap(), bytes);
    assert!(orch_host::reclaim::latest_maintenance_report(&root)
        .unwrap()
        .is_some());
    // The unchanged real retirement producer remains independently required.
    let close = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/close.rs"),
    )
    .unwrap();
    let record = close.split("fn run_record_locked").nth(1).unwrap();
    assert!(record
        .chars()
        .take(6000)
        .collect::<String>()
        .contains("retire_task_sites"));
    std::fs::remove_dir_all(root).unwrap();
}
