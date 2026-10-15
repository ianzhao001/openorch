# OpenOrch

[中文](#中文) · [English](#english) · [Changelog](CHANGELOG.md)

OpenOrch connects installed AI harnesses to Codex Desktop and DSH Web through one
shared skill and a bundled local runtime. Choose your own clients/models, consult
one or several members, and let the current host synthesize their original answers.

Source snapshot updated **2026-10-15** — see [Unreleased changes](CHANGELOG.md).
Latest source tag: [`v0.1.0-alpha.14`](https://github.com/ianzhao001/openorch/tree/v0.1.0-alpha.14).
Latest downloadable bundle: [`v0.1.0-alpha.13`](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.13).
Runtime version: `0.1.0`. The source tag, current main and downloadable bundle are different snapshots.

## 中文

### 安装

本版面向 Apple Silicon macOS。需要 Python 3.9+、Git，以及已安装的 Codex 或 DSH。
DSH 插件管理还使用其正常的 Node.js/pnpm 环境，并从官方包源安装声明的技能依赖。
完整 Release 包已包含运行核心，使用者不需要 Rust，也不需要任何私有仓库。

截至 2026-10-15，`alpha.14` 只有源码标签，没有可下载的 Release 附件。
[历史 Release](https://github.com/ianzhao001/openorch/releases) 中最新有附件的是 `alpha.13`；
它不包含当前 main 的未发布修复，也不包含 `alpha.14` 的 DSH `AGENTS.md` 包白名单修复。
使用历史包时按该包自己的说明和 SHA-256 文件验证；需要当前代码时，使用下方源码构建说明。
本次只同步源码与文档，不发布或替换任何版本的二进制。

以下安装命令适用于已经下载、校验并解压的完整版本包：

```sh
bundle=/absolute/path/to/extracted-release
python3 "$bundle/install.py" --host both
```

只装一端时，将 `both` 改为 `codex` 或 `dsh`。若客户端不在 PATH，使用
`--codex-bin /绝对路径/codex` 或 `--dsh-bin /绝对路径/dsh`。
默认安装前缀为 `~/.local/share/openorch`；自定义 `--prefix` 后，更新和卸载也使用同一前缀。

安装器核对完整性、平台和运行核心，然后通过两个宿主的原生插件命令登记并回读。
部分成功会逐宿主列出。安装成功不等于已验证登录、模型身份或咨询结果。
源码仓不包含预编译运行时，请使用完整 Release 包进行插件安装。

### 配置一次，跨项目使用

Codex 安装后打开新任务使用 OpenOrch。DSH Web 新会话选择包含 Skills 的“标准模式”；
极简双工具预设没有 `skill` 工具，安装器不会改变用户的预设。

在已有首提交的 Git 项目中说：

> 使用 OpenOrch，先发现本机通道，让我选择客户端、原生模型设置和默认咨询成员。

发现过程不调用模型。用户选择一个默认单席，也可选择两到五个不同别名组成默认 fusion。
配置保存在本机 `~/.config/openorch/profile.json`，权限0600。
已有项目的 `.orch/harnesses.yaml` 保持原样；新项目仅在缺少配置时从个人默认初始化。
脏工作区可用，linked worktree 共享主仓配置；OpenOrch 不创建项目的首个提交。

之后直接说“用 OpenOrch 咨询默认成员：……”或“用 OpenOrch fusion 分析：……”。
宿主读取逐席完整结果，说明结论、分歧、失败和原始答卷位置。不同别名可以仍使用同一后端，
不能仅凭成员数宣称异构模型交叉验证。原生客户端按自己的额度或价格计费。

详细配置、显式成员覆盖、问题文件与项目内文本附件见
[插件说明](plugins/openorch/README.md)和[助手命令](plugins/openorch/HELPER.md)。

### 本次源码更新（未发布）

- Consult 保留适配器报告的超时类型，避免以总耗时重新推断失败原因。
- DSH 已观察到原生终态但最终答卷集合不合法时，保留终态事实，同时继续以退出码 74 拒绝成功。
- `orch-ui` 按 grapheme 的显示列宽裁切终端文字。
- 可选 `selfhost` 增加按实际分配空间计量的存储策略、保留窗口和带完整恢复证据的精确清理；
  归属确认不等于删除许可。默认六命令产品面不增加这些维护入口。

这些是已同步的源码行为，不代表历史安装包已获得更新，也不承诺任意工作区都能满足容量上限。

### 当前源码支持边界

| 类型 | 本版支持 |
| --- | --- |
| 插件宿主 | Codex Desktop、DSH Web |
| Consult 目标 | Codex、Claude、OpenCode、Cursor、MiMo、CodeBuddy、SmartClaw、DSH、Pi、ZCode；以本机实际发现结果为准 |
| 当前不支持的 Consult 目标 | AGY |
| 命令面 | 默认6个叶命令；可选 selfhost 30个；既有 `orch-ui` 保留 |

DSH 作为宿主可调用其它受支持通道；宿主集成与目标通道能力分开判断。
本插件不扩展执行/审查动作支持，不新增后台 scheduler、自动接替或自动重试。
缺少客户端、未登录、unsupported、partial、空答或 tool-only 结果都会保留为实际状态，
不会以进程退出0或文件出现冒充有效答卷，也不会偷偷替换模型。

### 更新与卸载

对新 Release 运行同一安装命令。每个版本保存在安装器前缀的独立目录中；同字节重复安装幂等，
同版本不同字节和外来同名登记会被拒绝。个人配置、项目配置及咨询证据均保留。

```sh
python3 /path/to/release/install.py --host codex --uninstall
python3 /path/to/release/install.py --host dsh --uninstall
```

卸载通过对应宿主移除注册，宿主自行管理其缓存。安装器前缀内的版本载荷不会自动删除，
另一宿主可以继续引用；个人默认和项目证据也不会删除。遇到部分失败时按错误修复后，
只对需要处理的宿主重跑。原生登录或权限要求使用客户端的正常流程处理。

### 运行时来源与源码

完整版本包应从固定产品源码构建；包内 `runtime/provenance.json` 记录该包的源码提交、
工具链与构建参数，并对应它自己的 Release tag，不能借用其它版本的资格证明。
全包 `manifest.json` 与包内七资源 `runtime/manifest.json` 用于检测字节变化。
核心是 macOS ad-hoc 签名，没有 Apple Developer ID 签名或 notarization；不宣称
可复现构建或不存在的 GitHub attestation。验证示例：

```sh
codesign --verify --strict "$bundle/plugins/openorch/runtime/orch"
"$bundle/plugins/openorch/runtime/orch" --version
"$bundle/plugins/openorch/runtime/orch" guide --check
```

历史发布验证使用 Rust 1.98.0；暂未声明 MSRV。构建核心可运行：

```sh
cargo build --release -p orch-cli --no-default-features \
  --locked --manifest-path orch/Cargo.toml
```

需要手动 selfhost 时增加 `--features selfhost`。公开源码保留产品代码与测试，省略私有
自举账本、历史 fixture 和运行证据，不宣称能复现正典仓的每一个历史 workspace test。
默认产品面包含 `guide`、`doctor`、`harness list`、`harness lint`、`wake`、`consult`；
完整参数以安装核心的 help/guide 为准。

`SOURCE-MANIFEST.json` 只列导出的相对路径与SHA-256。公开树不包含私有历史、roster、
本机配置或原生转写；`coordination/scripts/wake-multica.sh` 是唯一公开的 coordination 资源。
产品仍为 alpha，仅验证 Apple Silicon macOS。许可证为 [MIT](LICENSE)。

## English

### Install and use

As of 2026-10-15, `alpha.14` is a source tag without downloadable Release assets.
The latest bundle with assets is [alpha.13](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.13);
it does not include current main's unreleased fixes or alpha.14's DSH `AGENTS.md`
package-allowlist fix. Follow that bundle's own instructions and checksum when
using a historical release, or build current source using the commands above.
This update publishes source and documentation, not new or replacement binaries.
Complete historical bundles target Apple Silicon macOS. Python3.9+, Git and the native Codex/DSH client are
required. DSH uses its normal Node.js/pnpm environment and fetches the declared
filesystem-skill dependency. The bundle includes the runtime; Rust and the private
development repository are not required. The source checkout alone is not an
installable runtime bundle.

Verify the chosen archive against its accompanying SHA-256 file, extract it, then run
`python3 BUNDLE/install.py --host both`. Select `codex` or `dsh` for one host.
Optional `--codex-bin`, `--dsh-bin` and `--prefix` accept explicit paths. Reuse the
same prefix for updates/removal. Native registrations are reported only after
readback; partial results remain visible.

Start a new Codex task after installation. In DSH Web use the Standard preset
with Skills; the minimal two-tool preset does not provide the skill tool. In an
existing Git project, ask OpenOrch to discover clients and help select your own
native model settings and defaults. Discovery calls no models and does not verify
login. Save one single advisor and optionally two to five fusion aliases. The
mode0600 personal profile is reused across projects; existing project configuration
is preserved. Dirty/linked worktrees work, and no initial Git commit is created.

### Current source changes (unreleased)

- Consult preserves adapter-reported timeout identity instead of reclassifying failures from elapsed time.
- DSH retains the observed native terminal fact when final-answer validation fails, while still refusing success with exit 74.
- `orch-ui` clips terminal text by grapheme display width.
- Optional `selfhost` adds allocated-byte storage accounting, retention windows and exact retirement with complete recovery evidence. Ownership classification alone never grants deletion permission; these maintenance commands are not added to the default six-command surface.

These source changes do not update historical bundles or guarantee that every workspace can meet a storage budget.

### Codex host coordination

The release includes its published collaboration policy at
`plugins/openorch/AGENTS.md`. In Codex Desktop, use Codex-native delegation,
messaging, and waits to coordinate bounded local implementation work. OpenOrch
is only the explicit cross-harness consultation and fusion layer: it does not
duplicate Codex's task queue, background scheduler, automatic takeover, or retry
behavior. Each subtask has one execution owner, and the current host keeps
native-subagent results separate from OpenOrch consultation evidence.

Ask “Use OpenOrch to consult my default advisor about …” or “Use OpenOrch fusion
about …”. The host reads complete member outcomes and synthesizes conclusions,
disagreements, failures and original-answer links. Aliases can share a backend;
member count does not prove model diversity. Native client usage costs still apply.

Current Consult targets are Codex, Claude, OpenCode, Cursor, MiMo, CodeBuddy and
SmartClaw, with native compatibility channels for DSH, Pi and ZCode, subject to
runtime discovery. AGY Consult is unsupported. DSH Web can still host the plugin and invoke supported
targets. Execution/review capabilities and the default6/selfhost30/UI boundaries
are unchanged. There is no added daemon, scheduler, automatic takeover or retry.

### Update, remove and verify

Run a new release's installer with the same prefix and selected hosts. Same-version
different bytes or foreign registrations are refused. `--uninstall` removes only
selected native registrations; hosts manage their own caches. Installer-owned
version payloads, personal settings, project configuration and evidence remain.
Another host may still reference an older payload. Resolve a reported native
dependency/login/conflict through its normal workflow, then rerun the affected host.

The complete bundle and seven runtime resources have separate integrity inventories.
The bundled `runtime/provenance.json` identifies fixed source inputs and
compiler/build details for this Release tag; it does not reuse an older
release's source-equality claim. The core is ad-hoc signed, not Developer ID signed
or notarized; no reproducible-build or unavailable attestation claim is made.
Build-from-source commands and signature checks appear above; historical release validation used Rust1.98.0,
and no MSRV is declared.

The public snapshot includes product sources and their relative-path SHA-256
inventory, not private history, machine configuration, roster, transcripts or the
full private selfhost test history. See the [plugin README](plugins/openorch/README.md),
[helper reference](plugins/openorch/HELPER.md), [changelog](CHANGELOG.md) and
[MIT license](LICENSE).


## MCP / ACP

The current source includes four MCP tools for Codex through a package-local
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
