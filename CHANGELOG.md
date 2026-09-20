# Changelog

[中文](#中文) · [English](#english)

This file records the public OpenOrch release line. Internal selfhost rounds, task ledgers, reviews, and runtime evidence are intentionally excluded.

## 中文

### [v0.1.0-alpha.10](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.10) — 2026-09-20

- 新增本机 Native Discovery 与 WebUI 显式、有限的 Fusion；固定问题、HEAD、角色和原生配置快照。
- 详情读取不再被后台轮询打断，提供独立取消与 12 秒浏览器超时。
- 保守展示历史证据不完整与未知终态；OpenCode 只读就绪检查及同通道启动间隔保护。
- 保留 alpha.9 的 DSH 包清单修复，统一公开文档、支持范围和插件版本；默认核心仍为六叶命令。

### [v0.1.0-alpha.9](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.9) — 2026-09-16

- 将 AGENTS.md 纳入 DSH 包清单，使原生安装可以物化并核验完整插件；产品核心与 alpha.8 相同。


### [v0.1.0-alpha.8](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.8) — 2026-09-16

本版在 r89 的只读终端观察面板基础上，新增本机 loopback-only WebUI；默认 CLI、手动 selfhost 和插件调用边界保持不变。

- 新增独立 `orch-web` 前台服务与内嵌 WebUI，只绑定 `127.0.0.1`，不增加默认 `orch` 命令、不控制 provider 或修改项目
- 以启动 capability、严格同源/Origin/Host 校验、无 CORS、CSP/no-store/nosniff/no-referrer 和无任意文件接口保护本机观察 API
- 支持任务、全部调用和成员三视图；关联任务与未关联调用分区显示，共享 30 卡窗口，窗口内异常优先且可按 30 加载更多
- 双主题、双语、宽中窄布局、键盘可达性、减少动态效果、刷新与详情过期保护；失败读取保留旧快照并诚实显示错误
- 页面不加载 CDN、远程字体或答卷图片；不可信 Markdown 经受限语义重建，外链仅保留绝对 HTTP/HTTPS 且附安全关系
- `orch-web` 与既有 `orch-tui` 都仅从源码构建；预编译插件包仍为默认 6 叶 runtime 与四个 wrapper

仅验证 Apple Silicon macOS。核心为 ad-hoc 签名，非 Developer ID、未 notarize；公开快照不包含 r90 账本、审查现场、运行配置、历史 fixture 或模型转写。

### [v0.1.0-alpha.7](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.7) — 2026-09-14

本版发布 r89 的只读观测产品增量，继续保持默认 CLI、手动 selfhost 和插件宿主边界。

- 新增独立 `orch-tui` 前台观察面板：只读刷新 invocation 事实，不启动、取消、收取、reconcile 或 GC
- 增加 invocation observation 捕获、文件大小上限与默认安全状态，保留旧快照并将失败显式标为未知
- TUI 支持调用/通道/任务导航、详情、项目切换、手动刷新和安全复制；不把观察结果消费成 planner 答卷
- PTY、宽字符、kernel 状态、clipboard 子进程和凭据格式均采用保守失败边界；非 TTY 与无效参数不会伪装成功
- DSH、Pi、ZCode 作为 Consult 目标与宿主能力分开记录；AGY 仍不是 Consult 目标
- 预编译包仍只提供默认 6 叶 `orch` 与四个 wrapper；`orch-ui`/`orch-tui` 仅从源码构建

本版仅验证 Apple Silicon macOS。二进制为 ad-hoc 签名，非 Developer ID、未 notarize；公开快照
不携带 r89 账本、私有审查现场、历史 fixture、模型转写或本机配置。

### [v0.1.0-alpha.6](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.6) — 2026-09-13

本版继续完善全通道的原生 Consult/执行安全边界，并修复 r88 暴露的通道、回收和固定 source gate 问题。

- DSH envelope 原生 model/settings 快照、Pi/ZCode/DSH 项目历史和 native terminal 事实继续纳入 action 绑定
- 新增 DSH session uniqueness、native monitor reclaim、等价 generation 回收和固定 source ref gate 保护
- 增加 Claude auto permission mode 的显式渲染与 native-project-history 保留
- 强化 quarantine、reclaim、hook、verify/collect gate 的恢复路径，避免 foreign PID、旧 source 或错误回执越过边界
- 默认6/selfhost30、插件安装边界与无后台自动编排约束保持不变；AGY 仍不是 Consult 目标

### [v0.1.0-alpha.5](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.5) — 2026-09-12

本版将更多原生通道接入 Consult，并把原生设置与调用项目历史绑定纳入真实验证。

- DSH、Pi、ZCode 进入 Consult 目标；每次 action 保留 provider/model/effort 与原生终态事实
- DSH Consult 支持原生设置快照和完整项目历史定位，Pi/ZCode 支持原生终答与项目历史分离
- 强化 action discovery、账号环境边界、Pi legacy envelope、凭据漂移与失败/未知终态的保留语义
- 增加 review quarantine 与 pending-receipt 类型化恢复，避免错误或晚到答卷越过裁决边界

### [v0.1.0-alpha.4](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.4) — 2026-09-10

OpenOrch 现在提供 Codex Desktop 与 DSH Web 共用的插件包。核心版本仍为 `orch 0.1.0`。

- 同一份技能、助手和预编译核心，通过两个宿主的原生插件机制安装
- 发现本机通道，保存用户选择的默认单席和多席组，跨 Git 项目复用；已有配置保持原样
- 单席与显式 fusion 使用既有 Consult 通路，宿主综合完整答卷、分歧和失败
- 完整 Release 携带五个运行资源及校验清单，无需使用者安装 Rust
- 版本目录、外来登记、同版本不同字节及单宿主卸载的边界检查；更新和卸载保留用户数据
- DSH 使用安装包相对路径的独立技能提供者，处理本地包依赖和 hoisted pnpm 路径
- 修复 SmartClaw 重复失败工具回执造成的原生终态歧义；超时仍不能被晚到答卷改写为成功
- 显式产品源导出和公开输入/制品对照，继续排除私有历史、配置、账本与转写

本版仅验证 Apple Silicon macOS。Codex/DSH 宿主不代表所有同名通道均支持 Consult：
当前 AGY 仍不是受支持的 Consult 目标。执行/审查能力、默认6/selfhost30
命令边界及现有 UI 保持；不新增后台自动编排。二进制为 ad-hoc 签名，非 Developer ID，
不宣称不存在的 attestation 或可复现构建。

### [v0.1.0-alpha.3](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.3) — 2026-09-09

这是一次产品边界重构版本。CLI 仍报告 `orch 0.1.0`，但默认安装面与 alpha.2 不兼容。

#### 主要变化

- 将默认产品收敛为显式 harness 调用与轻量咨询，公开命令树固定为 6 个叶命令
- 新增本机 `.orch/harnesses.yaml` 配置发现、严格校验与 action 能力筛选
- 将进程、SmartClaw 和 managed harness 统一到固定输入、可信终态与逐席产物通道
- `consult` 改为显式成员选择和轻量 fusion；结果先稳定保存，再由调用者判断
- 引入 generic review 与 schema v3 手动 selfhost，成员、模型、额度和期限不再进入代码授权签名
- 默认 CLI 与 selfhost writer/UI 依赖隔离；`orch-ui` 继续作为只读 workspace 成员保留和验收
- 新增受管 Cargo 诊断缓存及统一的 storage maintenance/status/sweep，unknown、active、dirty 和 blocked 现场继续保守留存
- 强化原生完成、final-drain、receiptless terminal、deadline、review accounting 与 wrapper 字节身份

#### 破坏性变化

- 默认构建不再公开旧任务、轮次、账本和自动编排写入口；手动生命周期须显式启用 `selfhost`
- 不再提供 scheduler/daemon/wave/nudge/resume/retry-dead/handshake 自动编排承诺
- 独立二进制分发现在必须同时携带同版本 `scripts/` 下的四个 code-owned wrapper

#### 已知边界

- 仅验证 Apple Silicon macOS；预编译二进制为 ad-hoc 签名且未 notarize
- `selfhost` 预编译二进制不随本版本发布，但可从源码构建
- 公开快照不携带正典账本、历史 fixture 或运行证据，不宣称可复现全部历史 workspace tests
- CLI、配置和事件格式仍处于 alpha 阶段

### [v0.1.0-alpha.2](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.2) — 2026-08-27

- 引入动态 review panel、法定人数与补位闭环
- 增加 lease-scoped review spool、原子提升与崩溃恢复
- 增加 candidate/merge gate lane、collect proof 复用与 final-tree proof
- 加强重派、resume、backend receipt 和 terminal envelope

### [v0.1.0-alpha.1](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.1) — 2026-08-15

- 首个公开 Alpha 产品源码快照
- 发布 Apple Silicon macOS CLI 与 SHA-256 校验文件
- 建立源码、MIT License、README 和匿名发布边界

---

## English

### [v0.1.0-alpha.10](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.10) — 2026-09-20

- Add local Native Discovery and explicit finite Web Fusion with fixed questions, HEAD, roles and native settings.
- Keep detail reads independent of polling, with cancellation and a 12-second browser timeout.
- Preserve historical-unverified/unknown terminal diagnostics; check OpenCode readiness and space same-driver launches.
- Retain the alpha.9 DSH package fix and align public documentation, capabilities and plugin versions. The core still exposes six default leaf commands.

### [v0.1.0-alpha.9](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.9) — 2026-09-16

- Include AGENTS.md in the DSH package inventory so native installation can materialize and verify the complete plugin. Core behavior matches alpha.8.


### [v0.1.0-alpha.8](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.8) — 2026-09-16

This release adds a local loopback-only WebUI alongside r89's read-only terminal observation panel. Default CLI, manual selfhost, and native-plugin boundaries remain unchanged.

- Adds the independent `orch-web` foreground service and embedded WebUI, bound only to `127.0.0.1`; it adds no default `orch` command, provider control, or project writes
- Protects local observation APIs with a startup capability, exact same-origin/Origin/Host checks, no CORS, CSP/no-store/nosniff/no-referrer headers, and no arbitrary-file endpoint
- Adds Tasks, All calls, and Members views with separate associated/unassociated sections, a shared 30-card window, in-window failure/invalid priority, and load-more by 30
- Adds bilingual themes, responsive layouts, keyboard access, reduced motion, refresh/detail staleness protection, and honest retained snapshots after failed reads
- Loads no CDN, remote font, or answer image; untrusted Markdown is reconstructed from a restricted semantic set and links are limited to absolute HTTP/HTTPS with safe relations
- `orch-web` and `orch-tui` are source-built only; the prebuilt plugin bundle remains the default six-leaf runtime plus four wrappers

Only Apple Silicon macOS is validated. The core is ad-hoc signed, not Developer ID signed or notarized. The public snapshot excludes r90 ledgers, review sites, runtime configuration, historical fixtures, and model transcripts.

### [v0.1.0-alpha.7](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.7) — 2026-09-14

This release publishes the r89 read-only observation increment while keeping the
default CLI, manual selfhost, and native plugin-host boundaries unchanged.

- Adds the independent `orch-tui` foreground observation panel: it refreshes invocation facts only and never starts, cancels, collects, reconciles, or GCs work
- Adds bounded invocation-observation capture and conservative defaults; failed reads retain the old snapshot and surface an explicit unknown state
- TUI navigation covers invocation/channel/task views, details, project switching, refresh, and safe copy without consuming planner answers
- PTY, Unicode width, kernel state, clipboard children, and credential-format handling fail closed; non-TTY and invalid arguments never look successful
- DSH, Pi, and ZCode are documented as Consult targets separately from the DSH plugin host; AGY remains unsupported for Consult
- The prebuilt bundle still contains only the default six-leaf `orch` and four wrappers; `orch-ui`/`orch-tui` are source-built only

Only Apple Silicon macOS is validated. The binary is ad-hoc signed, not Developer ID
signed, and not notarized. The public snapshot excludes r89 ledgers, private review
sites, historical fixtures, model transcripts, and machine configuration.

### [v0.1.0-alpha.6](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.6) — 2026-09-13

This release continues the native Consult/execute safety work and fixes the channel, reclaim, and fixed-source gate gaps found in r88.

- Binds DSH envelope settings, Pi/ZCode/DSH project history, and native terminal facts to each action
- Adds DSH session uniqueness, native monitor reclamation, equivalent-generation reclaim, and fixed source-ref gate protection
- Adds explicit Claude auto-permission rendering while preserving native project history
- Hardens quarantine, reclaim, hook, verify, and collect recovery paths against foreign PIDs, stale sources, and invalid receipts
- Retains default6/selfhost30, plugin installation boundaries, and no background automatic orchestration; AGY remains unsupported for Consult

### [v0.1.0-alpha.5](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.5) — 2026-09-12

This release expanded native Consult coverage and bound native settings and original-project history to actual invocation evidence.

- DSH, Pi, and ZCode became Consult targets with provider/model/effort and native-terminal facts preserved per action
- DSH Consult used pinned native settings and full calling-project history; Pi/ZCode retained native final answers and project-history separation
- Hardened action discovery, account-environment boundaries, the Pi legacy envelope, credential-drift handling, and failed/unknown terminal preservation
- Added review quarantine and typed pending-receipt recovery so bad or late answers cannot cross verdict boundaries

### [v0.1.0-alpha.4](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.4) — 2026-09-10

OpenOrch now ships one plugin for Codex Desktop and DSH Web. The core still reports `orch 0.1.0`.

- Shared skill, helper and prebuilt runtime installed through both native plugin mechanisms
- Client discovery and user-chosen single/fusion defaults reused across Git projects, preserving existing configuration
- Existing Consult routing for one or several members, with complete-answer synthesis and honest partial/failure outcomes
- Self-contained release with five runtime resources and integrity inventories; no end-user Rust requirement
- Immutable version payloads, foreign-registration checks, same-version byte protection and data-preserving removal
- An installed-package-relative DSH skill provider, local dependency materialization and hoisted-pnpm path resolution
- Correct handling of duplicate error-only SmartClaw tool receipts without promoting a late answer past a hard deadline
- Explicit product-source export and source/artifact comparisons, excluding private history, configuration, ledgers and transcripts

Only Apple Silicon macOS is validated. AGY remains an unsupported Consult target even though
DSH Web is a supported plugin host. Execution/review capabilities, default6/selfhost30 and retained UI boundaries
are unchanged; no automatic orchestration is added. The binary is ad-hoc signed, not Developer ID signed;
no unavailable attestation or reproducible-build guarantee is claimed.

### [v0.1.0-alpha.3](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.3) — 2026-09-09

This release restructures the product boundary. The CLI still reports `orch 0.1.0`, but the default installation surface is not compatible with alpha.2.

#### Highlights

- Narrows the default product to explicit harness invocation and lightweight consultation with exactly six public leaf commands
- Adds machine-local `.orch/harnesses.yaml` discovery, strict validation, and action capability filtering
- Unifies process, SmartClaw, and managed harnesses behind immutable input, trusted terminal, and per-member artifact contracts
- Makes `consult` use explicit member selection and lightweight fusion; stable results are returned for caller judgment
- Adds generic review and schema-v3 manual selfhost behavior, keeping members, models, quotas, and deadlines outside code-authorization signatures
- Isolates default CLI dependencies from selfhost writers and UI while retaining and validating the read-only `orch-ui` workspace member
- Adds managed Cargo diagnostic caches and unified storage maintenance/status/sweep with conservative holds for unknown, active, dirty, or blocked sites
- Hardens native completion, final drain, receiptless terminals, deadlines, review accounting, and wrapper byte identity

#### Breaking changes

- The default build no longer exposes legacy task, round, ledger, or automatic-orchestration writers; explicit manual lifecycle commands require the `selfhost` feature
- No scheduler, daemon, wave, nudge, resume, retry-dead, or handshake automation is promised
- Portable binary distributions must now keep the four matching code-owned wrappers under an adjacent `scripts/` directory

#### Known boundaries

- Only Apple Silicon macOS is validated; the binary is ad-hoc signed and not notarized
- No prebuilt selfhost binary is published, though it remains buildable from source
- The public snapshot excludes canonical ledgers, historical fixtures, and runtime evidence and does not claim reproduction of every historical workspace test
- CLI, configuration, and event formats remain alpha

### [v0.1.0-alpha.2](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.2) — 2026-08-27

- Added dynamic review panels, quorum, and backfill
- Added lease-scoped review spools, atomic promotion, and crash recovery
- Added candidate/merge gate lanes, collect-proof reuse, and final-tree proofs
- Hardened reassignment, resume, backend receipts, and terminal envelopes

### [v0.1.0-alpha.1](https://github.com/ianzhao001/openorch/releases/tag/v0.1.0-alpha.1) — 2026-08-15

- First public alpha product-source snapshot
- Published the Apple Silicon macOS CLI and SHA-256 checksum
- Established the source, MIT License, README, and anonymous publication boundary
