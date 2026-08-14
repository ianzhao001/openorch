//! B240 seeded-red contract: 收轮日志轮转的「归轮」与「fail-fast 收窄」（H122，r65 收轮实撞）。
//!
//! Expected red: compile。`orch_host::logrotate::{RotateReport, AttributionConflict,
//! RotateFailure, RotateUnresolved}` 在 B240 之前都不存在——今天只有 `RotateOutcome`，
//! 它既不报跨轮冲突也不报单项失败，于是两类残留项都无处可去。
//!
//! 现场（r65 收轮实测，取证已入 backlog H122）：
//! `收轮后日志轮转失败（RoundClosed 已落账，不回滚）: 日志归档目标已存在而源仍在，拒绝覆盖:
//!  coordination/runtime/logs/archive/r64/B227-gate-check.log.gz`
//! ⇒ `archive/r65/` 根本没建、74 份日志全部滞留；碰撞的 5 个全是 r64 顺延重开卡
//! （B225/B227）在 r65 又跑了一遍留下的同名日志。
//!
//! 根因① 归错轮：`rotate_closed_round_logs` 在 `closed_tasks` 上找到第一个含该 task 的轮就
//! `break`。`closed_tasks` 是 `Vec<(String, BTreeSet<String>)>`（`logrotate.rs:291` 声明、
//! `:321` 按 `ledgers` 的顺序推入），而 `ledgers` 在 `:285` 被 `sort_by(左轮名 cmp 右轮名)`
//! 排成**字典序** ⇒ `"r64" < "r65"`，r65 产生的日志被归到 r64。最讽刺的是 `insert_candidate`
//! **本来就正确处理这种情况**（同一路径匹配到两个轮时移出候选并标 conflict），
//! 是调用方的 `break` 让这套保护根本没机会执行第二次。
//! 根因② fail-fast 过宽：**两个**归档循环里的 `compress_one(...)?`（已收轮段与孤儿段）
//! 都让第一个失败项中止整轮转，于是一个碰撞把 74 份日志全部拖住。
//! 根因③ 静默：冲突项被倒进私有 `protected` 就丢弃，返回结构没有任何字段能表达「有 5 个
//! 文件因跨轮歧义被留在原地」。
//!
//! 本种子只钉「交付后永远为真」的不变量：跨轮歧义 fail closed 且逐条可见、单项失败不中止
//! 其余项且逐条可见（**两个循环都算**）、未决项绝不被孤儿归档二次收割、唯一归属项语义不变、
//! 生产调用点不得把结果吞掉。
//!
//! Negative mutations that must turn the named case red（**每一条都只改值/改一行控制流，
//! 注入后代码仍能编译**——若某条注入让整文件编译失败，那不是定向变异，是伪自证）：
//! M1. 恢复 `closed_tasks` 循环里的 `break`（跨轮候选归给字典序第一个已收轮）
//!     -> `a_log_claimed_by_two_closed_rounds_is_never_given_to_the_first_round` 红。
//! M2. **保留 `conflicts` 字段但永不填充**：冲突项照旧被移出候选、照旧进 `protected`，
//!     只是 `report.conflicts` 恒为空 Vec（= 今天的静默换个位置继续存在）
//!     -> `every_cross_round_conflict_is_reported_with_all_claiming_rounds` 红。
//! M3. 把**已收轮**归档循环改回 `compress_one(root, &round, &source)?`（单项失败即中止整轮转）
//!     -> `one_failing_archive_does_not_abort_the_rest_of_the_rotation` 红。
//! M4. **`RotateFailure.reason` 一律填空串**（结构不变、字段还在，只是不写内容）
//!     -> `every_failure_is_itemised_with_source_round_and_reason` 红。
//! M5. 把所有已收轮候选一律保守掉（唯一归属项也不再归档）
//!     -> `uniquely_attributed_logs_still_archive_under_their_own_round` 红。
//! M6. 生产调用点把轮转结果吞掉（`orch-cli/src/main.rs` 的两处之一改成 `let _ =` /
//!     `.ok()` / `unwrap_or_default()`）
//!     -> `every_production_call_site_surfaces_the_rotation_result` 红（读真实源码，裁定⑬范式）。
//!     **注入面在 frozenPaths 内，复演协议见卡面 §4.6：只许临时未提交编辑、禁 git 写命令、
//!     还原后必须贴 `git diff --stat` 空输出。**
//! M7. 把**孤儿**归档循环改回 `compress_one(root, &orphan_round, &source)?`
//!     -> `one_failing_orphan_archive_does_not_abort_the_remaining_orphans` 红。
//!     （backlog H122 只点了已收轮段那一处；孤儿段是本卡的增量发现，必须同样有永久回归闸。）
//! M8. 冲突项或失败项不再进 `protected`（于是它们在孤儿循环里被当散落日志压进
//!     `archive/orphans-before-<current>/` 并**删源**——比今天的缺陷更坏，且不可逆）
//!     -> `conflicted_and_failed_sources_are_never_swept_into_the_orphan_archive` 红。

use std::fs;
use std::path::{Path, PathBuf};

use orch_host::logrotate::{
    rotate_closed_round_logs, AttributionConflict, RotateFailure, RotateReport, RotateUnresolved,
};
use orch_host::util::test_scratch_dir;

const LOGS: &str = "coordination/runtime/logs";

/// 在飞轮的远古 `RoundOpened`：孤儿判据是 `modified < opened_at`，夹具文件都是刚写的，
/// 于是孤儿分支**恒不命中**——用于只想验「已收轮归档段」的用例。
const ANCIENT_OPEN: &str = "2020-01-01T00:00:00Z";

/// 在飞轮的远未来 `RoundOpened`：所有散落文件都满足 `modified < opened_at`，
/// 于是孤儿分支**必定命中**——用于验孤儿段的 fail-fast 与「未决项不得被孤儿收割」。
const FUTURE_OPEN: &str = "2099-01-01T00:00:00Z";

fn event(kind: &str, round: &str, task: Option<&str>, ts: &str) -> String {
    serde_json::json!({
        "eventId": format!("EV-{kind}-{round}-{}", task.unwrap_or("round")),
        "ts": ts,
        "actor": "runtime:orch",
        "type": kind,
        "round": round,
        "taskId": task,
        "payload": {},
    })
    .to_string()
}

fn write_ledger(root: &Path, round: &str, events: &[String]) {
    let dir = root.join(format!("coordination/rounds/{round}"));
    fs::create_dir_all(&dir).expect("建轮目录失败");
    fs::write(dir.join("events.jsonl"), format!("{}\n", events.join("\n"))).expect("写轮账本失败");
}

fn log_path(root: &Path, name: &str) -> PathBuf {
    root.join(LOGS).join(name)
}

fn archive_gz(root: &Path, round: &str, name: &str) -> PathBuf {
    root.join(LOGS)
        .join("archive")
        .join(round)
        .join(format!("{name}.gz"))
}

/// 复刻 r65 收轮的真实形态：两个**已收轮**（r64/r65）+ 一个在飞轮（r66）。
/// r65 现场的碰撞项全部来自「r64 顺延重开、在 r65 又跑了一遍」的卡（B225/B227），
/// 于是同一个日志文件名同时落在两个已收轮的任务集里。
struct Fixture<'a> {
    tag: &'a str,
    /// 从 r64 顺延重开、在 r65 也发过卡的任务（r65 现场是 B225 与 B227）。
    /// 空数组 = 无歧义对照组。
    reopened_in_r65: &'a [&'a str],
    /// 在飞轮 r66 的 `RoundOpened` 时刻，决定孤儿分支是否命中（见两个常量）。
    current_opened_at: &'a str,
    /// 不匹配任何任务前缀、也不是 `wake-*.log` 的散落日志（孤儿归档的唯一合法对象）。
    strays: &'a [&'a str],
    /// 预先摆好的 `archive/<round>/<name>.gz`，用来让某一项的归档**必然失败**——
    /// `compress_one` 对已存在的归档目标拒绝覆盖，这正是 r65 现场的失败源。
    precreated_archives: &'a [(&'a str, &'a str)],
}

impl Fixture<'_> {
    fn build(&self) -> PathBuf {
        let root = test_scratch_dir(self.tag);
        let logs = root.join(LOGS);
        fs::create_dir_all(&logs).expect("建日志目录失败");
        fs::create_dir_all(root.join("coordination/runtime")).expect("建 runtime 目录失败");
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r66\n")
            .expect("写 CURRENT-ROUND 失败");

        for name in [
            // 排序最前：留给「必然失败」的那一项，用来证明失败不中止其后各项。
            "B090-gate-check.log",
            "B225-gate-testFast.log",
            "B227-gate-check.log",
            "B231-verify.jsonl",
            // 在飞轮 r66 的日志：一个字节都不许动。
            "B240-gate-check.log",
            // legacy 探活重链：永不轮转。
            "wake-executor-desktop.log",
        ] {
            fs::write(logs.join(name), format!("{name} payload\n")).expect("写夹具日志失败");
        }
        for name in self.strays {
            fs::write(logs.join(name), format!("{name} payload\n")).expect("写散落日志失败");
        }

        write_ledger(
            &root,
            "r64",
            &[
                event("RoundOpened", "r64", None, "2026-07-01T00:00:00Z"),
                event("DispatchIssued", "r64", Some("B090"), "2026-07-01T01:00:00Z"),
                event("DispatchIssued", "r64", Some("B225"), "2026-07-01T01:00:00Z"),
                event("DispatchIssued", "r64", Some("B227"), "2026-07-01T01:00:00Z"),
                event("RoundClosed", "r64", None, "2026-07-02T00:00:00Z"),
            ],
        );

        let mut r65 = vec![
            event("RoundOpened", "r65", None, "2026-07-03T00:00:00Z"),
            event("DispatchIssued", "r65", Some("B231"), "2026-07-03T01:00:00Z"),
        ];
        for task in self.reopened_in_r65.iter().copied() {
            r65.push(event(
                "DispatchIssued",
                "r65",
                Some(task),
                "2026-07-03T01:00:00Z",
            ));
        }
        r65.push(event("RoundClosed", "r65", None, "2026-07-04T00:00:00Z"));
        write_ledger(&root, "r65", &r65);

        write_ledger(
            &root,
            "r66",
            &[
                event("RoundOpened", "r66", None, self.current_opened_at),
                event("DispatchIssued", "r66", Some("B240"), self.current_opened_at),
            ],
        );

        for (round, name) in self.precreated_archives {
            let dir = root.join(LOGS).join("archive").join(round);
            fs::create_dir_all(&dir).expect("建既存归档目录失败");
            fs::write(dir.join(format!("{name}.gz")), b"pre-existing archive\n")
                .expect("写既存归档失败");
        }

        root
    }
}

/// 有未决项时的统一取值口：轮转必须（a）把能做的都做完，（b）以错误告知调用方还有残留，
/// （c）错误里带得走结构化报告——三者缺一，H122 的可见性要求就没落地。
fn rotate_expecting_unresolved(root: &Path) -> (String, RotateReport) {
    let error = rotate_closed_round_logs(root)
        .err()
        .expect("存在跨轮冲突或单项失败时，轮转必须以「有未决项」结束，而不是假装全绿");
    let rendered = format!("{error:#}");
    let unresolved: &RotateUnresolved = error
        .downcast_ref::<RotateUnresolved>()
        .expect("未决错误必须携带结构化报告 RotateUnresolved，不得只留一句话");
    (rendered, unresolved.report.clone())
}

fn conflict_for<'a>(report: &'a RotateReport, name: &str) -> &'a AttributionConflict {
    report
        .conflicts
        .iter()
        .find(|conflict| conflict.source.ends_with(name))
        .unwrap_or_else(|| {
            panic!(
                "报告必须逐条列出跨轮冲突项 {name}；实得 {:?}",
                report.conflicts
            )
        })
}

fn failure_for<'a>(report: &'a RotateReport, name: &str) -> &'a RotateFailure {
    report
        .failures
        .iter()
        .find(|failure| failure.source.ends_with(name))
        .unwrap_or_else(|| panic!("报告必须逐条列出失败项 {name}；实得 {:?}", report.failures))
}

/// 人读文案里的逐条明细行（`CONFLICT …` / `FAILED …`）。可见性的机械判据是**逐条**，
/// 不是「文案里出现过这个词」——后者会被无关内容（比如归档映射清单）顺手满足。
fn itemised_lines(rendered: &str, prefix: &str) -> Vec<String> {
    rendered
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(prefix))
        .map(str::to_string)
        .collect()
}

fn itemised_line_for(rendered: &str, prefix: &str, name: &str) -> String {
    itemised_lines(rendered, prefix)
        .into_iter()
        .find(|line| line.contains(name))
        .unwrap_or_else(|| {
            panic!("人读文案缺少点名 {name} 的 `{prefix}` 明细行；实得:\n{rendered}")
        })
}

fn sorted_sources(sources: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted = sources.to_vec();
    sorted.sort();
    sorted
}

/// 调用点所在的**一条语句**：向前取到上一个 `;` / `{` / `}` 之后，向后取到第一个 `;` 或 `{`
/// （取先到者）为止。
///
/// 刻意不用固定字符数的窗口：本轮 B233/B235/B237 都会在 `main.rs` 落笔，几十行的粗窗口
/// 会把邻近无关代码吞进来——任何一句无关的 `let _ =` 都能把这条冻结契约误红，
/// 反过来窗口里任何一个无关的 `)?` 也能满足正向判据。判据必须收到语句粒度。
fn enclosing_statement(source: &str, at: usize) -> &str {
    let start = source[..at]
        .rfind(|ch| ch == ';' || ch == '{' || ch == '}')
        .map(|index| index + 1)
        .unwrap_or(0);
    let tail = &source[at..];
    let end = [tail.find(';'), tail.find('{')]
        .into_iter()
        .flatten()
        .min()
        .expect("调用点之后必须有 `;` 或 `{` 收尾");
    source[start..at + end + 1].trim()
}

/// `match <call> { … }` 的臂体：从调用点起，取到与 `match` **同缩进**的那个收尾 `}` 为止。
/// 用缩进定界而不是数大括号——格式化字符串里的 `{}` / `{error:#}` 会把大括号计数带歪。
fn match_arms(source: &str, at: usize) -> &str {
    let line_start = source[..at].rfind('\n').map(|index| index + 1).unwrap_or(0);
    let head = &source[line_start..at];
    let indent = &head[..head.len() - head.trim_start().len()];
    let closer = format!("\n{indent}}}");
    let tail = &source[at..];
    let end = tail
        .find(&closer)
        .map(|offset| offset + closer.len())
        .expect("match 调用点必须有与之同缩进的收尾 `}`");
    &source[at..at + end]
}

#[test]
fn a_log_claimed_by_two_closed_rounds_is_never_given_to_the_first_round() {
    // M1：r65 实撞形态。旧实现在 closed_tasks 上 `break`，字典序第一个已收轮（"r64" < "r65"）
    // 直接吃掉归属；`insert_candidate` 的跨轮保护因此永远拿不到第二次调用。
    // 歧义必须 fail closed：两侧都不归，源日志原地保留等人工处置。
    //
    // 本条只钉「归到哪」，不钉返回通道——通道由
    // `every_cross_round_conflict_is_reported_with_all_claiming_rounds` 单独钉。
    // **但这不等于「一条 M 只打红一个用例」**：M1 恢复 break 后 conflicts 恒空，
    // `rotate_expecting_unresolved` 的 `.err().expect` 会直接 panic，那条用例同样会红；
    // M2 同理会波及 `every_production_call_site_surfaces_the_rotation_result`（未决数 3→2）。
    // 复演判据是「该 M 点名的用例必红」，附带打红别的用例不是夹具串扰，不必回退。
    let root = Fixture {
        tag: "b240-attrib-two-rounds",
        reopened_in_r65: &["B227"],
        current_opened_at: ANCIENT_OPEN,
        strays: &[],
        precreated_archives: &[],
    }
    .build();
    let _ = rotate_closed_round_logs(&root);

    assert!(
        !archive_gz(&root, "r64", "B227-gate-check.log").exists(),
        "跨轮同名日志不得按字典序归给 r64"
    );
    assert!(
        !archive_gz(&root, "r65", "B227-gate-check.log").exists(),
        "也不得换个顺序归给 r65——无法唯一归属就一个都不许归"
    );
    assert!(
        log_path(&root, "B227-gate-check.log").is_file(),
        "无法唯一归属的日志必须原地保留（源被删掉才是真正的不可逆损失）"
    );
    assert!(
        archive_gz(&root, "r64", "B090-gate-check.log").exists(),
        "一个歧义项不得把同轮其余唯一归属项一起拖住（否则「都不归」就成了万能解）"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn every_cross_round_conflict_is_reported_with_all_claiming_rounds() {
    // M2：`insert_candidate` 早就会把跨轮候选移出候选集，但它只塞进一个私有集合，
    // 报告与调用方都看不见 ⇒ 等于静默跳过。冲突必须逐条可见，并点名**全部**竞争轮，
    // 否则运维不知道该去哪两个轮之间做人工裁定。
    //
    // 夹具用 r65 现场的完整形态：B225 与 B227 都是从 r64 顺延重开的卡。
    let root = Fixture {
        tag: "b240-attrib-conflict-report",
        reopened_in_r65: &["B225", "B227"],
        current_opened_at: ANCIENT_OPEN,
        strays: &[],
        precreated_archives: &[],
    }
    .build();
    let (rendered, report) = rotate_expecting_unresolved(&root);

    let sources = report
        .conflicts
        .iter()
        .map(|conflict| conflict.source.clone())
        .collect::<Vec<_>>();
    assert_eq!(sources.len(), 2, "两个重开卡的同名日志都必须报成冲突");
    assert_eq!(
        sources,
        sorted_sources(&sources),
        "冲突清单必须按 source 升序，报告才能被机械比对；实得 {sources:?}"
    );

    for name in ["B225-gate-testFast.log", "B227-gate-check.log"] {
        let conflict = conflict_for(&report, name);
        assert_eq!(
            conflict.rounds,
            vec!["r64".to_string(), "r65".to_string()],
            "{name} 的竞争轮清单必须是去重升序的**全部**认领轮；实得 {:?}",
            conflict.rounds
        );
        // 同一行内点名两个轮：只查「文案里出现过 r64」会被归档映射清单之类的无关内容满足。
        let line = itemised_line_for(&rendered, "CONFLICT", name);
        for round in ["r64", "r65"] {
            assert!(
                line.contains(round),
                "同一条 CONFLICT 明细行必须同时点名 {round}；实得: {line}"
            );
        }
    }
    assert!(
        report.failures.is_empty(),
        "本夹具没有归档失败项，失败清单必须为空；实得 {:?}",
        report.failures
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn uniquely_attributed_logs_still_archive_under_their_own_round() {
    // M5 回归：修跨轮歧义不得把唯一归属项一起保守掉。无冲突、无失败时必须仍是 Ok——
    // 否则收轮每次都被一个假警报打扰，人很快就会开始无视它。
    let root = Fixture {
        tag: "b240-attrib-unique",
        reopened_in_r65: &[],
        current_opened_at: ANCIENT_OPEN,
        strays: &[],
        precreated_archives: &[],
    }
    .build();
    let report: RotateReport =
        rotate_closed_round_logs(&root).expect("无冲突、无失败时轮转必须返回 Ok");

    for (round, name) in [
        ("r64", "B090-gate-check.log"),
        ("r64", "B225-gate-testFast.log"),
        ("r64", "B227-gate-check.log"),
        ("r65", "B231-verify.jsonl"),
    ] {
        assert!(
            archive_gz(&root, round, name).exists(),
            "唯一归属的 {name} 必须归到 {round}"
        );
        assert!(
            !log_path(&root, name).exists(),
            "{name} 归档提交后源日志必须删除"
        );
        assert!(
            report
                .archived
                .iter()
                .any(|mapping| mapping.source.ends_with(name) && mapping.round == round),
            "报告必须逐条列出归档映射 {round}/{name}；实得 {:?}",
            report.archived
        );
    }
    let archived_sources = report
        .archived
        .iter()
        .map(|mapping| mapping.source.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        archived_sources,
        sorted_sources(&archived_sources),
        "归档清单必须保持按 source 升序（既有语义，加宽报告不得打乱）；实得 {archived_sources:?}"
    );
    assert!(
        report.conflicts.is_empty(),
        "无跨轮同名时不得凭空报冲突；实得 {:?}",
        report.conflicts
    );
    assert!(
        report.failures.is_empty(),
        "全部成功时失败清单必须为空；实得 {:?}",
        report.failures
    );
    assert!(
        log_path(&root, "B240-gate-check.log").is_file(),
        "在飞轮（r66）的日志一个字节都不许动"
    );
    assert!(
        log_path(&root, "wake-executor-desktop.log").is_file(),
        "legacy 探活重链文件永不轮转"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn one_failing_archive_does_not_abort_the_rest_of_the_rotation() {
    // M3：已收轮归档循环里的 `compress_one(...)?` 就是 r65 让 74 份日志全部滞留的那一行。
    // 夹具让**排序最前**的 B090 必然失败（归档目标已存在，拒绝覆盖），
    // 其后的项必须照常落档——这正是「记录并继续」与「fail-fast」的分水岭。
    let root = Fixture {
        tag: "b240-fail-does-not-abort",
        reopened_in_r65: &[],
        current_opened_at: ANCIENT_OPEN,
        strays: &[],
        precreated_archives: &[("r64", "B090-gate-check.log")],
    }
    .build();
    let (_, report) = rotate_expecting_unresolved(&root);

    assert!(
        log_path(&root, "B090-gate-check.log").is_file(),
        "失败项的源日志必须原样保留（归档没成，源就不能删）"
    );
    for (round, name) in [
        ("r64", "B225-gate-testFast.log"),
        ("r64", "B227-gate-check.log"),
        ("r65", "B231-verify.jsonl"),
    ] {
        assert!(
            archive_gz(&root, round, name).exists(),
            "排在失败项之后的 {name} 必须仍完成归档（单项失败不得中止整轮转）"
        );
        assert!(
            !log_path(&root, name).exists(),
            "{name} 归档提交后源日志必须删除"
        );
    }
    assert_eq!(
        report.archived.len(),
        3,
        "已完成的归档必须全部进报告；实得 {:?}",
        report.archived
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn every_failure_is_itemised_with_source_round_and_reason() {
    // M4：失败项不得退化成一个计数、也不得只剩一个空壳字段。运维要能直接读出
    // 「哪个文件、本该归哪个轮、为什么失败」，否则「记录并继续」只是把 fail-fast
    // 换成了静默丢弃。两个失败项还顺带钉死失败清单的排序。
    let root = Fixture {
        tag: "b240-failure-itemised",
        reopened_in_r65: &[],
        current_opened_at: ANCIENT_OPEN,
        strays: &[],
        precreated_archives: &[
            ("r64", "B090-gate-check.log"),
            ("r64", "B225-gate-testFast.log"),
        ],
    }
    .build();
    let (rendered, report) = rotate_expecting_unresolved(&root);

    let sources = report
        .failures
        .iter()
        .map(|failure| failure.source.clone())
        .collect::<Vec<_>>();
    assert_eq!(sources.len(), 2, "两个归档目标都已存在，必须报两条失败");
    assert_eq!(
        sources,
        sorted_sources(&sources),
        "失败清单必须按 source 升序；实得 {sources:?}"
    );

    for name in ["B090-gate-check.log", "B225-gate-testFast.log"] {
        let failure = failure_for(&report, name);
        assert_eq!(failure.round, "r64", "{name} 必须记下它本该归属的轮");
        assert!(
            !failure.reason.trim().is_empty(),
            "{name} 必须带可读原因（空字符串等于没记）"
        );
        assert!(
            rendered.contains(failure.reason.trim()),
            "结构化 reason 必须原样出现在人读文案里，两条通道不许各说各话；reason={:?}",
            failure.reason
        );
        let line = itemised_line_for(&rendered, "FAILED", name);
        assert!(
            line.contains("r64"),
            "FAILED 明细行必须点名本该归属的轮；实得: {line}"
        );
        assert!(
            log_path(&root, name).is_file(),
            "{name} 归档失败，源日志必须原地保留"
        );
        assert!(
            !report
                .archived
                .iter()
                .any(|mapping| mapping.source.ends_with(name)),
            "失败项绝不能同时出现在 archived 里；实得 {:?}",
            report.archived
        );
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn every_production_call_site_surfaces_the_rotation_result() {
    // M6：「谁调它」。未决项只有经由生产调用点才会到人眼前——任何一处把结果吞掉，
    // 本卡新增的冲突/失败清单就又变成了一个没人读的字段（H111 同族）。
    // 前半读真实生产源码（不读清单）；后半证明「送到人眼前的那段文案」本身自带逐条明细，
    // 否则接线接了也白接。
    let cli_main = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../orch-cli/src/main.rs"),
    )
    .expect("读 orch-cli/src/main.rs");

    let call_sites = cli_main
        .match_indices("rotate_closed_round_logs(")
        .map(|(at, _)| at)
        .collect::<Vec<_>>();
    assert!(
        call_sites.len() >= 2,
        "orch-cli 必须保留 `sites rotate-logs` 与 `round close` 两个轮转入口；实得 {}",
        call_sites.len()
    );
    for at in call_sites {
        let statement = enclosing_statement(&cli_main, at);
        for swallowed in ["unwrap_or_default", ".ok()", "let _ ="] {
            assert!(
                !statement.contains(swallowed),
                "轮转结果被 {swallowed} 吞掉，未决项永远到不了人眼前：{statement}"
            );
        }
        let propagates = statement.trim_end().ends_with("?;");
        let dispatches = statement.contains("match ") && statement.trim_end().ends_with('{');
        assert!(
            propagates || dispatches,
            "轮转结果必须要么以 `?` 向上传播、要么进入显式 match 分派；实得：{statement}"
        );
        if dispatches {
            let arms = match_arms(&cli_main, at);
            for swallowed in ["unwrap_or_default", ".ok()", "let _ ="] {
                assert!(
                    !arms.contains(swallowed),
                    "match 臂里把结果 {swallowed} 掉，等于没分派：{arms}"
                );
            }
            for required in ["Ok(", "Err(", ":#}"] {
                assert!(
                    arms.contains(required),
                    "match 调用点必须显式处理 {required}（Err 臂要把错误原文以 `{{…:#}}` 打出来）；实得：{arms}"
                );
            }
        }
    }

    // 接线只有在「送到人眼前的那段文案自带明细」时才有价值：
    // `round close` 的 Err 臂打印的就是这段 `{error:#}`，而 `sites rotate-logs` 的 `?`
    // 会**跳过整个 Ok 分支**（逐条 closed/ORPHAN/REFUSED 打印全部不执行），
    // 于是这段文案是运维唯一能看到的东西。
    let root = Fixture {
        tag: "b240-callsite-render",
        reopened_in_r65: &["B227"],
        current_opened_at: ANCIENT_OPEN,
        strays: &[],
        precreated_archives: &[
            ("r64", "B090-gate-check.log"),
            ("r64", "B225-gate-testFast.log"),
        ],
    }
    .build();
    let (rendered, report) = rotate_expecting_unresolved(&root);

    let unresolved = report.conflicts.len() + report.failures.len();
    assert_eq!(
        unresolved, 3,
        "夹具设定：1 个跨轮冲突（B227）+ 2 个归档失败（B090/B225）；实得 conflicts={:?} failures={:?}",
        report.conflicts, report.failures
    );
    let itemised =
        itemised_lines(&rendered, "CONFLICT").len() + itemised_lines(&rendered, "FAILED").len();
    assert_eq!(
        itemised, unresolved,
        "每个未决项都必须在人读文案里各占一行（计数摘要不算可见）；实得:\n{rendered}"
    );
    let headline = rendered.lines().next().unwrap_or_default();
    assert!(
        headline.contains(&report.archived.len().to_string()),
        "第一行必须先报出**已完成**的归档数，否则运维会把这个 Err 误读成整体回滚；实得: {headline}"
    );
    assert!(
        headline.contains(&unresolved.to_string()),
        "第一行还必须报出待人工处置的项数；实得: {headline}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn one_failing_orphan_archive_does_not_abort_the_remaining_orphans() {
    // M7：孤儿归档循环里也有一个 `compress_one(...)?`。backlog H122 只点了已收轮段那一处，
    // 但孤儿段是同一个形状的第二个单点熔断——一份散落日志归不动，其余散落日志就全部滞留。
    // 冻结契约必须两处都覆盖，否则交付后任何人把它改回 `?` 都不会被抓到。
    //
    // 夹具把在飞轮的 RoundOpened 设成远未来时刻，让 `modified < opened_at` 恒成立，
    // 孤儿分支因此必定命中；排序最前的 zz-stray-a 的孤儿归档目标被预置成已存在 ⇒ 必然失败。
    let root = Fixture {
        tag: "b240-orphan-fail-continues",
        reopened_in_r65: &["B227"],
        current_opened_at: FUTURE_OPEN,
        strays: &["zz-stray-a.log", "zz-stray-b.log"],
        precreated_archives: &[("orphans-before-r66", "zz-stray-a.log")],
    }
    .build();
    let (rendered, report) = rotate_expecting_unresolved(&root);

    assert!(
        archive_gz(&root, "orphans-before-r66", "zz-stray-b.log").exists(),
        "排在失败孤儿之后的 zz-stray-b.log 必须仍完成孤儿归档（孤儿段同样不得单点熔断）"
    );
    assert!(
        !log_path(&root, "zz-stray-b.log").exists(),
        "zz-stray-b.log 孤儿归档提交后源必须删除"
    );
    assert!(
        report
            .orphaned
            .iter()
            .any(|mapping| mapping.source.ends_with("zz-stray-b.log")
                && mapping.round == "orphans-before-r66"),
        "完成的孤儿归档必须逐条进报告；实得 {:?}",
        report.orphaned
    );

    assert!(
        log_path(&root, "zz-stray-a.log").is_file(),
        "孤儿归档失败时源必须原地保留"
    );
    let failure = failure_for(&report, "zz-stray-a.log");
    assert_eq!(
        failure.round, "orphans-before-r66",
        "孤儿段的失败项必须记下它本该落的孤儿轮名，而不是留空或写在飞轮名"
    );
    assert!(
        !failure.reason.trim().is_empty(),
        "孤儿段失败项同样必须带可读原因"
    );
    itemised_line_for(&rendered, "FAILED", "zz-stray-a.log");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn conflicted_and_failed_sources_are_never_swept_into_the_orphan_archive() {
    // M8：本卡把 fail-fast 收窄成「记录并继续」之后，**归档失败的候选会第一次留在磁盘上**，
    // 而孤儿循环紧接着就在同一份 disk_files 上跑。若失败项（以及既有的冲突项）没有进
    // `protected`，它们会被当成散落日志压进 `archive/orphans-before-<current>/` 并**删源**：
    // 归错了轮、还删了源，比今天的缺陷更坏且不可逆。
    //
    // 夹具同样用远未来 RoundOpened 让孤儿分支必定命中，并留一份真散落日志 zz-stray-b.log
    // 证明孤儿分支**确实在跑**（否则本用例会空转成假绿）。
    let root = Fixture {
        tag: "b240-orphan-never-eats-unresolved",
        reopened_in_r65: &["B227"],
        current_opened_at: FUTURE_OPEN,
        strays: &["zz-stray-b.log"],
        precreated_archives: &[("r64", "B090-gate-check.log")],
    }
    .build();
    let (_, report) = rotate_expecting_unresolved(&root);

    assert!(
        archive_gz(&root, "orphans-before-r66", "zz-stray-b.log").exists(),
        "真散落日志必须仍被孤儿归档——否则本用例的其余断言只是因为孤儿分支没跑而空转"
    );

    for name in [
        // 跨轮冲突项：无法唯一归属，绝不能改由孤儿路径「兜底」。
        "B227-gate-check.log",
        // 归档失败项：源还在磁盘上，绝不能被孤儿循环二次收割。
        "B090-gate-check.log",
    ] {
        assert!(
            log_path(&root, name).is_file(),
            "{name} 是未决项，源必须原地保留等人工处置"
        );
        assert!(
            !archive_gz(&root, "orphans-before-r66", name).exists(),
            "{name} 绝不能被孤儿归档收割（那是归错轮 + 删源的不可逆损失）"
        );
        assert!(
            !report
                .orphaned
                .iter()
                .any(|mapping| mapping.source.ends_with(name)),
            "{name} 不得出现在 orphaned 清单里；实得 {:?}",
            report.orphaned
        );
    }

    assert!(
        log_path(&root, "B240-gate-check.log").is_file(),
        "在飞轮的日志即便满足孤儿时间判据也永不轮转"
    );
    assert!(
        log_path(&root, "wake-executor-desktop.log").is_file(),
        "legacy 探活重链文件即便满足孤儿时间判据也永不轮转"
    );

    let _ = fs::remove_dir_all(&root);
}
