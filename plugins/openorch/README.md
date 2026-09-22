# OpenOrch · Codex + DSH Web

在当前宿主中咨询本机已安装的 AI 通道，或显式选择多名成员进行 fusion。
个人默认配置可跨 Git 项目复用，由宿主综合结论、分歧和原始答卷。

Consult installed AI harnesses from your current host, then synthesize their
answers with original evidence. Save personal defaults once and reuse them in
Git projects.

## 安装 / Install

本次为 [`0.1.0-alpha.14`](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.14)
Release。使用经验证的完整 Apple Silicon macOS 安装包；三个核心程序的版本均为 `0.1.0`。
源码仓本身不含预编译运行时，不能代替完整安装包。

Use the verified Release bundle. It includes
the default CLI, MCP and ACP binaries plus four channel resources. No Rust toolchain is needed.
Prerequisites: Python 3.9+, Git, and the native Codex or DSH client. DSH's plugin
manager also requires its normal Node.js/pnpm environment and network access to
install the declared filesystem-skill dependency. Tested host versions and build
provenance belong to the release notes/provenance, not an assumed future range.

```sh
python3 /absolute/path/to/extracted-release/install.py --host both
```

Use `--host codex` or `--host dsh` to install one host. `--codex-bin ABS` and
`--dsh-bin ABS` select native executables when they are absent from PATH.
`--prefix ABS` changes the default `~/.local/share/openorch` version directory;
reuse the same prefix for later upgrades and removal.

安装器逐文件核完整性，核 Apple Silicon macOS 和默认六命令核心，再检查两个宿主的
同名登记。只有各宿主原生回读成功后才输出该宿主已注册；部分成功会明确列出。
安装器不声称已完成模型调用。安装后在新的 Codex 任务或 DSH Web 会话中使用 OpenOrch。

DSH Web 的工具由会话的 Agent 预设决定。若当前为仅提供 bash 和
`str_replace_editor` 的极简预设，请在新会话中选择包含技能发现与 `skill` 工具的预设。
安装器注册技能提供者，不修改用户已有的 Agent 预设。

For DSH Web, choose a session preset that includes skill discovery and the
`skill` tool. The minimal two-tool preset does not provide that tool; plugin
installation preserves the user's preset choice.

The installer registers a local Codex marketplace/plugin and a DSH Web bundle.
It verifies native readback, including installed DSH resource bytes. Native DSH
directory links are normalized through its package manager to `file:` packages
so dependencies can be installed. If pnpm lists an absent virtual-store path,
the installer resolves the actual package through Node from the Web profile.
No manual skill copy is required. DSH uses an
isolated `openorch` skill provider derived from the installed ESM package path.

## 首次使用 / First use

在已有首提交的 Git 项目中说：“使用 OpenOrch，帮我配置默认咨询成员。”
宿主先发现本机通道，用户选择客户端、原生模型设置、默认单席及可选的多席组。
发现操作不调用模型，也不证明登录成功；缺少登录时使用对应客户端的正常登录流程。

In an existing Git project, ask: “Use OpenOrch and help configure my default
advisors.” Choose clients/models already available to you. Personal settings are
stored locally at `~/.config/openorch/profile.json` with mode 0600. Existing
project `.orch/harnesses.yaml` remains authoritative and is preserved byte for
byte. Dirty worktrees work; linked worktrees share their main repository's config.
OpenOrch does not create the project's first commit.

Then ask “Use OpenOrch to consult my default advisor about …” or “Use OpenOrch
fusion about …”. Single mode uses one member; fusion uses two to five distinct
aliases. Explicit members override only that call. The current host reads all
complete outcomes and explains disagreements, failures and original answer links.
Two aliases can still use the same backend; member count alone does not establish
independent model diversity. Native clients may incur their normal usage costs.

配置细节和完整助手命令见 [HELPER.md](HELPER.md)。更新个人默认配置需要明确选择，
不会自动覆盖已有项目配置。没有后台常驻编排或自动追加咨询轮次。

## Codex 宿主协作 / Codex host coordination

发布包包含 [AGENTS.md](AGENTS.md) 的协作规则副本。在 Codex Desktop 中，局部实现的
拆分、子代理并行、消息和等待优先交给 Codex 原生多代理机制；OpenOrch 仅在用户明确要求
OpenOrch、Fusion 或跨客户端独立咨询时提供逐席咨询与汇总。每个子任务只保留一个执行所有者，
不会由 OpenOrch 复制 Codex 的任务队列、后台 scheduler、自动接替或重试。

The release includes a copy of the collaboration policy at [AGENTS.md](AGENTS.md).
In Codex Desktop, use Codex-native delegation, messaging, and waits for local
implementation work. OpenOrch is only the explicit cross-harness consultation
and fusion layer. One subtask has one execution owner; OpenOrch does not add a
duplicate task queue, scheduler, automatic takeover, or retry loop.

## 支持边界 / Current support

| Surface | Current behavior |
| --- | --- |
| Plugin hosts | Codex Desktop and DSH Web |
| Consult targets | Codex, Claude, OpenCode, Cursor, MiMo, CodeBuddy, SmartClaw, DSH, Pi and ZCode, subject to actual discovery/configuration |
| Unsupported Consult targets | AGY |
| Core surfaces | Default CLI 6 commands; selfhost CLI 30; existing UI preserved |

DSH 作为插件宿主可调用受支持的其它通道；这与 DSH 自身能否成为 Consult 目标是两件事。
执行和审查动作的支持范围没有由本插件扩展。配置模型名称不等于原生证据已验证该身份。

Missing clients, unsupported members, failed/partial calls and empty or tool-only
answers remain visible. Do not treat a process exit or partial text as success.
The plugin does not substitute providers or models to hide a failed selection.

## 更新、卸载与恢复 / Update, remove, recover

Run the new release's installer with the same prefix and chosen host(s). Each
version has an immutable directory. Installing the same bytes again is idempotent;
different bytes under the same version are rejected. Foreign OpenOrch names or
paths are rejected before registration changes. Resolve them explicitly in the
native host, or choose the original owned prefix.

```sh
python3 /absolute/path/to/release/install.py --host codex --uninstall
python3 /absolute/path/to/release/install.py --host dsh --uninstall
```

卸载只移除所选宿主的原生登记；宿主自行管理其缓存。安装器前缀内的版本目录、个人默认、项目配置或咨询证据不会删除。
另一宿主可以继续引用旧版本。原生步骤失败时，按输出修复依赖或冲突后重跑所选宿主；
已成功的宿主和保留的文件会明确列出。

Uninstall removes registrations only. Old payloads remain because another host
may still use them. For damaged/missing files, re-extract a complete release.
Checksums detect byte changes; they do not attest publisher identity. An ad-hoc
macOS signature is not a Developer ID signature. Use the native host's normal
approval/login flow; no global security-setting changes are required.

## 发行构建 / Release construction

`scripts/package.py --runtime-dir ABS --output NEW_DIR --version SEMVER` packages
one source with supplied default `orch 0.1.0`, `orch-mcp 0.1.0`, `orch-acp 0.1.0` and four scripts. It checks
the six-command surface, rejects missing/symlink resources and writes a full
relative-path inventory plus an adjacent `.tar.gz`. It is a packaging check,
not a reproducible-build or publisher-attestation claim. The release builder
must supply and verify the runtime built from its fixed product source; optional
`runtime/provenance.json` is carried into the full inventory.

In the source checkout's plugin directory, the exporter is
`scripts/export.py --source GIT_ROOT --output NEW_EMPTY_DIR`; this development
command is not included in the installed runtime bundle.
It selects committed ordinary files in explicit product roots and derives the
public root README/CHANGELOG/LICENSE from the plugin's public templates. Generated
targets/runtime, private history/configuration and untracked files are excluded.
Product symlinks, dirty product bytes and a nonempty/symlink destination are refused
before publication. It writes only local artifacts and a relative SHA-256 inventory;
it never initializes Git or pushes refs. Maintainers publish from an independent
public checkout with exact old-ref checks, then verify public downloads and installs.


## MCP / ACP

`0.1.0-alpha.14` includes four MCP tools for Codex through a package-local
stdio server; DSH retains the shared skill/helper entry and has no MCP claim.
Rust now owns personal configuration, default selection and setup through the
same parser and consultation lifecycle as CLI/Web. Python is a verified launcher.

Run the installed helper's `configure` and `attach` for the intended Git worktree,
then restart the host to load its explicit private project registry. The current
directory never grants MCP access. Existing project bytes and native model defaults
are retained. Explicit profile updates return verified snapshot/actual-old-file
backup paths; recovery and all helper options are documented in [HELPER.md](HELPER.md).

Package runtime inputs must include all three binaries and four wrappers. Build
CLI separately from protocol binaries to preserve dependency isolation:

```sh
cargo build --locked --manifest-path orch/Cargo.toml -p orch-cli --no-default-features --release
cargo build --locked --manifest-path orch/Cargo.toml -p orch-mcp -p orch-acp --release
```

MCP consultations require explicit members and verified answer pagination. No
implicit synthesizer, retry, model fallback or detached scheduler is added.
Recorded native qualification accepts OpenCode/Claude DeepSeek Flash/max; Codex
ACP1.12.0/native0.155.1 refuses unsupported effort=max before Prompt. AGY Consult
and ACP remain unavailable. Actual runtime discovery/evidence remain authoritative.
