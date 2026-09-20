# OpenOrch · Codex + DSH Web

在当前宿主中咨询本机已安装的 AI 通道，或显式选择多名成员进行 fusion。
个人默认配置可跨 Git 项目复用，由宿主综合结论、分歧和原始答卷。

Consult installed AI harnesses from your current host, then synthesize their
answers with original evidence. Save personal defaults once and reuse them in
Git projects.

## 安装 / Install

下载 [OpenOrch Release](https://github.com/ianzhao001/openorch/releases) 中的完整
Apple Silicon macOS 插件归档并解压。插件版本为 `0.1.0-alpha.10`，核心版本仍是
`orch 0.1.0`。源码仓本身不含运行核心，不能代替完整 Release 安装包。

Download and extract the complete Apple Silicon macOS plugin archive. It includes
the default runtime and four channel resources. No Rust toolchain is needed.
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
不会自动覆盖已有项目配置。没有后台常驻编排或自动追加咨询轮次。源码还保留独立的只读
`orch-tui` 观察面板；它不启动、取消、收取或回收任何调用，也不消费 planner 答卷。

## 支持边界 / Current support

| Surface | Current behavior |
| --- | --- |
| Plugin hosts | Codex Desktop and DSH Web |
| Consult targets | Codex, Claude, OpenCode, Cursor, MiMo, CodeBuddy, SmartClaw, DSH, Pi, ZCode, subject to actual discovery/configuration |
| Unsupported Consult targets | AGY |
| Core surfaces | Default CLI 6 commands; selfhost CLI 30; read-only `orch-tui`; explicit finite Fusion in `orch-web` |

DSH 既可作为插件宿主，也可作为 Consult 目标；Pi、ZCode 同样受支持。宿主集成与目标通道
能力仍需分别核验，并以运行时发现、原生终态和项目历史证据为准。
执行和审查动作的支持范围没有由本插件扩展。配置模型名称不等于原生证据已验证该身份。

`orch-tui` 与 `orch-web` 仅随源码构建，不随默认六命令预编译 runtime 发布。源码构建命令为：

```sh
cargo build -p orch-ui --bin orch-tui --locked --manifest-path orch/Cargo.toml
orch/target/debug/orch-tui --root /absolute/project
cargo run --locked --manifest-path orch/Cargo.toml -p orch-ui --bin orch-web --all-features -- --root /exact/git/root
```

终端面板以只读方式每两秒刷新已捕获的 invocation 观察，明确区分来源时间、陈旧、终态与未知，
并对展示/复制内容做安全裁剪；非 TTY、参数错误或初始化失败均不会伪装成成功。

`orch-web` 是仅绑定 `127.0.0.1` 的本机浏览器观察面板。启动 capability、精确同源检查、无 CORS、严格响应安全头和 opaque ID API 共同限制浏览器访问；它没有任意文件接口。观察接口保持只读；用户可显式保存项目本地角色/组合并启动有限 Fusion 咨询，但不会收取、reconcile 或 GC 调用，答案 Markdown 被视为不可信内容并在本地净化，页面不加载外部资源。

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
one source with a supplied default `orch 0.1.0` runtime and four scripts. It checks
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

WebUI source now supports explicit finite Fusion and read-only Native Discovery. See the public README for build instructions and boundaries.
