//! B244 · 门心跳 sidecar 契约（watchdog addendum §2.2 的 r67 修正形：**不写门日志**）。
//!
//! 首红形态：**compile** —— `gate_heartbeat_sidecar_path` / `spawn_gate_heartbeat` /
//! `GateHeartbeatHandle` 尚不存在（`error[E0432]`）。种子字节冻结；改种子 = 整棒 FAIL。
//!
//! 落点裁定（用户 2026-08-08）：心跳写独立 sidecar `{tag}-gate-{name}.hb`，
//! **禁止**写进门日志——`gate_log_sinks` 给子进程的是 truncate 打开 + 共享游标句柄
//! （B232 冻结），父进程追加的字节会被子进程按旧偏移的后续写覆盖损坏。
//!
//! 用例 ↔ 验收映射：
//!   M1 sidecar_path_shape                    —— 命名与门日志同目录同前缀，后缀 .hb
//!   M2 heartbeat_lines_are_fixed_form_non_json —— ≥2 行、单调 t=+、定长有界、非 JSON
//!      （receipt 判定器对可解析含 error 键的 JSON 行直接 Err——心跳行必须解析失败）
//!   M3 stop_before_first_interval_writes_nothing —— 快门零心跳行（不制造噪声基线）
//!   M4 emitter_is_wired_into_the_gate_spawn_window —— 接线存在性：`run_gate_command`
//!      之后 6000 字节窗口内、`.spawn(` 之后出现 `spawn_gate_heartbeat(`（字节级检索，
//!      与 E9/B239 同锚：`fn run_gate_command` 全文首现分割）
//!
//! 本种子不新增任何对窗口内容的其它字节断言——窗口纪律由 E9 与 B239 种子自护。
//! scratch 一律落 `orch/target/test-tmp`（H38），测试自建现场自己清。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use orch_host::gate::{gate_heartbeat_sidecar_path, spawn_gate_heartbeat, GateHeartbeatHandle};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = repo_root()
        .join("orch/target/test-tmp")
        .join(format!("b244-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch 应可创建");
    dir
}

fn cleanup(tag: &str) {
    let _ = std::fs::remove_dir_all(
        repo_root()
            .join("orch/target/test-tmp")
            .join(format!("b244-{tag}-{}", std::process::id())),
    );
}

#[test]
fn sidecar_path_shape() {
    let dir = Path::new("/tmp/whatever");
    let path = gate_heartbeat_sidecar_path(dir, "B9-A0001", "testFast");
    assert_eq!(path, dir.join("B9-A0001-gate-testFast.hb"));
    // 与门日志（{tag}-gate-{name}.log）同目录同前缀——轮转/归档按同 tag 同规则处置。
    let hb = path.file_name().and_then(|n| n.to_str()).expect("有文件名");
    assert!(hb.ends_with(".hb") && hb.starts_with("B9-A0001-gate-testFast"));
}

#[test]
fn heartbeat_lines_are_fixed_form_non_json() {
    let dir = scratch_dir("m2");
    let sidecar = gate_heartbeat_sidecar_path(&dir, "b244", "m2");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 3")
        .spawn()
        .expect("假门子进程应能拉起");
    let handle: GateHeartbeatHandle =
        spawn_gate_heartbeat(&sidecar, child.id(), Duration::from_millis(400));
    std::thread::sleep(Duration::from_millis(1500));
    handle.stop();
    let _ = child.kill();
    let _ = child.wait();

    let text = std::fs::read_to_string(&sidecar).expect("sidecar 应已落盘");
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(lines.len() >= 2, "1.5s/400ms 间隔应产出 ≥2 行心跳，实得 {}", lines.len());
    let mut last_t: i64 = -1;
    for line in &lines {
        assert!(line.starts_with("[gate-hb] t=+"), "心跳行前缀必须定形: {line}");
        assert!(line.len() <= 160, "心跳行必须定长有界（≤160B）: {line}");
        // 非 JSON：wake.rs 的 backend receipt 判定器对「可解析 JSON 且含 error 键」的行
        // 直接判 Err——心跳行必须让 serde_json 解析失败，且不含 error 字样。
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_err(),
            "心跳行不得是可解析 JSON: {line}"
        );
        assert!(!line.contains("error"), "心跳行不得含 error 字样: {line}");
        let after = &line["[gate-hb] t=+".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let t: i64 = digits.parse().expect("t=+NNN 应为十进制秒");
        assert!(t >= last_t, "t 必须单调不减: {line}");
        last_t = t;
    }
    cleanup("m2");
}

#[test]
fn stop_before_first_interval_writes_nothing() {
    let dir = scratch_dir("m3");
    let sidecar = gate_heartbeat_sidecar_path(&dir, "b244", "m3");
    let handle = spawn_gate_heartbeat(&sidecar, std::process::id(), Duration::from_millis(400));
    std::thread::sleep(Duration::from_millis(120));
    handle.stop();
    let bytes = std::fs::read(&sidecar).unwrap_or_default();
    assert!(
        bytes.is_empty(),
        "首个间隔未到即停 ⇒ 零心跳行（快门不得制造噪声），实得 {} 字节",
        bytes.len()
    );
    cleanup("m3");
}

#[test]
fn emitter_is_wired_into_the_gate_spawn_window() {
    // 字节级检索，避开 6000 字节切片可能落在多字节字符中间的 char-boundary 陷阱。
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
    let src = include_str!("../src/gate.rs").as_bytes();
    let anchor = find(src, b"fn run_gate_command").expect("gate.rs 应含 run_gate_command 定义");
    let region_start = anchor + b"fn run_gate_command".len();
    let region_end = (region_start + 6000).min(src.len());
    let window = &src[region_start..region_end];
    let spawn_at = find(window, b".spawn(").expect("窗口内应有门命令 spawn 点");
    let hb_at = find(window, b"spawn_gate_heartbeat(")
        .expect("run_gate_command 窗口内必须接线 spawn_gate_heartbeat");
    assert!(
        hb_at > spawn_at,
        "心跳必须在门命令 spawn 之后启动（先有 child pid 才有观测对象）"
    );
}
