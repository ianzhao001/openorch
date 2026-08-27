---
title: orch AI 机械契约指南
guideSchemaVersion: 1
scope: portable-mechanical-contract
---

# orch AI 机械契约指南

本指南随 `orch` 源码交付，并被编译进 `orch` 二进制。它只描述跨项目稳定、由运行时
强制的机械语义。项目自己的模型选择、人员排班、审查分档、故障经验和用户裁定应写入
`coordination/AI-OPERATOR-RUNBOOK.md`；当前轮次、提交和任务投影只写入
`coordination/CURRENT.md`。项目策略不得放宽本文契约。

参数细节以同一二进制的 `orch <command> --help` 为准。若指南、项目策略、历史交接与
运行时实际拒绝发生冲突，停止状态变更，以事件账本和当前二进制的 fail-closed 结果为准；
新颖故障再查源码。

<!-- orch-guide-section:scope -->
## 使用范围与读取顺序

AI 接管一个项目时按以下顺序读取：

1. 运行 `orch guide --check`，再读 `orch guide` 或所需 `--section`。
2. 读项目的 `coordination/AI-OPERATOR-RUNBOOK.md`；它只能增加本地约束。
3. 运行 `orch current` 并读 `coordination/CURRENT.md`，取得动态状态。
4. 只有异常恢复或审计时才读取事件尾部、证据、日志和源码。

本文不替代 executor 的 `coordination/PROTOCOL.md`，也不授权任何合并、发布、删除、
审批或绕过门的动作。`orch guide` 本身只向 stdout 输出内嵌文本，不要求项目存在
`coordination/`，也不读写事件账本。
<!-- orch-guide-section-end:scope -->

<!-- orch-guide-section:truth -->
## 真值、投影与观察信号

按机械权威排序理解状态：

1. 当前轮 `events.jsonl` 中通过解析和身份校验的事件是 durable 事实；状态名称只是事件折叠结果。
2. Git 提交、refs、固定 HEAD 的 REPORT/review/evidence 是事件所绑定的可复核证据。
3. WAL 是账本的逐字节恢复镜像；它只能修复严格前缀缺失，不能创造业务事实。
4. `CURRENT.md`、`status`、`snapshot` 和 UI 是派生观察面；刷新投影不等于改变任务事实。
5. heartbeat、PID、日志增长和客户端窗口是 liveness 证据，不是 durable action 的所有权证明。

所有 attempt、review 和 durable action 都必须绑定当前 round/task/attempt 以及相应的固定
Git/证据身份。字段缺失、类型错误、代际不一致、账本坏行或并发状态模糊时均拒绝猜测。

### 等待、通知与继续执行

必须区分四个不同事实：provider 已终态、产物字节已稳定、完成通知已入队、后续 driver 已被调度。
前一项不自动推出后一项。后台 monitor 启动成功或 callback/工具结果进入队列，只证明观察/投递动作；
若没有活的消费者或可核验的 continuation receipt，完成结果可以永久停在队列中而不推进状态机。

需要 terminal 后继续调用 orch 的 driver 必须二选一：

1. 保持一个有界、事件驱动的前台阻塞消费，terminal 后在同一调用链 final-drain 并继续；或
2. 把 continuation 交给 runtime-owned daemon/scheduler，并取得 durable 的调度与消费身份。

反复查询 provider 不是替代方案；单一机械 wait 可以保持安静，观察窗到期仍按
`ReportAwaitExpired` 等 typed 非终态语义处理。进程退出时必须 final-drain，并把 provider terminal、
稳定产物、round/task/attempt/role/fixed HEAD 一起核验；PID 消失、一次空读或通知已排队都不能冒充交卷。
接口没有暴露 continuation-scheduled/consumed 事实时，调用方不得承诺“完成后会自动唤醒并续跑”。
<!-- orch-guide-section-end:truth -->

<!-- orch-guide-section:lifecycle -->
## Round、task 与 seal 生命周期

正常生产链如下：

```text
RoundOpened
  → plan / SeedOracleVerified / PlanSignedOff
  → DispatchIssued / WorkspaceLeased / wake / DispatchAcked / AttemptStarted
  → executor seeded-red / implementation / gates / mutations / REPORT-last
  → ReportObserved
  → ReportCollectClaimed → ReportCollectExecuting → ReportCollectExecuted
  → ReportCollectCompleted
  → required reviews + evidence
  → VerdictIssued(PASS, fixed HEAD)
  → MergeStarted → MergeExecuted → post-merge gates → TaskRecorded + SiteRetired × 0..N
  → all tasks terminal → RoundClosed
```

- 一个 task 可以有多个单调递增 attempt；新 attempt 不得冒充旧 attempt 的事实。
- `ReportObserved` 只证明 immutable REPORT 已被收取，不证明 collect 门已经成功。
- `VerdictIssued(PASS)` 必须绑定当前 attempt、latest collect、固定 HEAD、签核 IR、审查和证据。
- 正常收口使用 `seal`；`merge`、`record` 是调试或受控恢复兼容入口，不是更弱的替代门。
- `TaskRecorded` 可与该任务现场的 `SiteRetired` 在同一 checked batch 落账；post-merge
  授权只接纳 runtime actor、round/task、唯一 `TaskRecorded` 锚点、既有 `WorkspaceLeased`
  的 `siteId/generation/attemptId/role/agent` 和单次退休全部匹配的事件，绝不按裸
  `SiteRetired` kind 放行。
- `MergeExecuted` 后门红不得回滚或伪装为 `TaskRecorded`；只走运行时已有的恢复/补记语义。
- `round close` 只在任务集合满足闭轮判据时执行；`--force` 是显式审计动作，不是默认路径。

### Runtime policy 与动态 review panel

runtime policy 不是可变全局开关。每个 attempt 的 `policyBaseSha` 必须逐字等于它第一条
`DispatchIssued.baseSha`；运行时只从该 commit 的 `PROJECT-BINDING.yaml`、ROUND-IR 与 round
ledger blob 解析模式。之后的 activate/deactivate 只影响以其 accounting commit 或后继为 base 的
新 dispatch，不重写在飞或历史 attempt。跨轮 active 状态必须由新轮签名 binding 显式携带
source round/event/policy/event digest；禁止扫描历史猜测全局状态。

`runtime-policy activate` 要求 owner task 已有 canonical `MergeExecuted + TaskRecorded`、owner merge
仍是 main 祖先，且无在飞 action/wake/review route/merge barrier。activate/deactivate 都持 exclusive
protocol lease，把 typed policy event 写入 WAL+ledger 后，以临时 index 和 main CAS 创建只含当前 round
ledger 的 scoped accounting commit，再从新 commit 回读状态；同一 exact 已完成命令的幂等重放会
回读既有 event/commit 而不追加第二份事实。deactivate 的 `--reason` 可选；缺省明确记录
`operator-requested`。

Panel 模式下，`review panel select` 必须恰好选择三个不同 agent，至少两席 formal 且至少一席
primary lineage。`ReviewPanelSelected + 3×ReviewSeatRouted` 先在同一 ledger batch 与 Git accounting
commit 中落定，之后 provider 才能使用预分配 wakeId 启动；崩溃恢复只消费同一 route，不重选。
`retry` 只允许一个 business-invalid seat 的 generation 2；admission/auth/pin 等
system-terminal-invalid 不消耗该预算，只能由 `backfill` 绑定 exact source terminal。所有 deadline
继续由 role 与 policy-base ROUND-IR 的 `requiredEvidence` 数量调用既有单调公式推导。

Panel reviewer 只写 lease worktree 内、含 seatId/generation/wakeId/policyBaseSha 的 staging artifact。
runtime 验证 regular/no-symlink、frontmatter、固定 HEAD、terminal output path/SHA、完整 bytes 与
substantive body 后，以 fsync + hard-link create-if-absent no-clobber 发布 generation-scoped canonical
review；`ReviewSpoolPromoted + delivery + ReviewSeatTerminated` 同 batch，并把 canonical blob 与 ledger
同一 scoped commit。CLI reconcile、runloop、serve/deadline tick 共用 current-attempt reconciler；旧
attempt verdict 不抑制 successor，当前 attempt verdict 仍抑制晚到产物。

Agy route 在发送 formal 题目之前，用同一 signed argv/model/effort 发送随机 canary；只有 stdout
逐字等于 canary（最多一个行结尾）才进入至少五秒稳定窗，随后且仅随后启动一次 formal。canary、
认证、pin、formal empty/failure 都是 system-terminal-invalid，不产生业务 gen2。

带非空 frozen-contract supersession 的 post-merge barrier 永远不允许
`post-merge-gate-released`；唯一出口是 `record --at-tip` 在同一 record batch 生成
TaskRecorded/FrozenContractSuperseded。seal 的瞬时 owner intent 只允许携 exact token 的 orch
no-ff merge 更新 main；Drop 后 intent 消失，因此 H29 的后续修复提交仍可达。
<!-- orch-guide-section-end:lifecycle -->

<!-- orch-guide-section:commands -->
## 命令总表

以下隐藏标记由 `orch guide --check` 与当前 Clap 命令树精确比对；新增、删除或重命名公开
叶子命令而未更新指南会使检查失败。

<!-- orch-guide-command:init -->
<!-- orch-guide-command:bind -->
<!-- orch-guide-command:doctor -->
<!-- orch-guide-command:ledger recover -->
<!-- orch-guide-command:review reconcile -->
<!-- orch-guide-command:review deliver -->
<!-- orch-guide-command:review panel select -->
<!-- orch-guide-command:review panel retry -->
<!-- orch-guide-command:review panel backfill -->
<!-- orch-guide-command:sites gc -->
<!-- orch-guide-command:sites sweep-scratch -->
<!-- orch-guide-command:sites sweep-trial-cache -->
<!-- orch-guide-command:sites rotate-logs -->
<!-- orch-guide-command:sites sweep-targets -->
<!-- orch-guide-command:agent list -->
<!-- orch-guide-command:agent lint -->
<!-- orch-guide-command:agent set-pin -->
<!-- orch-guide-command:runtime-policy activate -->
<!-- orch-guide-command:runtime-policy deactivate -->
<!-- orch-guide-command:status -->
<!-- orch-guide-command:schema -->
<!-- orch-guide-command:guide -->
<!-- orch-guide-command:run-task -->
<!-- orch-guide-command:check -->
<!-- orch-guide-command:verify -->
<!-- orch-guide-command:verdict -->
<!-- orch-guide-command:seal -->
<!-- orch-guide-command:merge -->
<!-- orch-guide-command:record -->
<!-- orch-guide-command:snapshot -->
<!-- orch-guide-command:dispatch -->
<!-- orch-guide-command:await-report -->
<!-- orch-guide-command:retry-dead -->
<!-- orch-guide-command:bootstrap -->
<!-- orch-guide-command:round open -->
<!-- orch-guide-command:round sign-off -->
<!-- orch-guide-command:round seed-verified -->
<!-- orch-guide-command:round close -->
<!-- orch-guide-command:nudge -->
<!-- orch-guide-command:wake -->
<!-- orch-guide-command:handshake -->
<!-- orch-guide-command:resume -->
<!-- orch-guide-command:cost -->
<!-- orch-guide-command:schedule -->
<!-- orch-guide-command:stall-check -->
<!-- orch-guide-command:plan -->
<!-- orch-guide-command:consult -->
<!-- orch-guide-command:approve -->
<!-- orch-guide-command:run -->
<!-- orch-guide-command:step -->
<!-- orch-guide-command:current -->
<!-- orch-guide-command:serve -->
<!-- orch-guide-command:mcp serve -->
<!-- orch-guide-command:session show -->
<!-- orch-guide-command:session set -->
<!-- orch-guide-command:inbox add -->
<!-- orch-guide-command:inbox list -->
<!-- orch-guide-command:inbox done -->
<!-- orch-guide-command:run-wave -->

| 类别 | 命令 | 机械效果 |
|---|---|---|
| 产品只读/提示 | `guide`, `bind`, `doctor`, `status`, `schema`, `agent list`, `agent lint`, `cost`, `schedule`, `stall-check`, `check`, `mcp serve`, `bootstrap` | 输出内嵌契约、探测、校验、投影或提示词；不追加业务事件。`bootstrap --copy` 只额外写剪贴板，测试命令仍可能写构建缓存。 |
| 派生输出 | `current`, `snapshot` | `current` 幂等覆写一个派生 Markdown，`planSignedOff` 只在 latest canonical `TaskValidated` 的 revision/digest 有匹配 user sign-off 时为 yes；`snapshot --write` 才写 runtime 快照。二者不创造任务事实。 |
| 项目地基 | `init`, `round`, `plan`, `consult`, `approve` | 建立协调骨架、计划/签核/种子与审批事实；必须服从对应生命周期前置条件。 |
| 注册表修订 | `agent set-pin` | 只在开放且已签核的轮内外科式修改一个 agent 的 provider/model/effort，并追加有序 `AgentPinAmended`；不改 IR revision、不补签。 |
| runtime policy | `runtime-policy activate/deactivate` | 从捕获的 committed main 回读签名 binding/IR/ledger；只在 owner Recorded、无在飞 action/wake/barrier 时，以 ledger-only scoped accounting commit 改变未来 dispatch 的 policy-as-of。 |
| 执行 | `run-task`, `dispatch`, `await-report`, `retry-dead` | Tier S/Tier F 的执行、收取和有限恢复；可能写信号、事件、worktree 和门证据。 |
| 会话控制 | `nudge`, `wake`, `handshake`, `resume`, `session` | attempt-scoped 或 session-scoped 控制；`session show` 只读，`session set` 修改注册态。 |
| 审查收口 | `review`, `verify`, `verdict`, `seal`, `merge`, `record` | 固定 HEAD 审查、裁决、合并与补记；正常成功路径优先 `seal`。 |
| 运维恢复 | `ledger recover`, `sites` | WAL 严格前缀恢复或租约约束的现场清理；删除类操作不会因方便而放宽所有权判据。 |
| 驱动 | `run`, `step`, `serve`, `run-wave` | 读取同一账本并驱动机械分支；多个驱动者仍受 durable lease 和屏障约束。 |
| 指令队列 | `inbox` | `list` 只读；`add`/`done` 改变项目指令队列。 |

条件型命令必须按具体参数判断：`ledger recover` 默认干跑，只有 `--apply` 回灌；
`snapshot` 默认只读，只有 `--write` 落盘；`bootstrap --copy` 会写剪贴板；`verdict --dry-run`
不落裁决；`agent list/lint` 只读而 `agent set-pin` 改 registry 与账本；`wake` 下的 `status`
是只读观察，其他 action 可能改变会话控制事实。

每一个生产 gate child spawn 都先做一次新鲜的文件系统准入。默认 gate 峰值估计为
38 GiB，再保留 8 GiB floor，因此默认至少需要 46 GiB 可用空间；machine overlay 只能提高
这两个预算。fresh 的 `verify`/`close` gate 在 spawn 前取得并持有 permit，已持有 collect
permit 的 trial、fallback、seed-red replay 与 final gate 则逐次重新探测，且不会把自己的
reservation 重复计算；新出现而未被 permit 覆盖的文件系统 fail-closed。拒绝输出同时给出
需要量、当前量（探测失败时明确为未知）和 `orch sites gc` 自救提示；账本事件保留 raw bytes、
probe、probeReason 与闭合的 PreAttempt/Attempt identity。gate 的每次拒绝或成功决定都通过专用
入口，在同一 ledger lock 内读取 exact pair 并幂等落 refusal/recovered；没有待恢复 refusal 的
成功决定是 no-op，不会伪造 unpaired recovered。Active merge barrier 只允许持 lexical lifecycle
capability 且绑定 exact task/round 的同形审计，事实落盘但不改变 barrier 任一字段；
`Reclaim`/`RoundClose` 入口仍豁免，确保低空间时可以执行回收。
归档 record-chain 复验把同轮、`actor=runtime:orch` 且 closed payload 完整成立的 canonical
refused→recovered storage pair 视为合法 observation suffix；actor、round 或 payload
任一漂移仍 fail closed，不会借 storage kind 绕过历史授权链。

`stall-check` 只读采样当前轮账本、任务分支产物、进程与 wake 日志，再以确定性判定表输出
attempt 级和 round 级停滞信号；它不跑门、不调用模型、不写 refs/ledger/WAL/runtime。
退出 `0` 表示本次采样无需 planner 介入，`1` 表示至少一项需要介入；Clap 参数解析拒绝
仍为 `2`，未分类 IO/运行时错误按下表返回 `5`。
它只实现 H121① 的信号面，不代替 planner 心跳、自动收取或容量补救。

### 状态变更命令契约

CLI 的公共 disposition 只描述**本命令声明的效果推进到哪一步**：

<!-- orch-guide-disposition:EffectAchieved -->
<!-- orch-guide-disposition:EffectPartial -->
<!-- orch-guide-disposition:Rejected -->
<!-- orch-guide-disposition:EffectUnknown -->

| 码 | disposition | 机械判据 | 调用方动作 |
|---:|---|---|---|
| `0` | `EffectAchieved` | 声明的终态效果已达成，且无未解释失败 | 继续，无需查账 |
| `1` | `EffectPartial` | 至少一个单元达成、至少一个单元失败/拒绝，且摘要已经打印 | 读摘要，幂等重跑剩余单元 |
| `2` | `Rejected` | 严格在任何效果之前拒绝，且错误链带 durable `ActionRejection` | 修输入后重试 |
| `5` | `EffectUnknown` | 无终态效果，且无法证明零副作用；这是所有未分类错误的默认 | 先读账本再决定，禁止盲重跑 |

因此声称 `Rejected(2)` 必须有 `ActionRejection` 证据；普通 `bail!`、IO 或无法分类的
错误不再冒充零效果，而是 `EffectUnknown(5)`。最外层 typed command disposition 优先于
叶子 cause，避免命令已推进效果后仍被早先的零效果守卫误报为 `2`。`3/4/6/7/64/70-72`
是各命令独立定义的 outcome，不进入上述四码表；机检或 verifier 明确 FAIL 仍可使用 `1`。
状态命令即使非零，也可能已经留下合法的 claimed/released/rejected 事实；重试前必须重读账本。

| 命令 | 必要前置条件 | 成功后的机械效果 | 拒绝或中断后的合法动作 |
|---|---|---|---|
| `init` | 目标根可规范化且可写，**且不存在 `coordination/runtime/CURRENT-ROUND`**（活跃轮内 fail-closed 拒绝） | 只创建缺失的 coordination 骨架并补必要 git 规则；`AI-OPERATOR-RUNBOOK.md` 已存在时绝不覆盖 | 修复具体权限/形状后幂等重跑；活跃轮内新增的产品骨架由 planner 直接提交，**不靠重跑 bootstrap 回补**；不要先删除用户 runbook |
| `ledger recover --apply` | 指定 round 的 tracked ledger 是 WAL 的逐字节严格前缀，或两者已一致 | 仅原子回灌 WAL 中缺失的原字节并写恢复 receipt；一致时零写入 | 分叉即停写并审计两份输入；不得借它修 lease、门或 attempt |
| `round open` | round id 合法；默认不存在未闭合活动轮；公开的 legacy `--signed-off` 当前 fail-closed，必须先 plan 再单独 sign-off | 建目录，追加 `RoundOpened`，更新 `CURRENT-ROUND` 与 BOARD 开版投影 | 只对“`RoundOpened` 已落、但 `CURRENT-ROUND` 尚未指向它”的特定 crash window 可同参补指针；普通 replay 会拒绝，`--force` 只用于明确审计的跨轮决策 |
| `plan` | 活动轮的 ModeConfig、卡片、绑定和 registry 可静态验证；一次固定 main OID 后聚合拒绝 `writeSet ∩ landed anchors`，只有 exact/schema-valid/target-matching supersession 可豁免；真实 Rust-only binding 的 `check`（mixed Rust+Node 为 `rustCheck`）须在首个 `--` 前含独立 `--all-targets`；contract change 还须通过 replan 在飞守卫 | 原子更新派生 ROUND-IR，并在新 revision 追加精确 `TaskValidated`。仅 `actor="verifier:root"` 的 `VerdictIssued(FAIL/BLOCKED)` 按 exact attempt identity 释放 replan 守卫 | `PASS`、非 root actor、未知或缺失 verdict 均 fail-closed 保持 attempt live；修正输入后重跑，IR/revision 漂移时重新 seed 验证并重新 sign-off。此判据不释放 scheduler 容量，也不授权 successor/takeover |
| `round seed-verified` | task 属于 validated IR；seed、目标与 expected-red 身份一致 | 在隔离现场实跑 oracle，成功后追加绑定当前 IR revision 的 `SeedOracleVerified` | 修复 seed/card/门后重新 `plan` 和预验；`--record-only` 只是显式 legacy 逃生舱 |
| `round sign-off` | 当前 validated IR 未漂移，要求重验的 task 已有本 revision 的 seed proof | 仅显式用户动作追加 exact `PlanSignedOff` | 任一输入变化后重新 `plan`/预验/签核；不得复制旧签核 payload |
| `agent set-pin <AGENT> ... --reason ...` | 当前轮开放且 exact IR revision 已由用户签核；agent 已登记；至少一个新 pin 非空且不同；目标 invocation argv 未内联待改 pin | 在一个 protocol transition 内外科式改 `agents.yaml`，写后逐字段证明除目标三 pin 外零差异，再追加含 before/after、前后 digest、round/revision/actor/reason 的 `AgentPinAmended`。只读 IR 校验按账本顺序从签名 genesis 折叠这些 delta，原 `PlanSignedOff` 不变 | 未签核/已收轮、空转、未知 agent、第四字段差异、argv literal、账本或文件失败均拒绝并回滚；roles/capacity/quota/tools/argv 的变化仍走完整 `plan` + seed reverify + sign-off |
| `consult` | 当前计划期尚无匹配 `PlanSignedOff`，输入和附件均在允许范围 | 生成有界咨询产物和 provider receipt；不自动修改 IR 或代替用户签核 | 修复 preset/provider/附件后仍只在计划期重试 |
| `approve` | task 在 active signed IR，action 属公开高风险枚举 | 相邻追加 `PermissionRequested` 与用户 `PermissionDecided`；只记录授权，不执行外部动作 | denied 或失败时保持原动作未授权；新动作/新范围需新审批 |
| `dispatch` | signed IR、task/agent/attempt 基线和 Git 身份均满足；普通路径无模糊活 attempt；运行时在 capacity lock 内先要求 signed `dependsOn` 全部已有 `TaskRecorded`，并对具有 canonical merge 身份的依赖证明 attempt base 包含其 `MergeExecuted.mergeSha`，再按 signed agent/quota domain 与 durable in-flight 投影 admission | 创建/复用精确 worktree，追加 attempt/workspace/`DispatchIssued` 与 durable wake 事实，GO 绑定固定 SHA | 依赖未 Recorded 时返回 `BlockedByDependency` 并点名 blockers；依赖已 Recorded 且已知 canonical merge、但 base 落后时返回 `ForwardBaselineRequired` 及 `dependency`/`merge_sha`/`attempt_base_sha` 三元组，二者不得混同。历史 `TaskRecorded` 若没有 merge 身份，保留 recorded-only 兼容，不伪造 stale 判断。前向修复信号不自动合并、更不在活跃 dirty worktree 上合并；capacity、dead、BLOCKED、stall、ambiguous 仍分别处置。项目 slot 可额外收紧，不能放宽 runtime admission，也不能手写 GO |
| `run-task` | 与 dispatch 同类的卡片、agent、Git 和门契约可用，且不存在同名遗留 branch/worktree | legacy Tier S one-shot 驱动 provider、REPORT、机检和 gate；它不具备 Tier F collect/wake 的阶段恢复 | 中断后保全并审计现场；已有 branch/worktree 会直接拒绝，需 planner 明确处置现场或改走 Tier F，不能声称“从最后阶段自动恢复” |
| `await-report` | 存在 current DispatchContext；REPORT/BLOCKED 必须绑定该 attempt | REPORT 路径执行 durable collect、机检、gate 与 receipt；BLOCKED 路径追加 canonical terminal。观察窗从本次命令开始计时，先到时记录非终态 `ReportAwaitExpired`（命令失败审计仍会有 `ActionRejected`）；只有从 newest exact authenticated implementation `WakeIssued.ts` 起算的 `runtimeLimit` 已到，才追加既有 `EscalationRaised` + `AttemptTimedOut` 终态链 | `ReportAwaitExpired` 核对 `observerSecs`/`runtimeLimitSecs` 后可用更长观察窗重跑；它不授权 `retry-dead` 或 takeover。Busy 等 lease；stall/dead/BLOCKED 按恢复矩阵；门红修真实原因，不能把 release 当 PASS |
| `retry-dead` | operator 只应在 `await-report` 已机械判 dead 后调用，且未超有界重试策略；当前命令的 retry 决策只看 dead 计数/activity，不会独立证明“刚发生 dead” | 退避后重验身份，满足时派 successor attempt | 返回 `4` 表示不自动重派；交 planner 选合法改派或修卡，不循环调用，也不把命令可调用性当死亡证明 |
| `bootstrap` | agent 可解析，且提示所需项目事实存在 | stdout 渲染客户端提示；`--copy` 仅写剪贴板，不写 signal、attempt 或 ledger | 修复缺失项目事实后重渲染；复制成功不等于会话已启动 |
| `nudge` | task/agent 必须精确匹配 current DispatchContext；状态在允许集合、wake budget 可用，且默认没有未消费 NUDGE | 写 attempt-scoped NUDGE，再记录 `NudgeIssued` 并尝试真实 wake/POKE；provider/fence 失败时控制文件仍可能待消费 | 非零后查文件和 ledger；`--force` 只允许覆盖控制文件，绝不释放 durable lease |
| `wake <agent>` | active IR 允许该 agent，registry/capacity/消息来源合法；review 参数必须成完整 tuple。`--reissue <SOURCE_WAKE_ID>` 只接受同 agent 的 managed OpenCode formal-review source：它已有唯一 accepted receipt、authenticated `session-death-declared`，且 source identity/当前 review slot/账本链全部精确一致；task、attempt、role、deadline 从 source 推导，禁止调用方再传 review tuple/deadline | 普通真实 wake 后记录 `WakeIssued`。精确重放若复用仍存活的 wake，stdout 明示被复用的 `wakeId` 与“未起新进程”，不会伪装成真实派发。**只有 formal review 席位（`primary`/`secondary`）**的完整 tuple 才在同一受控效果内记录 `ReviewRequested`；**`nongate` 角色被运行时显式拒绝产生 formal review 事件**（它仍照常落 `WakeIssued`、`WorkspaceLeased` 等 wake/site durable 事实）。合法 reissue 精确替换自己的容量槽、仍重跑 agent/shared quota admission，并真 spawn 新 wake；新 `WakeIssued.supersededWakeId` 指向 source，且与同 identity 的新 `ReviewRequested` 原子落账。死亡证据允许首次显式更换 message；同 digest 重放幂等返回唯一 active successor（多跳时返回链尾），不 append 或 spawn 第三个 wake | live 或 pending source、缺/错死亡证据、非 OpenCode provider、身份/引用残链、重复 successor、无关容量或 shared quota 满载均 `ActionRejected` + 非零退出，且不得 spawn；已有活 successor 时再次改 message 也拒绝，绝不把未投递的新消息静默报成幂等成功。spawn 不等于 engaged，不得手补 review 请求。此出口不替换 reviewer，也不实现 nudge/H115 的 live-session supersede |
| `handshake` | agent 已登记且 wake budget/通道可用 | 发起真实 probe wake并留下 wake 事实；provider receipt expectation 从 exact durable `WakeIssued` 重建，Pi/ZCode 的 signed provider/model/effort 不得在 CLI 二次构造时丢失；exact engaged 返回 `0`，Pending/未咬合/timeout 返回 `3`，不创建实现 attempt | `--no-wake` 必为 Pending `3`；修复通道后重试，不能把 spawn 当握手成功 |
| `resume` | current DispatchContext、GO/ACK、Git 和 attempt 身份可精确重建 | 追加 `ResumeIssued` 并驱动 `ResumeWake` durable chain；不 mint successor、不 kill、不释放 collect。REPORT 已提交时通常只提示继续 `await-report`；但若 task branch 的同一 REPORT 字节仍与 current attempt 的 `ReportObserved` 完全一致，且其后存在完整 `Executing → MechCheckFailed → Released → ActionRejected(await-report)` 终态链，则提示原 attempt 修真实原因、重跑验证并把 REPORT 作为最后动作更新；`frozen` 拒绝还会明确要求逐字节逆向恢复 seed、禁止 checkout/reset/stash，并把增量覆盖迁到非冻结 writeSet 载体。恢复消息沿用 attempt-scoped implementation continuation，但不放宽全局 digest fence：仅当旧 wake 有唯一 accepted receipt、唯一 `ManagedWakeTerminated{managedScopeTerminated:true,outcomeClass:DeliveredTerminal}`，且当前 `ResumeIssued → ResumeWakeClaimed → ResumeWakeLaunching` 的 action/owner/generation 完全一致时，才以 `resumedFromWakeId` 绑定的新 `WakeIssued` 原子替换旧 owner；容量为 1 也只换槽、不增槽，重放只认同一 replacement | live Busy 或 completed replay 可能仍返回既有 prompt/`0`；`0` 不保证发生了新注入。REPORT/attempt/collect lineage、旧 wake receipt/terminal proof、resume generation 任一不精确都不得打开返修唤醒，失败后按 exact lineage 审计 |
| `review deliver` / `review reconcile` | review 文件、role、agent、attempt、固定 HEAD 和 main commit 全部匹配 | deliver 先把产物路由到 canonical/late 路径；reconcile 只为已提交的精确产物补 `ReviewDelivered` | 文件未提交、身份不符或 verdict 已变时按提示走 late monotonic 路径；不伪造 review |
| `verify` | signed IR 允许 legacy verifier，当前模式不是 root-manual 禁用档 | 运行固定 HEAD verifier 并输出/记录其 review 结果；FAIL 返回非零 | 修复实现或 review 证据后重新走正常验证；不得把 verifier 自述当 root PASS |
| `verdict` | exact task/attempt/expected HEAD/expected main、latest collect、reviews/evidence 与 IR 全匹配；normal 路径要求当前签核早于 dispatch，受签 exact-attempt bootstrap permit 仅允许下述两条闭合迁移链 | `--dry-run` 只证明可裁决；真实调用追加 root `VerdictIssued`，PASS 留下 fixed authorization tuple；FAIL/BLOCKED 被成功记录也返回 `0` | 漂移即用新身份重算；不要把退出 `0` 等同 PASS。main 写屏障要到后续 canonical `MergeStarted` 才开启 |
| `seal` | 当前 attempt 有 0 或 1 条 exact root PASS，expected HEAD 完全一致；0 时 seal 内创建 PASS，1 时校验并续跑，重复/非 canonical 拒绝 | durable replay 地完成 root PASS、`MergeStarted`/Git merge/`MergeExecuted`/合后门/`TaskRecorded`，并在同一 checked batch 退休有完整锚点的任务现场后清场 | 中断后用同一 tuple 重放；完整锚定的同批 `SiteRetired` 不再触发“未授权事件”假失败，也不得重复追加生命周期事件；其他 suffix 与 `CanonicalRootSuffix` 仍 fail-closed；特殊屏障只用精确 `merge`/`record` 恢复 |
| `merge` / `record` | 仅接受运行时识别的分步恢复 tuple；`record` 还需祖先与 post-merge gate 证明 | `merge` 闭合已有 PASS 的 merge 链；`record` 只在完整授权后补 `TaskRecorded` | 默认不替代 `seal`；`--recover`/`--at-tip` 都要保留额外审计事实，拒绝时不 reset main |
| `round close` | 默认 active IR task 均有 canonical `TaskRecorded`，无未闭合屏障，main/IR 未漂移；更高 revision 签名移出的历史 task 只在其已于该 validation 前 `blocked/changes_requested`、且之后仅有受管终态清理事实时忽略，任何再派发或状态推进仍拒绝；`--force` 强制非空 note 且只豁免 active task 未 Recorded | 闭轮前输出逐项 removed/refused/failed/freedBytes；已释放 site/target 按 durable lease disposition 当轮回收，不以 24h 挂钟保留，再追加 `RoundClosed`，写 DONE 与 BOARD 收轮投影 | receipt 对账失败降级为可见诊断且不禁用 GC；拒收现场与其 target 均保留。清理副作用可能早于闭轮拒绝，force 不豁免坏账本、身份/拓扑漂移或非法 archived chain |
| `session set` | agent 已登记，mode/session id 合法；root-manual active round 机械拒绝修改 registry | 改项目 registry 配置；不会清除 durable fault overlay，也不会迁移 in-flight attempt | 新 generation 的 exact Engaged 证据才能清 overlay；先处理在飞会话 |
| `inbox add` / `inbox done` | active IR 在临界点前后保持一致，文件名/内容安全 | 原子创建 pending 指令或把精确文件推进到 done；不改变 task/attempt | 修复漂移或文件身份后重试，不直接移动目录冒充处理 |
| `sites gc` | active IR 可解析；只回收 released generation | reconcile 可能先补真实 terminal facts；对账失败单独诊断但仍继续回收。物理回收与 exact Git worktree registry 剪枝在同一 ledger 临界区内闭合；`freedBytes 由动作前后测量`，口径是同一 worktree/target 边界内 regular-file、symlink 的 apparent bytes 饱和差，不含目录元数据、文件系统块分配，也不把它解释成全盘可用空间增量 | refused、target failure、半闭合或 registry invariant 红均返回非零；不得从 PID/目录年龄推断 release。`gc 幂等重放` 在对象已完整回收时输出 removed=0、freedBytes=0 并返回 0；它不会把 complete journal 再计成一次删除 |
| `sites sweep-*` / `rotate-logs` | 目标必须在各自固定 scratch/cache/log 管辖域 | scratch 逐项隔离错误并输出 removed/failed/freedBytes；其他入口按各自 keep/newest-generation/closed-round/TTL 规则清理或压缩 | scratch 仅在零回收且存在失败时返回非零；对开放轮、active lease、debug/keep 对象 fail-closed，不能扩大删除根 |

### 非门 attempt 收据与告警

每个非门席的收据由派发该席的 planner 写入
`coordination/runtime/nongate-inbox/<round>/<attemptId>-<agent>.json`；executor 与
`verdict`/`seal` 都只读。schemaVersion 1 至少包含 `round`、`attemptId`、`agent`、
`fixedHead`、`wakeId`、`wakeIssuedEventId`、`workspaceLeasedEventId`，以及
`invocation.{provider,model,effort,preset,cwd}`。`cwd` 必须是实际审查 worktree 的绝对路径，
并与所引用 lease 的 `paths.worktree` 精确相同。

`nongate receipt 四态`为 `answered`、`failed`、`timedOut`、`empty`，四者都表示“已尝试”；
后三态必须保留未经归一化的非空 `terminalReason`。文件存在本身不构成证据：收据必须
`绑定 WakeIssued 与 WorkspaceLeased`，两个 event id 都要在本轮账本唯一存在，并逐字匹配
runtime actor、task、round、attempt、agent、wakeId；lease 还必须是 `role=nongate`，且
`reviewedHead` 等于收据与命令的 fixed HEAD。跨 attempt 借事件、伪 event id、目录里只有别席
文件或 cwd 不等于 lease worktree，一律视为缺收据。

期望非门集合从常设非门席出发，再`排除同 attempt formal reviewer`（以 signed
`requiredReviews` 为准），避免同一 agent 自审。缺失或无效时，`verdict` 与 `seal` 都逐席输出
稳定前缀 `[orch] nongate receipt warning:`，并带 agent、attemptId、fixedHead，要求 planner
显式裁定；这是纯 advisory，`不改变 verdict 或 seal 的退出码`、门执行、root verdict、merge、
`TaskRecorded` 或后续 DAG。失败、超时、空答在合规留证后同样不阻塞。

### Signed review fallback 与 closed quorum

历史卡没有 `reviewQuorum` 时继续采用 `LegacyStrict`：`requiredReviews` 的每个 formal 席都必须
逐席交付，上一节按常设 roster 生成的 nongate 收据仍只告警。新卡若启用 review 扩展，则必须把
全部义务签入卡与 ROUND-IR：`requiredReviews` 保持严格的 `{role, agent}` 两字段，formal fallback
由并行 `reviewFallbacks: [{role, fallbackAgent}]` 按 role 声明（每个 formal role 至多一个），
nongate 义务由显式 `nongateSeats: [{agent, preset}]` 给出，capacity role 只证明资格、绝不自动制造义务；
`sourceBindings.harnessRegistryDigest` 同时冻结终态与收据能力面。缺省、未知字段、身份重叠、
角色能力不匹配或不可达的票数在 plan 期 fail-closed。

`fallbackAgent` 只有在原 reviewer 的唯一 `ReviewRequested`/`WakeIssued` 已绑定 authenticated
`ManagedWakeTerminated(managedScopeTerminated=true)`，且 outcome 不是 operational error 或
authenticated cancel 时才可选择。运行时先查 staged 与 canonical artifact：完整产物必须先
reconcile，partial/malformed 直接升级，不能用“通道失败”覆盖已有答案。合法转换在一个受控效果内
依序追加 `ReviewFallbackSelected`、target `WakeIssued`、target `ReviewRequested`；`orch step` 与
`orch serve` 都走同一 runloop transition。相同或并发 tick 复用唯一 target wake，fallback 再失败
只产生 exhausted escalation，不会派第三名 reviewer。原 reviewer 的迟到 PASS 不能抢回槽位；
迟到 FAIL/BLOCKED 在 root linearization 前仍是单调否决。

新卡的 `reviewQuorum.minimumSubstantive` 目前下界为 2。只有 formal `ReviewDelivered`，或同时具备
exact artifact 与绑定 `answered` receipt 的 `NongateReviewDelivered`，才是 substantive result；
failed/timedOut/empty/channel error 都是零票。agent 与 delivery event 双重去重，原则是
**one agent + one delivery = one voice**：同一 nongate artifact 即使还支撑 formal substitution，
仍只计 one voice，也至多替一个 formal 席。任何 formal 或 nongate 的 substantive FAIL/BLOCKED
优先于全部 PASS，永不能被覆盖；pending formal 即使已有两票 PASS 也保持未闭合。

当 `nongateMaySubstituteFailedFormal=true` 且达到
`minimumNongatePassForSubstitution` 时，已耗尽 signed fallback（或没有 fallback）的 formal
channel error 可由唯一 substantive nongate PASS 闭合。root 在 `VerdictIssued` 同一原子 batch 中
先追加 `ReviewSeatSubstituted`，逐字绑定 role、from/to agent、fixed head、source terminal event、
nongate delivery event 与 verdict event。expected-main、merge/record 与 archived round-close replay
都会重验这三类新 durable event；手写、错序、重复替席、后来删 artifact/receipt 都会使 PASS 失效。

`verdict` 的 signed exact-attempt bootstrap permit 有且仅有两条互斥语义。存在 legacy16
validation 时只允许原 A-prime 顺序：legacy validation → dispatch/receipt/collect → round first
production validation → exact sign-off → root，失败后绝不回退到另一条路径。legacy 集为空时，
production-replan ratification 还必须从唯一 `DispatchIssued.baseSha` 的提交树逐事件绑定 dispatch
前账本，并复算旧 ROUND-IR revision/digest、卡面 SHA、空 permit 与 agent；随后证明全部 exact
`AttemptBlocked` 早于 receipt/collect、当前更高 production validation 与唯一 post-collect sign-off
早于 root。第二条 dispatch、错身份/错顺序 block、额外 post-collect sign-off、较晚 validation、
任一 prior root/merge/record 或 dispatch 后 crash/timeout/fail 都 fail-closed。该路径不伪造新的
Resume/Nudge/collect，也不授权普通任务复用 permit；同一已落 root 的 exact 幂等 replay 保持有效。

Gate 的 canonical 运行证据使用 **phase-scoped gate log**：
`{task}-round-{round}-{attempt}-{phase}-{gateRunId}-gate-{name}.{log|hb|orphans|fixtures}`。
`gateRunId` 是每次 child spawn 前由 runtime 新生成的 ULID；同一 attempt、phase 与 commandRef
重跑也不得复用。它包住而不替换旧的 round-scoped stem
`{tag}-round-{round}-gate-{name}.{log|hb|orphans|fixtures}`，四类 sibling 仍必须共享同一 stem。

`candidate-lanes-v1` 激活后，门集合从 attempt 第一条 `DispatchIssued.baseSha` 对应的
committed binding 解析，绝不读取后来移动的 main 或工作树配置。`collect` 使用 candidate lane：
任务卡自己的 `seeds[].target` 闭合派生 `seedTargets`，再运行 source-reader closure 与 `check`；
`final-tree-v1` active 时 collect 的 `trial` 只保留 detached merge conflict/path preflight、传空门集，
workspace full 延后到 seal；该 policy dormant 或历史 attempt 的 `trial` 仍使用 merge lane。
`root-reuse-v1` dormant 时 root 使用 merge lane；
`final-tree-v1` dormant 时 postmerge/recovery 使用 merge lane。对应 policy 激活后由各自 owner task
的 typed contract 接管；policy 尚未激活的历史/在飞 attempt 保持 signed fast lane，red replay
始终单独运行卡面 `gates.fast[0]`，不计入 candidate 性能样本，也不因窄门激活而删除。
merge/fast 必须覆盖卡面已签 `gates.fast` 的全部 commandRef，缺命令或少任一项都 fail closed。

source-reader closure 逐字校验 binding 内的 descriptor 指针、descriptor SHA、其 base descriptor
指针与 base SHA，再将 candidate 实际改动的 production subject 反向闭合到 runnable integration
tests。seed 参数只能来自本卡 seed 落点；reader target 只能由 canonical
`orch/crates/<package>/tests/<stem>.rs` 转换。未知 literal/dynamic/macro reader、descriptor 漂移、
target 无法转换或未知 commandRef 都不会退化成只跑 `check`：runtime 先追加 canonical
`GateLaneEscalated(candidate→fast)`，绑定 attempt、policyBaseSha、原因与 ordered resolved argv
digest，再运行 signed fast lane；该事件必须唯一且位于首条 collect `GateExecuted` 之前，后来补写
或移到门后的 escalation 不能追认 receipt。resolved-command digest 必须从 policy-base committed binding
重新计算；它属于 reuse/escalation identity，不得塞进 `GateExecuted` 形成第十一键。r81 在 descriptor
冻结后落地的 reader 边只认 committed supplemental manifest 的 exact SHA-256
`983adb428653f908842e31dbffcefda7b50ec3bd888eff205607c37c6f9bf512`；清单缺失或改字节同样升级
fast，清单本身绝不授予 narrow target。

`CollectGateSuccessReceipt` 的创建、Completed/current-main 重验与 archived CAS replay 共用上述
同一个 immutable resolver。active policy 下 receipt 保存实际展开后的**有序 invocation**，不是
卡面 fast 集的替身；多个 seed/reader target 可以产生重复 semantic `commandRef`，必须按
`sequence + gateRunId` 逐项绑定，绝不能 set/dedup。receipt attestation 另绑定 candidate tree、
active IR revision/digest、binding/card SHA、ordered resolved-command digest、toolchain/environment
以及每门 event/raw-log CAS；任一 base、policy、descriptor、selector、顺序或 CAS 漂移都
fail closed。collect 还会在每个 gate 前后重抓工作树 subject tree 并与 pinned candidate tree
逐字比较；门若持久改写 tracked 输入，不记录可复用的 `GateExecuted`，也不生成 receipt。历史 dormant
attempt 仍按 signed fast 重放；red-replay 继续是独立 phase，不混入
candidate receipt。`GateExecuted` 在两条路径上都保持精确十键。

`root-reuse-v1` 只按 attempt 第一条 `DispatchIssued.baseSha` 的 policy-as-of 生效。active 时，
root PASS 先从 completed collect receipt 重新读取 attestation CAS 与每门 raw-log CAS，再独立重算
candidate tree、attempt、IR revision/digest、binding/card SHA、ordered resolved-command digest，并
现场重抓 toolchain/environment；正常推进的 main SHA 明确不属于 reuse identity。全部逐字段相等且
source gate 全绿时，每门生成一条 typed `GateReused(collect→root)`，与 `VerdictIssued` 在同一个
checked append batch 落账；verdict gate binding 的 `gateRunId` / `reusedEventId` 必须恰有一个。
existing verdict、expected-main、merge/record 与 archived replay 都沿 `reusedEventId` 回到唯一 source
`GateExecuted`、completed receipt、attestation 和 raw-log CAS，任何 actor/round/task/attempt/phase/
identity/CAS 漂移都 fail closed。

active reuse 的 identity miss 先落 attempt-scoped typed `GateReuseMiss`，再运行真实 merge/root lane；
缺失、red、不可读或 CAS 损坏的 source proof 绝不复用。`verdict --dry-run` 在任何 gate/storage append
之前返回；若 exact verdict 已存在，则先只读复验其 execution reference、receipt 与 CAS，再以零门、
零 append 返回。active policy 下 FAIL/BLOCKED 也不运行 root 门；dormant/历史 attempt 保持既有真实
root 门语义。V1 lane-aware reuse miss fallback 与 dormant real-root 都从该 attempt 的
`DispatchIssued.baseSha` 读取 merge-lane command refs/specs，绝不改读移动后的 expected-main
binding；只有 policy-base 早于 PROJECT-BINDING 入库、且没有 candidate-lanes 合同的 pre-lane
历史 attempt 保留 legacy expected-main 兼容。重复调用不重置 miss 次序，也不会为同一已落 root
verdict 追加第二批 reuse 事实。

`final-tree-v1` 同样只按 attempt 第一条 `DispatchIssued.baseSha` 生效，并且不增加第七种
`GatePhase`。active attempt 只有在账本存在唯一 canonical pending root PASS 时，才在既有
merge-lifecycle lease 内捕获 main/candidate/root authorization，以 `git merge-tree --write-tree`
生成 synthetic tree，并在 `Trial` phase 对 policy-base merge lane 跑一次 workspace full。
每门原始日志先写 evidence CAS、写后回读并重算 SHA/bytes；CAS 缺失、损坏或 pointer 漂移都是
硬失败，绝不伪装成 `GateReuseMiss`。`WorkspaceFullPermit` 只包住实际 full gate child，不包住
synthetic-tree 计算、input recheck、`MergeStarted` 或真实 merge。

full 全绿并释放 permit 后，seal 重读 main/candidate/authorization、active IR/card、policy-base
binding/ordered argv、toolchain 与 environment。真实 input drift 追加 attempt-scoped typed
`GateReuseMiss(phase=trial)`；missNo 从同 attempt 的全部 phase 事实复算，最多 2，重复 seal 不清零。
ref-CAS 争碰只重读、零 miss。Trial 红或累计第二个 miss 时，在尚无 `MergeStarted` 的前提下用
exact `AttemptBlocked(stage=approved-reattempt, verdictEventId=...)` 闭合 pending PASS；没有 pending
PASS 的同形 terminal 仍由既有验证器拒绝。只有稳定 full green 后才允许追加 `MergeStarted`。

真实 no-ff merge 后从 exact `MergeExecuted.mergeSha^{tree}` 读取 actual tree。它与 tested tree 的
40 位小写对象名逐字相等时，不 spawn PostMerge/Recovery child；每门追加 typed
`GateReused(trial→postmerge)`，并与 canonical `TaskRecorded` 处于同一 checked lifecycle atomic
batch（冻结的 `FrozenContractSuperseded` grammar 要求 `TaskRecorded` 先出现，reuse observations
作为同批连续 suffix）。不相等时必须真跑完整 PostMerge；红继续沿既有不可逆 barrier recovery，
绝不 reset main 或伪造 Recorded。`MergeExecuted` 已落而 `TaskRecorded` 未落的恢复会重读同一
attestation/raw-log CAS：actual tree 相等则补同批 reuse+record，证明不完整或树不等才跑真实 Recovery。

r81 的 owner task B305 自身 attempt-base 仍为 dormant，所以它按 legacy trial/postmerge full 完成；
`final-tree-v1` 只由 active integration fixture 证明，r81 不宣称已经量得最终 70% SLO。r82 若启用，
必须在新轮 signed binding 中以 exact source activation event/policy/event digest 做 carryForward，
再用第一张 canary 实测。doctor/archive/hook/tree identity 任一异常时，仅在 runtime idle 后机械
deactivate；deactivation 只影响以其后 main 为 base 的未来 dispatch，在飞 attempt 仍保持原语义。

full lane 由 repo-scoped `WorkspaceFullPermit` 跨进程串行；owner 绑定 gateRunId、PID、进程 birth
identity、repo identity 与 action，只有持有者的不可伪造独占 handle 才能在同 repo、同
gateRunId/action 内重入。owner 在 child spawn 后还绑定 gate child PGID/birth 与确定性 fixture
registry；spawn 前先 durable 标记 `spawning`，所以任何 post-spawn bind/cleanup 失败都不会被
`unspawned` 误删。full wrapper 只在调用线程的显式 capability scope 内把 child 绑定到 owner；同进程
candidate/direct runner 没有该 scope，仍保持窄门独立。父进程死亡本身不授权回收，只有 exact child group 和全部 registry group 均已收敛才可
替换 owner。live child、birth 漂移、torn/malformed registry 都 fail closed，cleanup 失败保留 owner
证据；环境变量字符串不构成所有权。acquire/release 的文件锁只覆盖短临界区，Cargo
子进程运行期间不持 ledger lock；candidate 窄门不取 full permit，可与另一条 candidate 窄门并行。
每个 gate child spawn 前还会把 SIGINT/SIGHUP/SIGTERM disposition 恢复为默认值，避免后台父进程
继承的 ignored signal 污染测试与超时清理语义。

每次完成的生产 gate 都追加同形 `GateExecuted`：`commandRef`、闭合的 `phase`
（`red-replay|collect|trial|root|postmerge|recovery`）、`gateRunId`、`exitCode`、`durationMs`、
`subjectTreeSha`、`logSha256`、`logBytes`、`toolchainDigest`、`environmentDigest`。其中
`toolchainDigest` 来自 Cargo realpath + `cargo -V` + `rustc -vV` + target triple；environment
摘要覆盖 OS build、arch、sandbox class、machine overlay，以及会改变构建语义的继承环境变量。
采集失败即 gate 流程失败，不用空串、编译期常量、时间戳或缓存冒充现场。collect attestation
同时绑定 `gateRunId` 与两个 digest，后续采用方才能追回具体运行并判断环境是否一致。

active `MergeStarted` 屏障只把 exact task/round、actor=`runtime:orch` 的 `GateExecuted` 当作
透明观测放行；它既不推进 `merge_executed`，也不闭合屏障。其它 kind、actor、task 或 round
仍 fail closed，既有 canonical `MergeExecuted` / merge escalation / `TaskRecorded` 生命周期臂
继续独立校验。这一例外只让 postmerge/recovery 的红绿与耗时可耐久追溯，不授予状态转换权限。

`sites rotate-logs` 对含 exact round 的格式按轮归属；历史无轮号文件继续走 task-prefix 兼容识别，
若被多轮主张就保留并报告冲突，不猜测或迁移。root-verdict 的幂等读取对新 verdict 使用其绑定的
`gateRunId` 精确回读 phase-scoped 路径；旧 verdict 只有 scoped 文件不存在、且恰有一份
regular-file legacy candidate，其 SHA-256 与长度都和账本绑定一致时才兼容读取。
scoped/legacy 并存、多候选、symlink、缺失或字节绑定不符都 fail closed。
| `run` / `step` / `serve` / `run-wave` | active signed IR 与驱动模式匹配 | 只是反复选择并调用上述同一机械 transition；没有额外权限；`run-wave` blocked 返回 `1`、awaiting-root 返回 `7` | 停在判断边界交 planner；重启驱动前先重读账本，不能假设上一拍零副作用 |

**plan 派发准入 floors。** 授权型 `orch plan` 在任何 ROUND-IR 字节或 `TaskValidated`
写入前只捕获一次完整 main commit OID，所有卡共享该快照。每张卡的精确路径或 `dir/**`
writeSet 与完整 landed anchor 集合求交，结果按 `(taskId,target)` 严格排序、去重并一次报全；
只有经完整卡面 schema 校验且 target 精确相等的 `frozenContractSupersessions` 声明豁免自己的
target，父目录 glob、错拼或多余字符串均不豁免。唯一不属于“新授权”的消歧是：已 Recorded
任务重放自己原始 `SeedRelocated` 的 exact seed target，且当前 materialized target 仍与 immutable
seed source 字节相同；别的任务、别的 target 或任意字节变化仍是冲突。纯 `compile_ir` 与
readonly replay 不执行这次
授权重分类，因而后来移动的 main 不会反向污染已签 IR。真实 Rust-only 项目还要求
`commands.check`，mixed Rust+Node 项目要求 `commands.rustCheck`；所选命令必须存在且非空，并在
首个 `--` 终止符前携带独立 `--all-targets` token；长字符串拼接或 terminator
后的 token 均拒绝。该错误与既有 `--locked` floor 一次聚合；非 Rust binding 保持历史语义。

**已落位 seed 的 SHA-256 守卫。** 项目可在 signed `PROJECT-BINDING.yaml` 中用
`oracle.landedSeedBaseline` 绑定不可变 genesis manifest；descriptor、manifest bytes、closed schema、
排序、计数与 provenance 必须同时成立。迁移采用**非追溯基线化**：本仓的声明审计口径为
243 pairs / 36 drift / 4 missing，durable effective-genesis 口径为 225 unique / 224 present /
1 tombstone，另显式排除 3 个从未 relocation 的 branch-only target。36 组既存漂移以
`migration-baseline` 明示；这既不追认也不否认其历史语义，只拒绝锚点之后的修改、删除或墓碑复活。
后续 `SeedRelocated` 只有在 earlier matching oracle 与 later post-green `TaskRecorded` 链完整时才成为
delta；genesis 永不改写。`mech::check` 在入口各解析一次 main 与 candidate commit OID，landed guard、
merge-base、diff、commit walk 与 blob 检查全程消费同一对不可移动对象，不重读 task/main ref。

受限 supersession 的 Authorized 声明只能来自 active signed ROUND-IR 所绑定的 exact task-card bytes，
并闭合绑定 target、原始/effective anchor、新旧整文件、变更形状、逐条处置和两席 review。
授权是互斥闭枚举：历史 recovery 字节继续在顶层携带完整 `blockedAttempt` + 已 Recorded
`replacement`，没有 `authorization` 字段；planner-adjudicated 字节必须同时省略两个 recovery 锚点，
并携带 `authorization.kind=planner-adjudicated`、用户 actor/date/quote、非空且不重复的
`removedAssertions`，以及 fixed candidate/effective tree 中 regular adjudication blob 的 path + SHA-256。
缺一臂、混合两臂或裸 supersession 均拒绝；历史 recovery card/event JSON 无需迁移即可 parse/replay。

字节变更形状同样是互斥闭枚举。历史 literal-swap 继续使用原顶层
`oldLiteralSha256` / `newLiteralSha256` / `subjectPrefix` 字节形状，且判据不变：新旧文本除同一偏移处
恰好一个带双引号的 64 字符小写十六进制摘要外逐字节相同，subject prefix 的 old/new 摘要必须与
该 literal pair 一致。structured evolution 必须省略上述三键，只携带有序 edit units；每个 unit
以不可变 old 的半开字节区间和该窗口 SHA-256 定位，并用闭合 `replace` 或 `delete` action 表达结果。
运行时拒绝空单元集、乱序、重叠、越界、旧窗口摘要漂移及单个整文件 catch-all，然后按声明顺序从
old 重建；重建结果与 new 的首个不一致以 `unexplained byte offset=<N>` fail-closed。delete unit 必须
与 planner-adjudicated `removedAssertions` 逐条双向绑定。literal 校验失败不会回落尝试 structured，
两种形状同时存在或同时缺失也在解析/运行时边界拒绝。

planner arm 还会从固定 old/new tree 复算每条 removed assertion：old 必须存在、new 必须消失，且不同的
`retainedCoverage` 标识必须仍存在于 new；裁定文书只从固定 tree 读取，工作树同名文件不构成证据。
两臂都继续要求 runtime actor、declared old 等于当前 landed、declared old 等于原始
`SeedRelocated.sha256`；recovery 额外要求 replacement 已 Recorded，planner 不伪造该锚点。
只有 merge 后门全绿，唯一 canonical helper 才在普通 `seal`、`record --at-tip` 与 H48 green recovery
三路原子追加 `TaskRecorded → FrozenContractSuperseded*`（按 target 排序）`→ SiteRetired* →`
可选 `RecordGateRelaxed`。普通 `append`/`append_checked` 无 authority；expected-main suffix 会重建完整
signed payload（含原授权变体）、事件顺序、retirement 集合与 recovery tail；planner arm 不合成
blocked/replacement。`FrozenContractSuperseded` 是 known inert
audit fact，不替代 `TaskRecorded` 状态边；其按 initiator 的 Effective 次数进入 `RoundClosed`，
Authorized 但未落该事件的不计数。

Formal review 的容量槽按 `(task, role)` 投影，并保留 current request 的 exact attempt 身份；同一槽后追加的普通
`ReviewRequested` 是 current request，账本顺序最后者胜出且仍只占一个槽。它只重瞄容量与
backend receipt 对账，不删除历史请求，也不等于 `--reissue` 的 authenticated wake 链或 review
产物文件 supersede。`AttemptBlocked` / `AttemptCrashed` / `AttemptTimedOut` /
`AttemptFailed` 会释放 exact attempt 的 implementation 槽与全部 formal review 槽，不波及其他
attempt；既有 substantive `ReviewDelivered` 的 exact tuple 释放语义不变。

Provider 已 spawn 后，若 canonical backend receipt 对账失败，运行时会把 action-scoped
`wake-backend-receipt-degraded` 与 `ActionRejected` 原子落账。degraded 明示外部效果未知，既不冒充
accepted receipt，也不授权 review/reissue/release；该 wake 仍按可能存活处理并阻止自动重复 spawn。
daemon 不循环重报 degraded，操作者只能对 exact wake 修复合法 lineage 后做 late reconcile，不能从
非零退出推断 provider 没启动。

完整 review tuple 省略 `--deadline-secs` 时，`wake` 从当前 signed ROUND-IR 读取该 task 的
`requiredEvidence` 条目数，并按 role × workload 推导默认值；结果永不短于 1800 秒，也不超过
managed-wake 天花板 21600 秒，超大工作量饱和而不回绕。显式值始终优先，其中 `0` 仍表示
关闭 review SLA、回退到 21600 秒的 managed safety ceiling。该推导只改变新 review wake；
`--reissue` 继续逐字继承 source deadline。

<!-- orch-guide-harness:invocation-envelope -->
### Runtime → managed wrapper 调用信封

Pi、ZCode 与 DSH 的受管 wrapper 通过同一份 provider-neutral 环境信封接收运行时事实；字段集合是
闭合契约，恰为：

`ORCH_HARNESS_ID`、`ORCH_HARNESS_ACTION_ID`、`ORCH_HARNESS_WAKE_ID`、
`ORCH_HARNESS_ROUND`、`ORCH_HARNESS_TASK_ID`、`ORCH_HARNESS_ATTEMPT_ID`、
`ORCH_HARNESS_ROLE`、`ORCH_HARNESS_CWD`、`ORCH_HARNESS_FIXED_HEAD`、
`ORCH_HARNESS_PROVIDER`、`ORCH_HARNESS_MODEL`、`ORCH_HARNESS_EFFORT`、
`ORCH_HARNESS_PROVIDER_BIN`、`ORCH_HARNESS_REVIEW_OUTPUT_PATH`、
`ORCH_HARNESS_ORCH_BIN`、`ORCH_HARNESS_DEADLINE_SECS`。

`ORCH_HARNESS_ROLE` 只接受 `primary|secondary|nongate|implement`；implement 的 review 落点必须用
显式 `NO_REVIEW_OUTPUT` 哨兵。`ORCH_HARNESS_CWD` 与 review 落点必须是绝对路径，fixed head 必须是
完整 40 位小写 SHA；provider/orch binary 必须是绝对、存在且可执行的常规文件。

<!-- orch-guide-harness:envelope-complete-or-refuse -->
只要出现任一信封键，十六键就必须全部非空并在任何 provider `spawn` 之前完成校验；缺键、空白、
相对路径、短/大写 SHA、未构建或不可执行的 binary 都直接拒绝，不用空串或本机缺省继续启动。
`ORCH_HARNESS_ORCH_BIN` 指向运行中的已构建 orch，无法提供时 reviewer 不会被派发。

<!-- orch-guide-harness:legacy-alias-precedence -->
旧 `ORCH_PI_*` / `ORCH_ZCODE_*` / `ORCH_DSH_*` 变量只作一轮兼容别名：信封存在时信封逐字段取胜，
冲突必须打印诊断；信封完全缺席时才进入旧变量兼容分支。provider executable 的受控候选只允许
信封 `ORCH_HARNESS_PROVIDER_BIN`，兼容分支才可读取对应 `ORCH_*_BIN`；信封模式绝不从 `PATH`
回落同名 provider。每条 `WakeIssued` 同时携带本次 spawn 前读取的 `harnessRegistryDigest`，使描述符
轮中漂移可事后核对。

### `wake` 的 positional action

Clap 将下列形式都暴露为一个 `wake` 叶命令，因此 command marker 只有 `wake`；AI 仍须按
第一个 positional 精确分流：

| 形式 | 机械语义 |
|---|---|
| `wake <agent> ...` | 向 active IR 允许且 registry 可注入的 agent 发送一次 wake；真派发首行保持 `orch wake <agent>: 注入已完成`，精确复用则改为含既有 `wakeId` 与 `未起新进程` 的首行且不追加 `WakeIssued`。完整 review tuple 才同事务记录 `ReviewRequested`；同 `(task,role)` 后发请求成为 current（其 attempt 身份随请求精确保留），但不隐藏多活 wake，也不覆盖 canonical review 产物。spawn/channel receipt 不等于 engaged。 |
| `wake <agent> --reissue <SOURCE_WAKE_ID> [--message ...\|--message-file ...]` | 仅重投同 agent 的 managed OpenCode formal review。source 必须已有 accepted receipt、authenticated death declaration，并仍精确绑定当前 task/attempt/role 槽；review tuple/deadline 全部继承，调用方不得覆盖。成功真 spawn 新 wake 并以 `supersededWakeId` 原子链接；首次显式 changed message 由 exact death evidence 豁免 digest fence。若 source 已有合法 successor，同 digest 重放幂等返回唯一 active 链尾，绝不第三次 spawn；此时再次改 message 因 successor 仍活而非零拒绝，不能静默丢消息。live/pending、非 OpenCode、错绑/残链/重复 successor 或容量门失败均落 `ActionRejected` 并非零退出、零 spawn。该能力不换 reviewer，也不关闭 H115。 |
<!-- orch-guide-wake-action:status -->
| `wake status <wakeId> [--json]` | orphan-control 只读面；要求唯一可信的 managed Codex/OpenCode/Pi/ZCode wake 与 authenticated descriptor，输出 redacted 状态。它绕过普通 active/stale preflight，但仍需 canonical 项目事实。 |
<!-- orch-guide-wake-action:attach -->
| `wake attach <wakeId> --mode <resume\|fork> --reason ... [--deadline-secs <1..=21600>]` | 仅用于 formal managed OpenCode review wake：要求该 exact `wakeId` 恰一条可信 `ReviewRequested`、可信 session receipt、review 未交付、attempt 仍可审且预算可用。source 未终态时先走 canonical cancel；resume 必须同 session，fork 必须新 session，失败不 fallback/relaunch。attach 的显式上限与 managed-wake ceiling 同为 21600 秒；省略该 flag 时仅 attach 操作自身仍默认 1800 秒。 |
<!-- orch-guide-wake-action:cancel -->
| `wake cancel <wakeId> --reason ...` | 对 authenticated managed wake 首写者获胜地提交/重查 cancel control；CLI 自身不发进程信号。Accepted/AlreadyCanceling 的 `0` 只表示请求已确认，不表示 scope 已终止；Pending/CleanupPending 用同一命令或 status 重查。 |
<!-- orch-guide-wake-action:declare-dead -->
| `wake declare-dead <wakeId>` | 仅 formal managed OpenCode review wake 可用，且须 authenticated immutable terminal status、`managedScopeTerminated=true`、无 operational error；幂等记录 `session-death-declared`。`TruncatedNoTerminal`、`StoppedByHardDeadline`、`StoppedByAuthenticatedCancel` 会释放同 round/task/attempt/role/agent 且同 wakeId 的 continuation，使后续普通 `wake` 可真起新进程，并由新 `WakeIssued.supersededWakeId` 链回死亡 source；该普通替代允许 source 在从未形成 accepted/degraded receipt 时仅凭上述精确可释放死亡证据接续，successor 仍须从自己的日志取得 receipt。显式 `--reissue` 则始终要求 source 已有 accepted receipt。`DeliveredTerminal`、`OperationalError` 与非终态 phase 不释放。更晚的 `WakeIssued` 会重新占用该槽。它不杀进程、不判 implementation attempt dead、不释放 lease。 |
<!-- orch-guide-section-end:commands -->

<!-- orch-guide-section:leases -->
## Durable action 与租约

`DispatchWake`、`ResumeWake` 和 `ReportCollect` 使用可回放 durable action。其 lineage 至少由
`actionId`、`owner`、`leaseGeneration` 以及 attempt 的稳定身份共同约束；处于租约阶段的
anchor 还必须携带可解析的 `leaseUntil`，terminal/released 事件不靠该字段续租。
每次恢复必须先 fold 全部同 scope 事件；不能只看最后一个字符串相似的事件。

通用原则：

- `Claimed` 表示某一代取得所有权；后续阶段必须保持相同 owner/generation。
- `Completed` 是幂等 terminal；合法 replay 不重复副作用。
- `Released` 只由运行时在锁内依据合法前驱和精确身份追加。
- 活租约判据是事件中的 `leaseUntil` 晚于当前系统时间。PID 消失、heartbeat 停止、日志不再增长或操作者声称进程已死，都不能提前释放活租约。
- 直接 kill 持有者不会释放 durable lease，反而可能制造必须等到期的恢复窗口。

`DispatchWake` 与 `ResumeWake` 的正常阶段均为
`Claimed → Launching → Delivered → Completed`；`Released` 只允许来自 `Claimed` 或
`Launching`。运行时在 fallible provider spawn 前先落 `Launching`，spawn 失败再用相同
owner/generation release。`Launching` 表示外部副作用可能已开始，因此活租约不能由另一 caller
猜测接管。`Delivered` 只固化真实 spawn/channel facts，不表示 provider 已 engaged；`Completed`
只表示本 wake action 完成，不表示 attempt 完成，并可按完整 lineage 幂等重放。过期 `Claimed` 可在锁内 exact
release 后由新 generation 接管；过期 `Launching` 的外部注入结果不明，运行时会拒绝重复
注入，**不会**自动 release/reclaim。此时先审计 exact provider/managed-wake receipt，再走已有
orphan-control；不得把它类比成可自动接管的 collect `Executing`。

`ReportCollect` 的正常阶段是 `Claimed → Executing → Executed → Completed`。特殊恢复：

- 活的 `Claimed` 或 `Executing`：其他 caller 得到 Busy，不追加接管事件。
- 过期 `Claimed`：运行时在同一锁事务中 release 旧 generation，再 claim 新 generation。
- 过期 `Executing`：运行时先追加精确旧身份的 `ReportCollectReleased`，永久标记
  `outcomeUnknown: true`，再以新 owner/generation claim 并重跑门。
- 已有合法 `Executed`：验证 gate receipt 后可只补 `Completed`，不重复跑门。
- 已有合法 `Completed`：验证绑定后幂等返回。

<!-- orch-guide-invariant:live-collect-lease-not-preemptible -->
`ReportCollectExecuting` 活租约不可抢占；没有 `force-release`、PID 特赦或 heartbeat 特赦。

<!-- orch-guide-invariant:expired-collect-outcome-unknown -->
过期的 `ReportCollectExecuting` 恢复必须带 `outcomeUnknown: true`，因为旧持有者可能在门的任意位置崩溃。

### 终态后的资源回收边界

durable terminal、`TaskRecorded` 或 `SiteRetired` 只证明协议身份已终止/退休；除非另有进程回收 receipt，
它们不自动证明 wrapper、process group、fsmonitor 或文件缓存已经释放。`sites gc`/`sweep-*` 的成功只绑定
各自管辖的文件系统对象，也不等于 RSS 已下降。

任何自动内存/磁盘回收器都必须消费 authenticated ownership：无 active lease/wake、进程身份未复用、
worktree clean，且提交已被 main 包含或有保全 ref。runtime-owned 对象应在其 terminal 后尽快回收，
而不是默认等待 TTL 或闭轮；shared/user-owned、active、dirty、blocked、debug 或身份不明对象必须保留。
回收应幂等并逐项报告 terminated/removed/refused、RSS 与 logical/physical bytes 前后值。禁止用 broad
进程名匹配或目录年龄代替 ownership 证明。
<!-- orch-guide-section-end:leases -->

<!-- orch-guide-section:nudge-resume -->
## Nudge、wake、resume 与改派

<!-- orch-guide-invariant:nudge-not-release-collect -->
`nudge` 不是租约释放命令，也不是通用解锁命令。它绑定当前 DispatchContext 的 task、
attempt 和 agent，写入 NUDGE 信号、记录 `NudgeIssued`，并在可用时尝试 managed wake。

- heartbeat 持续推进且执行者正常工作时不 nudge；继续由 `await-report` 或 daemon 等待。
- NUDGE 文件会在客户端回到 `wait-dispatch` 循环时消费；managed provider 也可能在工作 turn
  内收到即时 wake。`NudgeIssued.payload.deliveryState="delivered"` 只表示 spawn 维度，不等于
  NUDGE 文件已消费、provider 已 engaged 或 attempt 已终态。
- 已有未消费 NUDGE 时默认拒绝覆盖。`--force` 只覆盖该控制文件，仍不跨 attempt、不释放
  collect lease、不改变 verdict 或门结果。
- 对已经提交诚实 BLOCKED 的旧 attempt，nudge 可以要求旧会话结束并回到等待；能够派生新
  attempt 的原因是旧 attempt 已有 canonical terminal，而不是 nudge 创造了解锁事实。
- 工作会话卡死、无法回到等待或需要用账本/Git 重建上下文时，使用 `resume`；resume 自己也有
  durable wake identity，失败不能猜成成功或静默 fallback。
- 同一 attempt 再次发生 collect 终态拒绝时，`resume` 从当前 task branch tip 与最新拒绝重建
  generation；只有 actionId 等于该 current plan 的 completed/pending generation 可以幂等重放。
  拒绝证据沿用 `ReportObserved` 实际记录的 control epoch：当前 dispatch segment 的起点，或
  该 observation 前最新的同 attempt `NudgeIssued` / `ResumeIssued`；后续才追加的 control 不会
  反向改写既有 observation。缺失、跨 segment 或无法解析到精确控制事件的 epoch 一律拒绝。
  同 segment 的旧 generation 仍须通过 lineage/fold 校验，但不得覆盖新 prompt；最后一条
  `DispatchIssued` 始终切开 resume segment，新的 dispatch 不继承旧 attempt 的 generation。
- `wake` 是通用注册会话控制面；`handshake` 用于派发前探活，失败不应消耗实现 attempt。
- `retry-dead` 只处理 `await-report` 已机械判死后的有界重派；stall、慢门和一次静默不等于 dead。
- 新 attempt/改派必须由 `dispatch` 的 takeover 判据或显式受控入口生成，绝不手写 GO 或事件。

<!-- orch-guide-harness:terminal-envelope -->
### Harness 统一终态信封

受管 harness 的 provider-neutral 终态是闭集：`answered`、`failed`、`timedOut`、
`empty`、`canceled`。未知字符串必须拒绝，不能靠 catch-all 归入某个已知状态。
每条终态记录包含 `state`、未经分类名改写的 `exactReason`、`turnEnded`、
`finalTextSha256`、`outputPath` / `outputSha256`、`usage`、`usageAbsentReason`、
`managedScopeTerminated` 与 `mechanicalTerminalAbsent`。没有 usage 时 `usage` 明确为
JSON `null`，同时给非空原因；不得用零值冒充 provider 报数。

描述符声明 `terminal: native|derived` 的通道必须在 final-drain 后产生恰好一条
`ManagedWakeTerminated`；只有 activity、heartbeat、日志增长或 PID 消失都不能产生
`answered`。声明 `terminal: absent` 的通道产生一条显式
`ManagedWakeTerminalAbsent`，说明本通道没有机械终态；它不冒充进程已经结束，也不触发
site release。

`resume` 受控替换产生的 successor `WakeIssued` 与普通 wake 共用同一 harness identity
绑定：带 `resumedFromWakeId` 时仍必须携带 descriptor-derived `harnessId`、
`terminalCapability` 与 `harnessRegistryDigest`。终态对账只按 exact wakeId 闭合，重复 reconcile
仍恰好一条 `ManagedWakeTerminated`；另一个 wake 的合法终态不能替它释放或遮蔽生命周期。

<!-- orch-guide-harness:exit-zero-is-not-answered -->
`exit 0` **不等于** `answered`。只有已认证的 turn-end 加稳定 final text 摘要或稳定输出
artifact 摘要，才可判 `answered`；exit 0 但只有进度句、工具活动或零帧 EOF 一律为
`empty`。进程退出前必须 final-drain，退出本身不补造终帧。

<!-- orch-guide-harness:exit-code-manifest -->
Pi、ZCode、DSH 三个受管 stream wrapper 以机器可读
`# orch-exit-code: <code> <state> <reason>` manifest 声明保留码，并与 Rust 的唯一映射表
双向一致：

| code | state | reason |
|---:|---|---|
| 0 | `answered` | `exact-terminal`（仍须满足稳定答卷判据） |
| 3 | `failed` | `launch-failure` |
| 64 | `failed` | `invalid-arguments` |
| 65 | `failed` | `invalid-workspace` |
| 66 | `failed` | `invalid-envelope` |
| 67 | `failed` | `pin-mismatch`（ZCode） |
| 70 | `empty` | `zero-frame-eof` |
| 71 | `empty` | `truncated-no-terminal` |
| 72 | `timedOut` | `hard-deadline` |
| 74 | `failed` | `identity-drift`（DSH） |

manifest 只描述 wrapper 保留码。为兼容既有冻结契约，provider-native 非零退出码仍可逐字
透传；它们统一归 `failed`，原码与原因保留在 provider/supervisor 证据中，不被伪装成
manifest 保留码。本表不覆盖 `wake-opus-review.sh`、`wake-multica.sh` 或
`wake-dclaw.sh`。
<!-- orch-guide-section-end:nudge-resume -->

<!-- orch-guide-section:recovery -->
## 恢复矩阵

先运行只读观察，再执行表中动作。任何一行的“下一步”都不承诺最终门会变绿。

| 观察到的状态 | 先核对 | 合法下一步 | 禁止推断 |
|---|---|---|---|
| heartbeat/日志持续推进 | 当前 attempt、generation、最近事件 | 继续 `await-report` 或让 daemon 驱动 | 运行时间长不等于 stall |
| 无 REPORT、heartbeat fresh | GO ack、进程/managed wake 状态 | 等待；必要时发送与当前工作相关的 nudge | 无输出不等于 dead |
| `ReportAwaitExpired` / 观察窗到期 | exact attempt、`observerSecs`、`runtimeLimitSecs`、provider 进展；`runtimeLimitSecs=0` 表示没有已认证期限 | 继续等待，或用合适的更长观察窗重跑 `await-report` | 观察者停止等待不等于 runtime deadline、dead、stall 或 takeover 授权 |
| `await-report` 返回 stall | snapshot、heartbeat 代际、provider receipt | 先诊断；可达会话用 nudge，需重建现场用 resume | stall 不授权 takeover |
| `await-report` 返回 dead | terminal 事件、当前 attempt | `retry-dead`；耗尽后交 planner 选择合法改派/BLOCKED | PID 不在不能释放其他 durable action |
| executor 给出 BLOCKED | canonical BLOCKED、AttemptBlocked、卡面/IR | 若卡面/IR 需修改则重新 plan、验证和签核；结束旧会话后按现有或修订后的合法计划派 successor | 不在旧 attempt 上偷偷扩大 writeSet |
| collect 报 live owner/Busy | 最新同 action 的 owner、generation、`leaseUntil` | 等租约到期后重跑 `await-report` | 不 kill、不伪造 release、不改时钟 |
| collect 门红 | `GateExecuted`、完整日志、`ReportCollectReleased` | 修复真实原因或按 attempt 规则返工；再次收取会重新跑门 | 租约已释放不代表门会绿 |
| review FAIL / root verdict FAIL·BLOCKED / PASS | 固定 HEAD、review/evidence binding、IR；root verdict 还须核对 exact actor、attempt identity 与 verdict payload | exact root FAIL/BLOCKED 可进入卡面定义的 repair/takeover 流程，并在修卡后重跑 `plan`；PASS 继续既有 `seal`/merge barrier | 非 root 或畸形 verdict 不释放 replan 守卫；不改审查产物冒充 PASS，也不以 replan 绕过 PASS merge barrier |
| backend receipt degraded | exact wake、current `ReviewRequested`、immutable log window 与 degraded/rejection identity | 修复合法 request lineage 后只对 exact wake 做 late reconcile；保持现场 | 非零不证明 provider 未启动；不再 wake 双开、不手写 accepted receipt、不把 degraded 当 review/reissue 授权 |
| stale-binary 拒绝状态变更 | build imprint 与 main 差异 | 用当前源码 locked rebuild，再重试原命令 | `--allow-stale-binary` 不是默认恢复 |
| merge 后门红 | MergeStarted/MergeExecuted、失败门、main 拓扑 | 使用 `merge --recover`/`record` 已建模的精确分支 | 不 reset main，不伪造 TaskRecorded |
| ledger/WAL 不一致 | `orch doctor` 与 `ledger recover` 干跑 | 仅 strict-prefix 情形才 `--apply` | WAL 工具不修 lease、门、attempt |

<!-- orch-guide-invariant:ledger-recover-wal-only -->
`ledger recover` 只处理 WAL 与账本的逐字节严格前缀关系；它不释放业务租约、不补门结果、
不结束 attempt，也不替代 `await-report`、`resume`、`seal` 或人工裁决。

### 两个事故场景的机械推演

**collect 持有者被 kill：** 先从账本定位 exact `ReportCollectExecuting` 的 owner、generation
和 `leaseUntil`。租约仍活时，即使 PID 已消失也只等待；不运行 `nudge`、`ledger recover`、
伪造 release 或改时钟。到期后重新执行同一 task 的 `await-report`：运行时在锁内为旧代追加
`ReportCollectReleased(outcomeUnknown: true)`，再 claim 新代并重跑门。恢复只证明可以重跑；
新门仍可能因真实测试失败而红。

**BLOCKED attempt 经 nudge 交接：** 先确认 BLOCKED 文件已成为 current attempt 的 canonical
`AttemptBlocked`，而不是陈旧文件。`nudge` 只要求旧会话收尾并回到 wait loop；它不创造 terminal、
不释放 collect、不把旧 attempt 变回可写。若卡面/IR 要修改，先重新 `plan`、必要的 seed 预验和
sign-off；随后用合法 dispatch/takeover 派 successor attempt。旧 NUDGE 未消费时默认不覆盖，
确需 `--force` 也只替换 attempt-scoped 控制文件。
<!-- orch-guide-section-end:recovery -->

<!-- orch-guide-section:safety -->
## Fail-closed 与禁止动作

以下动作不能作为普通恢复手段：

- 对 tracked 事件账本运行 `git reset --hard`、`git checkout -- <ledger>` 或 `git stash`；
  未提交 durable 事件可能被无声删除。
- 手工追加、修改或重排 `runtime:orch`、verifier、merge lifecycle 事件。
- 修改系统时钟、事件里的 `leaseUntil`、owner 或 generation 来制造“过期”。
- kill 正在持有 collect/merge/verify 等关键动作的进程，然后假设所有权已释放。
- 用 `main..branch` 代替任务协议规定的 merge-base 域检查，或在执行中把浮动 `main` 当固定基线。
- REPORT 提前写、修改冻结 seed、弱化断言、越出 writeSet、绕过 required review/evidence；纯授权 predicate 不是修改许可。
- 在 merge/post-merge 不确定窗口 reset 或回滚 main；先保全账本、WAL、Git 拓扑和完整日志。

任何恢复命令的成功只证明该命令自己的后置条件，不自动证明下一门、审查、合并或闭轮成功。
<!-- orch-guide-section-end:safety -->

<!-- orch-guide-section:worked-example -->
## 正常一轮的完整调用序列

下面是 Tier F 正常成功路径的**真实参数形状**（占位符用尖括号）。它只覆盖 happy path：
任何一步非零都回到「恢复矩阵」，不要顺着往下走。参数细节仍以 `orch <command> --help` 为准。

```text
# ① 开轮（--signed-off 是 legacy 语法位，当前 fail-closed：必须先 plan 再单独 sign-off）
orch round open <ROUND> --purpose "<one line>"

# ② 写任务卡与种子 → git commit → 编译并静态校验 ROUND-IR
orch plan

# ③ 逐卡真跑 oracle 预验（隔离现场落位种子跑门，核对 expected-red 身份）
orch round seed-verified <TASK> --expected-red "<exact failing line>"

# ④ 计划签核（HITL#1，须用户亲自执行；永不被其他命令隐式触发）
orch round sign-off --note "<why this plan>"

# ⑤ 派发前探活（失败不消耗实现 attempt）
orch handshake <AGENT>

# ⑥ 派发 + 收取（收取内含机检、先红复跑双核验与门）
orch dispatch <TASK>
orch await-report <TASK> --timeout-secs <SECS>

# ⑦ 请审查：只有完整 tuple 才会同一受控效果内记录 ReviewRequested；省略 deadline 即按 IR 推导
orch wake <AGENT> --review-for <TASK> --attempt <ATTEMPT> \
  --role <primary|secondary|nongate> --message-file <PATH> [--deadline-secs <SECS>]

# ⑧ 审查产物提交后登记（此处 role 只接受 primary|secondary）
orch review deliver <TASK> <ATTEMPT> --role <primary|secondary> --agent <AGENT>

# ⑨ 先干跑证明可裁决（--verdict 取值小写 pass|fail|blocked）
orch verdict <TASK> --attempt <ATTEMPT> --expected-head <BRANCH_SHA> \
  --expected-main <MAIN_SHA> --verdict pass --dry-run

# ⑩ 正常收口：一条命令走完 root PASS → merge → 合后门 → TaskRecorded → 清场
#    seal 自己抓取 main，因此它没有 --expected-main
orch seal <TASK> --attempt <ATTEMPT> --expected-head <BRANCH_SHA>

# ⑪ 全部 task 都有 canonical TaskRecorded 后收轮
orch round close --note "<round summary>"
```

⑨ 与 ⑩ 不是两次裁决：`seal` 在当前 attempt 有 0 条 exact root PASS 时自己创建 PASS，
有 1 条时校验并续跑。所以 ⑨ 只需 `--dry-run` 证明可裁决，不必单独落一次真实 verdict。

### 状态变更命令的必填参数

| 命令 | 必填 positional | 必填 flag |
|---|---|---|
| `round open` | `<ID>` | — |
| `round seed-verified` | `<TASK>` | `--expected-red` |
| `round sign-off` | — | —（`--note` 可选） |
| `round close` | — | —（用 `--force` 时 note 须非空） |
| `plan` | — | — |
| `agent set-pin` | `<AGENT>` | `--reason`，并至少一个 `--provider` / `--model` / `--effort` |
| `runtime-policy activate` | `<POLICY>` | — |
| `runtime-policy deactivate` | `<POLICY>` | —（`--reason` 可选，缺省记录 operator-requested） |
| `handshake` | `<AGENT>` | — |
| `dispatch` | `<TASK>` | 用 `--new-attempt` 时须 `--reason` |
| `await-report` | `<TASK>` | — |
| `nudge` | `<AGENT>` | `--task`（绝不按 agent 最近任务猜 attempt 身份） |
| `wake` | `<AGENT>` | 作审查请求时 `--review-for` + `--attempt` + `--role` 须成完整 tuple |
| `resume` | `<TASK>` | — |
| `retry-dead` | `<TASK>` | — |
| `review deliver` | `<TASK> <ATTEMPT>` | `--role`、`--agent` |
| `review panel select` | `<TASK>` | `--attempt`、恰三次 `--seat <role>:<agent>` |
| `review panel retry` | `<TASK>` | `--attempt`、`--seat-id` |
| `review panel backfill` | `<TASK>` | `--attempt`、`--seat <role>:<agent>` |
| `verdict` | `<TASK>` | `--attempt`、`--expected-head`、`--expected-main`、`--verdict` |
| `seal` | `<TASK>` | `--attempt`、`--expected-head` |
| `merge` / `record` | `<TASK>` | —（`--recover` / `--at-tip` 是显式逃生舱） |
| `ledger recover` | — | `--apply` 才回灌；省略时只打印计划 |

`--allow-stale-binary` 在每个状态变更命令上都存在，但它是**留证逃生舱、不是恢复手段**：
build imprint 与 main 不一致时的正常动作是用当前源码 locked rebuild，再重试原命令。
<!-- orch-guide-section-end:worked-example -->

<!-- orch-guide-section:quick-reference -->
## 一分钟速查

```text
先看状态        orch current && orch status
检查健康        orch doctor
排查停滞        orch stall-check
看可调度选择    orch schedule <TASK>
正常派发前      orch handshake <AGENT>
派发/收取       orch dispatch <TASK> ; orch await-report <TASK>
正常在做        等待，不 nudge
可达但需纠偏    orch nudge <AGENT> --task <TASK> ...
会话需重建      orch resume <TASK>
机械判死后      orch retry-dead <TASK>
观察窗到期      核 ReportAwaitExpired；仍活则延长窗口重跑 await-report
collect Busy    查 leaseUntil；活则等，到期再 await-report
正常收口        reviews/evidence 完整后 orch seal ...
WAL 异常        orch ledger recover（先干跑；只修严格前缀）
参数不确定      orch <command> --help
用法/整轮序列   orch guide --section worked-example
契约不确定      orch guide --section <name>
```

`await-report` 的公开退出语义：`0` 收取成功；`3` liveness dead；`6` executor BLOCKED；
`4` liveness stall、非终态观察窗到期（`ReportAwaitExpired`）、authenticated runtime deadline
到期，或 collect Busy、机检/门红等 durable `ActionRejected`；`2` 只给有 durable
`ActionRejection` 证据的零效果入口拒绝，未被 await wrapper 分类的普通错误默认
`EffectUnknown(5)`。
退出码只是分类入口，下一动作仍需绑定当前 attempt、事件和证据。
<!-- orch-guide-section-end:quick-reference -->
