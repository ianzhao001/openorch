//! PROJECT-BINDING.yaml 薄封装（design/02 §5）。只解析本切片需要的字段，未知字段全容忍。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Binding {
    #[serde(default)]
    pub commands: BTreeMap<String, CommandSpec>,
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(default)]
    pub oracle: Oracle,
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub git: Git,
}

#[derive(Debug, Deserialize)]
pub struct CommandSpec {
    pub argv: Vec<String>,
    #[serde(rename = "timeoutSeconds", default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(rename = "trialTimeoutSeconds", default)]
    pub trial_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub approval: Option<String>,
}

fn default_timeout() -> u64 {
    600
}

#[derive(Debug, Deserialize)]
pub struct Workspace {
    #[serde(rename = "worktreeRoot", default = "default_worktree_root")]
    pub worktree_root: String,
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace {
            worktree_root: default_worktree_root(),
        }
    }
}

fn default_worktree_root() -> String {
    ".worktrees".into()
}

#[derive(Debug, Deserialize)]
pub struct Oracle {
    #[serde(default = "default_oracle_dialect")]
    pub dialect: String,
}

impl Default for Oracle {
    fn default() -> Self {
        Oracle {
            dialect: default_oracle_dialect(),
        }
    }
}

fn default_oracle_dialect() -> String {
    "vitest".into()
}

#[derive(Debug, Default, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub ecosystems: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Scope {
    #[serde(rename = "protectedPaths", default)]
    pub protected_paths: Vec<String>,
    #[serde(rename = "reviewRequiredPaths", default)]
    pub review_required_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Git {
    #[serde(rename = "implementerMayCommit", default = "default_true")]
    pub implementer_may_commit: bool,
    #[serde(rename = "implementerMayMerge", default)]
    pub implementer_may_merge: bool,
    #[serde(rename = "pushPolicy", default = "default_push_policy")]
    pub push_policy: String,
    #[serde(rename = "mergePolicy", default = "default_merge_policy")]
    pub merge_policy: String,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            implementer_may_commit: true,
            implementer_may_merge: false,
            push_policy: default_push_policy(),
            merge_policy: default_merge_policy(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_push_policy() -> String {
    "forbidden".into()
}

fn default_merge_policy() -> String {
    "ff-only-else-no-ff".into()
}

impl Binding {
    pub fn has_ecosystem(&self, ecosystem: &str) -> bool {
        self.project
            .ecosystems
            .iter()
            .any(|value| value.eq_ignore_ascii_case(ecosystem))
    }
}

/// 按任务卡顺序把门名解析到 binding 的可用命令名集合。
///
/// 重复项保留首现；任一名称缺失时一次性返回全部缺名，禁止静默缩窄门面。
pub fn resolve_gates(
    names: &[String],
    available: &[String],
) -> std::result::Result<Vec<String>, String> {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for name in names {
        if resolved.contains(name) || missing.contains(name) {
            continue;
        }
        if available.contains(name) {
            resolved.push(name.clone());
        } else {
            missing.push(name.clone());
        }
    }
    if missing.is_empty() {
        Ok(resolved)
    } else {
        Err(format!("绑定缺命令: {}", missing.join(", ")))
    }
}

/// Rust 生态绑定下 testFast / check 门命令的 `--locked` 规则机器化（B106）。
///
/// **严格聚合**（planner r46-repair 决策）：对 `testFast` 与 `check` 两个门命令
/// 逐条检查，以下任一情形即聚合报错，文案必须含命令名：
///   1. 命令缺失（binding 未声明该命令名）；
///   2. 命令存在但 argv 为空；
///   3. `--locked` 未作为独立 argv 出现在 `--` 终止符**之前**（把 `--` 之后的
///      `--locked` 当 Cargo 选项而不当门强制，拒绝）。
/// 缺命令、空 argv、缺 flag 一次性聚合返回全部错误文案。
/// 非绑定为 rust 生态的项目不强制（直接 Ok）。
///
/// 注意：本函数只看 binding 本身，不看根目录是否有 Cargo 标记。`load()` 在
/// 真实文件系统 Rust 项目根（`root/Cargo.toml` 或 `root/orch/Cargo.toml`）时才
/// 调用本严格校验；合成 fixture 根（无 Cargo 标记）与非 Rust 绑定在 `load()`
/// 处跳过，以保持既有合成测试（如 `merge_irreversible` 仅 `postGate`）不受扰。
pub fn validate_locked_rust_gates(b: &Binding) -> std::result::Result<(), Vec<String>> {
    if !b.has_ecosystem("rust") {
        return Ok(());
    }
    let mut errors: Vec<String> = Vec::new();
    for name in ["testFast", "check"] {
        let Some(spec) = b.commands.get(name) else {
            errors.push(format!("命令 {name} 缺失：Rust 绑定必须声明 {name} 且含独立 --locked"));
            continue;
        };
        if spec.argv.is_empty() {
            errors.push(format!("命令 {name} argv 为空：必须含独立 --locked"));
        } else if !has_locked_before_terminator(&spec.argv) {
            errors.push(format!(
                "命令 {name} 缺独立 --locked：argv 必须在 `--` 终止符之前含独立 --locked"
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// 判断 argv 是否在 `--` 终止符之前含独立 `--locked` token。
fn has_locked_before_terminator(argv: &[String]) -> bool {
    argv.iter()
        .take_while(|tok| *tok != "--")
        .any(|tok| tok == "--locked")
}

/// 判断根目录是否为真实文件系统 Rust 项目根（planner r46-repair 决策）。
///
/// 命中 `root/Cargo.toml`（顶层 Cargo 工作区/包）或 `root/orch/Cargo.toml`
/// （自举布局：外层仓无顶层 Cargo.toml，orch 子目录才是工作区根）之一即视为
/// 真实 Rust 根。合成 fixture 根（无任一标记）返回 false，`load()` 据此跳过
/// 严格 Rust 门校验，保持既有合成测试不受扰。
/// 无外部消费者（仅本文件 `load()` 使用），保持私有（reviewer r46 HOLD 修复）。
fn is_rust_project_root(root: &Path) -> bool {
    root.join("Cargo.toml").is_file() || root.join("orch/Cargo.toml").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    // B108 scratch 纪律（reviewer r46 HOLD 修复）：测试 scratch 必须落在调用
    // worktree 的 `orch/target/test-tmp`，目录名含 pid 与模块级 AtomicU64
    // fetch_add 序号（线程安全、无时钟依赖），不得使用 std::env::temp_dir()。
    static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// 从 CARGO_MANIFEST_DIR（<worktree>/orch/crates/orch-host）上溯两级到
    /// worktree/orch，在 `target/test-tmp/{tag}-{pid}-{seq}` 建目录并返回。
    fn b106_scratch_dir(tag: &str) -> std::path::PathBuf {
        let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
        let dir = orch_root.join("target/test-tmp").join(format!(
            "{tag}-{}-{}",
            std::process::id(),
            SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_scope_and_git_use_legacy_safe_defaults() {
        let binding: Binding = serde_yaml::from_str(
            "commands: {}\nunknownTopLevelField: tolerated\n",
        )
        .unwrap();
        assert!(binding.scope.protected_paths.is_empty());
        assert!(binding.scope.review_required_paths.is_empty());
        assert!(binding.git.implementer_may_commit);
        assert!(!binding.git.implementer_may_merge);
        assert_eq!(binding.git.push_policy, "forbidden");
        assert_eq!(binding.git.merge_policy, "ff-only-else-no-ff");
    }

    #[test]
    fn trial_timeout_is_optional_and_explicit_value_is_preserved() {
        let binding: Binding = serde_yaml::from_str(
            "commands:\n  implicit:\n    argv: [cargo, test]\n    timeoutSeconds: 7\n  explicit:\n    argv: [cargo, test]\n    timeoutSeconds: 7\n    trialTimeoutSeconds: 11\n",
        )
        .unwrap();
        assert_eq!(binding.commands["implicit"].trial_timeout_seconds, None);
        assert_eq!(binding.commands["explicit"].trial_timeout_seconds, Some(11));
        assert_eq!(binding.commands["implicit"].timeout_seconds, 7);
    }

    // B106 严格聚合：缺命令本身也报错（planner r46-repair 决策）。
    fn rust_binding_with(yaml: &str) -> Binding {
        serde_yaml::from_str(&format!(
            "project: {{ecosystems: [rust]}}\n{yaml}\n"
        ))
        .unwrap()
    }

    #[test]
    fn strict_missing_testfast_and_check_are_both_aggregated() {
        // 既缺 testFast 又缺 check：两条错误一次性聚合，文案各自含命令名。
        let b = rust_binding_with("commands:\n  postGate:\n    argv: [\"true\"]\n");
        let errors = validate_locked_rust_gates(&b).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("testFast")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("check")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("缺失")), "{errors:?}");
    }

    #[test]
    fn strict_missing_only_testfast_aggregates_testfast_name() {
        // 只缺 testFast（check 存在且 locked）：错误文案含 testFast 不含 check 缺失。
        let b = rust_binding_with(
            "commands:\n  check:\n    argv: [cargo, check, --workspace, --locked]\n",
        );
        let errors = validate_locked_rust_gates(&b).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("testFast")), "{errors:?}");
        // check 存在且 locked，不应有 check 缺失/缺 locked 错误。
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("命令 check") && e.contains("缺失")),
            "{errors:?}"
        );
    }

    #[test]
    fn strict_empty_argv_is_aggregated_with_command_name() {
        // 命令存在但 argv 为空：聚合报错，文案含命令名。
        let b = rust_binding_with(
            "commands:\n  testFast:\n    argv: []\n  check:\n    argv: [cargo, check, --locked]\n",
        );
        let errors = validate_locked_rust_gates(&b).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("testFast") && e.contains("argv 为空")),
            "{errors:?}"
        );
    }

    #[test]
    fn strict_non_rust_binding_skips_gate() {
        // 非 rust 生态：即便缺 testFast/check 也不强制（直接 Ok）。
        let b: Binding = serde_yaml::from_str(
            "project: {ecosystems: [node]}\ncommands:\n  postGate:\n    argv: [\"true\"]\n",
        )
        .unwrap();
        assert!(validate_locked_rust_gates(&b).is_ok());
    }

    #[test]
    fn is_rust_project_root_detects_top_level_and_nested_cargo_marker() {
        // 顶层 Cargo.toml
        let tmp = b106_scratch_dir("orch-b106-rust-root-toplevel");
        std::fs::write(tmp.join("Cargo.toml"), "[workspace]\n").unwrap();
        assert!(is_rust_project_root(&tmp));
        // 嵌套 orch/Cargo.toml（自举布局）
        let tmp2 = b106_scratch_dir("orch-b106-rust-root-nested");
        std::fs::create_dir_all(tmp2.join("orch")).unwrap();
        std::fs::write(tmp2.join("orch/Cargo.toml"), "[workspace]\n").unwrap();
        assert!(is_rust_project_root(&tmp2));
        // 合成 fixture 根：无任一标记 → false
        let tmp3 = b106_scratch_dir("orch-b106-synth-root");
        assert!(!is_rust_project_root(&tmp3));
        std::fs::remove_dir_all(&tmp).unwrap();
        std::fs::remove_dir_all(&tmp2).unwrap();
        std::fs::remove_dir_all(&tmp3).unwrap();
    }

    // load 级负向证明：真实 Rust 项目根 + 缺 testFast/check 的 malformed binding
    // 必须在执行前被 load() 拒绝（planner r46-repair 决策）。
    #[test]
    fn load_rejects_malformed_binding_at_real_rust_root() {
        let root = b106_scratch_dir("orch-b106-load-reject");
        std::fs::create_dir_all(root.join("orch")).unwrap();
        // 真实 Rust 根标记（自举布局：orch/Cargo.toml）
        std::fs::write(root.join("orch/Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(root.join("coordination")).unwrap();
        // malformed：声明 rust 生态但只给 postGate，缺 testFast 与 check
        std::fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            "project: {ecosystems: [rust]}\ncommands:\n  postGate:\n    argv: [\"true\"]\n",
        )
        .unwrap();
        let err = load(&root).unwrap_err().to_string();
        assert!(err.contains("testFast"), "{err}");
        assert!(err.contains("check"), "{err}");
        assert!(err.contains("缺失"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // load 级对照：合成 fixture 根（无 Cargo 标记）即便缺 testFast/check 也跳过
    // 严格 Rust 门，保持既有合成测试（merge_irreversible）不受扰。
    #[test]
    fn load_skips_strict_gate_at_synthetic_fixture_root() {
        let root = b106_scratch_dir("orch-b106-load-synth");
        std::fs::create_dir_all(root.join("coordination")).unwrap();
        // 无 Cargo.toml / orch/Cargo.toml 标记 → 合成 fixture 根
        std::fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            "project: {ecosystems: [rust]}\ncommands:\n  postGate:\n    argv: [\"true\"]\n",
        )
        .unwrap();
        let b = load(&root).expect("合成 fixture 根跳过严格 Rust 门，不应拒绝");
        assert!(b.has_ecosystem("rust"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

pub fn load(root: &Path) -> Result<Binding> {
    let p = root.join("coordination/PROJECT-BINDING.yaml");
    let text = fs::read_to_string(&p).with_context(|| format!("读取绑定失败: {}", p.display()))?;
    let mut binding: Binding =
        serde_yaml::from_str(&text).with_context(|| format!("解析绑定失败: {}", p.display()))?;
    // 只在真实文件系统 Rust 项目根强制严格 Rust 门（planner r46-repair 决策）：
    // 合成 fixture 根（无 Cargo.toml/orch/Cargo.toml 标记）与非 Rust 绑定跳过，
    // 保持既有合成测试（如 merge_irreversible 仅 postGate）不受扰。
    if is_rust_project_root(root) {
        validate_locked_rust_gates(&binding)
            .map_err(|errs| anyhow::anyhow!("Rust 门 --locked 校验失败: {}", errs.join("; ")))?;
    }
    // B151 便携性分层：machine.yaml 覆盖 argv[0]。若本机有 .orch/machine.yaml
    // 且声明的工具键（如 cargo）与某命令 argv[0] 完全相等（可移植默认是裸名），
    // 用 machine 路径替换 argv[0]——这是 detached worktree / 不同机器上 cargo
    // 不在 PATH 时仍能跑门的关键。无 machine.yaml 或无匹配键 → argv 不变。
    apply_machine_overlay(root, &mut binding)?;
    Ok(binding)
}

/// B151 便携性分层：从 `<root>/.orch/machine.yaml` 或主仓 `.orch/machine.yaml`
///（worktree 经 `git rev-parse --git-common-dir` 上溯到主仓）加载本机工具路径，
/// 覆盖 binding 中 argv[0] 与某工具键相等的命令。无 machine.yaml 或无匹配 → 不动。
///
/// **绝不裸名/PATH 猜测**：argv[0] 若是裸 `cargo` 且无 machine.yaml 覆盖，则保留裸名
/// （将由调用方负责 PATH；本机 doctor/AGENTS.md 指引使用者提供 machine.yaml）。
/// 这是 design 意图：可移植默认 = 裸名，本机路径 = machine.yaml 覆盖层。
fn apply_machine_overlay(root: &Path, binding: &mut Binding) -> Result<()> {
    let machine = match load_machine_config(root)? {
        Some(cfg) => cfg,
        None => match resolve_main_repo_machine_config(root)? {
            Some(cfg) => cfg,
            None => return Ok(()),
        },
    };
    for spec in binding.commands.values_mut() {
        if let Some(first) = spec.argv.first_mut() {
            if let Some(machine_path) = machine.tools.get(first) {
                *first = machine_path.clone();
            }
        }
    }
    Ok(())
}

/// 从 `git rev-parse --git-common-dir` 上溯到主仓，读主仓的 `.orch/machine.yaml`。
/// detached worktree（如 postmerge 复跑门现场）本机只有 git-tracked 文件，
/// 没有 `.orch/machine.yaml`（gitignored）；但其主仓可能有。无 git / 无文件 → None。
pub fn resolve_main_repo_machine_config(root: &Path) -> Result<Option<MachineConfig>> {
    let common_dir = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--git-common-dir")
        .output()
        .context("git rev-parse --git-common-dir 失败")?;
    if !common_dir.status.success() {
        return Ok(None);
    }
    let common_dir_str = String::from_utf8_lossy(&common_dir.stdout).trim().to_string();
    if common_dir_str.is_empty() {
        return Ok(None);
    }
    let main_root = Path::new(&common_dir_str)
        .parent()
        .filter(|p| p.is_dir())
        .map(std::path::absolute);
    let Some(main_root) = main_root else {
        return Ok(None);
    };
    let main_root = main_root.context("规范化主仓根为绝对路径失败")?;
    load_machine_config(&main_root)
}

// ─────────────────────────── B151 便携性分层 ───────────────────────────
//
// 本机绝对路径（如 `/Users/admin/.cargo/bin/cargo`）此前硬编码在
// PROJECT-BINDING.yaml 的 gate 命令 argv[0]——换机器即坏。B151 引入
// `.orch/machine.yaml`（gitignore 承载本机工具路径）+ `resolve_tool_path`
// 作为 argv[0] 的解析层：machine 优先 → project 兜底 → 双缺响亮 Err，
// 绝不裸名 / PATH 猜测（M1）。`machine_config_ignored` 防止 machine.yaml
// 被误入库（M2），`current_md_consistent` 为 `orch doctor` 的 CURRENT.md
// 一致性检查提供判定（M3）。

/// `.orch/machine.yaml` 的本机工具路径表（薄层：只解析 `tools` 段，未知字段容忍）。
///
/// 文件缺失视为本机无覆盖（返回空表，调用方回退 PROJECT-BINDING 默认）。
/// 解析失败响亮报错（不静默兜底——那是 M1 的退化形态）。
#[derive(Debug, Default, Deserialize)]
pub struct MachineConfig {
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub storage: StorageMachineConfig,
}

/// Machine-local storage policy. `storage` applies these values as
/// `max(compiled_default, configured)`, so an overlay can tighten but never relax safety.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageMachineConfig {
    pub floor_bytes: Option<u64>,
    pub audit_reserve_bytes: Option<u64>,
    pub dispatch_estimate_bytes: Option<u64>,
    pub wake_estimate_bytes: Option<u64>,
    pub gate_estimate_bytes: Option<u64>,
}

/// 读取 `.orch/machine.yaml`。文件不存在 → `Ok(None)`（无本机覆盖）。
pub fn load_machine_config(root: &Path) -> Result<Option<MachineConfig>> {
    let p = root.join(".orch/machine.yaml");
    if !p.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&p)
        .with_context(|| format!("读取 machine config 失败: {}", p.display()))?;
    let cfg: MachineConfig = serde_yaml::from_str(&text)
        .with_context(|| format!("解析 machine config 失败: {}", p.display()))?;
    Ok(Some(cfg))
}

/// 解析某工具的可执行路径：machine config 优先 → project 默认兜底 → 双缺响亮 Err。
///
/// **绝不裸名 / PATH 猜测**：两层都没有声明时返回 Err，而不是回退到
/// `tool.to_string()`（M1 退化形态：换机器即静默坏）。`tool` 是稳定 key
/// （如 `cargo`），`project_default` 是 PROJECT-BINDING.yaml gate argv[0]
/// 的当前值（移植前是本机绝对路径，移植后是可移植默认如 `cargo`）。
pub fn resolve_tool_path(
    machine: Option<&str>,
    project_default: Option<&str>,
    tool: &str,
) -> std::result::Result<String, String> {
    if let Some(p) = machine {
        if !p.trim().is_empty() {
            return Ok(p.to_string());
        }
    }
    if let Some(p) = project_default {
        if !p.trim().is_empty() {
            return Ok(p.to_string());
        }
    }
    Err(format!(
        "工具 {tool} 既未在 .orch/machine.yaml 声明也无 PROJECT-BINDING 默认；拒绝裸名/PATH 猜测（B151 M1）"
    ))
}

/// 从根 + 工具名解析：先查 machine config，缺失则用 binding argv[0] 兜底。
///
/// gate/adapter 调用点改经本函数（B151 §2）。`command` 是 PROJECT-BINDING
/// 的命令名（如 `testFast`），用于查 argv[0] 作为 project_default。
pub fn resolve_command_tool(root: &Path, command: &str, tool: &str) -> Result<String> {
    let machine = load_machine_config(root)?;
    let machine_path = machine
        .as_ref()
        .and_then(|cfg| cfg.tools.get(tool))
        .map(String::as_str);
    let binding = load(root)?;
    let project_default = binding
        .commands
        .get(command)
        .and_then(|spec| spec.argv.first())
        .map(String::as_str);
    resolve_tool_path(machine_path, project_default, tool)
        .map_err(anyhow::Error::msg)
}

/// 判定 `.gitignore` 是否含 `.orch/machine.yaml` 行（M2：机器配置必须被忽略，
/// 防止本机路径再次泄漏进库）。逐行 trim 比较（与 doctor 的 `file_contains_line`
/// 同语义，便于测试）。
pub fn machine_config_ignored(gitignore_src: &str) -> bool {
    gitignore_src
        .lines()
        .any(|l| l.trim() == ".orch/machine.yaml")
}

/// 判定 CURRENT.md 与活轮 + main SHA 标记是否一致（M3）。
///
/// 一致 = CURRENT.md 文本同时含 `round: <round>` 与 `main: <main_sha>` 行。
/// `main_sha` 允许短前缀（`orch current` 生成短 SHA，与 BOARD/ledger 的短
/// 形态一致）。`round` 必须精确匹配。
pub fn current_md_consistent(current_src: &str, round: &str, main_sha: &str) -> bool {
    let has_round = current_src
        .lines()
        .map(|l| l.trim())
        .any(|l| l == format!("round: {round}"));
    let has_main = current_src
        .lines()
        .map(|l| l.trim())
        .any(|l| l.starts_with("main:") && l[5..].trim().starts_with(main_sha));
    has_round && has_main
}

#[cfg(test)]
mod b151_tests {
    use super::*;

    // B151 scratch 纪律（同 B106）：scratch 必须落在调用 worktree 的
    // `orch/target/test-tmp`，目录名含 pid + 模块级 AtomicU64 fetch_add 序号
    // （线程安全、无时钟依赖）。**禁止 std::env::temp_dir() / /tmp**（硬禁令）。
    static B151_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn b151_scratch_dir(tag: &str) -> std::path::PathBuf {
        let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
        let dir = orch_root.join("target/test-tmp").join(format!(
            "b151-{tag}-{}-{}",
            std::process::id(),
            B151_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_command_tool_machine_overrides_project() {
        // machine.yaml 优先于 PROJECT-BINDING argv[0]。
        let tmp = b151_scratch_dir("resolve-cmd-machine");
        std::fs::create_dir_all(tmp.join(".orch")).unwrap();
        std::fs::create_dir_all(tmp.join("coordination")).unwrap();
        std::fs::write(
            tmp.join(".orch/machine.yaml"),
            "tools:\n  cargo: /opt/machine/cargo\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("coordination/PROJECT-BINDING.yaml"),
            "project:\n  ecosystems: [rust]\ncommands:\n  testFast:\n    argv: [cargo, test]\n",
        )
        .unwrap();
        let resolved = resolve_command_tool(&tmp, "testFast", "cargo").unwrap();
        assert_eq!(resolved, "/opt/machine/cargo");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_command_tool_falls_back_to_binding_default() {
        // 无 machine.yaml：回退 PROJECT-BINDING argv[0]（可移植默认 cargo）。
        let tmp = b151_scratch_dir("resolve-cmd-fallback");
        std::fs::create_dir_all(tmp.join("coordination")).unwrap();
        std::fs::write(
            tmp.join("coordination/PROJECT-BINDING.yaml"),
            "project:\n  ecosystems: [rust]\ncommands:\n  testFast:\n    argv: [cargo, test]\n",
        )
        .unwrap();
        let resolved = resolve_command_tool(&tmp, "testFast", "cargo").unwrap();
        assert_eq!(resolved, "cargo");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_command_tool_refuses_when_neither_machine_nor_binding() {
        // 命令名缺失 argv → 双缺，响亮 Err（M1 退化形态拒绝）。
        let tmp = b151_scratch_dir("resolve-cmd-refuse");
        std::fs::create_dir_all(tmp.join("coordination")).unwrap();
        std::fs::write(
            tmp.join("coordination/PROJECT-BINDING.yaml"),
            "project:\n  ecosystems: [rust]\ncommands:\n  otherGate:\n    argv: [sh, -c, exit 0]\n",
        )
        .unwrap();
        let err = resolve_command_tool(&tmp, "testFast", "cargo").unwrap_err();
        assert!(err.to_string().contains("cargo"), "{err}");
        assert!(err.to_string().contains("拒绝"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
