//! ═══ 红种子契约 · B21 ═══（落位: orch/crates/orch-host/tests/cursor_adapter.rs，逐字节复制）
//! 预期红（redForm: compile）：adapter::supported_builtins 尚不存在（E0425，文件级编译红）。
//! 规格来源：research/r13-R2-cursor-cli-capability.md（R2 调研，官方文档实核版）——
//!   cursor-agent headless=-p/--print · --output-format stream-json · --force 自动放行。
//! 变异清单（E9 下界）: M1 supported_builtins 缺 "cursor" → ① 红；M2 argv 省略 --force → ③ 红；M3 program 写成 "cursor" → ② 红
use std::path::Path;

use orch_host::adapter;

fn argv(cmd: &std::process::Command) -> Vec<String> {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn builtins_include_cursor_and_existing_three() {
    // ① cursor 成为一等内置档，且不挤掉既有三档
    let b = adapter::supported_builtins();
    for name in ["opencode", "codex", "claude", "cursor"] {
        assert!(b.contains(&name), "missing builtin: {name}");
    }
}

#[test]
fn cursor_program_and_headless_flag() {
    // ② program=cursor-agent（安装器软链名）+ -p headless + prompt 原样入参
    let cmd = adapter::build_command("cursor", "hello", Path::new("/tmp"), None, None).unwrap();
    let a = argv(&cmd);
    assert_eq!(a[0], "cursor-agent");
    assert!(a.contains(&"-p".to_string()) || a.contains(&"--print".to_string()));
    assert!(a.contains(&"hello".to_string()));
}

#[test]
fn cursor_stream_json_and_force() {
    // ③ stream-json 结构化输出 + --force 无人值守放行（R2 §5：--yolo 为其别名，取正名）
    let cmd = adapter::build_command("cursor", "hi", Path::new("/tmp"), None, None).unwrap();
    let a = argv(&cmd);
    let i = a.iter().position(|s| s == "--output-format").expect("--output-format missing");
    assert_eq!(a[i + 1], "stream-json");
    assert!(a.contains(&"--force".to_string()));
}

#[test]
fn cursor_workdir_is_current_dir() {
    // ④ workdir 经 current_dir 生效（与 codex/claude 内置档同构；worktree 任务的落点保证）
    let wd = Path::new("/tmp");
    let cmd = adapter::build_command("cursor", "hi", wd, None, None).unwrap();
    assert_eq!(cmd.get_current_dir(), Some(wd));
}
