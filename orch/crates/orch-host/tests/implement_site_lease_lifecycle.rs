//! B238 seeded-red contract: 任务现场纳入账本租约模型（H123 修法②，用户 2026-08-06 明令）。
//!
//! Expected red: compile —— `error[E0599]`：`SiteRole::Implement` 变体在 B238 之前不存在
//! （`no variant or associated item named `Implement` found for enum `SiteRole`）。
//! **本种子刻意只引入这一个缺失符号**：任何未解析的 `use` / 函数路径都会让 rustc 停在
//! resolve 阶段（E0432/E0425）并**根本不打印 E0599**，声明的首红形态当场变脏。因此本文件
//! 不引用 B237 的 `orch_host::reclaim`（它是本卡的运行期依赖，不是编译期锚点）。
//!
//! 缺口（H123 缺口二，读码取证，行号为 r66 开轮实测）：
//! `reclaimable_sites`（`sites.rs:1925`）的候选集唯一来源是 `lease_identities`（`sites.rs:638`），
//! 而 `WorkspaceLeased` 的唯一构造点 `workspace_leased_event`（`sites.rs:744`）只被
//! `lease_review_site_with`（`sites.rs:769`）调用，生产可达调用点只有 `wake.rs:12350`
//! （在 `wake.rs:12317 fn provision_review_site` 内）——**只为审查现场产生**。
//! 任务现场 `.worktrees/<task>` 由执行者按 GO 提示词自己 `git worktree add`
//! （`attempt.rs:3419 go_worktree_instruction`，逐字 `git worktree add .worktrees/{id} -b task/{id} {sha}`），
//! orch 侧从不落租约事件 ⇒ 对 GC 而言不是「判为不可回收」，而是**根本不在候选集里**。
//! 实证（r66 开轮实测快照）：`.worktrees/B224/B226/B228` 是 r64 的卡，熬过 r64、r65 两次收轮，
//! 占 18.5G。`orch sites` 的自我描述已点破范围——`orch-cli/src/main.rs:48` 逐字
//! `/// Lease-gated review-site lifecycle operations.`
//!
//! 四条来之不易的判据（写在这里，避免下一任把它们当成可有可无的细节）：
//! ① **先租后建是合法窗口**：worktree 由执行者创建，orch 落租约时它还不存在。此刻 reap 必须
//!    零动作、零错误、零升级，且不得把 registry 不变量打伪（否则每张在飞卡每轮刷一条假警报）。
//! ② **任务现场不套用 detached-HEAD 判据**：它在 `task/<T>` 分支上、HEAD 随提交移动。沿用审查
//!    现场的 `detached && HEAD == reviewedHead`（`sites.rs:1191-1198`）在任务现场恒假 =
//!    永远 Refuse = 本卡回收面归零。去留只由四判据决定（B237 `reclaim.rs`：TaskRecorded +
//!    会话不在场 + 工作树干净 + 已并入 main），且**分支永远保留**。
//! ③ **implement 租约必须省略 wakeId**：`managed_wake_is_pending`（`sites.rs:560-573`）会让
//!    「带 wakeId 且无 authenticated terminal 证据」的租约被 `retire_task_sites` 跳过——那正好
//!    把任务现场送回「永不释放」，即本卡要杀死的形态。本文件第 3 个用例把这个暗雷现场复现。
//! ④ **回收必须按路径让路，不能按代/身份判定**：审查现场每一代的路径都带 `-gNN` 后缀
//!    （`sites.rs:146-152 expected_worktree`），各代互不相干，所以 `reap_one` 按单代判定天然安全；
//!    任务现场**所有代共用同一个 `.worktrees/<task>`**，于是「按代判定」会在重派发窗口里删掉
//!    别人正在用的在飞现场。第 9 个用例把这个新引入的误删窗口钉死。
//!
//! Negative mutations that must turn the named case red:
//! M1. `satisfies_formal_review_slot(Implement)` 返 true，或让新角色顺带拿到审查产物路径
//!     -> `implement_role_parses_and_never_satisfies_a_formal_review_slot` 红。
//! M2. 任务现场的路径契约写成 review 形状（`.worktrees/review-<attempt>-…-gNN`），
//!     或派发侧压根没落租约（`src/tierf.rs` 生产前缀里找不到角色）
//!     -> `dispatch_shaped_implement_lease_is_ledger_visible` 红。
//! M3a. `retire_task_sites` 把 implement 租约过滤掉（只退役审查现场）
//!      -> `task_recorded_retirement_releases_the_implement_lease` **前半段**红。
//! M3b. 删掉 / 弱化 `retire_task_sites` 的 `managed_wake_is_pending` 跳过判据
//!      （`sites.rs:610-616`）-> 同一用例**后半段**红。该判据正是「implement 租约必须无
//!      wakeId」的成因；它一旦消失，卡面否决项就失去了立足点。
//! M4. reap 在 worktree 尚未创建时报错 / 误删他物 / 刷升级 / 把 registry 不变量打伪
//!     -> `lease_before_worktree_is_legal_and_reap_touches_nothing` 红。
//! M5. 回收顺手删 `task/<T>` 分支，或动到主仓共享 `orch/target`
//!     -> `released_implement_site_reaps_worktree_and_preserves_branch` 红。
//! M6. implement 沿用 detached-HEAD / reviewedHead 判据（干净现场被判 Refuse）
//!     -> `implement_reap_requires_the_four_criteria_not_detached_head` 红。
//! M7. 给 `WorkspaceLeased` 增加任何**必填**字段（E6：既有冻结种子逐字手写该 payload）
//!     -> `handwritten_review_and_nongate_lease_shapes_still_parse` 红。该用例把两种既有手写
//!     形状放进**混入了 implement 租约**的同一份账本里折叠，所以它同时也是「新角色进场后
//!     既有折叠结果逐字不变」的探针，并且**引用了待实现符号**（`implement_identity`），
//!     不是一条只测既有行为的纯回归位。
//! M8. implement 与审查现场共用 generation 计数，或回收后复用旧编号
//!     -> `implement_identity_never_collides_with_review_generations` 红。
//! M9. `reap_one` 只看当前这一代的 LeaseState，不检查同一 worktree 路径上是否还有别的
//!     Active 代（或把判据写成按 identity/generation 比对而不是按路径比对）
//!     -> `an_active_lease_on_the_same_worktree_blocks_the_reap` 红。

use std::path::{Path, PathBuf};
use std::process::Command;

use orch_core::EventRecord;
use orch_host::ledger;
use orch_host::sites::{
    reap_released_sites, reclaimable_sites, retire_task_sites, site_identity, LeaseState, Site,
    SiteIdentity, SiteRole, SITE_RETIRED_EVENT_KIND,
};
use orch_host::util::test_scratch_dir;

const ROUND: &str = "rT";
const TASK: &str = "B907";
const ATTEMPT: &str = "B907-A0001";
const AGENT: &str = "executor-desktop";
/// 重派发（`tierf.rs:3820-3841`）换到的另一个执行席：换 agent = 换 identity = 新租约，
/// 但**指向同一个 `.worktrees/<task>`**。第 9 个用例靠它证明让路判据是按路径比对的。
const RETRY_AGENT: &str = "executor-antigravity";
const REVIEW_AGENT: &str = "executor-opencode";
const HEAD: &str = "0123456789012345678901234567890123456789";

fn event_with_id(event_id: &str, kind: &str, payload: serde_json::Value) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": event_id,
        "ts": "2026-08-06T00:00:00Z",
        "actor": "runtime:orch",
        "type": kind,
        "round": ROUND,
        "taskId": TASK,
        "payload": payload,
    }))
    .expect("构造事件失败")
}

fn payload_str<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event.payload.as_ref()?.get(key)?.as_str()
}

/// 任务现场租约的形状。**逐项对着实测写，不按「规范应该长什么样」写**：
/// - `paths.worktree` = `.worktrees/<task>`：GO 提示词逐字要求的位置（`attempt.rs:3427`），
///   **与 agent / generation 无关**——这正是它与审查现场的结构差异（见头注④）；
/// - `paths.target`   = `.worktrees/<task>/orch/target`：任务现场的 cargo 产物真实落点
///   （门在现场内跑，workspace 根是 `<worktree>/orch`），H123 里 48.8G 的绝大部分就在这儿；
/// - **无 `wakeId`**（见头注③），**不加任何新字段**（E6/M7）。
fn implement_lease_for(agent: &str, generation: u32, reviewed_head: &str) -> EventRecord {
    event_with_id(
        &format!("EV-LEASE-IMPL-{agent}-{generation}"),
        "WorkspaceLeased",
        serde_json::json!({
            "siteId": format!("{TASK}-implement-{agent}-g{generation:02}"),
            "generation": generation,
            "attemptId": ATTEMPT,
            "role": "implement",
            "agent": agent,
            "reviewedHead": reviewed_head,
            "paths": {
                "worktree": format!(".worktrees/{TASK}"),
                "target": format!(".worktrees/{TASK}/orch/target"),
            },
        }),
    )
}

fn implement_lease(generation: u32, reviewed_head: &str) -> EventRecord {
    implement_lease_for(AGENT, generation, reviewed_head)
}

fn implement_identity() -> SiteIdentity {
    site_identity(TASK, SiteRole::Implement, AGENT)
}

fn active_site(events: &[EventRecord], identity: &SiteIdentity, generation: u32) -> Site {
    match LeaseState::of(events, identity, generation) {
        LeaseState::Active {
            site: Some(site), ..
        } => site,
        other => panic!("期望 Active 且带可解析 site，实得 {other:?}"),
    }
}

/// 剥注释后再做接线扫描：否则一句注释就能把「派发侧真的落租约」喂饱
/// （范式取自 `tests/ledger_capability_gate.rs` 的同类源码扫描）。
///
/// 规则刻意保守，**只删不可能是代码的字节**，永远不会误删真代码：
/// - `trim_start()` 以 `//` 开头的整行丢弃；
/// - 行尾 `//` 只在它**之前不含双引号**时截断（含双引号的行原样保留，免得把
///   `"https://…"` 这类字符串腰斩）。
/// 块注释不处理：它在本仓生产代码里近乎绝迹，且漏剥只会让断言更宽松，不会误判红。
fn strip_comments(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(|line| match line.find("//") {
            Some(cut) if !line[..cut].contains('"') => &line[..cut],
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=b238", "-c", "user.email=b238@test.invalid"])
        .args(args)
        .output()
        .expect("git 调用失败");
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// 主仓夹具：真实 git 仓 + 账本/WAL 落点 + 共享 `orch/target` 哨兵。
/// 返回 `(root, base_sha)`；`base_sha` 是派发时的 main tip，也是 implement 租约的 reviewedHead。
fn repo(tag: &str) -> (PathBuf, String) {
    let root = test_scratch_dir(tag);
    git(&root, &["init", "-b", "main"]);
    // 真实仓里 orch/target 是 gitignored；夹具必须复刻，否则现场内的产物目录会被
    // 当成 untracked residue，判据形状与生产不一致。
    std::fs::write(root.join(".gitignore"), "orch/target/\n").expect("写 .gitignore 失败");
    std::fs::write(root.join("tracked.txt"), "baseline\n").expect("写 tracked.txt 失败");
    git(&root, &["add", ".gitignore", "tracked.txt"]);
    git(&root, &["commit", "-m", "baseline"]);
    for dir in [
        format!("coordination/rounds/{ROUND}"),
        "coordination/runtime/ledger-wal".to_string(),
        "orch/target".to_string(),
        ".worktrees".to_string(),
    ] {
        std::fs::create_dir_all(root.join(dir)).expect("建夹具目录失败");
    }
    std::fs::write(
        root.join(format!("coordination/rounds/{ROUND}/events.jsonl")),
        "",
    )
    .expect("建账本失败");
    std::fs::write(
        root.join(format!("coordination/runtime/ledger-wal/{ROUND}.jsonl")),
        "",
    )
    .expect("建 WAL 失败");
    // 主仓共享产物根：任务现场回收的误删半径哨兵，任何时候都不该被动到。
    std::fs::write(root.join("orch/target/shared-sentinel.txt"), "keep\n").expect("写哨兵失败");
    let base = git(&root, &["rev-parse", "HEAD"]);
    (root, base)
}

/// 与 GO 提示词逐字同形（`attempt.rs:3427`）：现场挂在 `task/<T>` 分支上，不是 detached。
fn add_task_worktree(root: &Path, base_sha: &str) -> PathBuf {
    git(
        root,
        &[
            "worktree",
            "add",
            &format!(".worktrees/{TASK}"),
            "-b",
            &format!("task/{TASK}"),
            base_sha,
        ],
    );
    root.join(format!(".worktrees/{TASK}"))
}

/// 执行者在现场提交 + 产物落盘，随后主仓 no-ff 并入 main（四判据里「已并入 main」成立）。
fn work_and_merge(root: &Path, worktree: &Path) {
    std::fs::write(worktree.join("tracked.txt"), "task work\n").expect("写现场文件失败");
    git(worktree, &["add", "tracked.txt"]);
    git(worktree, &["commit", "-m", "B907 work"]);
    std::fs::create_dir_all(worktree.join("orch/target")).expect("建现场 target 失败");
    std::fs::write(worktree.join("orch/target/blob.bin"), "x").expect("写现场产物失败");
    git(
        root,
        &[
            "merge",
            "--no-ff",
            &format!("task/{TASK}"),
            "-m",
            "merge B907",
        ],
    );
}

fn append_lease(root: &Path, reviewed_head: &str) -> EventRecord {
    let lease = implement_lease(1, reviewed_head);
    ledger::append(root, ROUND, std::slice::from_ref(&lease)).expect("落 implement 租约失败");
    lease
}

/// TaskRecorded + 由**生产函数** `retire_task_sites` 现铸的 SiteRetired，与 `close.rs:625`
/// 的批次同形（不手写退役事件：手写会让「生产者是否覆盖任务现场」这条契约悄悄失守）。
fn append_task_recorded_retirement(root: &Path, lease: &EventRecord) {
    let recorded = event_with_id(
        "EV-RECORDED-1",
        "TaskRecorded",
        serde_json::json!({"attemptId": ATTEMPT}),
    );
    let retirements = retire_task_sites(std::slice::from_ref(lease), TASK, &recorded.event_id);
    assert_eq!(
        retirements.len(),
        1,
        "TaskRecorded 必须为任务现场铸出 SiteRetired，实得 {retirements:?}"
    );
    let mut batch = vec![recorded];
    batch.extend(retirements);
    ledger::append(root, ROUND, &batch).expect("落 TaskRecorded + SiteRetired 失败");
}

fn ledger_events(root: &Path) -> Vec<EventRecord> {
    let read =
        orch_core::read_ledger(&root.join(format!("coordination/rounds/{ROUND}/events.jsonl")))
            .expect("读账本失败");
    assert!(
        read.bad_lines.is_empty(),
        "夹具账本不得有坏行: {:?}",
        read.bad_lines
    );
    read.events
}

#[test]
fn implement_role_parses_and_never_satisfies_a_formal_review_slot() {
    // M1：任务现场是一等公民（否则 GC 看不见它，H123 缺口二原样保留），
    // 但它**不是审查席位**——正门两席上限（H73）不因新角色被稀释。
    let role = SiteRole::parse("implement").expect("implement 必须是合法的现场角色");
    assert_eq!(role, SiteRole::Implement);
    assert_eq!(
        role.as_str(),
        "implement",
        "as_str 必须与账本里的字面值一致：fold 靠 parse/as_str 回环认身份"
    );
    assert_eq!(
        SiteRole::parse(role.as_str()),
        Ok(SiteRole::Implement),
        "parse/as_str 必须闭环"
    );
    assert!(
        !role.satisfies_formal_review_slot(),
        "任务现场绝不满足正式审查席位"
    );
    assert!(SiteRole::Primary.satisfies_formal_review_slot());
    assert!(SiteRole::Secondary.satisfies_formal_review_slot());
    assert!(!SiteRole::Nongate.satisfies_formal_review_slot());

    // 反拓宽：`SiteRole::parse` 是 9 条审查校验链的入口（`wake.rs:11957/12135/12346/12932/
    // 12942/13076/14700/14749/14884`，卡面 §1 逐条核到行）。新角色可解析，绝不等于它能拿到
    // 审查产物路径——否则「加一个变体」会静默拓宽一整族与审查无关的入口，这正是本项目反复
    // 吃过的亏。本用例钉住产物路径面这一条；其余 8 条由卡面 §4 点名逐条给结论。
    assert!(
        orch_host::wake::review_inbox_relpath(ROUND, ATTEMPT, "implement", AGENT).is_err(),
        "implement 不是审查角色，绝不许为它铸出 review-inbox 路径"
    );
    for legal in ["primary", "secondary", "nongate"] {
        assert!(
            orch_host::wake::review_inbox_relpath(ROUND, ATTEMPT, legal, REVIEW_AGENT).is_ok(),
            "既有审查角色 {legal} 的产物路径必须逐字不变"
        );
    }
}

#[test]
fn dispatch_shaped_implement_lease_is_ledger_visible() {
    // M2：dispatch 形状的租约必须能被 fold 认出来，并且路径契约随角色分叉。
    let events = vec![implement_lease(1, HEAD)];
    let site = active_site(&events, &implement_identity(), 1);
    assert_eq!(site.role, SiteRole::Implement);
    assert_eq!(site.site_id, format!("{TASK}-implement-{AGENT}-g01"));
    assert_eq!(site.task_id, TASK);
    assert_eq!(site.attempt_id, ATTEMPT);
    assert_eq!(site.worktree, format!(".worktrees/{TASK}"));
    assert!(
        site.wake_id.is_none(),
        "implement 租约必须省略 wakeId：带 wakeId 会被 managed-pending 判据挡在退役之外（见头注③）"
    );

    // 路径契约必须随角色分叉：`reap_one` 的第一道闸（`sites.rs:1170-1174`）拿它与账本
    // paths 对账。继续返回审查形状 ⇒ 任务现场恒 Refused ⇒ 本卡回收面归零。
    assert_eq!(
        site.expected_worktree(),
        format!(".worktrees/{TASK}"),
        "任务现场的 canonical worktree 是 .worktrees/<task>，不是 review-<attempt>-…-gNN"
    );
    assert_eq!(
        site.expected_target(),
        format!(".worktrees/{TASK}/orch/target"),
        "任务现场的产物目录在现场内部（门的 workspace 根是 <worktree>/orch）"
    );
    assert_eq!(site.target, site.expected_target());

    // 只落租约不构成可回收：释放仍然只能来自显式的终点事实（B224 契约）。
    assert!(reclaimable_sites(&events).is_empty());

    // 「谁调它」：能力交付了没人调，是本项目的头号复发缺陷（H111/r63 教训）。
    // 派发侧必须真的落这条租约，且**在派发处显式命名角色**（角色是账本事实的一部分，
    // 读码者要在派发处一眼看见它，不许藏进 helper 的默认参数里）。
    // 扫描面刻意收窄到 `#[cfg(test)]` 之前的**生产前缀**：加一个提到该角色的单元测试
    // 不构成接线。
    let tierf = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tierf.rs"),
    )
    .expect("读 src/tierf.rs 失败");
    let code = strip_comments(&tierf);
    let production = code
        .split("#[cfg(test)]")
        .next()
        .expect("split 至少产出一段");
    assert!(
        production.contains("SiteRole::Implement"),
        "dispatch 流程（src/tierf.rs 的生产代码）必须在 GuardEntry::Dispatch 许可作用域内显式落 \
         WorkspaceLeased{{role:implement}}，否则任务现场依旧不在 GC 的世界模型里"
    );
}

#[test]
fn task_recorded_retirement_releases_the_implement_lease() {
    // M3a：Released 侧靠既有生产者——`close.rs:622-631 task_recorded_batch` 在 TaskRecorded
    // 同批调用 `retire_task_sites`，它必须覆盖任务现场，而不是只认审查现场。
    let lease = implement_lease(1, HEAD);
    let recorded = event_with_id(
        "EV-RECORDED-1",
        "TaskRecorded",
        serde_json::json!({"attemptId": ATTEMPT}),
    );
    let retirements = retire_task_sites(std::slice::from_ref(&lease), TASK, &recorded.event_id);
    assert_eq!(
        retirements.len(),
        1,
        "TaskRecorded 必须为任务现场铸出恰一条 SiteRetired，实得 {retirements:?}"
    );
    assert_eq!(retirements[0].kind, SITE_RETIRED_EVENT_KIND);
    assert_eq!(
        payload_str(&retirements[0], "role"),
        Some("implement"),
        "退役事实必须逐字保留角色，否则跨代绑定会漂"
    );
    assert_eq!(
        payload_str(&retirements[0], "trigger"),
        Some("task-recorded")
    );

    let mut events = vec![lease, recorded];
    events.extend(retirements);
    match LeaseState::of(&events, &implement_identity(), 1) {
        LeaseState::Released {
            site,
            completion_receipt,
        } => {
            assert_eq!(site.worktree, format!(".worktrees/{TASK}"));
            assert!(
                completion_receipt.starts_with("SiteRetired:"),
                "释放凭据必须指名那条退役事实，实得 {completion_receipt}"
            );
        }
        other => panic!("TaskRecorded 退役后任务现场必须判 Released，实得 {other:?}"),
    }

    // M3b · 暗雷现场复现（纠偏 C）：只要 implement 租约带上 wakeId，且该 wake 没有
    // authenticated terminal 证据，`retire_task_sites`（`sites.rs:610-616`）就会跳过它——
    // 任务现场回到「永不释放」，恰是 H123 要杀死的形态。所以派发侧必须省略 wakeId，
    // 而这条跳过判据本身也必须原样保留（删掉它，本条断言立刻红）。
    let mut with_wake = implement_lease(1, HEAD);
    with_wake.payload.as_mut().expect("lease 必须有 payload")["wakeId"] =
        serde_json::json!("WAKE-IMPL-1");
    let pending = event_with_id(
        "EV-WAKE-1",
        "WakeIssued",
        serde_json::json!({
            "wakeId": "WAKE-IMPL-1",
            "agent": AGENT,
            "backendState": "pending",
            "controlWakeId": "WAKE-IMPL-1",
        }),
    );
    assert!(
        retire_task_sites(&[with_wake, pending], TASK, "EV-RECORDED-1").is_empty(),
        "带 wakeId 的 managed-pending 现场会被退役生产者跳过——这就是 implement 租约必须无 wakeId 的原因"
    );
}

#[test]
fn lease_before_worktree_is_legal_and_reap_touches_nothing() {
    // M4 · 实弹：租约先落账，worktree 由执行者稍后自己 `git worktree add`。
    // 这个窗口每张卡都会经历，它必须是**完全惰性**的：零删除、零错误、零升级事实。
    let (root, base) = repo("b238-lease-before-worktree");
    append_lease(&root, &base);
    let registry_before = git(&root, &["worktree", "list", "--porcelain"]);

    let outcome = reap_released_sites(&root, ROUND).expect("先租后建窗口内 reap 必须零错返回");
    assert!(outcome.removed.is_empty(), "无可回收现场：{outcome:?}");
    assert!(
        outcome.refused.is_empty(),
        "尚未创建的现场不构成拒收：{outcome:?}"
    );
    assert!(outcome.quarantined.is_empty());
    assert!(outcome.target_failures.is_empty());
    assert_eq!(
        git(&root, &["worktree", "list", "--porcelain"]),
        registry_before,
        "worktree 注册表不得被动过"
    );
    assert!(
        root.join("orch/target/shared-sentinel.txt").exists(),
        "主仓共享 orch/target 不在任务现场的回收半径内"
    );
    assert!(root.join("tracked.txt").exists());
    assert!(!root.join(format!(".worktrees/{TASK}")).exists());

    let invariant = outcome
        .registry_invariant
        .expect("reap 必须给出 registry 不变量");
    assert!(
        invariant.missing.is_empty(),
        "先租后建是合法窗口，不得报成账本/注册表失配——否则每张在飞卡每轮刷一条假警报，\
         真警报会被淹没；实得 {invariant:?}"
    );

    let events = ledger_events(&root);
    assert!(
        matches!(
            LeaseState::of(&events, &implement_identity(), 1),
            LeaseState::Active { .. }
        ),
        "现场未创建不等于租约作废：租约必须保持 Active"
    );
    assert!(reclaimable_sites(&events).is_empty());
    assert!(
        !events.iter().any(|event| event.kind == "EscalationRaised"),
        "先租后建不是异常，不得刷升级事件：{events:?}"
    );
    std::fs::remove_dir_all(&root).expect("清理夹具失败");
}

#[test]
fn released_implement_site_reaps_worktree_and_preserves_branch() {
    // M5 · 实弹：已 Recorded、已并入 main、干净的任务现场必须真的被删掉
    //（H123 里 48.8G 的直接来源），但 `task/<T>` 分支一根汗毛都不许动（E2）。
    let (root, base) = repo("b238-released-implement-site");
    let worktree = add_task_worktree(&root, &base);
    work_and_merge(&root, &worktree);
    let lease = append_lease(&root, &base);
    append_task_recorded_retirement(&root, &lease);

    let events = ledger_events(&root);
    assert_eq!(
        reclaimable_sites(&events).len(),
        1,
        "TaskRecorded 退役后任务现场必须进入回收候选集：{events:?}"
    );

    let outcome = reap_released_sites(&root, ROUND).expect("回收任务现场失败");
    assert_eq!(
        outcome.removed,
        vec![format!("{TASK}-implement-{AGENT}-g01")],
        "干净且已并入 main 的任务现场必须被回收；refused={:?} targetFailures={:?}",
        outcome.refused,
        outcome.target_failures
    );
    assert!(!worktree.exists(), "现场目录必须真的消失");
    assert!(
        orch_host::gitx::branch_exists(&root, &format!("task/{TASK}")),
        "回收绝不许删 task/<T> 分支：分支是可复算的历史，现场只是它的一次物化"
    );
    assert!(
        root.join("orch/target/shared-sentinel.txt").exists(),
        "主仓共享 orch/target 不在任务现场的回收半径内"
    );
    assert!(root.join("tracked.txt").exists(), "主仓 checkout 不得被动");
    assert!(
        outcome
            .registry_invariant
            .expect("reap 必须给出 registry 不变量")
            .holds,
        "回收后 worktree 注册表与租约事实必须重新对齐"
    );
    std::fs::remove_dir_all(&root).expect("清理夹具失败");
}

#[test]
fn implement_reap_requires_the_four_criteria_not_detached_head() {
    // M6 · 实弹：同一个现场、同一个 HEAD 形态，只切换**一条判据**的真假，
    // 去留就必须跟着翻转——这机械地证明「去留由四判据决定」，而不是由 HEAD 形态决定。
    let (root, base) = repo("b238-four-criteria");
    let worktree = add_task_worktree(&root, &base);
    work_and_merge(&root, &worktree);
    let lease = append_lease(&root, &base);
    append_task_recorded_retirement(&root, &lease);

    // 夹具前提：现场挂在分支上（非 detached）且 HEAD 已随提交移动。审查现场的判据
    //（`sites.rs:1191-1198` detached && HEAD == reviewedHead）在这里**恒假**。
    assert_eq!(
        git(&worktree, &["symbolic-ref", "--short", "HEAD"]),
        format!("task/{TASK}"),
        "任务现场必须在 task/<T> 分支上，这正是它与审查现场的结构差异"
    );
    assert_ne!(
        git(&worktree, &["rev-parse", "HEAD"]),
        base,
        "任务现场的 HEAD 必然已随提交移动，reviewedHead 只是派发时的基线"
    );

    // ① 「工作树干净」为假 ⇒ Refused，且一个字节都不许删、不许替人清理。
    std::fs::write(worktree.join("tracked.txt"), "uncommitted\n").expect("弄脏现场失败");
    let refused = reap_released_sites(&root, ROUND).expect("拒收路径必须正常返回而不是报错");
    assert!(
        refused.removed.is_empty(),
        "带未提交修改的现场绝不许删：{refused:?}"
    );
    assert_eq!(
        refused.refused.len(),
        1,
        "拒收必须留一条可读记录，而不是静默跳过：{refused:?}"
    );
    assert!(worktree.exists());
    assert_eq!(
        std::fs::read(worktree.join("tracked.txt")).expect("读现场文件失败"),
        b"uncommitted\n",
        "拒收路径不得替人清理现场"
    );
    assert!(orch_host::gitx::branch_exists(
        &root,
        &format!("task/{TASK}")
    ));

    // ② 只把那一条判据恢复为真（HEAD 形态、分支、reviewedHead 全都没变）⇒ 允许回收。
    git(&worktree, &["checkout", "--", "tracked.txt"]);
    let reaped = reap_released_sites(&root, ROUND).expect("回收任务现场失败");
    assert_eq!(
        reaped.removed,
        vec![format!("{TASK}-implement-{AGENT}-g01")],
        "四判据全真后必须回收；沿用 detached-HEAD 判据会让这里恒为 Refused：{reaped:?}"
    );
    assert!(!worktree.exists());
    assert!(orch_host::gitx::branch_exists(
        &root,
        &format!("task/{TASK}")
    ));
    std::fs::remove_dir_all(&root).expect("清理夹具失败");
}

#[test]
fn handwritten_review_and_nongate_lease_shapes_still_parse() {
    // M7 · 反拓宽：以下两种 payload 是既有冻结种子里**逐字**手写的形状
    //（`site_teardown_never_touches_active.rs:48-79`、`nongate_lease_lifecycle.rs:64-81`）。
    // 给 `WorkspaceLeased` 增加任何必填字段，都会让它们从「可解析」掉成「证据不全」，
    // 于是 fold 对全部历史账本 fail-closed——历史事件不会被重写，永远不会长出新字段。
    //
    // 两种形状都放进**混入了 implement 租约**的同一份账本里折叠：新角色进场之后，既有形状的
    // 折叠结果必须逐字不变，而新角色自己也不许挤进回收候选集。
    let review_lease = event_with_id(
        "EV-LEASE-REVIEW",
        "WorkspaceLeased",
        serde_json::json!({
            "siteId": format!("{TASK}-primary-{REVIEW_AGENT}-g01"),
            "generation": 1,
            "attemptId": ATTEMPT,
            "role": "primary",
            "agent": REVIEW_AGENT,
            "reviewedHead": HEAD,
            "paths": {
                "worktree": format!(".worktrees/{TASK}-primary-{REVIEW_AGENT}-g01"),
                "target": format!("orch/target/review-{ATTEMPT}-primary-{REVIEW_AGENT}-g01"),
            },
        }),
    );
    let review_release = event_with_id(
        "EV-RELEASE-REVIEW",
        "WorkspaceReleased",
        serde_json::json!({
            "siteId": format!("{TASK}-primary-{REVIEW_AGENT}-g01"),
            "generation": 1,
            "attemptId": ATTEMPT,
            "role": "primary",
            "agent": REVIEW_AGENT,
            "completionReceipt": "runtime:orch/managed-wake-terminated",
        }),
    );
    let mixed = vec![review_lease, review_release, implement_lease(1, HEAD)];
    let reclaimable = reclaimable_sites(&mixed);
    assert_eq!(
        reclaimable.len(),
        1,
        "既有 review 形状必须逐字节仍可折叠，且新角色不得挤进候选集：{reclaimable:?}"
    );
    assert_eq!(
        reclaimable[0].site_id,
        format!("{TASK}-primary-{REVIEW_AGENT}-g01")
    );
    assert_eq!(reclaimable[0].role, SiteRole::Primary);
    assert!(
        matches!(
            LeaseState::of(&mixed, &implement_identity(), 1),
            LeaseState::Active { .. }
        ),
        "混入的 implement 租约本身仍是 Active：只落租约不构成可回收"
    );

    let nongate_lease = event_with_id(
        "EV-LEASE-NONGATE",
        "WorkspaceLeased",
        serde_json::json!({
            "siteId": format!("{TASK}-nongate-executor-pi-g01"),
            "generation": 1,
            "attemptId": ATTEMPT,
            "role": "nongate",
            "agent": "executor-pi",
            "reviewedHead": HEAD,
            "paths": {
                "worktree": format!(".worktrees/{TASK}-nongate-executor-pi-g01"),
                "target": format!("orch/target/review-{ATTEMPT}-nongate-executor-pi-g01"),
            },
        }),
    );
    let nongate_batch = vec![nongate_lease, implement_lease(2, HEAD)];
    match LeaseState::of(
        &nongate_batch,
        &site_identity(TASK, SiteRole::Nongate, "executor-pi"),
        1,
    ) {
        LeaseState::Active {
            site: Some(site), ..
        } => {
            assert_eq!(site.role, SiteRole::Nongate);
            assert_eq!(
                site.worktree,
                format!(".worktrees/{TASK}-nongate-executor-pi-g01")
            );
            assert!(
                site.wake_id.is_none(),
                "该形状本来就没有 wakeId，必须仍然合法"
            );
        }
        other => panic!(
            "非门手写形状必须仍能解析出 site：新增必填字段会让它掉成 site:None，实得 {other:?}"
        ),
    }
}

#[test]
fn implement_identity_never_collides_with_review_generations() {
    // M8：同一个 task、同一个 agent 也可能既有任务现场又有审查现场。两条身份必须各自
    // 独立计数；复用编号会让崩溃重放分不清「已回收的 g01」和「刚建的 g01」。
    let implement = implement_identity();
    let review = site_identity(TASK, SiteRole::Primary, AGENT);
    assert_eq!(implement.site_id, format!("{TASK}-implement-{AGENT}"));
    assert_ne!(
        implement.site_id, review.site_id,
        "任务现场与审查现场必须是两条身份，不得共用一个 site_id 前缀"
    );

    let implement_release = event_with_id(
        "EV-RELEASE-IMPL-1",
        "WorkspaceReleased",
        serde_json::json!({
            "siteId": format!("{TASK}-implement-{AGENT}-g01"),
            "generation": 1,
            "attemptId": ATTEMPT,
            "role": "implement",
            "agent": AGENT,
            "completionReceipt": "runtime:orch/managed-wake-terminated",
        }),
    );
    let review_lease = event_with_id(
        "EV-LEASE-REVIEW-1",
        "WorkspaceLeased",
        serde_json::json!({
            "siteId": format!("{TASK}-primary-{AGENT}-g01"),
            "generation": 1,
            "attemptId": ATTEMPT,
            "role": "primary",
            "agent": AGENT,
            "reviewedHead": HEAD,
            "paths": {
                "worktree": format!(".worktrees/{TASK}-primary-{AGENT}-g01"),
                "target": format!("orch/target/review-{ATTEMPT}-primary-{AGENT}-g01"),
            },
        }),
    );
    let events = vec![
        implement_lease(1, HEAD),
        implement_release,
        implement_lease(2, HEAD),
        review_lease,
    ];

    assert_eq!(
        SiteIdentity::next_generation(&events, &implement),
        3,
        "已回收的编号绝不复用"
    );
    assert_eq!(
        SiteIdentity::next_generation(&events, &review),
        2,
        "审查现场的 generation 空间不得被任务现场推高"
    );

    let reclaimable = reclaimable_sites(&events);
    assert_eq!(
        reclaimable.len(),
        1,
        "只有已释放的那一代可回收，其余（含同路径的新一代）必须留场：{reclaimable:?}"
    );
    assert_eq!(
        reclaimable[0].site_id,
        format!("{TASK}-implement-{AGENT}-g01")
    );
}

#[test]
fn an_active_lease_on_the_same_worktree_blocks_the_reap() {
    // M9 · 实弹：本卡自己引入的新误删窗口。审查现场每一代的路径都带 `-gNN` 后缀
    //（`sites.rs:146-152 expected_worktree`），各代路径互不相同，所以 `reap_one`
    //（`sites.rs:1169-1250`）只看「当前这一代的 LeaseState」是安全的；任务现场**所有代
    // 共用同一个 `.worktrees/<task>`**，按代判定就会删掉别人正在用的在飞现场。
    //
    // 这里挡路的那一代属于**另一个 agent**（重派发，`tierf.rs:3820-3841`）：identity 与 siteId
    // 都不同，generation 反而同为 1，**唯一相同的是 worktree 路径**。所以让路判据只能按路径
    // 比对；任何按 identity / generation 比对的实现在这一条上必然红。
    let (root, base) = repo("b238-same-path-active-lease");
    let worktree = add_task_worktree(&root, &base);
    work_and_merge(&root, &worktree);
    let lease = append_lease(&root, &base);
    append_task_recorded_retirement(&root, &lease);

    let reassigned = implement_lease_for(RETRY_AGENT, 1, &base);
    ledger::append(&root, ROUND, std::slice::from_ref(&reassigned)).expect("落重派发租约失败");

    let events = ledger_events(&root);
    assert_eq!(
        reclaimable_sites(&events).len(),
        1,
        "被退役的那一代仍然进候选集——缺陷正是「进了候选集就照删」：{events:?}"
    );

    let outcome = reap_released_sites(&root, ROUND).expect("同路径冲突必须是拒收而不是报错");
    assert!(
        outcome.removed.is_empty(),
        "同一路径上还有 Active 租约时，一个字节都不许删：{outcome:?}"
    );
    assert_eq!(
        outcome.refused.len(),
        1,
        "让路必须留一条可读记录，而不是静默跳过：{outcome:?}"
    );
    assert!(
        outcome.refused[0].contains(&format!("{TASK}-implement-{RETRY_AGENT}-g01")),
        "拒收理由必须指名是哪条 Active 租约挡住的，实得 {:?}",
        outcome.refused
    );
    assert!(outcome.target_failures.is_empty());
    assert!(worktree.exists(), "在飞任务现场必须完好无损");
    assert_eq!(
        std::fs::read(worktree.join("tracked.txt")).expect("读现场文件失败"),
        b"task work\n",
        "让路路径不得替人清理现场"
    );
    assert!(orch_host::gitx::branch_exists(
        &root,
        &format!("task/{TASK}")
    ));

    let after = ledger_events(&root);
    let retry_identity = site_identity(TASK, SiteRole::Implement, RETRY_AGENT);
    assert!(
        matches!(
            LeaseState::of(&after, &retry_identity, 1),
            LeaseState::Active { .. }
        ),
        "让路不得改变在飞租约的状态"
    );
    std::fs::remove_dir_all(&root).expect("清理夹具失败");
}
