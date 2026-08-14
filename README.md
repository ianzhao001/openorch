# OpenOrch

[中文](#中文) · [English](#english)

OpenOrch 是一个由 Git 驱动的多智能体协作运行时。命令行程序名为 `orch`。

> 当前版本：`0.1.0` alpha
>
> 当前预发布：[`v0.1.0-alpha.1`](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.1)

---

## 中文

### 项目定位

OpenOrch 在本地 Git 项目中组织多个 AI 编程智能体，把计划、执行、验证、审查、合并和恢复约束为可检查的机械流程。

它不是 AI 模型或云端托管服务。OpenOrch 调用本机已有的智能体 CLI 和项目工具，并把 Git 提交、隔离工作树、事件记录与验证证据组合成可恢复的协作运行时。

### 核心能力

- Git 驱动的持久事实记录与只读状态投影
- 任务、执行尝试、租约、审查和收口生命周期
- 基于 Git worktree 的隔离执行现场
- 固定提交身份的测试门、证据检查和合并授权
- 本地智能体 CLI 适配与可配置的候选、能力和并发约束
- 单步、前台持续运行和常驻 `serve` 驱动模式
- 健康检查、停滞诊断、成本统计与安全恢复入口
- fail-closed 语义：身份、状态或副作用不明确时拒绝猜测
- 编译进二进制的机械契约指南，可与公开 CLI 命令树互相校验

### 预发布状态

`v0.1.0-alpha.1` 是产品快照，不是稳定版。

- `orch --version` 输出 `orch 0.1.0`
- CLI、配置格式和事件结构仍可能变化
- 建议先在非关键项目或专用分支中评估
- 升级前请阅读 Release 说明，并保留 Git 与运行状态备份

### 平台支持

本次 Release 只提供：

- Apple Silicon macOS
- 架构：`aarch64` / `arm64`
- 产物：`openorch-v0.1.0-alpha.1-aarch64-apple-darwin.tar.gz`

Intel Mac、Linux 和 Windows 没有本次预编译产物，也不在本次发布的已验证支持范围内。

预编译二进制使用 **ad-hoc 签名**，没有 Apple Developer ID 签名，也没有经过 notarization。macOS 可能显示安全提示；如果设备策略不接受此类二进制，请从源码构建。不要全局关闭 Gatekeeper。

### 从 Release 安装

在一个新目录中下载归档及其校验文件：

```sh
release=v0.1.0-alpha.1
archive=openorch-v0.1.0-alpha.1-aarch64-apple-darwin.tar.gz
base=https://github.com/ianzhao001/openorch/releases/download/$release

curl -fLO "$base/$archive"
curl -fLO "$base/$archive.sha256"
shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive"
```

归档只包含：

- `orch`
- `LICENSE`

验证架构、签名和程序：

```sh
file ./orch
codesign --verify --verbose=2 ./orch
codesign -dv --verbose=4 ./orch
./orch --version
./orch guide --check
```

`file` 应报告 Mach-O arm64，签名详情应显示 ad-hoc 签名。校验全部通过后，将 `orch` 复制到你的 `PATH` 中即可。

### 从源码构建

需要 Git、Rust 和 Cargo。此快照已使用 Rust/Cargo 1.97.1 验证，但尚未声明最低 Rust 版本。

```sh
git clone https://github.com/ianzhao001/openorch.git
cd openorch
git checkout v0.1.0-alpha.1

cargo build --release --locked \
  --manifest-path orch/Cargo.toml \
  --package orch-cli

cargo check --workspace --locked \
  --manifest-path orch/Cargo.toml
```

构建结果位于：

```text
orch/target/release/orch
```

验证：

```sh
orch/target/release/orch --version
orch/target/release/orch guide --check
```

### 快速开始

先查看不会修改项目的产品指南：

```sh
orch guide --check
orch guide --section quick-reference
orch guide --section worked-example
```

在一个干净的 Rust 或 Node.js Git 项目中，可以先执行只读探测：

```sh
cd your-project
orch --root . bind
```

`bind` 只打印建议，不写入项目。确认建议和工具命令符合预期后，再初始化运行时骨架：

```sh
orch --root . init
orch --root . doctor
```

`init` 会在当前项目中创建运行时所需文件，并补充 Git 忽略与属性规则。执行前请提交或备份已有改动。

参数和完整生命周期始终以当前二进制为准：

```sh
orch --help
orch <command> --help
orch guide
```

### 安全提示

OpenOrch 可以启动外部智能体、创建 Git worktree、运行项目命令和测试门，并在受控流程中更新 Git 状态。使用前请注意：

- 只在你信任并理解其构建与测试命令的项目中运行
- 使用最小权限的本机凭据，不要向智能体暴露不必要的密钥
- 在状态变更前保持工作区干净，并保留可恢复的 Git 备份
- 仔细检查项目绑定、智能体配置、可写范围和高风险动作授权
- 不要手工改写持久事件、租约或验证证据
- 命令返回部分成功或副作用未知时，先检查状态和证据，不要盲目重跑
- 清理、发布、推送和外部网络动作应始终由用户明确授权

### 仓库结构

```text
orch/
├── Cargo.toml
├── Cargo.lock
├── crates/
│   ├── orch-core/   # 协议域模型与状态投影
│   ├── orch-host/   # Git、worktree、门、账本、调度与恢复
│   ├── orch-cli/    # orch 命令行入口
│   └── orch-ui/     # 只读 UI 组件；alpha 版尚未接入公开 CLI
└── docs/            # 编译进二进制的机械契约指南

.githooks/
└── reference-transaction  # Git 引用写保护守卫
```

### 产品快照边界

本仓库只发布产品源码、Git 引用守卫、README、许可证和必要的忽略规则。

开发过程中的计划、研究、交接资料、完整自举夹具、日志、缓存、生成工作树和 `target` 构建目录不属于本产品快照。

公开快照保留产品源码中的测试，但完整自举测试夹具没有随仓库发布；其中两条工作区测试路径依赖未发布夹具。因此，本版本以 locked release build 和 workspace check 作为公开源码验收入口，不宣称公开快照能够独立运行并通过完整的 `cargo test --workspace`。

### 许可证

OpenOrch 使用 [MIT License](LICENSE)。

---

## English

### What OpenOrch is

OpenOrch is a Git-driven runtime for coordinating multiple AI coding agents. Its command-line executable is named `orch`.

It is not an AI model or a hosted service. OpenOrch invokes locally available agent CLIs and project tools, combining Git commits, isolated worktrees, durable events, and verification evidence into a recoverable workflow.

### Core capabilities

- Git-backed durable facts with read-only state projections
- Task, attempt, lease, review, and completion lifecycles
- Isolated execution sites built with Git worktrees
- Fixed-commit test gates, evidence checks, and merge authorization
- Local agent CLI adapters with configurable capability and concurrency limits
- Single-step, foreground loop, and long-running `serve` modes
- Health checks, stall diagnostics, cost reporting, and bounded recovery tools
- Fail-closed behavior when identity, state, or side effects are ambiguous
- An embedded mechanical contract guide checked against the public CLI tree

### Prerelease status

`v0.1.0-alpha.1` is a product snapshot, not a stable release.

- `orch --version` reports `orch 0.1.0`
- CLI behavior, configuration formats, and event structures may still change
- Evaluate it in a non-critical repository or dedicated branch first
- Read the Release notes and preserve Git and runtime-state backups before upgrading

### Platform support

This Release provides one prebuilt artifact:

- Apple Silicon macOS
- Architecture: `aarch64` / `arm64`
- Artifact: `openorch-v0.1.0-alpha.1-aarch64-apple-darwin.tar.gz`

Intel macOS, Linux, and Windows binaries are not provided or claimed as validated for this release.

The binary is **ad-hoc signed**. It is not signed with an Apple Developer ID and is not notarized. macOS may display a security warning. If your device policy does not permit such binaries, build from source instead. Do not disable Gatekeeper globally.

### Install from the Release

Download the archive and checksum file into a new directory:

```sh
release=v0.1.0-alpha.1
archive=openorch-v0.1.0-alpha.1-aarch64-apple-darwin.tar.gz
base=https://github.com/ianzhao001/openorch/releases/download/$release

curl -fLO "$base/$archive"
curl -fLO "$base/$archive.sha256"
shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive"
```

The archive contains only:

- `orch`
- `LICENSE`

Verify the architecture, signature, and executable:

```sh
file ./orch
codesign --verify --verbose=2 ./orch
codesign -dv --verbose=4 ./orch
./orch --version
./orch guide --check
```

`file` should report a Mach-O arm64 executable, and the signature details should identify an ad-hoc signature. After verification, copy `orch` into a directory on your `PATH`.

### Build from source

Git, Rust, and Cargo are required. This snapshot was verified with Rust/Cargo 1.97.1, but no minimum supported Rust version is declared yet.

```sh
git clone https://github.com/ianzhao001/openorch.git
cd openorch
git checkout v0.1.0-alpha.1

cargo build --release --locked \
  --manifest-path orch/Cargo.toml \
  --package orch-cli

cargo check --workspace --locked \
  --manifest-path orch/Cargo.toml
```

The executable is written to:

```text
orch/target/release/orch
```

Verify it with:

```sh
orch/target/release/orch --version
orch/target/release/orch guide --check
```

### Quick start

Begin with the read-only product guide:

```sh
orch guide --check
orch guide --section quick-reference
orch guide --section worked-example
```

In a clean Rust or Node.js Git project, inspect the proposed binding first:

```sh
cd your-project
orch --root . bind
```

`bind` prints a proposal without writing to the project. After reviewing the proposed tools and commands, initialize the runtime scaffold:

```sh
orch --root . init
orch --root . doctor
```

`init` creates runtime files and updates the project’s Git ignore and attribute rules. Commit or back up existing changes before running it.

The installed binary is the authority for parameters and lifecycle details:

```sh
orch --help
orch <command> --help
orch guide
```

### Security

OpenOrch can launch external agents, create Git worktrees, execute project commands and test gates, and update Git state through controlled workflows.

- Run it only in projects whose build and test commands you trust
- Use least-privilege local credentials and avoid exposing unnecessary secrets
- Keep the working tree clean and maintain recoverable Git backups
- Review project bindings, agent configuration, writable scopes, and high-risk approvals
- Do not manually rewrite durable events, leases, or verification evidence
- If a command reports partial success or unknown effects, inspect state and evidence before retrying
- Cleanup, publication, push, and external network actions should always require explicit user authorization

### Repository layout

```text
orch/
├── Cargo.toml
├── Cargo.lock
├── crates/
│   ├── orch-core/   # Protocol domain model and state projections
│   ├── orch-host/   # Git, worktrees, gates, ledger, scheduling, and recovery
│   ├── orch-cli/    # The orch command-line entry point
│   └── orch-ui/     # Read-only UI components; not exposed by the alpha CLI
└── docs/            # Mechanical contract embedded in the executable

.githooks/
└── reference-transaction  # Git reference write guard
```

### Product snapshot boundary

This repository publishes only the product source, Git reference guard, README, license, and required ignore rules.

Development planning, research and handoff material, complete self-bootstrapping fixtures, logs, caches, generated worktrees, and `target` build outputs are outside this product snapshot.

The product source retains its tests, but the complete self-bootstrapping fixtures are not published. Two workspace test paths depend on those omitted fixtures. The public verification claim for this release is therefore limited to the locked release build and workspace check; it does not claim that the snapshot can independently pass the complete `cargo test --workspace` suite.

### License

OpenOrch is available under the [MIT License](LICENSE).
