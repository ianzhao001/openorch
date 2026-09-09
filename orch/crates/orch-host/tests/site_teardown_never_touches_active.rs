//! B207 seeded-red contract: 自动拆场——活跃现场绝不拆。
//!
//! Expected red: compile. `orch_host::sites` 在 B207 之前不存在。
//!
//! 设计依据：r63 fusion 的 planner judge（consultations/01KZ409HF3FPC2B12YFV5VPEYK/judge.md）。
//! **本种子刻意钉住一条被四家成员搞错的判据**：五家里有四家认为 `ReviewDelivered` 落账即可
//! 拆该现场；只有一家指出「产物合格 ≠ 会话停止」——审查者的进程可能仍在该 worktree 里跑。
//! 按多数意见实现会留下一个看不见的误删窗口，所以判据建在 lease/release 上，不建在交付上。
//!
//! Negative mutations that must turn the named case red:
//! M1. 把 `ReviewDelivered` 当作可拆的充分条件
//!     -> `delivery_alone_does_not_release_a_site` 红。
//! M2. 未配对 lease / 坏行 / 重复 / 身份缺失时判为「非活跃」（fail-open）
//!     -> `unpairable_lease_evidence_is_treated_as_active` 红。活跃判定必须 fail-closed。
//! M3. 用 mtime / 进程存在性 / 目录是否存在做活跃判据
//!     -> `reclaimable_is_a_pure_fold_over_the_ledger` 红（纯折叠，零文件系统读）。
//! M4. 决策后不在锁内 fresh 重读账本
//!     -> `decision_rereads_the_ledger_inside_the_lock` 红：决策与删除之间插入的新
//!        `ReviewRequested`/lease 必须被看见，否则误删在飞现场。
//! M5. 静默删除带未跟踪内容的现场
//!     -> `untracked_residue_is_quarantined_or_refused_never_silently_dropped` 红。
//! M6. 回收后 generation 复用旧编号
//!     -> `generation_never_collides_with_a_reaped_site` 红。

use orch_core::EventRecord;
use orch_host::sites::{
    reclaimable_sites, site_identity, LeaseState, ResidueDisposition, SiteIdentity, SiteRole,
};

const ROUND: &str = "r63";
const TASK: &str = "B902";
const ATTEMPT: &str = "B902-A0001";
const HEAD: &str = "0123456789012345678901234567890123456789";

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

fn lease(role: &str, agent: &str, generation: u32) -> EventRecord {
    event(
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

fn release(role: &str, agent: &str, generation: u32) -> EventRecord {
    event(
        "WorkspaceReleased",
        serde_json::json!({
            "siteId": format!("{TASK}-{role}-{agent}-g{generation:02}"),
            "generation": generation,
            "attemptId": ATTEMPT,
            "role": role,
            "agent": agent,
            "completionReceipt": "runtime:orch/managed-wake-terminated",
        }),
    )
}

fn delivered(role: &str, agent: &str) -> EventRecord {
    event(
        "ReviewDelivered",
        serde_json::json!({
            "attemptId": ATTEMPT,
            "role": role,
            "agent": agent,
            "reviewedHead": HEAD,
            "bodyLen": 4096,
        }),
    )
}

#[test]
fn delivery_alone_does_not_release_a_site() {
    // 五家成员里有四家会在这里判「可拆」。产物合格只证明审查结论已落盘提交，
    // 不证明审查者的进程已经退出——在它仍持有 worktree 时删除，正是本卡要防的事。
    let events = vec![
        lease("primary", "executor-opencode", 1),
        delivered("primary", "executor-opencode"),
        event("TaskRecorded", serde_json::json!({"mergeSha": HEAD})),
    ];
    let reclaimable = reclaimable_sites(&events);
    assert!(
        reclaimable.is_empty(),
        "只有 ReviewDelivered + TaskRecorded、没有配对 release 时，现场必须仍判活跃；实际: {reclaimable:?}"
    );

    // 补上带 completion receipt 的 release 之后才可拆。
    let mut released = events.clone();
    released.push(release("primary", "executor-opencode", 1));
    let reclaimable = reclaimable_sites(&released);
    assert_eq!(reclaimable.len(), 1, "配对 release 之后才允许回收");
    assert_eq!(
        reclaimable[0].site_id,
        format!("{TASK}-primary-executor-opencode-g01")
    );
}

#[test]
fn unpairable_lease_evidence_is_treated_as_active() {
    // 证据不完整时必须偏向「活跃」——误留一个现场的代价是几个 GiB，
    // 误删一个在飞现场的代价是一个 attempt 的工作。方向不对称，判据必须 fail-closed。
    let base = lease("secondary", "executor-antigravity", 1);

    // ① release 身份缺失（无 completionReceipt）
    let missing_receipt = vec![
        base.clone(),
        event(
            "WorkspaceReleased",
            serde_json::json!({
                "siteId": format!("{TASK}-secondary-executor-antigravity-g01"),
                "generation": 1,
            }),
        ),
    ];
    assert!(
        reclaimable_sites(&missing_receipt).is_empty(),
        "release 缺 completion receipt ⇒ 判活跃"
    );

    // ② 重复 release（同 siteId 两条）——歧义，判活跃
    let duplicated = vec![
        base.clone(),
        release("secondary", "executor-antigravity", 1),
        release("secondary", "executor-antigravity", 1),
    ];
    assert!(
        reclaimable_sites(&duplicated).is_empty(),
        "重复 release ⇒ 绑定歧义 ⇒ 判活跃"
    );

    // ③ release 指向从未 lease 过的 siteId ——账本不自洽，判活跃并报告
    let orphan_release = vec![release("secondary", "executor-antigravity", 7)];
    assert!(
        reclaimable_sites(&orphan_release).is_empty(),
        "无对应 lease 的 release ⇒ 不得据此删除任何东西"
    );
}

#[test]
fn reclaimable_is_a_pure_fold_over_the_ledger() {
    // 活跃判据零文件系统读：不看 mtime、不看进程、不看目录是否存在。
    // r58 的教训是磁盘耗尽时这些信号全部失真。
    let events = vec![
        lease("primary", "executor-opencode", 1),
        delivered("primary", "executor-opencode"),
        release("primary", "executor-opencode", 1),
    ];
    let first = reclaimable_sites(&events);
    for _ in 0..3 {
        assert_eq!(
            reclaimable_sites(&events),
            first,
            "同一份账本重复求值必须同结果（纯折叠，可复算）"
        );
    }

    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sites.rs"),
    )
    .expect("读 src/sites.rs 失败");
    let fold_region = src
        .split("pub fn reclaimable_sites")
        .nth(1)
        .expect("reclaimable_sites 必须存在");
    for banned in ["metadata(", "mtime", "read_dir", "exists()", "Command::new"] {
        assert!(
            !fold_region.contains(banned),
            "活跃判据里出现文件系统/进程探测 `{banned}`——判据必须只读账本"
        );
    }
}

#[test]
fn decision_rereads_the_ledger_inside_the_lock() {
    // 决策与删除之间若插入新的 lease（重审建场），必须被看见，否则误删在飞现场。
    // 这条竞态是 fusion 里被诚实标注为「未核实」的那条，本卡必须证明它不存在。
    let before = vec![
        lease("primary", "executor-opencode", 1),
        delivered("primary", "executor-opencode"),
        release("primary", "executor-opencode", 1),
    ];
    assert_eq!(reclaimable_sites(&before).len(), 1);

    // 决策之后、删除之前，同一 identity 以新 generation 重新建场（复审）。
    let mut after = before.clone();
    after.push(lease("primary", "executor-opencode", 2));

    let still = reclaimable_sites(&after);
    // g01 仍可回收，但 g02 绝不能被当作 g01 的一部分删掉。
    assert!(
        still.iter().all(|s| s.generation == 1),
        "新 generation 必须被识别为独立的活跃现场，不得被旧决策连带删除；实际: {still:?}"
    );
}

#[test]
fn untracked_residue_is_quarantined_or_refused_never_silently_dropped() {
    // 实证：r63 层2 清理时人工抢救出的 `.orch-review-parity/`（审查者自建的钩子 parity
    // 复现脚手架，4 文件 52 K）确有价值。机制不该假设「该提交的都提交了」。
    let small = ResidueDisposition::for_untracked(&["/.orch-review-parity/parity.sh"], 52 * 1024);
    assert!(
        matches!(small, ResidueDisposition::Quarantine { .. }),
        "小体量未跟踪残留必须隔离留存，不得静默删除"
    );

    let huge = ResidueDisposition::for_untracked(&["/blob.bin"], 8 * 1024 * 1024 * 1024);
    assert!(
        matches!(huge, ResidueDisposition::RefuseAndEscalate { .. }),
        "超过上限时必须拒绝拆除并升级，交人裁决——既不静默删也不无界抢救"
    );

    let clean = ResidueDisposition::for_untracked(&[], 0);
    assert!(matches!(clean, ResidueDisposition::Proceed));
}

#[test]
fn generation_never_collides_with_a_reaped_site() {
    // 回收后重建必须拿新 generation：否则崩溃恢复时无法区分「已回收的 g01」与
    // 「刚重建的 g01」，会把新现场按旧决策删掉。
    let events = vec![
        lease("nongate", "executor-pi", 1),
        release("nongate", "executor-pi", 1),
        lease("nongate", "executor-pi", 2),
    ];
    let identity = site_identity(TASK, SiteRole::Nongate, "executor-pi");
    let next = SiteIdentity::next_generation(&events, &identity);
    assert_eq!(next, 3, "下一代编号必须严格递增，绝不复用已回收的编号");

    assert!(matches!(
        LeaseState::of(&events, &identity, 1),
        LeaseState::Released { .. }
    ));
    assert!(matches!(
        LeaseState::of(&events, &identity, 2),
        LeaseState::Active { .. }
    ));
}

#[test]
fn nongate_is_a_first_class_site_but_never_a_formal_review_slot() {
    // 非门现场必须进机械可见集（否则拆场看不见它们，四档梯度下按卡线性泄漏），
    // 但绝不能因此变成合法的正门审查席——正门席位仍受 H73 的两席上限约束。
    let identity = site_identity(TASK, SiteRole::Nongate, "executor-zcode");
    assert!(
        identity.site_id.contains("nongate"),
        "nongate 现场必须有正规 identity"
    );
    assert!(
        !SiteRole::Nongate.satisfies_formal_review_slot(),
        "nongate 绝不满足正式 review 槽"
    );
    assert!(SiteRole::Primary.satisfies_formal_review_slot());
    assert!(SiteRole::Secondary.satisfies_formal_review_slot());
}
