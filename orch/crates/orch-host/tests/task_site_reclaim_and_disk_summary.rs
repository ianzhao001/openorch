//! ═══ 冻结契约 · B237（收轮磁盘回收执行者 + sweep-targets + 三元组打印）═══
//! 落位: orch/crates/orch-host/tests/task_site_reclaim_and_disk_summary.rs（逐字节复制，落位后冻结）
//!
//! 预期红（redForm: compile）：`orch_host::reclaim` 模块在 B237 之前不存在
//! → error[E0432]: unresolved import `orch_host::reclaim`。
//! 同族第二处编译红：`orch_host::buildcache::{sweep_targets, sweep_targets_for_round,
//! SweepTargetsReport}` 亦不存在。
//! 注意：`check` 门是 `cargo check --workspace`（不带 `--all-targets`，不编译 tests/），
//! 所以首红只会由 `testFast` 产生，`check` 在种子落位后仍是绿的。
//!
//! 由来（r64/r65 两轮收轮实测，不是推测）：
//! ① r64 人工回收 138G、r65 人工回收 63G——机械机制连续两轮没接住。r65 跑完全部机械回收
//!    后仍占 87G，其中 66.8G 结构上无人治理。
//! ② `close.rs` 的 `cleanup_disposition(agent_is_registered)`：注册过的执行者一律走
//!    `CleanupDisposition::DeferredTierF`，而我们所有执行者都在 `coordination/agents.yaml` 里
//!    ⇒ 每张卡必然延后；延后分支只有一条 println!，穷尽搜索确认 `DeferredTierF` 无任何生产消费者，
//!    `.worktrees/<task>` 的删除路径全仓仅 `close.rs` 的 best-effort 分支一处，对已注册执行者不可达。
//!    **延后本身刻意且合理（会话可能仍持有现场）——缺的是延后之后的那个执行者。**
//! ③ `sites::retire_task_sites` 只处置 `WorkspaceLeased` 建出来的审查/非门现场，
//!    执行者自己的 `.worktrees/<task>` 从来不是 lease 现场，因此 `sites gc` 天生看不见它。
//! ④ 跨轮事实：r64 三张卡（B224/B226/B228）的 `TaskRecorded` 落在 **r64** 账本里，
//!    其现场却熬过了 r65 收轮。**判据一只查当前轮 = 交付「正确但无用」**，主案例颗粒无收。
//!
//! 本种子刻意复刻的四处**真实形态**（铁律二：夹具按实测长，不按「规范应该长什么样」长）：
//! ⓐ `.worktrees/` 下的主导形态是 **detached** 登记现场（`sites.rs:1545` / `sites.rs:1806` 的
//!    `gitx::worktree_add_detached`），而 `gitx::current_branch` 对 detached 返回的是 **Err**
//!    不是 `Ok(None)`——`gitx.rs:534-537` 走 `run_str`→`run`，`run` 在非零退出时 `bail!`
//!    （`gitx.rs:22-28`），而 `git symbolic-ref --quiet --short HEAD` 在 detached 时静默退出 1。
//!    ⇒ 用 `?` 上抛就会让「主仓里只要有一个审查现场，整个回收就返 Err」，被收轮 `⚠️` 吞掉。
//!    本夹具**每一个** `task_site_fixture` 都带一个 detached 现场，任何 `?` 写法必然全红。
//! ⓑ 心跳有**两种**带任务绑定的真实形状，二者只在 `generation` 前缀与 `pid` 上一致，
//!    在 `phase` 与 `ordinal` 上**互相矛盾**——判据二只许看前两者：
//!      · wait-script 派发形（`scaffold.rs:73` 的 printf 模板）：`phase` 被**硬编码成
//!        `"waiting"`**、带 `ordinal`、`pid` 是 wait-script 自己的 `$$`；
//!      · runtime 接管形（`wake::working_heartbeat_json`，`wake.rs:6621-6634`，由
//!        `write_working_heartbeat` 落盘）：`phase == "working"`、**根本没有 `ordinal` 字段**、
//!        `pid` 是 provider spawn pid。
//!    `liveness.rs:133-136` 已经因为这个不一致把 waiting/working 分流处理。
//!    ⇒ 靠 `phase == "working"` 或靠 `ordinal` 存在与否判「执行者仍在」的实现，会在真实主仓上
//!    漏判正在干活的执行者并**真删一个在飞现场**。② 的相①用两种形状各断言一次，把它当场打红。
//! ⓒ 同一目录里还有三种**不得阻断回收**的形状：`generation == "waiting"` 的空闲活体
//!    （`scaffold.rs:59` 的缺省分支）、连 `generation` 字段都没有的旧形心跳、以及 26 份
//!    `*.json.stale-*` 历史快照。把「缺 generation」判成 `Indeterminate` 会让**每一个**现场
//!    永久 Keep、本卡交付即空转。
//! ⓓ `survey_disk` 的分类默认值必须是 `Ungoverned`——默认值取反就等于把缺口重新藏起来。
//!
//! 心跳目录本身缺席也是真实形态（全新仓 / 已清理的仓 / ⑤⑩ 的夹具都没有
//! `coordination/runtime/heartbeats/`）：**目录不存在 = 零候选 = `Met`**，不是 `Indeterminate`。
//! 把 `read_dir` 的 `NotFound` 当错误会让 ⑤⑩ 永久红。
//!
//! 负向变异下界（每条都必须把点名用例打红；M1 按「改哪一层」分列，审查者据此复演）：
//! M1-decide. 从 `decide_task_site_reclaim` 的合取里删掉四判据中任一条 -> ① 红。
//! M1-probe1. 把判据一（跨轮账本）探针恒置 `Met` -> ⑤ 红。
//! M1-probe2. 把判据二（执行者进程不存活）探针恒置 `Met` -> ② 相①红。
//! M1-probe3. 把判据三（工作树干净）探针恒置 `Met` -> ③ 红。
//! M1-probe4. 把判据四（HEAD 已并入 main）探针恒置 `Met` -> ④ 红。
//! M1-detached. 候选集判据写成 `gitx::current_branch(&path)?`（用 `?` 上抛 detached 的 Err）
//!     -> ②③④⑤⑩ 全红（夹具里恒有一个 detached 现场）。
//! M1-phase. 判据二读 `phase`（只认 `"working"` 或只认 `"waiting"`）或读 `ordinal` 存在与否
//!     -> ② 相①的两轮里必有一轮红（两种真实形状在这两个字段上互相矛盾）。
//! M1-hb. 把「`generation` 缺失」或「`generation == "waiting"`」判成 `Indeterminate`/`Unmet`
//!     -> ② 相②红（本卡「能不能真的回收」的分水岭）。
//! M1-nodir. 把「heartbeats 目录不存在」判成 `Indeterminate` -> ⑤⑩ 红。
//! M2. 回收现场时顺手删 `task/<T>` 分支 -> ② 红（E2 铁律：任何回收都不删分支）。
//! M3. sweep-targets 把 `orch/target/debug` 纳入删除面 -> ⑥ 红（v1 只测量、只报告、永不删除）。
//! M4. 收轮不接线，或接线把老 needle 挤出 `RoundCmd::Close` 起 8000 字节窗口
//!     -> ⑨ `round_close_wires_reclaim_sweep_targets_and_summary` 红。
//! M5. `freed_bytes` 恒 0、恒常数、或计成「本次扫到的总字节」（把 refused/preserved/debug
//!     的字节也算进去）-> ⑦ `sweep_targets_reports_freed_bytes` 红。
//! M6-fold. `summarize_disk` 把 `GovernedResidual` 也计进 Z -> ⑧ 红。
//! M6-survey. `survey_disk` 把无机制覆盖的路径（`debug` / `t`）判成 `GovernedResidual`，
//!     或把分类默认值取成 `GovernedResidual` -> ⑩ 红。
//! M7. 判据一只查当前轮账本 -> ⑤ `recorded_in_an_earlier_closed_round_still_counts` 红。
//! M8. sweep-targets 删掉仍被 Active lease 引用的 target，对带 `.git` 标记的目录裸
//!     `remove_dir_all`，或把「读不到账本」降级成空 events 后照删 -> ⑥ 红。
//! M9. 判据「无法判定」时 fail-open（把 Indeterminate 当作真）-> ① 红。
//!
//! 本种子不断言当前工作树里存在哪些残留目录（那是暂态事实，且交付本身就是要消灭它）：
//! 全部实弹用例都在自建夹具里复刻缺陷形态，断言的是交付完成后永远为真的不变量。
//!
//! 本种子**不**对 `TaskSiteDisposition` 做穷举 match、也不对任何输出型结构写 struct literal：
//! 全部经由构造器（`fail_closed()` / `with_*` / `new` / `default()`）与访问器
//! （`is_reclaim()` / `keep_reason()`）与字段**读取**，因此枚举可加变体、结构可加字段，
//! 后续卡（B238）不会被本种子锁死（`GateResult` / `WakeSpec` 同族教训）。
//! **但本种子确实冻死了两件事**（卡面 §2.1 已披露）：判据数恒为 4（① 的 81 组合穷举）；
//! `orch/target/debug` 与 `orch/target/t` 恒为 `Ungoverned`（⑩）。要改这两条只能新开卡换种子。

use orch_host::reclaim::{
    decide_task_site_reclaim, reclaim_task_sites, summarize_disk, survey_disk,
    tasks_recorded_in_any_round, Criterion, DiskGovernanceEntry, Governance, TaskSiteCriteria,
};

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use orch_core::EventRecord;
use orch_host::buildcache::{sweep_targets, sweep_targets_for_round, SweepTargetsReport};
use orch_host::util::test_scratch_dir;

const REVIEWED_HEAD: &str = "0123456789012345678901234567890123456789";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git 启动失败");
    assert!(
        out.status.success(),
        "git {args:?} 失败({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 最小主仓夹具：main 一个基提交；`task/<task>` 一个提交并已 `--no-ff` 并入 main；
/// `.worktrees/<task>` 是挂在 `task/<task>` 上的**已登记** worktree（生产 `gitx::worktree_add`
/// 形态）；`.worktrees/<task>-primary-executor-oc-g01` 是一个 **detached** 审查现场
/// （生产 `gitx::worktree_add_detached` 形态，`.worktrees/` 下的主导形态）；
/// `TaskRecorded` 落在 `recorded_round` 的账本里，`CURRENT-ROUND` 指向 `current_round`。
///
/// **刻意不建 `coordination/runtime/heartbeats/`**：全新仓与已清理的仓都是这个形状，
/// 「心跳目录不存在 ⇒ 零候选 ⇒ 判据二 `Met`」是本卡写死的规则（卡面 §2.1②）。
fn task_site_fixture(tag: &str, task: &str, recorded_round: &str, current_round: &str) -> PathBuf {
    let root = test_scratch_dir(tag);
    git(&root, &["init", "-q"]);
    git(
        &root,
        &["config", "user.email", "orch-test@example.invalid"],
    );
    git(&root, &["config", "user.name", "orch test"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join("README.md"), "base\n").expect("写 README 失败");
    fs::write(
        root.join(".gitignore"),
        ".worktrees/\ncoordination/runtime/\norch/target/\n.cowork-temp/\n",
    )
    .expect("写 .gitignore 失败");
    git(&root, &["add", "README.md", ".gitignore"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    git(&root, &["branch", "-M", "main"]);

    let branch = format!("task/{task}");
    git(&root, &["checkout", "-q", "-b", &branch]);
    fs::write(root.join("feature.txt"), "work\n").expect("写 feature 失败");
    git(&root, &["add", "feature.txt"]);
    git(&root, &["commit", "-q", "-m", "feature"]);
    git(&root, &["checkout", "-q", "main"]);
    git(
        &root,
        &[
            "merge",
            "--no-ff",
            "--quiet",
            &branch,
            "-m",
            &format!("merge {task}"),
        ],
    );

    fs::create_dir_all(root.join(".worktrees")).expect("建 .worktrees 失败");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            &format!(".worktrees/{task}"),
            &branch,
        ],
    );
    // detached 审查现场：`gitx::current_branch` 对它返回 **Err**（不是 `Ok(None)`）。
    // 回收器必须把 Err 当作「不是 task 分支」跳过；用 `?` 上抛 = 真实主仓上整体失败。
    let main_sha = git(&root, &["rev-parse", "HEAD"]);
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &format!(".worktrees/{task}-primary-executor-oc-g01"),
            &main_sha,
        ],
    );

    fs::create_dir_all(root.join("coordination/runtime")).expect("建 runtime 失败");
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{current_round}\n"),
    )
    .expect("写 CURRENT-ROUND 失败");
    for round in [recorded_round, current_round] {
        fs::create_dir_all(root.join(format!("coordination/rounds/{round}")))
            .expect("建轮目录失败");
    }
    // 当前轮账本存在但**不含**本任务的 TaskRecorded——跨轮查缺失时判据一必然落空。
    if current_round != recorded_round {
        fs::write(
            root.join(format!("coordination/rounds/{current_round}/events.jsonl")),
            format!("{}\n", ledger_line("RoundOpened", None, current_round)),
        )
        .expect("写当前轮账本失败");
    }
    fs::write(
        root.join(format!("coordination/rounds/{recorded_round}/events.jsonl")),
        format!(
            "{}\n",
            ledger_line("TaskRecorded", Some(task), recorded_round)
        ),
    )
    .expect("写记账轮账本失败");
    root
}

fn ledger_line(kind: &str, task: Option<&str>, round: &str) -> String {
    let mut value = serde_json::json!({
        "eventId": format!("EV-{kind}-{}-{round}", task.unwrap_or("round")),
        "ts": "2026-08-04T00:00:00Z",
        "actor": "runtime:orch",
        "type": kind,
        "round": round,
        "payload": {"postMergeGates": "all-green"},
    });
    if let Some(task) = task {
        value["taskId"] = serde_json::Value::String(task.to_string());
    }
    value.to_string()
}

fn write_heartbeat(root: &Path, file: &str, value: serde_json::Value) {
    let dir = root.join("coordination/runtime/heartbeats");
    fs::create_dir_all(&dir).expect("建 heartbeats 目录失败");
    fs::write(dir.join(file), value.to_string()).expect("写心跳失败");
}

/// 真实形状 (d)：**wait-script 派发形**——逐字段复刻 `scaffold.rs:73` 的 printf 模板
/// （`gen="$target_task-$target_attempt"` 见 `scaffold.rs:60-61`，`ord` 由 `A0001` 去零得来
/// 见 `scaffold.rs:62-70`）。注意 wait-script **把 `phase` 硬编码成 `"waiting"`**，
/// 它永远不写 `"working"`——`pid` 是 wait-script 自己的 `$$`。
fn dispatched_waiting_heartbeat(
    agent: &str,
    task: &str,
    attempt: &str,
    pid: u32,
) -> serde_json::Value {
    let ordinal: u64 = attempt
        .strip_prefix('A')
        .and_then(|digits| digits.trim_start_matches('0').parse().ok())
        .unwrap_or(0);
    serde_json::json!({
        "agent": agent,
        "phase": "waiting",
        "ts": "2026-08-06T00:00:00Z",
        "pid": pid,
        "round": "r66",
        "generation": format!("{task}-{attempt}"),
        "ordinal": ordinal,
    })
}

/// 真实形状 (e)：**runtime 接管形**——逐字段复刻 `wake::working_heartbeat_json`
/// （`wake.rs:6621-6634`），由 `write_working_heartbeat` 原子替换掉 wait-script 心跳；
/// 生产调用点 `tierf.rs:4877` / `tierf.rs:7207` 传的 `generation` 就是 `attempt_id`
/// （`attempt.rs:1076` 的 `format!("{task}-A{:04}", ordinal)`）。
/// **没有 `ordinal` 字段**，`phase == "working"`，`pid` 是 provider spawn pid。
/// 这是「执行者正在干活」的形状——判据二漏掉它 = 真删一个在飞现场。
fn working_heartbeat(agent: &str, task: &str, attempt: &str, pid: u32) -> serde_json::Value {
    serde_json::json!({
        "agent": agent,
        "phase": "working",
        "ts": "2026-08-06T00:00:00Z",
        "pid": pid,
        "round": "r66",
        "generation": format!("{task}-{attempt}"),
    })
}

/// 形态 (a)：空闲等待中的**活体**（实测 `executor-desktop.json` / `executor-claw.json`）。
/// `scaffold.rs:59` 的 `gen="waiting"` 缺省分支——pid 活着，但它没有持有任何任务现场。
fn waiting_heartbeat(agent: &str, pid: u32) -> serde_json::Value {
    serde_json::json!({
        "agent": agent,
        "phase": "waiting",
        "ts": "2026-08-06T03:55:10Z",
        "pid": pid,
        "round": "r65",
        "generation": "waiting",
        "ordinal": 0,
    })
}

/// 形态 (b)：旧形心跳（实测 `executor-antigravity.json` / `executor-claw2.json`）——
/// **连 `generation` / `round` / `ordinal` 字段都没有**，只有四个键。
fn legacy_heartbeat(agent: &str, pid: u32) -> serde_json::Value {
    serde_json::json!({
        "agent": agent,
        "phase": "waiting",
        "ts": "2026-07-25T15:51:12Z",
        "pid": pid,
    })
}

fn write_blob(dir: &Path, name: &str, bytes: usize) {
    fs::create_dir_all(dir).expect("建夹具目录失败");
    fs::write(dir.join(name), vec![0u8; bytes]).expect("写夹具负载失败");
}

/// 未配对 release 的 `WorkspaceLeased`（= Active 现场），payload 形状复刻
/// `tests/site_teardown_never_touches_active.rs` 已冻结的那一份，不新增必填字段（E6）。
fn leased_event(task: &str, role: &str, agent: &str) -> EventRecord {
    serde_json::from_value(serde_json::json!({
        "eventId": format!("EV-LEASE-{task}-{role}"),
        "ts": "2026-08-06T00:00:00Z",
        "actor": "runtime:orch",
        "type": "WorkspaceLeased",
        "round": "r66",
        "taskId": task,
        "payload": {
            "siteId": format!("{task}-{role}-{agent}-g01"),
            "generation": 1,
            "attemptId": format!("{task}-A0001"),
            "role": role,
            "agent": agent,
            "reviewedHead": REVIEWED_HEAD,
            "paths": {
                "worktree": format!(".worktrees/{task}-{role}-{agent}-g01"),
                "target": format!("orch/target/review-{task}-A0001-{role}-{agent}-g01"),
            },
        },
    }))
    .expect("构造 WorkspaceLeased 失败")
}

/// Explicit paired ownership evidence for the low-level target sweeper.
fn released_events(task: &str, role: &str, agent: &str) -> Vec<EventRecord> {
    let lease = leased_event(task, role, agent);
    let mut release = lease.clone();
    release.event_id = format!("EV-RELEASE-{task}-{role}");
    release.kind = "WorkspaceReleased".into();
    release.payload.as_mut().unwrap()["completionReceipt"] =
        serde_json::json!("runtime:orch/managed-wake-terminated");
    vec![lease, release]
}

/// `survey_disk` 必须**逐路径**给出治理归属（仓相对路径，`/` 分隔，无 `./` 前缀）；
/// 缺项本身就是缺陷——没有条目的路径在三元组里等于不存在。
fn entry_of<'a>(entries: &'a [DiskGovernanceEntry], path: &str) -> &'a DiskGovernanceEntry {
    entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("survey_disk 必须逐路径给出治理归属，缺 {path}：{entries:?}"))
}

#[test]
fn four_criteria_are_conjunctive_and_any_false_blocks_removal() {
    // M1-decide/M9：四判据合取且 fail-closed。三态 × 四位 = 81 种组合，**只有唯一的全 Met 组合**
    // 允许回收；「无法判定」与「假」同等阻断——误留一个现场的代价是几十 GiB，
    // 误删一个仍被会话持有的现场的代价是一整个 attempt 的工作，方向不对称。
    const ALL: [Criterion; 3] = [Criterion::Met, Criterion::Unmet, Criterion::Indeterminate];
    let mut combinations = 0usize;
    let mut reclaimable = 0usize;
    for recorded in ALL {
        for absent in ALL {
            for clean in ALL {
                for merged in ALL {
                    let criteria = TaskSiteCriteria::fail_closed()
                        .with_ledger_recorded(recorded)
                        .with_executor_process_absent(absent)
                        .with_worktree_clean(clean)
                        .with_head_merged_into_main(merged);
                    let disposition = decide_task_site_reclaim(&criteria);
                    combinations += 1;
                    let all_met = [recorded, absent, clean, merged]
                        .iter()
                        .all(|value| *value == Criterion::Met);
                    if all_met {
                        reclaimable += 1;
                        assert!(
                            disposition.is_reclaim(),
                            "四判据全真必须允许回收，否则机制永远空转：{criteria:?} -> {disposition:?}"
                        );
                    } else {
                        assert!(
                            !disposition.is_reclaim(),
                            "任一判据非 Met（含无法判定）都必须阻断删除：{criteria:?} -> {disposition:?}"
                        );
                    }
                }
            }
        }
    }
    assert_eq!(combinations, 81, "三态四判据必须穷举 81 种组合");
    assert_eq!(reclaimable, 1, "81 种组合里只有唯一的全真组合允许回收");

    // 缺省即 fail-closed，且非 Reclaim 的裁定必须带可读理由——缺口每轮可见的前提是它有名字。
    // （刻意用访问器而不是 match：不把 `TaskSiteDisposition` 的变体集合与字段集合冻死。）
    let default_disposition = decide_task_site_reclaim(&TaskSiteCriteria::fail_closed());
    assert!(
        !default_disposition.is_reclaim(),
        "fail_closed() 的缺省判据绝不允许判 Reclaim：{default_disposition:?}"
    );
    let reason = default_disposition
        .keep_reason()
        .expect("非 Reclaim 的裁定必须能报出理由（keep_reason 返 Some）");
    assert!(
        !reason.trim().is_empty(),
        "Keep 的理由不能是空串：{default_disposition:?}"
    );
}

#[test]
fn reclaim_removes_worktree_but_never_the_task_branch() {
    // M1-probe2/M1-phase/M1-detached/M1-hb/M2：本用例是「延后之后的那个执行者」的存在性证明，
    // 同时钉死 E2 铁律、detached 现场的 Err 处置、以及心跳的**两种**在场形状与三种非阻断形状。
    let root = task_site_fixture("b237-reclaim-basic", "B901", "r66", "r66");
    let site = root.join(".worktrees/B901");
    let detached = root.join(".worktrees/B901-primary-executor-oc-g01");
    assert!(site.is_dir(), "夹具必须先造出任务现场");
    assert!(detached.is_dir(), "夹具必须先造出 detached 审查现场");

    // 相①：执行者仍在。真实系统里「仍在」有**两种**形状，它们在 `phase` 与 `ordinal` 上互相
    // 矛盾（wait-script 恒 `phase:"waiting"`+带 ordinal；runtime 接管形 `phase:"working"`+无
    // ordinal），只在 `generation` 前缀与 `pid` 上一致。判据二只许看后两者——
    // 任何读 `phase` / 读 `ordinal` 的实现，在这两轮里必有一轮把在飞现场判成可删。
    let live = std::process::id();
    for (shape, payload) in [
        (
            "wait-script 派发形（scaffold.rs:73，phase 恒为 waiting、带 ordinal）",
            dispatched_waiting_heartbeat("executor-desktop", "B901", "A0001", live),
        ),
        (
            "runtime 接管形（wake.rs:6621-6634，phase=working、无 ordinal）",
            working_heartbeat("executor-desktop", "B901", "A0001", live),
        ),
    ] {
        write_heartbeat(&root, "executor-desktop.json", payload);
        let kept = reclaim_task_sites(&root).expect(
            "回收器不得因活体现场、也不得因 detached 登记项（current_branch 返 Err）而失败",
        );
        assert!(site.is_dir(), "执行者进程存活时绝不许删现场（{shape}）");
        let kept_verdict = kept
            .verdicts
            .iter()
            .find(|verdict| verdict.task_id == "B901")
            .expect("必须逐现场出裁定，而不是静默跳过");
        assert_eq!(
            kept_verdict.criteria.executor_process_absent,
            Criterion::Unmet,
            "活体心跳必须把进程判据判 Unmet（{shape}）——判据二只看 generation 前缀与 pid：\
             {kept_verdict:?}"
        );
        assert!(
            !kept_verdict.disposition.is_reclaim(),
            "活体心跳下不得判 Reclaim（{shape}）：{kept_verdict:?}"
        );
        assert!(
            !kept
                .verdicts
                .iter()
                .any(|verdict| verdict.worktree.contains("primary-executor-oc-g01")),
            "detached 审查现场归 sites gc 管，不得进入本卡的裁定面：{kept:?}"
        );
    }

    // 相②：撤走带任务绑定的心跳，但把真实主仓里**同时存在的另外三种形状**留在场上，
    // 全部用活体 pid——它们一律不得阻断回收，否则本卡交付即空转：
    // (a) `generation == "waiting"` 的空闲活体（实测 executor-desktop.json）；
    // (b) 完全没有 `generation` 字段的旧形心跳（实测 executor-antigravity.json）；
    // (c) `*.json.stale-*` 历史快照（扫描面只认 extension == "json"，实测目录下 26 份）。
    write_heartbeat(
        &root,
        "executor-desktop.json",
        waiting_heartbeat("executor-desktop", live),
    );
    write_heartbeat(
        &root,
        "executor-antigravity.json",
        legacy_heartbeat("executor-antigravity", live),
    );
    write_heartbeat(
        &root,
        "executor-opencode.json.stale-preR34",
        working_heartbeat("executor-opencode", "B901", "A0001", live),
    );

    let report = reclaim_task_sites(&root).expect("回收器不得失败");
    let reclaimed_verdict = report
        .verdicts
        .iter()
        .find(|verdict| verdict.task_id == "B901")
        .expect("回收也必须出裁定");
    assert_eq!(
        reclaimed_verdict.criteria.executor_process_absent,
        Criterion::Met,
        "generation==\"waiting\" 的活体、无 generation 字段的旧形心跳、`.json.stale-*` 历史快照——\
         三者都不得阻断回收，否则真实主仓里每一个现场都会永久 Keep：{reclaimed_verdict:?}"
    );
    assert!(!site.exists(), "四判据全真时必须真删 .worktrees/B901");
    assert!(report.freed_bytes > 0, "回收必须回报释放字节：{report:?}");
    assert!(
        reclaimed_verdict.disposition.is_reclaim(),
        "回收事实必须逐项落在报告里：{report:?}"
    );
    assert!(
        git_ok(&root, &["rev-parse", "--verify", "refs/heads/task/B901"]),
        "分支一律保留——merge_irreversible 已把这条钉死，回收器不得越界"
    );
    assert!(
        detached.is_dir(),
        "detached 审查现场一个字节都不许动：{report:?}"
    );
    // 必须走 git 的 worktree 注销，而不是裸 remove_dir_all 留下悬挂登记项。
    let registry = git(&root, &["worktree", "list", "--porcelain"]);
    assert!(
        !registry.contains(".worktrees/B901\n") && !registry.ends_with(".worktrees/B901"),
        "已登记 worktree 必须经 git 注销，注册表仍残留说明是裸删：{registry}"
    );
}

#[test]
fn dirty_worktree_is_reported_and_left_intact() {
    // M1-probe3：`git status` 非空 ⇒ 现场原样保留并进报告。未提交的字节是唯一副本。
    let root = task_site_fixture("b237-dirty", "B902", "r66", "r66");
    let site = root.join(".worktrees/B902");
    fs::write(site.join("feature.txt"), "uncommitted edit\n").expect("弄脏现场失败");

    let report = reclaim_task_sites(&root).expect("回收器不得失败");
    assert!(site.is_dir(), "工作树不干净时绝不许删");
    let verdict = report
        .verdicts
        .iter()
        .find(|verdict| verdict.task_id == "B902")
        .expect("脏现场必须出现在报告里，而不是被静默跳过");
    assert_eq!(
        verdict.criteria.worktree_clean,
        Criterion::Unmet,
        "脏树必须判 Unmet：{verdict:?}"
    );
    assert!(
        !verdict.disposition.is_reclaim(),
        "脏树不得判 Reclaim：{verdict:?}"
    );
    assert!(git_ok(
        &root,
        &["rev-parse", "--verify", "refs/heads/task/B902"]
    ));
    assert!(
        root.join(".worktrees/B902-primary-executor-oc-g01")
            .is_dir(),
        "detached 审查现场不属于本卡作用域"
    );
}

#[test]
fn unmerged_head_is_reported_and_left_intact() {
    // M1-probe4：分支上有 main 未含的提交 ⇒ 保留。本例现场是**干净**的，
    // 所以红只可能来自并入判据——把「四条里到底哪条在起作用」钉死。
    let root = task_site_fixture("b237-unmerged", "B903", "r66", "r66");
    let site = root.join(".worktrees/B903");
    fs::write(site.join("late.txt"), "post-merge work\n").expect("写后续改动失败");
    git(&site, &["add", "late.txt"]);
    git(&site, &["commit", "-q", "-m", "late work"]);

    let report = reclaim_task_sites(&root).expect("回收器不得失败");
    assert!(site.is_dir(), "HEAD 未并入 main 时绝不许删");
    let verdict = report
        .verdicts
        .iter()
        .find(|verdict| verdict.task_id == "B903")
        .expect("未并入的现场必须出现在报告里");
    assert_eq!(
        verdict.criteria.worktree_clean,
        Criterion::Met,
        "本例现场已提交、必须判干净，否则用例失去针对性：{verdict:?}"
    );
    assert_eq!(
        verdict.criteria.head_merged_into_main,
        Criterion::Unmet,
        "HEAD 不是 main 的祖先时必须判 Unmet：{verdict:?}"
    );
    assert!(
        !verdict.disposition.is_reclaim(),
        "未并入的现场不得判 Reclaim：{verdict:?}"
    );
}

#[test]
fn recorded_in_an_earlier_closed_round_still_counts() {
    // M7/M1-probe1：B224/B226/B228 形态——`TaskRecorded` 落在 r64 账本，收轮时的当前轮是 r66。
    // 只查当前轮 = 交付「正确但无用」（48.8G 主案例颗粒无收）。
    // 本夹具**没有** heartbeats 目录：目录不存在 ⇒ 零候选 ⇒ 判据二 `Met`（M1-nodir）。
    let root = task_site_fixture("b237-cross-round", "B904", "r64", "r66");
    let recorded = tasks_recorded_in_any_round(&root).expect("跨轮账本折叠不得失败");
    assert_eq!(
        recorded.by_task.get("B904").map(String::as_str),
        Some("r64"),
        "TaskRecorded 落在前一轮账本时，跨轮查必须找到并给出是哪一轮：{:?}",
        recorded.by_task
    );

    // 同一 taskId 在多轮都被记账（重开卡 / 重记账）是真实形态：值取**轮号最大**的那一轮，
    // 不是「先遇到的那一轮」。冻结后 `by_task` 的类型改不了，这条歧义必须现在钉死。
    for round in ["r64", "r66"] {
        let path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
        let mut text = fs::read_to_string(&path).expect("读夹具账本失败");
        text.push_str(&ledger_line("TaskRecorded", Some("B910"), round));
        text.push('\n');
        fs::write(&path, text).expect("追加夹具账本失败");
    }
    let recorded = tasks_recorded_in_any_round(&root).expect("跨轮账本折叠不得失败");
    assert_eq!(
        recorded.by_task.get("B910").map(String::as_str),
        Some("r66"),
        "同一 taskId 多轮记账时必须取轮号最大者：{:?}",
        recorded.by_task
    );

    let site = root.join(".worktrees/B904");
    let report = reclaim_task_sites(&root).expect("回收器不得失败");
    assert!(
        !site.exists(),
        "跨轮判据必须真的驱动回收，而不是只在折叠函数里成立"
    );
    assert!(
        report
            .verdicts
            .iter()
            .any(|verdict| verdict.task_id == "B904" && verdict.disposition.is_reclaim()),
        "跨轮回收事实必须落在报告里：{report:?}"
    );
    assert!(
        git_ok(&root, &["rev-parse", "--verify", "refs/heads/task/B904"]),
        "跨轮回收同样不许碰分支"
    );
}

#[test]
fn sweep_targets_never_touches_the_main_debug_target() {
    // M3/M8：删除面是**保守白名单**，不是黑名单。
    // `orch/target/debug` v1 只测量、只报告、永不删除——它进 summary 的 Y 并单列字节数。
    let root = test_scratch_dir("b237-sweep-debug");
    let target = root.join("orch/target");
    write_blob(&target.join("debug/deps"), "liborch_host.rlib", 4096);
    write_blob(
        &target.join("review-B905-A0001-primary-executor-oc-g01"),
        "blob.bin",
        12_345,
    );
    write_blob(
        &target.join("review-B905-A0001-secondary-executor-zc-g01"),
        "blob.bin",
        2048,
    );
    // 顶层带 `.git` 标记 = **不该出现**的形状（review target 是 build 目录，正常永不含 `.git`），
    // 但一旦出现就说明有人把登记现场落错了地方——裸 remove_dir_all 会毁掉一个 worktree。
    // 这是防御性守卫，且是规格 (b)「未被 registry 登记」在 orch/target 作用域下唯一有区分力的实证信号。
    let linked = target.join("review-B905-A0001-nongate-executor-pi-g01");
    write_blob(&linked, "blob.bin", 1024);
    fs::write(linked.join(".git"), "gitdir: /nowhere\n").expect("写 .git 标记失败");
    write_blob(&target.join("t-b204a3-codex"), "blob.bin", 512);
    write_blob(&target.join("test-tmp/keep-me"), "blob.bin", 256);

    // 账本：secondary 现场仍是 Active lease（只 leased、未 released）⇒ 其 target 不得删。
    let mut events = released_events("B905", "primary", "executor-oc");
    events.push(leased_event("B905", "secondary", "executor-zc"));
    let report: SweepTargetsReport =
        sweep_targets(&root, &events, Duration::from_secs(0)).expect("sweep-targets 不得失败");

    assert!(
        target.join("debug/deps/liborch_host.rlib").is_file(),
        "orch/target/debug 永不删除（v1 只测量、只报告）"
    );
    assert!(
        report.debug_bytes >= 4096,
        "debug 字节必须单列进报告，否则缺口仍然看不见：{report:?}"
    );
    assert!(
        !target
            .join("review-B905-A0001-primary-executor-oc-g01")
            .exists(),
        "已登记且配对释放的 review target 必须回收：{report:?}"
    );
    assert!(
        target
            .join("review-B905-A0001-secondary-executor-zc-g01/blob.bin")
            .is_file(),
        "仍被 Active lease 引用的 target 绝不许删：{report:?}"
    );
    assert!(
        linked.join("blob.bin").is_file(),
        "带 .git 标记的目录必须拒删：{report:?}"
    );
    assert!(
        report
            .refused
            .iter()
            .any(|path| path.contains("nongate-executor-pi-g01")),
        "拒删必须逐项可核对，不能只给个计数：{report:?}"
    );
    assert!(
        target.join("t-b204a3-codex/blob.bin").is_file(),
        "未登记手工探针不能按名称进入回收面：{report:?}"
    );
    assert!(
        target.join("test-tmp/keep-me/blob.bin").is_file(),
        "test-tmp 有自己的清扫器，sweep-targets 不得越界：{report:?}"
    );

    // 收轮入口不得把「读不到账本」降级成「空 events」——空 events 意味着一个 Active lease
    // 都看不见，于是**所有**在飞审查现场的 target 都落进删除面。读不到账本必须 Err
    // （收轮打 ⚠️ 降级、零删除），这是 fail-closed 的方向。
    let orphan = test_scratch_dir("b237-sweep-no-ledger");
    write_blob(
        &orphan.join("orch/target/review-B905-A0001-primary-executor-oc-g01"),
        "blob.bin",
        4096,
    );
    assert!(
        sweep_targets_for_round(&orphan, Duration::from_secs(0)).is_err(),
        "读不到 CURRENT-ROUND/账本时必须 Err，绝不能降级成空 events 后照删"
    );
    assert!(
        orphan
            .join("orch/target/review-B905-A0001-primary-executor-oc-g01/blob.bin")
            .is_file(),
        "失败路径必须零删除"
    );
}

#[test]
fn sweep_targets_reports_freed_bytes() {
    // M5：字节口径必须与**实际删除量**挂钩（`TrialCacheSweepReport.freed_bytes` 同型），
    // 既不能恒 0、不能是与输入无关的常数，也不能是「本次扫到的总字节」——
    // 所以夹具在同一次调用里同时摆了「被删的」「被拒删的」「永不删的」三种目录。
    let root = test_scratch_dir("b237-sweep-bytes");
    let probe = "review-B907-A0001-primary-executor-oc-g01";
    let events = released_events("B907", "primary", "executor-oc");
    let target = root.join("orch/target");
    write_blob(&target.join(probe), "blob.bin", 20_000);
    write_blob(&target.join("debug/deps"), "liborch_host.rlib", 500_000);
    let linked = target.join("review-B907-A0001-nongate-executor-pi-g01");
    write_blob(&linked, "blob.bin", 300_000);
    fs::write(linked.join(".git"), "gitdir: /nowhere\n").expect("写 .git 标记失败");

    let first =
        sweep_targets(&root, &events, Duration::from_secs(0)).expect("sweep-targets 不得失败");
    assert!(
        first.removed.iter().any(|path| path.contains(probe)),
        "回收清单必须逐项可核对：{first:?}"
    );
    assert!(
        first.freed_bytes >= 20_000,
        "freed_bytes 必须至少覆盖实际删除的字节：{first:?}"
    );
    assert!(
        first.freed_bytes < 300_000,
        "freed_bytes 只能计**实际删除**的字节：拒删的 .git 目录（300000B）与永不删除的 debug\
         （500000B）都不得计入，否则口径变成「本次扫到的总字节」、M5 的恒常数变异就抓不住了：\
         {first:?}"
    );
    assert!(
        linked.join("blob.bin").is_file() && target.join("debug/deps").is_dir(),
        "被计入分母的那两个目录必须真的还在盘上，否则本用例的上界失去意义：{first:?}"
    );

    let second =
        sweep_targets(&root, &events, Duration::from_secs(0)).expect("重复 sweep 必须幂等成功");
    assert!(second.removed.is_empty(), "幂等：第二遍零删除：{second:?}");
    assert_eq!(second.freed_bytes, 0, "幂等：第二遍零回收：{second:?}");

    let bigger = test_scratch_dir("b237-sweep-bytes-bigger");
    write_blob(&bigger.join("orch/target").join(probe), "blob.bin", 200_000);
    let big =
        sweep_targets(&bigger, &events, Duration::from_secs(0)).expect("sweep-targets 不得失败");
    assert!(
        big.freed_bytes > first.freed_bytes,
        "字节口径必须随实际删除量变化，不能是常数：{big:?} vs {first:?}"
    );

    // 没有 orch/target（全新仓/已清理）不是错误——收轮接线不得因此失败。
    let missing = test_scratch_dir("b237-sweep-missing");
    let none = sweep_targets(&missing, &[], Duration::from_secs(0))
        .expect("缺 orch/target 必须是 Ok 而非错误");
    assert_eq!(none.freed_bytes, 0);
    assert!(none.removed.is_empty());
}

#[test]
fn ungoverned_bytes_count_only_unmodeled_paths() {
    // M6-fold：X / Y / Z 三个口径互斥且有明确定义——
    // X = 本轮回收掉的字节（已经不在盘上）；Y = 仍占用的字节；Z ⊆ Y，
    // 只计「没有任何治理机制覆盖」的路径。Z 是这轮要让人每轮都看见的那个数。
    let entries = vec![
        DiskGovernanceEntry::new(
            ".worktrees/B904",
            18_500_000_000,
            Governance::ReclaimedThisRound,
        ),
        DiskGovernanceEntry::new(
            "orch/target/review-B905-A0001-primary-executor-oc-g01",
            2_000_000_000,
            Governance::ReclaimedThisRound,
        ),
        DiskGovernanceEntry::new(
            ".cowork-temp/trial-cache",
            3_000_000_000,
            Governance::GovernedResidual,
        ),
        DiskGovernanceEntry::new("orch/target/debug", 17_000_000_000, Governance::Ungoverned),
        DiskGovernanceEntry::new("orch/target/t", 1_000_000_000, Governance::Ungoverned),
    ];
    let summary = summarize_disk(&entries);
    assert_eq!(
        summary.reclaimed_bytes, 20_500_000_000,
        "X 只计本轮回收掉的"
    );
    assert_eq!(summary.occupied_bytes, 21_000_000_000, "Y 只计仍在盘上的");
    assert_eq!(summary.ungoverned_bytes, 18_000_000_000, "Z 只计无治理路径");
    assert!(summary.ungoverned_bytes <= summary.occupied_bytes, "Z ⊆ Y");
    let total: u64 = entries.iter().map(|entry| entry.bytes).sum();
    assert_eq!(
        summary.reclaimed_bytes + summary.occupied_bytes,
        total,
        "X 与 Y 必须互斥且合起来覆盖全集——已回收的字节绝不能同时算作仍占用"
    );

    // 再加一条**有治理机制覆盖**的残留：只抬 Y，绝不抬 Z。
    let mut widened = entries.clone();
    widened.push(DiskGovernanceEntry::new(
        "orch/target/test-tmp",
        5_000_000_000,
        Governance::GovernedResidual,
    ));
    let widened = summarize_disk(&widened);
    assert_eq!(
        widened.ungoverned_bytes, summary.ungoverned_bytes,
        "有治理的残留不得计进 Z，否则 Z 退化成「所有剩下的」，缺口又看不见了"
    );
    assert_eq!(
        widened.occupied_bytes,
        summary.occupied_bytes + 5_000_000_000,
        "有治理的残留必须计进 Y"
    );

    // 人读一行必须同时给出三个口径与精确字节（`freedBytes=` 先例同型）。
    let line = summary.render();
    for label in ["回收", "仍占用", "无人治理"] {
        assert!(line.contains(label), "三元组缺口径 {label}：{line}");
    }
    for bytes in [
        summary.reclaimed_bytes,
        summary.occupied_bytes,
        summary.ungoverned_bytes,
    ] {
        assert!(
            line.contains(&bytes.to_string()),
            "三元组必须给出精确字节 {bytes}：{line}"
        );
    }
}

#[test]
fn survey_disk_classifies_real_paths_so_the_gap_stays_visible() {
    // M6-survey：`survey_disk` 是修法④「让缺口每轮可见」的**唯一实现体**——
    // 把真实磁盘映射成 Governance 三分类的那一步。⑧ 只测纯折叠，抓不到分类逻辑；
    // 一个把所有路径都归 `GovernedResidual`（或干脆只喂空表）的实现会让 Z 恒为 0，
    // 而缺口重新变得看不见。**分类的默认值必须是 `Ungoverned`。**
    // 本夹具同样**没有** heartbeats 目录：零候选 ⇒ 判据二 `Met`（M1-nodir）。
    let root = task_site_fixture("b237-survey", "B906", "r66", "r66");
    let reclaimed = reclaim_task_sites(&root).expect("回收器不得失败");
    assert!(
        reclaimed.freed_bytes > 0,
        "夹具必须真回收出字节，否则本用例的 X 恒零、失去针对性：{reclaimed:?}"
    );
    assert!(
        !root.join(".worktrees/B906").exists(),
        "夹具前提：任务现场已被回收"
    );

    // 无任何机制覆盖：`debug`（17G 主案例）与手工探针 `t`。
    write_blob(
        &root.join("orch/target/debug/deps"),
        "liborch_host.rlib",
        8192,
    );
    write_blob(&root.join("orch/target/t"), "probe.bin", 4096);
    // 各有清扫器：`test-tmp` 归 `util::sweep_test_scratch_root`，
    // `.cowork-temp/trial-cache` 归 `buildcache::sweep_trial_cache`。
    write_blob(
        &root.join("orch/target/test-tmp/b237-old"),
        "blob.bin",
        2048,
    );
    write_blob(
        &root.join(".cowork-temp/trial-cache/slots/slot-0/generations/generation-1"),
        "blob.bin",
        1024,
    );

    let entries =
        survey_disk(&root, &reclaimed, &SweepTargetsReport::default()).expect("磁盘普查不得失败");

    assert_eq!(
        entry_of(&entries, "orch/target/debug").governance,
        Governance::Ungoverned,
        "orch/target/debug 没有任何治理机制——它是 66.8G 里最大的一块，必须记进 Z：{entries:?}"
    );
    assert_eq!(
        entry_of(&entries, "orch/target/t").governance,
        Governance::Ungoverned,
        "手工探针目录无人治理（注意 `t` 不匹配 `t-` 前缀白名单）：{entries:?}"
    );
    assert_eq!(
        entry_of(&entries, "orch/target/test-tmp").governance,
        Governance::GovernedResidual,
        "test-tmp 有 util::sweep_test_scratch_root 治，不得计进 Z：{entries:?}"
    );
    assert_eq!(
        entry_of(&entries, ".cowork-temp/trial-cache").governance,
        Governance::GovernedResidual,
        "trial-cache 有 buildcache::sweep_trial_cache 治，不得计进 Z：{entries:?}"
    );
    assert_eq!(
        entry_of(&entries, ".worktrees/B906").governance,
        Governance::ReclaimedThisRound,
        "本轮回收掉的现场必须以 ReclaimedThisRound 出现在普查里，否则 X 永远是 0：{entries:?}"
    );
    assert!(
        entry_of(&entries, "orch/target/debug").bytes >= 8192,
        "debug 的字节必须真测量，不能是 0 占位：{entries:?}"
    );

    let summary = summarize_disk(&entries);
    assert_eq!(
        summary.reclaimed_bytes, reclaimed.freed_bytes,
        "X 的字节全部且只来自两份报告（本例 sweep 报告为空）：{summary:?} vs {reclaimed:?}"
    );
    assert!(
        summary.ungoverned_bytes >= 12_288,
        "Z 必须真的把 debug 与 t 的字节算进去（≥ 8192+4096）：{summary:?}"
    );
    assert!(
        summary.ungoverned_bytes < summary.occupied_bytes,
        "Z 必须是 Y 的**真**子集——Z==Y 说明分类退化成「所有剩下的都无人治理」：{summary:?}"
    );
}

#[test]
fn round_close_wires_reclaim_sweep_targets_and_summary() {
    let root = orch_host::util::test_scratch_dir("maintenance-migrated-contract");
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&root)
        .status()
        .unwrap()
        .success());
    for (path, bytes) in [
        (
            ".cowork-temp/unregistered/target/cache",
            b"12345".as_slice(),
        ),
        ("orch/target/debug/cache", b"123".as_slice()),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let report = orch_host::reclaim::maintain_storage(&root, false).unwrap();
    assert_eq!(report.measured_logical_bytes, 8);
    assert_eq!(report.removed_logical_bytes, 0);
    assert_eq!(report.unmeasured_boundaries, 0);
    assert!(report.items.iter().any(|item| item.kind == "shared"));
    assert!(!report.filesystems_before.is_empty());
    assert!(!report.filesystems_after.is_empty());
    assert_eq!(
        fs::read(root.join(".cowork-temp/unregistered/target/cache")).unwrap(),
        b"12345"
    );
    fs::remove_dir_all(root).unwrap();
}
