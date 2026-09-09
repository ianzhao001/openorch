# Changelog

[中文](#中文) · [English](#english)

This file records the public OpenOrch release line. Internal selfhost rounds, task ledgers, reviews, and runtime evidence are intentionally excluded.

## 中文

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
