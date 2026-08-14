//! B227 seeded-red contract: 门执行进程组化收割——孤儿测试进程零新增（H106）。
//!
//! Expected red: compile。`orch_host::gate::reap_gate_process_group` 在 B227 之前不存在
//! （编译红锚定；r64 实测发现 assertion 形态的 oracle 会跑全量门，而门在「已 plan 未签核」
//! 窗口结构性必红——H101/H85 同族第三窗口，故本种子采用编译红短路）。
//! gate.rs 现状 `cmd.spawn()` 无进程组隔离、超时只 `child.kill()` 杀直接子进程——
//! 测试自 spawn 的孙进程 reparent 到 ppid=1，正是 r63 两个存活 8.7h/10.6h 孤儿、
//! 收取门挂死 53.9 分钟的出生机制。
//!
//! 本种子按 storage.rs `production_guards_precede_each_entrys_first_effect_boundary`
//! （storage.rs:743）的读源码范式钉住收割接线；行为级夹具由执行者在卡内补充
//! （writeSet 含新测试文件），种子只钉不变量，避免对内部可见性做过强预设。
//!
//! Negative mutations that must turn the named case red:
//! M1. 去掉 spawn 前的 process_group 设定
//!     -> `gate_spawn_creates_a_private_process_group` 红。
//! M2. 去掉 wait/timeout 之后的整组收割
//!     -> `gate_reaps_the_whole_group_after_wait_and_timeout` 红。
//! M3. 收割只挂在超时分支、正常退出分支漏收
//!     -> `gate_reaps_the_whole_group_after_wait_and_timeout` 红（两分支都要有收割标记）。

// 编译红锚定：本卡必须交付 pub fn reap_gate_process_group（对 -pgid SIGKILL、忽略 ESRCH）。
#[allow(unused_imports)]
use orch_host::gate::reap_gate_process_group;

fn gate_source() -> String {
    std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/gate.rs"),
    )
    .expect("读 gate.rs")
}

fn run_gate_command_region(src: &str) -> &str {
    let region = src
        .split("fn run_gate_command")
        .nth(1)
        .expect("gate.rs 必须含 run_gate_command（全部门形的唯一 spawn 咽喉）");
    &region[..region.len().min(6000)]
}

#[test]
fn gate_spawn_creates_a_private_process_group() {
    // M1：process_group 设定必须出现在 spawn 之前（child pid = pgid）。
    let src = gate_source();
    let region = run_gate_command_region(&src);
    let pg_at = region
        .find("process_group(0)")
        .expect("run_gate_command 必须为门命令建立私有进程组 process_group(0)");
    let spawn_at = region
        .find(".spawn(")
        .expect("run_gate_command 必须 spawn 门命令");
    assert!(
        pg_at < spawn_at,
        "process_group(0) 必须在 .spawn( 之前配置（builder 上设定）"
    );
}

#[test]
fn gate_reaps_the_whole_group_after_wait_and_timeout() {
    // M2/M3：整组收割（对 -pgid 的 SIGKILL，忽略 ESRCH）必须在 wait 与 timeout
    // 两个出口都执行——收割函数名钉为 reap_gate_process_group，出现 ≥2 次调用。
    let src = gate_source();
    let region = run_gate_command_region(&src);
    let calls = region.matches("reap_gate_process_group(").count();
    assert!(
        calls >= 2,
        "wait 与 timeout 两个出口都必须收割进程组（reap_gate_process_group 调用数 {calls} < 2）"
    );
    assert!(
        src.contains("fn reap_gate_process_group"),
        "gate.rs 必须定义 reap_gate_process_group（对 -pgid SIGKILL、忽略 ESRCH）"
    );
    assert!(
        src.contains("ESRCH") || src.contains("esrch"),
        "整组收割必须显式容忍 ESRCH（组内进程已自然退出不是错误）"
    );
}
