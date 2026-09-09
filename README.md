# OpenOrch

[中文](#中文) · [English](#english) · [Changelog](CHANGELOG.md)

OpenOrch is a Git-aware local runtime for explicit AI harness invocation and lightweight multi-model consultation. The executable is named `orch`.

> Current prerelease: [`v0.1.0-alpha.3`](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.3)
>
> CLI version: `orch 0.1.0`

---

## 中文

### 产品定位

OpenOrch 在本地 Git 项目中调用已有的 AI harness CLI，固定项目、配置、附件与请求身份，保存逐席结果，并要求可信原生终态后再交还调用者判断。默认产品不提供后台调度、自动接替或无人值守编排。

默认构建公开 6 个叶命令：

- `guide`
- `doctor`
- `harness list`
- `harness lint`
- `wake`
- `consult`

手动自举能力通过可选的 `selfhost` feature 提供，当前合计 30 个叶命令。它用于显式开轮、签核、隔离派发、前台收取、审查与 `seal`，不是自动 daemon。

### 核心能力

- 从被 Git 忽略的 `.orch/harnesses.yaml` 读取本机 harness 配置
- 每次 action 只读取一次配置与附件，绑定固定字节和 SHA-256
- 统一的进程、SmartClaw 与 managed harness 调用通道
- 保存 requested/effective 参数、stdout/stderr、完成事实与逐席产物
- 显式多成员 `consult` 与轻量 fusion，不以空答、tool-only 或退出码 0 冒充有效答卷
- 原生作业状态不明确时 fail closed，不因客户端退出或观察错误猜测完成
- 编译进二进制的机械指南，并与当前 feature 的真实命令树双向核对

### 从 Release 安装

本次只提供 Apple Silicon macOS (`aarch64-apple-darwin`) 预编译产物：

```sh
release=v0.1.0-alpha.3
archive=openorch-v0.1.0-alpha.3-aarch64-apple-darwin.tar.gz
base=https://github.com/ianzhao001/openorch/releases/download/$release

curl -fLO "$base/$archive"
curl -fLO "$base/$archive.sha256"
shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive"

./orch --version
./orch guide --check
```

归档包含：

```text
orch
LICENSE
scripts/
├── wake-multica.sh
├── wake-dsh-stream.sh
├── wake-pi-stream.sh
└── wake-zcode-stream.sh
```

四个 wrapper 必须与同版二进制保持相邻和逐字一致；不要只移动 `orch` 而遗失 `scripts/`。可将整个解压目录加入 `PATH`，或将二进制与 `scripts/` 一起安装到同一目录。

二进制采用 macOS ad-hoc 签名，未使用 Apple Developer ID，也未 notarize。请先验证校验和与签名；若设备策略不接受此类二进制，请从源码构建，不要全局关闭 Gatekeeper。

```sh
file ./orch
codesign --verify --strict --verbose=2 ./orch
codesign -dv --verbose=4 ./orch
```

### 从源码构建

需要 Git、Rust/Cargo；部分 wrapper 与交付门还使用 Python 3。该快照已用 Rust/Cargo 1.97.1 验证，但尚未声明 MSRV。

```sh
git clone https://github.com/ianzhao001/openorch.git
cd openorch
git checkout v0.1.0-alpha.3

cargo build --release -p orch-cli --no-default-features \
  --locked --manifest-path orch/Cargo.toml

./orch/target/release/orch --version
./orch/target/release/orch guide --check
```

如需手动 selfhost 命令面：

```sh
cargo build --release -p orch-cli --no-default-features --features selfhost \
  --locked --manifest-path orch/Cargo.toml
```

完整产品面交付检查：

```sh
RUST_TEST_THREADS=4 sh orch/scripts/check-feature-surfaces.sh /absolute/path/to/cargo
```

该检查分别验证默认 6 叶、selfhost 30 叶、默认依赖隔离和保留的 `orch-ui`。公开快照不包含正典仓的完整 selfhost 账本、历史 fixture 与运行证据，因此不宣称能独立复现全部历史 workspace tests。

### 配置与快速开始

目标目录必须是已有首提交的 Git 仓库。先将 `.orch/harnesses.yaml` 纳入忽略规则，再按本机真实安装路径填写配置：

```yaml
version: 1
harnesses:
  assistant:
    driver: claude
    executable: /absolute/path/to/claude
    enabled: true
    cwdPolicy: project-root
```

配置只保存在本机；不要提交 token、Cookie、OAuth 数据或其它密钥。然后运行：

```sh
orch --root /path/to/project harness lint
orch --root /path/to/project harness list --action consult
orch --root /path/to/project doctor
orch --root /path/to/project wake assistant --message-file /path/to/question.md
orch --root /path/to/project consult /path/to/question.md --harness assistant
```

重复 `--harness` 可显式选择同一轮咨询成员。完整参数以当前二进制为准：

```sh
orch --help
orch <command> --help
orch guide --section quick-reference
```

### 安全与已知边界

- 只在信任其代码、构建命令和 harness 的项目中运行
- 保持最小权限；不要把本机凭据复制进项目、日志或提示附件
- 控制器退出不等于持久原生作业结束；不明确的终态必须保留为 unknown/hold
- 完成通知、文件出现或退出码 0 都不能单独证明答卷有效
- 默认产品没有后台 scheduler、daemon、自动 retry 或自动接替
- 本次仅验证 Apple Silicon macOS；Intel Mac、Linux 与 Windows 没有预编译产物或支持承诺
- CLI、配置与事件格式仍处于 alpha 阶段，可能发生不兼容变化

### 仓库结构

```text
orch/                         Rust workspace、指南、测试与产品脚本
.githooks/reference-transaction
coordination/scripts/wake-multica.sh
README.md
CHANGELOG.md
LICENSE
```

`coordination/scripts/wake-multica.sh` 是唯一公开的 `coordination/` 文件，因为它是 SmartClaw 通道的编译期产品资源。本仓库不包含任务卡、账本、工作树、运行日志、模型记录、本机配置或正典仓历史。

### 许可证

OpenOrch 使用 [MIT License](LICENSE)。版本变化见 [CHANGELOG](CHANGELOG.md)。

---

## English

### What OpenOrch is

OpenOrch invokes locally installed AI harness CLIs inside Git repositories. It binds project, configuration, attachment, and request identities; preserves per-member results; and returns control only after checking native terminal facts. The default product does not provide background scheduling, automatic takeover, or unattended orchestration.

The default build exposes six leaf commands:

- `guide`
- `doctor`
- `harness list`
- `harness lint`
- `wake`
- `consult`

An optional `selfhost` feature provides 30 leaf commands for an explicit manual lifecycle: opening and signing rounds, isolated dispatch, foreground collection, review, and `seal`. It is not an automatic daemon.

### Core capabilities

- Machine-local harness configuration from ignored `.orch/harnesses.yaml`
- One immutable configuration and attachment snapshot per action, bound by bytes and SHA-256
- A unified invocation channel for process, SmartClaw, and managed harness drivers
- Preserved requested/effective parameters, stdout/stderr, completion facts, and member artifacts
- Explicit multi-member consultation with lightweight fusion
- No promotion of empty, tool-only, or exit-zero-only output into a valid answer
- Fail-closed handling when the native job or side effects remain ambiguous
- An embedded mechanical guide checked bidirectionally against the actual feature-specific CLI tree

### Install from the Release

The prerelease provides one prebuilt target: Apple Silicon macOS (`aarch64-apple-darwin`).

```sh
release=v0.1.0-alpha.3
archive=openorch-v0.1.0-alpha.3-aarch64-apple-darwin.tar.gz
base=https://github.com/ianzhao001/openorch/releases/download/$release

curl -fLO "$base/$archive"
curl -fLO "$base/$archive.sha256"
shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive"

./orch --version
./orch guide --check
```

Archive contents:

```text
orch
LICENSE
scripts/
├── wake-multica.sh
├── wake-dsh-stream.sh
├── wake-pi-stream.sh
└── wake-zcode-stream.sh
```

The four wrappers must remain byte-identical to and adjacent to the matching binary. Do not move only `orch` and leave `scripts/` behind. Add the extracted directory to `PATH`, or install the binary and `scripts/` together under one directory.

The binary is ad-hoc signed. It is not signed with an Apple Developer ID and has not been notarized. Verify the checksum and signature first. If local policy rejects this binary, build from source instead; do not disable Gatekeeper globally.

```sh
file ./orch
codesign --verify --strict --verbose=2 ./orch
codesign -dv --verbose=4 ./orch
```

### Build from source

Git and Rust/Cargo are required; some wrappers and the delivery gate also use Python 3. This snapshot was verified with Rust/Cargo 1.97.1, but no MSRV is declared.

```sh
git clone https://github.com/ianzhao001/openorch.git
cd openorch
git checkout v0.1.0-alpha.3

cargo build --release -p orch-cli --no-default-features \
  --locked --manifest-path orch/Cargo.toml

./orch/target/release/orch --version
./orch/target/release/orch guide --check
```

To build the manual selfhost surface:

```sh
cargo build --release -p orch-cli --no-default-features --features selfhost \
  --locked --manifest-path orch/Cargo.toml
```

Run the standalone product-surface gate with:

```sh
RUST_TEST_THREADS=4 sh orch/scripts/check-feature-surfaces.sh /absolute/path/to/cargo
```

It validates the default six leaves, the 30-leaf selfhost surface, default dependency isolation, and the retained `orch-ui`. The public snapshot omits the canonical repository's complete selfhost ledgers, historical fixtures, and runtime evidence, so it does not claim standalone reproducibility of every historical workspace test.

### Configuration and quick start

The target must be a Git repository with an initial commit. Ignore `.orch/harnesses.yaml`, then configure real absolute executable paths for this machine:

```yaml
version: 1
harnesses:
  assistant:
    driver: claude
    executable: /absolute/path/to/claude
    enabled: true
    cwdPolicy: project-root
```

Keep this file local. Never commit tokens, cookies, OAuth material, or other secrets. Then run:

```sh
orch --root /path/to/project harness lint
orch --root /path/to/project harness list --action consult
orch --root /path/to/project doctor
orch --root /path/to/project wake assistant --message-file /path/to/question.md
orch --root /path/to/project consult /path/to/question.md --harness assistant
```

Repeat `--harness` to select multiple consultation members explicitly. The installed binary is authoritative for parameters:

```sh
orch --help
orch <command> --help
orch guide --section quick-reference
```

### Security and known boundaries

- Run OpenOrch only in projects whose code, build commands, and harnesses you trust
- Use least-privilege credentials and do not copy local secrets into repositories, logs, or attachments
- Controller exit does not prove that a persistent native job ended; ambiguous terminal state remains unknown/held
- A notification, file appearance, or exit code zero cannot alone prove that an answer is valid
- The default product has no background scheduler, daemon, automatic retry, or automatic takeover
- Only Apple Silicon macOS is validated for this release; no Intel macOS, Linux, or Windows binary is provided
- CLI, configuration, and event formats are alpha and may change incompatibly

### Repository layout

```text
orch/                         Rust workspace, guide, tests, and product scripts
.githooks/reference-transaction
coordination/scripts/wake-multica.sh
README.md
CHANGELOG.md
LICENSE
```

`coordination/scripts/wake-multica.sh` is the only published file under `coordination/` because it is a compile-time SmartClaw product resource. Task cards, ledgers, worktrees, runtime logs, model transcripts, machine configuration, and canonical repository history are not included.

### License

OpenOrch is available under the [MIT License](LICENSE). See the [CHANGELOG](CHANGELOG.md) for release history.
