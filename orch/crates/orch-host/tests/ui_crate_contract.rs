//! B99 契约种子 · UI crate 骨架与依赖引入护栏（r45）
//!
//! B331 演进已 Recorded 的落位测试：保留原 UI/pin 断言，并把零污染范围扩至 CLI。
//! 预期红形态：**assertion**（`orch/crates/orch-ui/Cargo.toml` 尚不存在 → 每个用例都断言失败）
//!
//! ## 契约（HITL#2 已批准引入 ratatui/crossterm/axum/tokio，但必须带护栏）
//!
//! 1. `orch/Cargo.toml` 的 `members` 必须含 `crates/orch-ui`
//! 2. `orch/crates/orch-ui/Cargo.toml` 必须存在，且四个新依赖**全部精确钉死**
//!    （`=x.y.z` 形式；出现 `^`、`~`、`*` 或裸版本号即违约）
//! 3. UI 依赖不能进入 core/host/CLI；core 的 protocol-io 可声明可选 Tokio，默认 CLI 实际依赖图仍必须无 Tokio/协议 SDK/UI。
//!
//! UI/pin 判据基于 manifest，默认隔离另查实际 Cargo 依赖图；不依赖 UI 功能实现，因此在
//! 「crate 建好但功能为空」时即可转绿，符合 B99 的交付边界。
//!
//! ## 负向变异清单（REPORT §5 逐条自证）
//! 1. 任一新依赖改成浮动版本（如 `ratatui = "0.29"`）→ `ui_crate_pins_exact_versions` 红
//! 2. 把 `ratatui` 加进 orch-host/Cargo.toml → `core_and_host_stay_unpolluted` 红
//! 3. 从 workspace members 摘掉 crates/orch-ui → `workspace_includes_ui_crate` 红
//! 4. 删除 orch-ui/Cargo.toml → 对应存在性/依赖断言红
//! 5. 将 protocol-io 带入默认 CLI 依赖图 → default_cli_runtime_graph_excludes_ui_and_protocol_runtimes 红

use std::path::{Path, PathBuf};

/// 新引入的依赖名单（护栏对象）
const NEW_DEPS: [&str; 4] = ["ratatui", "crossterm", "axum", "tokio"];

fn repo_root() -> PathBuf {
    // tests 运行时 CARGO_MANIFEST_DIR = <root>/orch/crates/orch-host
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("解析仓根失败")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// 从 manifest 文本里取某依赖所在的那一行（简单行扫描，够用且不引新依赖）
fn dep_line(manifest: &str, dep: &str) -> Option<String> {
    manifest
        .lines()
        .map(str::trim)
        .find(|line| {
            line.starts_with(dep)
                && line[dep.len()..].trim_start().starts_with('=')
        })
        .map(str::to_string)
}

#[test]
fn workspace_includes_ui_crate() {
    let manifest = read(&repo_root().join("orch/Cargo.toml"));
    assert!(
        manifest.contains("crates/orch-ui"),
        "orch/Cargo.toml 的 members 必须含 crates/orch-ui；实际内容:\n{manifest}"
    );
}

#[test]
fn ui_crate_manifest_exists() {
    let path = repo_root().join("orch/crates/orch-ui/Cargo.toml");
    assert!(
        path.is_file(),
        "orch-ui crate 的 Cargo.toml 必须存在: {}",
        path.display()
    );
}

#[test]
fn ui_crate_pins_exact_versions() {
    let manifest = read(&repo_root().join("orch/crates/orch-ui/Cargo.toml"));
    for dep in NEW_DEPS {
        let line = dep_line(&manifest, dep)
            .unwrap_or_else(|| panic!("orch-ui 必须声明依赖 {dep}；实际内容:\n{manifest}"));
        assert!(
            line.contains("=\"=") || line.contains("version = \"="),
            "依赖 {dep} 必须精确钉死（形如 {dep} = \"=1.2.3\"），实际: {line}"
        );
        assert!(
            !line.contains('^') && !line.contains('~') && !line.contains('*'),
            "依赖 {dep} 不得使用浮动版本约束，实际: {line}"
        );
    }
}

#[test]
fn core_and_host_stay_unpolluted() {
    for crate_name in ["orch-core", "orch-host", "orch-cli"] {
        let manifest = read(&repo_root().join(format!("orch/crates/{crate_name}/Cargo.toml")));
        for dep in NEW_DEPS {
            // B366 adds explicitly enabled, shared protocol IO. A declaration
            // is not a default runtime dependency; verify both boundaries.
            if crate_name == "orch-core" && dep == "tokio" {
                let normal = manifest.split("[dependencies]").nth(1)
                    .and_then(|section| section.split("\n[").next()).unwrap_or("");
                if let Some(line) = dep_line(normal, dep) {
                    assert!(line.replace(' ', "").contains("optional=true"), "core Tokio must remain optional");
                }
                continue;
            }
            assert!(
                dep_line(&manifest, dep).is_none(),
                "{crate_name} 的依赖面不得被 {dep} 污染（新依赖只允许出现在 orch-ui）"
            );
        }
        // 同时确认 orch-ui 已建立——否则本用例会在 crate 尚不存在时假绿
        assert!(
            repo_root().join("orch/crates/orch-ui/Cargo.toml").is_file(),
            "零污染断言必须与 orch-ui 已建立同时成立，否则是假绿"
        );
    }
}


#[test]
fn default_cli_runtime_graph_excludes_ui_and_protocol_runtimes() {
    let cargo = option_env!("CARGO").unwrap_or("cargo");
    let output = std::process::Command::new(cargo)
        .current_dir(repo_root())
        .env_remove("_")
        .args(["tree", "-p", "orch-cli", "--no-default-features", "--offline",
            "--locked", "--manifest-path", "orch/Cargo.toml", "-e", "normal,build",
            "--prefix", "none", "--format", "{p}"])
        .output().expect("inspect default CLI dependency graph");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let graph = String::from_utf8(output.stdout).unwrap();
    for line in graph.lines() {
        let name = line.split_whitespace().next().unwrap_or("");
        assert!(!["orch-ui", "ratatui", "crossterm", "axum", "tokio", "rmcp",
            "agent-client-protocol", "agent-client-protocol-schema"].contains(&name),
            "default CLI unexpectedly links {name}");
    }
    assert!(graph.lines().any(|line| line.starts_with("orch-core ")));
}
