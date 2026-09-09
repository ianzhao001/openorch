//! B232 seeded-red contract: trial-cache 生命周期治理 + 试合并门日志采集不自我覆写
//! （H119，P0 磁盘治理，H57 第三个面；用户 2026-08-05 裁定「一卡三件事」）。
//!
//! Expected red: compile。`orch_host::buildcache::{sweep_trial_cache, TrialCacheSweepReport}`
//! 与 `orch_host::gate::gate_log_sinks` 在 B232 之前不存在——两个交付面各自的编译红锚定。
//!
//! 契约背景（r64 收轮实测 + r65 轮前代码取证）：
//! ① `.cowork-temp/trial-cache` 累积 114 GB（占项目 63%）：slot 上限 4（buildcache.rs:23）
//!    但 **generation 无上限、从不删除**（buildcache.rs:5-7「remains as evidence」）；
//!    `sites gc` 判据只有账本事件 + worktree 注册表（sites.rs:1347-1430），两者皆不含它；
//!    `sweep-scratch` 作用域仅 `orch/target/test-tmp`（util.rs:84-95）。
//! ② B208 暖启动生产 100% 未证成的可读根因：gate.rs:176-177 用**同一日志文件的两个独立 fd**
//!    （stdout `O_TRUNC` 从偏移 0 覆写、stderr `O_APPEND`），cargo 的 `Compiling` 行走 stderr
//!    写在文件前段，被 `cargo test` 海量测试 stdout 从偏移 0 覆写 ⇒ `assess_warm_log`
//!    （buildcache.rs:881-906）永远读不到 `Compiling orch-core/orch-host`，每道门白付一次
//!    完整 cold build。r63 验收走的是 `benchmark_gate`（`--no-run` + 字符串拼接）所以通过——
//!    「验收时成立」不等于「生产中成立」。
//!
//! Negative mutations that must turn the named case red:
//! M1. sweep 连最新 generation 一起删（把热缓存也扫掉，暖启动永无机会）
//!     -> `sweep_keeps_latest_generation_and_removes_older` 红。
//! M2. sweep 越界删 slot 的 `source/`（试合并 worktree 被扫，正在跑的门直接失败）
//!     -> `sweep_keeps_latest_generation_and_removes_older` 红（source 哨兵文件断言）。
//! M3. sweep 不幂等（第二遍继续报删除/报错）
//!     -> `sweep_is_idempotent` 红。
//! M4. 日志采集回退双 fd 覆写（stderr 前段被 stdout 从偏移 0 覆写）
//!     -> `gate_log_captures_both_streams_without_overwrite` 红。
//!
//! 生产接线（① collect.rs 试合并门改用 `gate_log_sinks`；② round close 前置调用
//! `sweep_trial_cache`，与 B226 sweep-scratch 同型；③ `orch sites sweep-trial-cache`
//! CLI 兜底入口）由卡面 requiredEvidence 钉住，不在本种子内断言。

use std::fs;
use std::process::Command;

use orch_host::buildcache::{sweep_trial_cache, TrialCacheSweepReport};
use orch_host::gate::gate_log_sinks;
use orch_host::util::test_scratch_dir;

/// 构造一个最小 trial-cache 布局：
/// `<root>/.cowork-temp/trial-cache/slots/slot-00/{source/KEEP.txt, generations/generation-00000N/target/blob.bin}`
fn fixture_root(tag: &str, generations: &[u32]) -> std::path::PathBuf {
    let root = test_scratch_dir(tag);
    let slot = root.join(".cowork-temp/trial-cache/slots/slot-00");
    fs::create_dir_all(slot.join("source")).expect("建 source 失败");
    fs::write(slot.join("source/KEEP.txt"), b"trial worktree sentinel").expect("写 source 哨兵失败");
    for generation in generations {
        let target = slot.join(format!("generations/generation-{generation:06}/target"));
        fs::create_dir_all(&target).expect("建 generation 失败");
        fs::write(target.join("blob.bin"), vec![0u8; 4096]).expect("写 generation 负载失败");
    }
    root
}

#[test]
fn sweep_keeps_latest_generation_and_removes_older() {
    // M1/M2：只删旧代，保最新代与 slot 的 source worktree。
    let root = fixture_root("b232-sweep-basic", &[1, 2, 3]);
    let report: TrialCacheSweepReport =
        sweep_trial_cache(&root, 1).expect("sweep 不得失败");
    assert_eq!(report.removed, 2, "三代保一 ⇒ 恰删两代");
    assert!(report.freed_bytes > 0, "删除必须回报字节数");
    let slot = root.join(".cowork-temp/trial-cache/slots/slot-00");
    assert!(
        slot.join("generations/generation-000003/target/blob.bin").exists(),
        "最新 generation 必须保留"
    );
    assert!(
        !slot.join("generations/generation-000001").exists()
            && !slot.join("generations/generation-000002").exists(),
        "旧 generation 必须删除"
    );
    assert!(
        slot.join("source/KEEP.txt").exists(),
        "slot 的 source（试合并 worktree）不在清扫范围内"
    );
}

#[test]
fn sweep_is_idempotent() {
    // M3：第二遍零删除、零错误。
    let root = fixture_root("b232-sweep-idem", &[1, 2]);
    sweep_trial_cache(&root, 1).expect("首遍 sweep 不得失败");
    let second = sweep_trial_cache(&root, 1).expect("第二遍 sweep 不得失败");
    assert_eq!(second.removed, 0, "幂等：第二遍必须零删除");
    assert_eq!(second.freed_bytes, 0, "幂等：第二遍必须零回收");
}

#[test]
fn sweep_missing_root_is_ok_zero() {
    // 没有 trial-cache（全新仓/已清理）不是错误——round close 接线不得因此失败。
    let root = test_scratch_dir("b232-sweep-missing");
    let report = sweep_trial_cache(&root, 1).expect("缺目录必须是 Ok 而非错误");
    assert_eq!(report.removed, 0);
}

#[test]
fn gate_log_captures_both_streams_without_overwrite() {
    // M4：stderr 先写的行（cargo 的 `Compiling …` 就在这个位置）必须在 stdout 洪流
    // 之后仍然可读——这是 warm 证成能否成立的机械前提。
    let root = test_scratch_dir("b232-gate-log");
    let log_path = root.join("gate.log");
    let (out_sink, err_sink) = gate_log_sinks(&log_path).expect("开日志汇失败");
    let status = Command::new("sh")
        .arg("-c")
        // 先写 stderr 哨兵行（模拟 Compiling 行落在文件前段），再灌 128KiB stdout。
        .arg("echo 'Compiling orch-core v0.0.0 (GUARD-SENTINEL)' 1>&2; dd if=/dev/zero bs=1024 count=128 2>/dev/null | tr '\\0' 'x'")
        .stdout(out_sink)
        .stderr(err_sink)
        .status()
        .expect("spawn 失败");
    assert!(status.success(), "夹具命令必须成功");
    // 子进程已退出 ⇒ 两条流的全部字节都已在文件里（sink 是子进程持有的 fd，无父侧缓冲）。
    let log = fs::read_to_string(&log_path).expect("读日志失败");
    assert!(
        log.contains("Compiling orch-core v0.0.0 (GUARD-SENTINEL)"),
        "stderr 先写的 Compiling 行不得被 stdout 覆写"
    );
    assert!(
        log.len() >= 128 * 1024,
        "stdout 体量必须完整落盘（实际 {} 字节）",
        log.len()
    );
}
