//! `orch init` / `orch bind` 地基命令化（design/02 §3/§5，design/05 §6）。
//!
//! `init` 只补齐缺失骨架和规则，不覆盖用户已有文件；`bind` 只读探测并返回提议，
//! PROJECT-BINDING.yaml 必须经过用户确认后再由后续生命周期写入。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};

const GITIGNORE_LINES: [&str; 3] = [
    "coordination/rounds/*/dispatch/",
    "coordination/runtime/",
    ".worktrees/",
];
const GITATTRIBUTES_LINE: &str = "coordination/rounds/*/events.jsonl merge=union";

const PROTOCOL_TEMPLATE: &str = r#"# Collaboration Protocol

## Executor contract

1. Work only inside the task card's `writeSet`; protected and frozen paths are read-only.
2. For seeded-red tasks, relocate every seed byte-for-byte and commit that relocation first.
3. Capture the failing gate before implementation; never weaken a test assertion to make it green.
4. Run every required test/check gate and prove each declared negative mutation.
5. Keep git local: executors may commit their task branch, but may not merge or push.
6. Write the six-section REPORT only after all implementation and verification work is complete.

## REPORT sections

1. Changed files versus writeSet
2. Commit sequence
3. Seed relocation and red evidence
4. Final gate evidence
5. Negative mutation evidence
6. Possible mistakes or limitations
"#;

const BOARD_TEMPLATE: &str = r#"# BOARD

> Human-readable collaboration ledger. Append entries; do not rewrite history.
"#;

const OPERATOR_RUNBOOK_TEMPLATE: &str = r#"# AI Operator Runbook

> 本文件属于当前项目的非机械操作策略。`orch guide` 中的产品机械契约优先；
> 本文件只能增加本地约束，不能放宽租约、attempt、门、审查、证据或合并判据。

## 接管顺序

1. 运行 `orch guide --check`，再读 `orch guide` 或相关 section。
2. 读本文件，了解项目自己的 agent、审查和降级策略。
3. 运行 `orch current` 并读取 `coordination/CURRENT.md`。
4. 仅在异常恢复或审计时读取事件尾部、证据、完整日志和源码。

## Agent roster 与容量

在这里记录本项目的执行者、角色、容量、quota domain 和候选顺序。动态值应引用
registry 或 signed IR，不要复制成多个相互漂移的真值。

## 审查与模型策略

在这里记录审查分档、模型偏好、主副审选择和允许的降级路径。已签核任务的席位
不得在执行中静默改写。

## Planner 判断原则

在这里记录需要人或 planner 判断、而非 orch 自动决定的边界。

## 项目故障纪律

在这里记录项目特有的 flaky gate 复现方法、完整日志位置和事故教训。不得把经验性
重试写成机械成功，也不得用它覆盖 `orch guide` 的 fail-closed 规则。

## 本机与部署

在这里记录本机私有工具覆盖、部署方式和用户裁定；不要提交密钥或机器私有凭据。
"#;

const WAIT_DISPATCH_TEMPLATE: &str = r#"#!/bin/sh
# wait-dispatch.sh <agentId> [segmentSec=540] [taskId] [attemptId]
# 阻塞等待派发信号（design/03 §4.1 全文 + 干跑加固：mkdir -p 目录自愈）。
# 零 token：睡眠发生在 shell，模型只在本脚本返回时被唤起一次。
agent="$1"; seg="${2:-540}"; target_task="${3:-}"; target_attempt="${4:-}"
[ -n "$agent" ] || { echo "usage: wait-dispatch.sh <agentId> [segmentSec] [taskId] [attemptId]"; exit 64; }
# 用 --git-common-dir 定位主仓根：从主仓或任意 worktree 内调用均正确（errata E2）
gitcommon="$(git rev-parse --path-format=absolute --git-common-dir)" || exit 65
root="$(dirname "$gitcommon")"
round="$(cat "$root/coordination/runtime/CURRENT-ROUND" 2>/dev/null)"
[ -n "$round" ] || { echo "NO_CURRENT_ROUND"; exit 66; }
disp="$root/coordination/rounds/$round/dispatch/$agent"
hb="$root/coordination/runtime/heartbeats/$agent.json"
mkdir -p "$disp" "$(dirname "$hb")"
gen="waiting"; ord=0
if [ -n "$target_task" ] && [ -n "$target_attempt" ]; then
  gen="$target_task-$target_attempt"
  case "$target_attempt" in
    A[0-9]*)
      ord="${target_attempt#A}"
      case "$ord" in *[!0-9]*|"") ord=0;;
        *) while [ "${ord#0}" != "$ord" ]; do ord="${ord#0}"; done; [ -n "$ord" ] || ord=0;;
      esac
      ;;
  esac
fi
t=0
while [ "$t" -lt "$seg" ]; do
  printf '{"agent":"%s","phase":"waiting","ts":"%s","pid":%s,"round":"%s","generation":"%s","ordinal":%s}\n' \
    "$agent" "$(date -u +%FT%TZ)" "$$" "$round" "$gen" "$ord" >"$hb.tmp" && mv "$hb.tmp" "$hb"
  go=""
  for candidate in "$disp"/GO-*.md; do
    [ -f "$candidate" ] || continue
    if [ -n "$target_task" ]; then
      name="$(basename "$candidate")"
      if [ -n "$target_attempt" ]; then
        [ "$name" = "GO-$target_task-$target_attempt.md" ] || continue
      else
        case "$name" in "GO-$target_task.md"|"GO-$target_task-"*.md) ;; *) continue ;; esac
      fi
    fi
    [ ! -f "$candidate.ack" ] || continue
    ack_tmp="$candidate.ack.tmp.$$"
    cp "$candidate" "$ack_tmp" || continue
    if ln "$ack_tmp" "$candidate.ack" 2>/dev/null; then
      rm -f "$ack_tmp"
      go="$candidate"
      break
    fi
    rm -f "$ack_tmp"
  done
  if [ -n "$go" ]; then
    echo GO_FOUND; cat "$go"; exit 0
  fi
  if [ -f "$disp/../DONE.md" ]; then
    echo ROUND_COMPLETE
    echo "→ 指令（通信 v2·轮间休眠合法）：①把会话小结写到 $root/coordination/rounds/$round/reports/$agent-SUMMARY.md 并 commit（若本轮已写过则跳过）；②若你的会话已在 coordination/agents.yaml 登记为 injectable，写完小结即可结束 turn——下一轮由运行时注入唤醒；未登记者重跑本命令等待下一轮。"
    exit 2
  fi
  if [ -f "$disp/NUDGE.md" ]; then
    echo NUDGE; cat "$disp/NUDGE.md"; mv "$disp/NUDGE.md" "$disp/NUDGE.md.seen"; exit 4
  fi
  sleep 20; t=$((t+20))
done
echo SEGMENT_TIMEOUT
echo "→ 指令（通信 v2·等待循环已降级为备用通道）：若你的会话已在 coordination/agents.yaml 登记为 injectable，可直接结束本 turn——运行时会在有新信号时注入唤醒你，无需守循环。未登记/不可注入者：立即原样重跑本命令（不要请求确认），连续 ≥5 段无信号后写 STALL 备注结束。"
exit 1
"#;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct InitReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub already_present: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BindProposal {
    pub ecosystems: Vec<String>,
    pub yaml: String,
}

/// Idempotently inject the coordination scaffold into an existing project root.
pub fn init(root: &Path) -> Result<InitReport> {
    if !root.is_dir() {
        bail!("项目根不是目录: {}", root.display());
    }

    let mut report = InitReport::default();
    for relative in [
        "coordination",
        "coordination/scripts",
        "coordination/modes",
        "coordination/rounds",
        "coordination/runtime",
        "coordination/runtime/heartbeats",
        "coordination/runtime/locks",
        "coordination/runtime/logs",
    ] {
        ensure_directory(root, relative, &mut report)?;
    }

    create_file_if_missing(
        root,
        "coordination/PROTOCOL.md",
        PROTOCOL_TEMPLATE,
        &mut report,
    )?;
    create_file_if_missing(
        root,
        "coordination/AI-OPERATOR-RUNBOOK.md",
        OPERATOR_RUNBOOK_TEMPLATE,
        &mut report,
    )?;
    create_file_if_missing(root, "coordination/BOARD.md", BOARD_TEMPLATE, &mut report)?;
    create_file_if_missing(
        root,
        "coordination/scripts/wait-dispatch.sh",
        WAIT_DISPATCH_TEMPLATE,
        &mut report,
    )?;
    ensure_executable(
        &root.join("coordination/scripts/wait-dispatch.sh"),
        "coordination/scripts/wait-dispatch.sh",
        &mut report,
    )?;

    ensure_lines(root, ".gitignore", &GITIGNORE_LINES, &mut report)?;
    ensure_lines(root, ".gitattributes", &[GITATTRIBUTES_LINE], &mut report)?;
    Ok(report)
}

/// Read project markers and return a deterministic binding proposal without writing it.
pub fn bind(root: &Path) -> Result<BindProposal> {
    if !root.is_dir() {
        bail!("项目根不是目录: {}", root.display());
    }

    let has_rust = root.join("Cargo.toml").is_file();
    let has_node = root.join("package.json").is_file();
    let ecosystems = match (has_rust, has_node) {
        (true, true) => vec!["rust".to_string(), "node".to_string()],
        (true, false) => vec!["rust".to_string()],
        (false, true) => vec!["node".to_string()],
        (false, false) => bail!(
            "未识别项目生态：{} 下没有 Cargo.toml 或 package.json",
            root.display()
        ),
    };
    let yaml = render_binding(has_rust, has_node);
    Ok(BindProposal { ecosystems, yaml })
}

fn ensure_directory(root: &Path, relative: &str, report: &mut InitReport) -> Result<()> {
    let path = root.join(relative);
    if path.is_dir() {
        report.already_present.push(relative.to_string());
        return Ok(());
    }
    fs::create_dir(&path).with_context(|| format!("创建目录失败: {}", path.display()))?;
    report.created.push(relative.to_string());
    Ok(())
}

fn create_file_if_missing(
    root: &Path,
    relative: &str,
    content: &str,
    report: &mut InitReport,
) -> Result<()> {
    let path = root.join(relative);
    if path.is_file() {
        report.already_present.push(relative.to_string());
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("创建文件失败: {}", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("写文件失败: {}", path.display()))?;
    report.created.push(relative.to_string());
    Ok(())
}

fn ensure_lines(
    root: &Path,
    relative: &str,
    required: &[&str],
    report: &mut InitReport,
) -> Result<()> {
    let path = root.join(relative);
    let existed = path.exists();
    let original = if existed {
        fs::read_to_string(&path).with_context(|| format!("读取文件失败: {}", path.display()))?
    } else {
        String::new()
    };
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|required_line| !original.lines().any(|line| line.trim() == *required_line))
        .collect();
    if missing.is_empty() {
        report.already_present.push(relative.to_string());
        return Ok(());
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开文件失败: {}", path.display()))?;
    if !original.is_empty() && !original.ends_with('\n') {
        writeln!(file)?;
    }
    for line in missing {
        writeln!(file, "{line}")?;
    }
    if existed {
        report.updated.push(relative.to_string());
    } else {
        report.created.push(relative.to_string());
    }
    Ok(())
}

fn ensure_executable(path: &Path, relative: &str, report: &mut InitReport) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata =
            fs::metadata(path).with_context(|| format!("读取权限失败: {}", path.display()))?;
        let current = metadata.permissions().mode();
        if current & 0o111 == 0 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(current | 0o111);
            fs::set_permissions(path, permissions)
                .with_context(|| format!("设置可执行权限失败: {}", path.display()))?;
            if !report.created.iter().any(|item| item == relative) {
                report.updated.push(format!("{relative} (chmod +x)"));
            }
        }
    }
    Ok(())
}

fn render_binding(has_rust: bool, has_node: bool) -> String {
    let mut yaml = String::from(
        r#"apiVersion: orch/v1alpha1
kind: ProjectBinding
metadata: {name: proposed-project, bindingRevision: 1}
project:
  root: "."
  primaryBranch: main
  sourceOfTruth: [coordination/PROTOCOL.md]
  ecosystems:
"#,
    );
    if has_rust {
        yaml.push_str("    - rust\n");
    }
    if has_node {
        yaml.push_str("    - node\n");
    }
    yaml.push_str(
        r#"workspace:
  defaultIsolation: git-worktree
  worktreeRoot: ".worktrees"
  branchPattern: "task/{taskId}"
commands:
"#,
    );

    match (has_rust, has_node) {
        (true, false) => yaml.push_str(
            r#"  testFast:
    argv: ["cargo", "test", "--workspace", "--locked"]
    timeoutSeconds: 600
  check:
    argv: ["cargo", "check", "--workspace", "--all-targets", "--locked"]
    timeoutSeconds: 600
gates:
  fast: [testFast, check]
  merge: [testFast, check]
"#,
        ),
        (false, true) => yaml.push_str(
            r#"  testFast:
    argv: ["npx", "vitest", "run"]
    timeoutSeconds: 600
  check:
    argv: ["npx", "tsc", "--noEmit"]
    timeoutSeconds: 600
gates:
  fast: [testFast, check]
  merge: [testFast, check]
"#,
        ),
        (true, true) => yaml.push_str(
            r#"  rustTest:
    argv: ["cargo", "test", "--workspace", "--locked"]
    timeoutSeconds: 600
  rustCheck:
    argv: ["cargo", "check", "--workspace", "--all-targets", "--locked"]
    timeoutSeconds: 600
  nodeTest:
    argv: ["npx", "vitest", "run"]
    timeoutSeconds: 600
  nodeCheck:
    argv: ["npx", "tsc", "--noEmit"]
    timeoutSeconds: 600
gates:
  fast: [rustTest, rustCheck, nodeTest, nodeCheck]
  merge: [rustTest, rustCheck, nodeTest, nodeCheck]
"#,
        ),
        (false, false) => unreachable!("bind rejects unknown ecosystems"),
    }

    yaml.push_str(
        r#"scope:
  protectedPaths: ["coordination/**"]
  reviewRequiredPaths: []
  publicEntryPoints: []
git:
  implementerMayCommit: true
  implementerMayMerge: false
  pushPolicy: forbidden
  mergePolicy: ff-only-else-no-ff
verification:
  contractModes: [seeded-red, verify-only]
  independentVerifier: required-for-write
data:
  secretsPolicy: reference-only
  forbiddenArtifactPatterns: ["*.pem", "*.key", ".env*"]
knownFailures: []
"#,
    );
    yaml
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "orch-scaffold-{label}-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn init_is_idempotent_and_builds_complete_scaffold() {
        let root = TestRoot::new("idempotent");
        let first = init(root.path()).unwrap();
        let second = init(root.path()).unwrap();

        assert!(!first.created.is_empty());
        assert!(second.created.is_empty());
        assert!(second.updated.is_empty());
        for relative in [
            "coordination/scripts",
            "coordination/modes",
            "coordination/rounds",
            "coordination/runtime/heartbeats",
            "coordination/runtime/locks",
            "coordination/runtime/logs",
        ] {
            assert!(root.path().join(relative).is_dir(), "{relative}");
        }
        assert!(root.path().join("coordination/PROTOCOL.md").is_file());
        assert!(root.path().join("coordination/BOARD.md").is_file());
        assert!(root
            .path()
            .join("coordination/AI-OPERATOR-RUNBOOK.md")
            .is_file());
        assert_eq!(
            fs::read_to_string(root.path().join("coordination/scripts/wait-dispatch.sh")).unwrap(),
            include_str!("../../../../coordination/scripts/wait-dispatch.sh")
        );
        assert!(
            orch_core::doctor(root.path())
                .iter()
                .all(|check| check.status != orch_core::CheckStatus::Fail),
            "fresh scaffold must pass every mandatory doctor check"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.path().join("coordination/scripts/wait-dispatch.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0);
        }
    }

    #[test]
    fn init_creates_but_never_overwrites_operator_runbook() {
        let root = TestRoot::new("operator-runbook");
        init(root.path()).unwrap();
        let path = root.path().join("coordination/AI-OPERATOR-RUNBOOK.md");
        let generated = fs::read_to_string(&path).unwrap();
        assert!(generated.contains("orch guide"));
        assert!(generated.contains("机械契约优先"));

        fs::write(&path, "# local operator policy\nkeep me\n").unwrap();
        let report = init(root.path()).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# local operator policy\nkeep me\n"
        );
        assert!(report
            .already_present
            .iter()
            .any(|entry| entry == "coordination/AI-OPERATOR-RUNBOOK.md"));
    }

    #[test]
    fn wait_dispatch_skips_acked_old_go_for_new_go() {
        let root = TestRoot::new("first-unacked-go");
        let git = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(git.success());
        init(root.path()).unwrap();

        let round = "r-test";
        let agent = "executor-test";
        let dispatch = root
            .path()
            .join("coordination/rounds")
            .join(round)
            .join("dispatch")
            .join(agent);
        fs::create_dir_all(&dispatch).unwrap();
        fs::write(
            root.path().join("coordination/runtime/CURRENT-ROUND"),
            format!("{round}\n"),
        )
        .unwrap();
        fs::write(dispatch.join("GO-B1.md"), "B1 stale\n").unwrap();
        fs::write(dispatch.join("GO-B1.md.ack"), "B1 stale\n").unwrap();
        fs::write(dispatch.join("GO-B2.md"), "B2 fresh\n").unwrap();

        let output = Command::new("sh")
            .arg("coordination/scripts/wait-dispatch.sh")
            .arg(agent)
            .arg("1")
            .current_dir(root.path())
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "status={:?}, stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("GO_FOUND\nB2 fresh\n"), "{stdout}");
        assert!(!stdout.contains("B1 stale"), "{stdout}");
        assert!(dispatch.join("GO-B2.md.ack").is_file());
        let heartbeat: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(
                root.path()
                    .join("coordination/runtime/heartbeats/executor-test.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(heartbeat["round"], round);
        assert_eq!(heartbeat["generation"], "waiting");
        assert_eq!(heartbeat["ordinal"], 0);
    }

    #[test]
    fn wait_dispatch_targeted_claims_do_not_cross_or_duplicate() {
        let root = TestRoot::new("targeted-atomic-go");
        let git = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(git.success());
        init(root.path()).unwrap();

        let round = "r-test";
        let agent = "executor-test";
        let dispatch = root
            .path()
            .join("coordination/rounds")
            .join(round)
            .join("dispatch")
            .join(agent);
        fs::create_dir_all(&dispatch).unwrap();
        fs::write(
            root.path().join("coordination/runtime/CURRENT-ROUND"),
            format!("{round}\n"),
        )
        .unwrap();
        fs::write(dispatch.join("GO-B1-A0001.md"), "B1 only\n").unwrap();
        fs::write(dispatch.join("GO-B2-A0001.md"), "B2 only\n").unwrap();

        let run = |task: &str| {
            Command::new("sh")
                .arg("coordination/scripts/wait-dispatch.sh")
                .arg(agent)
                .arg("1")
                .arg(task)
                .arg("A0001")
                .current_dir(root.path())
                .output()
                .unwrap()
        };
        let first = run("B2");
        let second = run("B1");

        assert!(first.status.success());
        assert!(second.status.success());
        let first_stdout = String::from_utf8(first.stdout).unwrap();
        let second_stdout = String::from_utf8(second.stdout).unwrap();
        assert!(first_stdout.contains("B2 only"), "{first_stdout}");
        assert!(!first_stdout.contains("B1 only"), "{first_stdout}");
        assert!(second_stdout.contains("B1 only"), "{second_stdout}");
        assert!(!second_stdout.contains("B2 only"), "{second_stdout}");
        assert!(dispatch.join("GO-B1-A0001.md.ack").is_file());
        assert!(dispatch.join("GO-B2-A0001.md.ack").is_file());
        let heartbeat: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(
                root.path()
                    .join("coordination/runtime/heartbeats/executor-test.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(heartbeat["round"], round);
        assert_eq!(heartbeat["generation"], "B1-A0001");
        assert_eq!(heartbeat["ordinal"], 1);
    }

    #[test]
    fn init_preserves_custom_gitignore_and_never_duplicates_required_lines() {
        let root = TestRoot::new("gitignore");
        fs::write(root.path().join(".gitignore"), "custom/\n.worktrees/").unwrap();

        init(root.path()).unwrap();
        init(root.path()).unwrap();

        let text = fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(text.lines().any(|line| line == "custom/"));
        for required in GITIGNORE_LINES {
            assert_eq!(
                text.lines().filter(|line| line.trim() == required).count(),
                1,
                "{required}"
            );
        }
    }

    #[test]
    fn bind_detects_rust_and_does_not_write_proposal() {
        let root = TestRoot::new("rust");
        fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();

        let proposal = bind(root.path()).unwrap();

        assert_eq!(proposal.ecosystems, vec!["rust"]);
        assert!(proposal
            .yaml
            .contains("argv: [\"cargo\", \"test\", \"--workspace\", \"--locked\"]"));
        assert!(proposal.yaml.contains(
            "argv: [\"cargo\", \"check\", \"--workspace\", \"--all-targets\", \"--locked\"]"
        ));
        assert!(proposal
            .yaml
            .contains("protectedPaths: [\"coordination/**\"]"));
        assert!(!root
            .path()
            .join("coordination/PROJECT-BINDING.yaml")
            .exists());
        let _: serde_yaml::Value = serde_yaml::from_str(&proposal.yaml).unwrap();

        // 回归证明：生成的 Rust 绑定必须通过 B106 锁门校验。
        let parsed: crate::binding::Binding = serde_yaml::from_str(&proposal.yaml).unwrap();
        assert!(crate::binding::validate_locked_rust_gates(&parsed).is_ok());
    }

    #[test]
    fn bind_detects_node_commands() {
        let root = TestRoot::new("node");
        fs::write(root.path().join("package.json"), "{}\n").unwrap();

        let proposal = bind(root.path()).unwrap();

        assert_eq!(proposal.ecosystems, vec!["node"]);
        assert!(proposal
            .yaml
            .contains("argv: [\"npx\", \"vitest\", \"run\"]"));
        assert!(proposal
            .yaml
            .contains("argv: [\"npx\", \"tsc\", \"--noEmit\"]"));
        let _: serde_yaml::Value = serde_yaml::from_str(&proposal.yaml).unwrap();
    }
}
