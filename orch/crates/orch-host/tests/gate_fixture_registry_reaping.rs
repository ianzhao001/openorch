//! B239 seeded-red contract: 门孤儿子形态机械收割——夹具登记 + 门后兜底收割 + ppid=1 差集断言
//! （H106 子形态；r65 两次活体抓获，用户 2026-08-06 裁定取修法 1+2）。
//!
//! Expected red: compile。`orch_host::gate` 里 `ORCH_GATE_FIXTURE_REGISTRY` /
//! `register_gate_fixture` / `reap_gate_fixture_registry` / `orphan_baseline` /
//! `orphans_since` / `orphan_failure_event` / `GateOrphan` 在 B239 之前一个都不存在
//! ⇒ 首红 `error[E0432]: unresolved imports`。
//! （沿用 B227/B233 的编译红锚定：assertion 形态的种子在「已 plan 未签核」窗口会跑全量门
//! 且结构性必红——H113/H101/H85 同族，编译红短路掉这个问题。）
//!
//! 契约背景（r65 现场取证，`coordination/rounds/r65/evidence/h106-orphan-subform-escapes-b227-r65.md`）：
//! B227 让 `run_gate_command` 把 `cargo test` 放进自己的进程组、两个出口 `kill(-pgid, 9)`
//! （`gate.rs:23-38` + `:222-239`）。**但只修了一半**：夹具是**被测试自己**用
//! `.process_group(0)` 拉起的（`wake.rs` 里 28 处），**另开新进程组**；`kill(-pgid)` 按 POSIX
//! 语义只投递给 pgid 等于该值的进程、**不沿父子链递归** ⇒ 够不着。两次活体抓获：
//! ① 03:59:34 `trial-cache/slots/slot-02/…/orch_host-… --exact wake::tests::b177_wake_f…`
//!    ⇒ **跨门毒化**（门 A 泄漏 → 门 B 红）；
//! ② 05:02:52 `postmerge-B225-<ULID>/…/orch_host-… --exact wake::tests::b177_wake_fixture_child`
//!    ⇒ **同轮自伤**（门自己泄漏 → 同次运行内自己红）。两次都做了单变量前后对照：
//!    清掉孤儿 → 同一道门直接转绿（n=2）。
//! 「同轮自伤」意味着「同一时刻只让一张卡跑门」这条操作纪律**治不了它**，必须机械修。
//!
//! 修法（用户已裁定，本种子只钉这两条的不变量）：
//! ①（治产生）夹具子进程自登记 `(pid, pgid)` 到 `ORCH_GATE_FIXTURE_REGISTRY` 指向的文件；
//!    `run_gate_command` 两个出口在各自 `reap_gate_process_group(pgid)` **之后**读登记逐个
//!    `kill(-pgid)` 并容忍 ESRCH。**不改夹具的进程组语义**（那是被测对象）。
//! ②（治检出）门前快照 ppid=1 集合，门后取差集，非空则走既有 `MechCheckFailed` 通道
//!    （`mech.rs:116` `failure_event`，stage 固定 `"gate-orphans"`）逐条列出 pid 与可执行名。
//!
//! 设计说明（写在种子里，避免下一任重造）：
//! - `register_gate_fixture` **显式接收 env 原值**而不是自己去读进程环境。两个理由：
//!   ⑴ 「未设 env ⇒ 零写盘」这条能在**门内**被验证（门跑测试时该 env 是被设上的，
//!      隐式读环境的 API 在门内根本测不出 no-op 分支）；
//!   ⑵ 本种子的用例因此**绝不会误写进真实的门登记表**——治理动作自己不得成为新的失败源
//!      （B222-A0002 的血教训：种子的清理动作打红了与它无关的用例）。
//! - `GateOrphan` 只经 `GateOrphan::new(pid, executable)` 构造，**不做穷举 struct literal**：
//!   `GateResult` 被 `tests/gate_summary.rs:25-31` 与 `gate_summary_line.rs:22-27` 的穷举字面量
//!   钉死到今天不能加字段（与 B233 的 `WakeSpec` 同族教训），本卡不再制造同类冻结面。
//!   本种子只钉两个字段的**读**（`pid` / `executable`）。
//! - `reap_gate_fixture_registry` 的回执语义 = **登记表里被处理过的每一个 pgid**
//!   （被杀掉、已经消失、或因出生身份不匹配而跳过，都算处理过）——回执是给人核对的清单，
//!   不是「杀成功计数」。
//!
//! 本种子锁死的三条口径（**交付后不可再收窄，写在这里免得下一任撞墙**）：
//! - **`orphans_since` 被永久锁成「无过滤的全集差集」**：用例⑤要求一个与门毫无关系的裸 `sleep`
//!   （可执行名含 `sleep`）必须出现在差集里 ⇒ 交付之后**再也不能在 `orphans_since` 内部**
//!   按可执行名 / 归属 / uid 收窄（那会让本冻结种子永久红 = 整棒 FAIL）。
//!   ⇒ **收窄只能落在生产消费者侧**（`close.rs` / `collect.rs`：只把 uid == 当前 uid 且
//!   可执行名落在门工作集内的行升级成 `MechCheckFailed`，其余只写可核对的旁证）。
//!   这一层的契约写死在卡面 §2.0「作用域分层」与 §2.3「消费者过滤契约」，审查者按那里核。
//!   不这么分层的话，这道新机械门第一天就会被 launchd 按需拉起的用户态 agent 淹没，
//!   随即被人关掉——B227 §4.2 那句人工观察项就是这么废掉的。
//! - **`fn run_gate_command` 这个字面量在 `gate.rs` 全文必须唯一**（用例①第 ⓿ 条机械钉住）：
//!   本文件与 B227 冻结种子（`tests/gate_process_group_reaping.rs:33-39`）都用
//!   `split("fn run_gate_command").nth(1)` 定位窗口，新增代码或**注释**里多写一次该字面量，
//!   `nth(1)` 会取到那一处之后的一小段，两边的窗口一起挪错、红出与真实缺陷无关的信息。
//!   要在注释里引用请写 `run_gate_command`（不带 `fn`）。
//! - **拓扑普查整次失败是环境噪声，不是本卡的缺陷**：macOS 侧一次全表普查会因为机器上
//!   **任何一个**与本仓无关的「内核可见但拿不到出生身份的不透明僵尸」直接 bail
//!   （`wake.rs:3343` `record_darwin_fallback_census_row`）。冻结种子必须对它免疫 ⇒ 用例⑤
//!   取基线与取差集都**重试**、把最后一次错误贴进 panic 消息，而不是一次 `Err` 就判红。
//!   生产侧的同款要求是卡面 §2.3 的「消费者降级契约」：拿到 `Err` 记一条可核对旁证后跳过本次
//!   检查，**绝不允许把它 `?` 进 postmerge/record/collect 的失败路径**——治理动作自己不得
//!   成为新的合入阻断源。
//!
//! Negative mutations that must turn the named case red:
//! M1. 兜底收割只挂超时出口（正常 wait 出口漏收），或把新增收割行写在超时臂的 `bail!` **之后**
//!     -> `run_gate_command_reaps_the_registry_on_both_exits` 红。
//!     ⚠️ 源码扫描只能钉住「顺序」这一半：**语义等价的死代码错误必须由审查者重放 M1**
//!     （卡面 §4 点名题即此题），不要以为①绿就等于两个出口都真的执行了收割。
//! M2. 把 ESRCH（组已自然退出）当成错误上抛，或登记文件不存在就 Err
//!     -> `registry_reap_tolerates_missing_file_and_dead_groups` 红。
//! M3. 收割只 `kill(pid)` 而不是 `kill(-pgid)`（够不着夹具另开的那一组，等于没修）
//!     -> `registered_cross_group_fixture_dies_at_reap` 红。
//! M4. 登记无视 env（未设也写盘）
//!     -> `registration_is_a_noop_without_the_gate_env` 红；
//!     或登记表落点不是门日志目录下的 `{tag}-gate-{name}.fixtures`（抽成 run_gate_command 之外的
//!     helper、改用 `with_extension`、改用系统临时目录……）
//!     -> `run_gate_command_reaps_the_registry_on_both_exits` 红（④ 的格式串断言 + temp_dir 负向断言）。
//!     ⚠️ **不要指望 hygiene 门**：`tests/scratch_hygiene_enforced.rs` 实测只断言
//!     `scanned_files >= 100`、5 个白名单文件不得被报、以及自建夹具文件必须被报；
//!     `tests/test_tmp_hygiene.rs` 只 `println!` 逐条打印 finding。**全仓没有任何一处断言
//!     `findings` 为空**（`findings.is_empty()` 只出现在 `temp_path_hygiene.rs:27` 的反向用例），
//!     `mech::scan_repo_temp_path_hygiene` 也没有生产侧 fail 消费者
//!     ⇒ 落点脏**不会**让任何门变红，本卡对落点的机械防线**只有** ④ 的两条源码断言。
//! M5. 差集把门前就已经存在的 ppid=1 进程也报进来（把别人的历史孤儿算到本门头上）
//!     -> `orphan_diff_lists_only_new_ppid1_escapees` 红。
//! M6. 逃逸事件丢 stage 或 reason，或 reason 只给计数不给 pid/可执行名
//!     -> `escape_failure_event_carries_stage_and_reason` 红（E19：payload 必须含 stage+reason）。

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use orch_host::gate::{
    orphan_baseline, orphan_failure_event, orphans_since, reap_gate_fixture_registry,
    register_gate_fixture, GateOrphan, ORCH_GATE_FIXTURE_REGISTRY,
};
use orch_host::util::test_scratch_dir;

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
}

/// `kill(-pgid, 0)`：只要组里还有**任何**成员（含尚未被回收的僵尸）就返回 true。
fn group_alive(pgid: u32) -> bool {
    unsafe { libc_kill(-(pgid as i32), 0) == 0 }
}

/// 用例结束（含 panic 展开）时确定性收掉自建进程。
///
/// 本种子会**故意**制造 r65 实测的那两种逃逸形状；如果它自己漏一个出去，
/// 就成了 H106 的新孤儿源、把后续每一道门都毒化。所以每个自建进程都挂一个 Drop 兜底。
struct ReapOnDrop {
    pid: i32,
    whole_group: bool,
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let target = if self.whole_group { -self.pid } else { self.pid };
        unsafe {
            libc_kill(target, 9);
        }
    }
}

fn host_source(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("读 {rel} 失败: {error}"))
}

/// `run_gate_command` 函数体起始处的窗口，与 B227 冻结种子
/// （`tests/gate_process_group_reaping.rs:33-39`）取的是同一个 6000 字节窗口。
/// 截断按 UTF-8 边界回退：窗口只会更短，因此本文件的断言不会弱于 E9 的。
fn run_gate_command_region(src: &str) -> &str {
    let region = src
        .split("fn run_gate_command")
        .nth(1)
        .expect("gate.rs 必须含 fn run_gate_command（全部门形的唯一 spawn 咽喉）");
    let mut end = region.len().min(6000);
    while end > 0 && !region.is_char_boundary(end) {
        end -= 1;
    }
    &region[..end]
}

fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut ready: F) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn run_gate_command_reaps_the_registry_on_both_exits() {
    // 读真实生产源码的接线契约（裁定⑬范式，同 storage.rs
    // `production_guards_precede_each_entrys_first_effect_boundary`）：
    // 能力交付了没人调，是本仓反复复发的结构性缺陷；这条把「谁调它」钉在源码里。
    let gate = host_source("src/gate.rs");

    // ⓿ 窗口锚点唯一性。本文件与 B227 冻结种子（tests/gate_process_group_reaping.rs:33-39）
    //    都用 split("fn run_gate_command").nth(1) 取窗口：只要新增代码**或注释**里多出一次
    //    该字面量，nth(1) 就会取到那一处之后的一小段，两边的窗口一起挪错，
    //    红出来的信息（例如「必须 spawn 门命令」）与真实缺陷毫无关系。
    //    这一条把那种失效变成一句能读懂的红。要在注释里引用请写 run_gate_command（不带 fn）。
    assert_eq!(
        gate.matches("fn run_gate_command").count(),
        1,
        "`fn run_gate_command` 这个字面量在 gate.rs 全文必须只出现一次（即真正的函数定义那一处）——\
         它是本文件与 B227 冻结种子共用的窗口锚点"
    );

    let region = run_gate_command_region(&gate);

    let spawn_at = region
        .find(".spawn(")
        .expect("run_gate_command 必须 spawn 门命令");

    // ① 两个出口都要兜底收割夹具登记表（M1 前半）。
    let registry_calls = region.matches("reap_gate_fixture_registry(").count();
    assert!(
        registry_calls >= 2,
        "wait 与 timeout 两个出口都必须在自身进程组收割之后兜底收割夹具登记表\
         （reap_gate_fixture_registry 调用数 {registry_calls} < 2）"
    );

    // ② 死代码防线（M1 后半，源码顺序能测出来的那一半）：
    //    超时臂的收割行写在 bail! 之后就永远不会执行，而门照样「看起来接了线」。
    let last_bail = region
        .rfind("bail!(")
        .expect("run_gate_command 的超时臂必须 bail 出去");
    let reaps_before_bail = region[..last_bail]
        .matches("reap_gate_fixture_registry(")
        .count();
    assert!(
        reaps_before_bail >= 2,
        "两个出口的登记表兜底收割都必须写在超时 bail! **之前**——写在 bail! 之后即是永不执行的死代码，\
         这是本卡最可能的实现错误。实际 bail! 前只有 {reaps_before_bail} 次"
    );

    // ③ 登记文件位置必须在 spawn 之前以 ORCH_GATE_FIXTURE_REGISTRY 交给子进程。
    assert_eq!(
        ORCH_GATE_FIXTURE_REGISTRY, "ORCH_GATE_FIXTURE_REGISTRY",
        "该 env 名是门（写）与夹具子进程（读）之间的跨进程线，值必须与常量名一致，便于人肉 grep 与实测"
    );
    let env_at = region
        .find("ORCH_GATE_FIXTURE_REGISTRY")
        .expect("run_gate_command 必须把登记文件位置以 ORCH_GATE_FIXTURE_REGISTRY 注入门命令");
    assert!(
        env_at < spawn_at,
        "登记文件位置必须在 .spawn( 之前配置到 Command 上（builder 语义），否则子进程读不到"
    );

    // ④ 登记文件落点必须是门日志目录下的确定性命名（M4 后半）。
    //    作用域刻意取 `..spawn_at`：env 必须在 spawn 之前挂到 Command 上，所以路径的计算
    //    **必然**落在这一段里；这个窗口小而稳（当前 ~800 字节，全部在 run_gate_command 体内），
    //    不会被函数尾部/测试模块的无关代码漂移影响。
    //    ⚠️ 这里钉的是**格式串字面量**，与 gate.rs:212 既有的 format!("{tag}-gate-{name}.log")
    //    同型同处：把路径算式抽成窗口外的 helper、或改写成 log_path.with_extension("fixtures")，
    //    都会让本条红——卡面 §2.4 第 0 条已把这行的**逐字写法**定死，按卡面写即可。
    //    之所以必须钉这么死：hygiene 全树扫描只打印不判红（见文件头 M4 的实测说明），
    //    落点脏在本仓**没有**任何别的机械防线。
    let pre_spawn = &region[..spawn_at];
    assert!(
        pre_spawn.contains("{tag}-gate-{name}.fixtures"),
        "登记表必须与门日志同目录同前缀、用与 gate.rs:212 同款的确定性格式串命名，\
         且该格式串必须内联在 run_gate_command 体内、spawn 之前（收割后保留为证据）"
    );
    assert!(
        !pre_spawn.contains("temp_dir("),
        "登记表落点不得来自系统临时目录——门日志目录是确定性的、可归档的，\
         系统临时目录既撞名又会被 Tier F 沙箱拒绝"
    );

    // ⑤ 自护：B227/E9（tests/gate_process_group_reaping.rs:41-77）的四个条件必须在
    //    **同一个 6000 字节窗口内**继续成立。本卡在 run_gate_command 体内加行，
    //    把 E9 挤出窗口是最容易的翻车方式。
    let pg_at = region
        .find("process_group(0)")
        .expect("E9 自护：run_gate_command 必须为门命令建立私有进程组 process_group(0)");
    assert!(
        pg_at < spawn_at,
        "E9 自护：process_group(0) 必须在 .spawn( 之前配置"
    );
    let group_calls = region.matches("reap_gate_process_group(").count();
    assert!(
        group_calls >= 2,
        "E9 自护：wait 与 timeout 两个出口都必须收割门自身进程组（实际 {group_calls} < 2）"
    );
    assert!(
        gate.contains("fn reap_gate_process_group"),
        "E9 自护：gate.rs 必须保留 reap_gate_process_group"
    );
    assert!(
        gate.contains("ESRCH") || gate.contains("esrch"),
        "E9 自护：整组收割必须显式容忍 ESRCH"
    );

    // ⑥ 窗口零膨胀：本卡新增的收割函数必须定义在 run_gate_command **之前**
    //    （与 reap_gate_process_group 相邻），否则 6000 字节窗口被函数体撑开、⑤ 当场破。
    let head = gate
        .split("fn run_gate_command")
        .next()
        .expect("gate.rs 必须有 run_gate_command 之前的前缀");
    assert!(
        head.contains("fn reap_gate_fixture_registry"),
        "reap_gate_fixture_registry 必须定义在 fn run_gate_command 之前，保证 E9 的 6000 字节窗口零膨胀"
    );

    // ⑦ 线的另一端：夹具子进程自登记必须真的存在于 wake.rs。
    //    登记表永远为空的话，①—⑥ 全绿也等于一行没修。
    let wake = host_source("src/wake.rs");
    assert!(
        wake.contains("register_gate_fixture("),
        "wake.rs 的 b177 夹具子进程入口必须调用 gate::register_gate_fixture 完成自登记\
         （单点注入覆盖全部现有与未来 spawn 点；等价的父侧包装函数同样满足本断言）"
    );
}

#[test]
fn registry_reap_tolerates_missing_file_and_dead_groups() {
    // M2：绝大多数门根本没有夹具，兜底收割不得因此变成新的门失败源。
    let dir = test_scratch_dir("b239-reap-tolerant");

    let missing = dir.join("absent.fixtures");
    let empty = reap_gate_fixture_registry(&missing)
        .expect("登记文件不存在必须友好返回 Ok（没有夹具的门是常态，不是错误）");
    assert!(empty.is_empty(), "不存在的登记文件必须返回空回执，实际 {empty:?}");

    // 已经自然退出的组：kill(-pgid) 会得到 ESRCH，与 gate.rs:23-38 同语义地容忍。
    let mut dead = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("拉起一次性子进程失败");
    let dead_pgid = dead.id();
    dead.wait().expect("等待一次性子进程失败");

    let registry = dir.join("r90-gate-testFast.fixtures");
    register_gate_fixture(Some(registry.as_os_str()), dead_pgid, dead_pgid)
        .expect("登记失败")
        .expect("设了登记表位置就必须落盘");

    let reaped = reap_gate_fixture_registry(&registry)
        .expect("组已自然退出不是错误：ESRCH 必须被容忍（否则每道干净的门都会红）");
    assert!(
        reaped.contains(&dead_pgid),
        "回执必须逐条列出登记表里被处理过的 pgid（含已消失的），便于人工核对；实际 {reaped:?}"
    );

    let again = reap_gate_fixture_registry(&registry).expect("重复收割必须幂等成功");
    assert!(
        again.contains(&dead_pgid),
        "幂等：同一份登记表再收一次仍然给出同一张清单；实际 {again:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn registered_cross_group_fixture_dies_at_reap() {
    // M3 实弹：复刻 r65 实测的真实逃逸形状——夹具**自己**开新进程组（b177 家族的刻意设计，
    // 测的就是跨组收割），于是门自身 pgid 的 kill(-pgid) 够不着它。
    // 组里额外留一个长命子代，专门抓「只 kill 组长 pid、不 kill(-pgid) 整组」这种半修。
    let dir = test_scratch_dir("b239-cross-group");
    let registry = dir.join("r90-gate-testFast.fixtures");
    let ready = dir.join("ready");
    let body = format!(
        "trap '' TERM; ( trap '' TERM; exec sleep 300 ) & : > '{}'; while :; do sleep 300; done",
        ready.display()
    );
    let mut child = Command::new("/bin/sh")
        .args(["-c", body.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("拉起跨组夹具失败");
    let pgid = child.id();
    let _guard = ReapOnDrop {
        pid: pgid as i32,
        whole_group: true,
    };

    assert!(
        wait_for(Duration::from_secs(20), || ready.exists()),
        "跨组夹具未在期限内就绪，本用例证明不了任何事"
    );
    assert!(
        group_alive(pgid),
        "收割前夹具组必须活着——否则本用例是空洞通过"
    );

    let written = register_gate_fixture(Some(registry.as_os_str()), pgid, pgid)
        .expect("登记失败")
        .expect("设了登记表位置就必须落盘");
    assert_eq!(written, registry, "登记回执必须给出真实落盘路径");

    let reaped = reap_gate_fixture_registry(&registry).expect("收割登记表失败");
    assert!(
        reaped.contains(&pgid),
        "回执必须逐条列出被处理的进程组；实际 {reaped:?}"
    );

    // 不能 wait() 阻塞：修法若失效，本用例会把门挂死——正是本卡要治的病。
    let gone = wait_for(Duration::from_secs(20), || {
        let _ = child.try_wait();
        !group_alive(pgid)
    });
    let _ = child.try_wait();
    assert!(
        gone,
        "登记过的跨组夹具必须在收割中**整组**消失（pgid={pgid}）——\
         这正是门自身 pgid 的 kill(-pgid) 够不着的那一组；只杀组长会让长命子代活下来"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn registration_is_a_noop_without_the_gate_env() {
    // M4 前半：直接 `cargo test` 的开发者不带这个 env，登记必须是彻底的 no-op——
    // 一个字节都不落盘，也不因此报错。
    let dir = test_scratch_dir("b239-registry-env-gate");
    let registry = dir.join("r90-gate-testFast.fixtures");

    assert!(
        register_gate_fixture(None, 4242, 4242)
            .expect("未设登记表位置时登记必须成功返回")
            .is_none(),
        "未设 ORCH_GATE_FIXTURE_REGISTRY 时不得给出落盘路径"
    );
    assert!(
        register_gate_fixture(Some(OsStr::new("")), 4242, 4242)
            .expect("空值等同未设，必须成功返回")
            .is_none(),
        "空值等同未设：不得把空路径当成合法登记表"
    );
    assert!(!registry.exists(), "no-op 分支不得创建登记表");
    assert_eq!(
        fs::read_dir(&dir).expect("读 scratch 目录失败").count(),
        0,
        "未设 env 时登记必须零写盘"
    );

    // 反向自护：设了位置就必须真的落盘，否则本用例会因「helper 什么都不做」而空洞通过。
    let written = register_gate_fixture(Some(registry.as_os_str()), 4242, 4243)
        .expect("登记失败")
        .expect("设了登记表位置就必须落盘");
    assert_eq!(written, registry);
    let text = fs::read_to_string(&registry).expect("读登记表失败");
    assert!(
        text.contains("4242") && text.contains("4243"),
        "登记行必须同时留下 pid 与 pgid（收割按 pgid，归责按 pid）；实际 {text:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn orphan_diff_lists_only_new_ppid1_escapees() {
    // M5：门前基线 → 门后差集。基线里已经存在的 ppid=1 进程（别人的历史孤儿、系统守护进程）
    // 绝不能算到本门头上，否则这道机械门第一天就会被噪声淹没、随即被人关掉。
    //
    // ⚠️ 本用例对**拓扑普查整次失败**免疫：macOS 侧一次全表普查会因为机器上任何一个与本仓
    //    无关的「内核可见但拿不到出生身份的不透明僵尸」直接 bail（wake.rs:3343
    //    record_darwin_fallback_census_row）。那是环境噪声，不是实现缺陷——冻结种子不能因此
    //    在某些机器上永久红。所以取基线与取差集都重试，并把最后一次错误贴进 panic 消息；
    //    「实现根本取不到差集」这条真缺陷仍然会在重试窗口耗尽后判红。
    let mut baseline_error: Option<String> = None;
    let mut sampled: Option<BTreeSet<u32>> = None;
    for _ in 0..10 {
        match orphan_baseline() {
            Ok(set) => {
                sampled = Some(set);
                break;
            }
            Err(error) => {
                baseline_error = Some(format!("{error:#}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    let baseline: BTreeSet<u32> = sampled
        .unwrap_or_else(|| panic!("取 ppid=1 基线失败（已重试 10 次）：{baseline_error:?}"));

    let dir = test_scratch_dir("b239-orphan-diff");
    let pidfile = dir.join("escapee.pid");
    // 父 sh 退出后 `sleep` 被 reparent 到 init ⇒ ppid=1，正是 r65 抓到的那个形状。
    let script = format!("sleep 120 & printf '%s' \"$!\" > '{}'", pidfile.display());
    let status = Command::new("/bin/sh")
        .args(["-c", script.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("拉起逃逸体失败");
    assert!(status.success(), "构造逃逸体的 shell 必须正常退出");

    let escapee: u32 = fs::read_to_string(&pidfile)
        .expect("读逃逸体 pid 失败")
        .trim()
        .parse()
        .expect("逃逸体 pid 必须可解析");
    let _guard = ReapOnDrop {
        pid: escapee as i32,
        whole_group: false,
    };

    let mut escapees = Vec::new();
    let mut diff_error: Option<String> = None;
    let found = wait_for(Duration::from_secs(30), || match orphans_since(&baseline) {
        Ok(rows) => {
            escapees = rows;
            escapees.iter().any(|orphan| orphan.pid == escapee)
        }
        Err(error) => {
            // 见本用例开头：普查整次失败是环境噪声，重试而不是当场判红。
            diff_error = Some(format!("{error:#}"));
            false
        }
    });
    assert!(
        found,
        "门后新生的 ppid=1 逃逸体必须出现在差集里（pid={escapee}）；实际 {escapees:?}，\
         最后一次快照错误 {diff_error:?}"
    );

    for orphan in &escapees {
        assert!(
            !baseline.contains(&orphan.pid),
            "基线里已存在的 ppid=1 进程不得进差集（pid={} exe={}）——差集是「本门新增」而不是「当前全部」",
            orphan.pid,
            orphan.executable
        );
    }
    let hit = escapees
        .iter()
        .find(|orphan| orphan.pid == escapee)
        .expect("差集必须含逃逸体");
    assert!(
        hit.executable.contains("sleep"),
        "差集条目必须带内核给出的可执行名（不是 argv、不是空串），否则事件里的 reason 无法定位；实际 {:?}",
        hit.executable
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn escape_failure_event_carries_stage_and_reason() {
    // M6：差集非空必须走既有 MechCheckFailed 通道（mech.rs:116 failure_event），
    // 不新造事件类型、不改 mech.rs。E19（tests/mech_event.rs）要求 payload 含 stage 与 reason。
    let orphans = vec![
        GateOrphan::new(4242, "orch_host-b1770ad"),
        GateOrphan::new(4243, "sleep"),
    ];
    let event = orphan_failure_event("B903", "r90", &orphans);

    assert_eq!(
        event.kind, "MechCheckFailed",
        "必须复用既有 MechCheckFailed 通道，不新造事件类型"
    );
    assert_eq!(event.actor, "runtime:orch");
    assert_eq!(event.task_id.as_deref(), Some("B903"));
    assert_eq!(event.round.as_deref(), Some("r90"));

    let payload = event.payload.expect("MechCheckFailed 必须带 payload");
    assert_eq!(
        payload["stage"], "gate-orphans",
        "stage 固定为 gate-orphans，使这类失败在账本里可被一条查询捞干净"
    );
    let reason = payload["reason"]
        .as_str()
        .expect("reason 必须是字符串（E19）");
    for (pid, executable) in [("4242", "orch_host-b1770ad"), ("4243", "sleep")] {
        assert!(
            reason.contains(pid) && reason.contains(executable),
            "reason 必须**逐条**列出 pid 与可执行名（只给个计数等于没证据）；缺 {pid}:{executable}，实际 {reason:?}"
        );
    }

    // 生产消费者：差集必须真的接在门循环上。「能力交付了没人调」是本仓反复复发的结构性缺陷，
    // 也正是 B227 §4.2 那句人工观察项从未被执行过的原因。
    // 本断言只钉**存在**；「每一个门循环都被括起来」由卡面 requiredEvidence 的
    // orphan-diff-brackets-every-production-gate-loop 由审查者逐点枚举。
    for rel in ["src/close.rs", "src/collect.rs"] {
        let src = host_source(rel);
        assert!(
            src.contains("orphan_baseline("),
            "{rel} 的门循环前必须取 ppid=1 基线"
        );
        assert!(
            src.contains("orphans_since("),
            "{rel} 的门循环后必须取差集"
        );
        assert!(
            src.contains("orphan_failure_event("),
            "{rel} 差集非空必须落 MechCheckFailed，不能只打印到 stdout"
        );
    }
}
