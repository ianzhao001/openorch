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
<!-- orch-guide-command:sites gc -->
<!-- orch-guide-command:sites sweep-scratch -->
<!-- orch-guide-command:sites sweep-trial-cache -->
<!-- orch-guide-command:sites rotate-logs -->
<!-- orch-guide-command:sites sweep-targets -->
<!-- orch-guide-command:agent list -->
<!-- orch-guide-command:agent lint -->
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
| 产品只读/提示 | `guide`, `bind`, `doctor`, `status`, `schema`, `agent`, `cost`, `schedule`, `stall-check`, `check`, `mcp serve`, `bootstrap` | 输出内嵌契约、探测、校验、投影或提示词；不追加业务事件。`bootstrap --copy` 只额外写剪贴板，测试命令仍可能写构建缓存。 |
| 派生输出 | `current`, `snapshot` | `current` 幂等覆写一个派生 Markdown；`snapshot --write` 才写 runtime 快照。二者不创造任务事实。 |
| 项目地基 | `init`, `round`, `plan`, `consult`, `approve` | 建立协调骨架、计划/签核/种子与审批事实；必须服从对应生命周期前置条件。 |
| 执行 | `run-task`, `dispatch`, `await-report`, `retry-dead` | Tier S/Tier F 的执行、收取和有限恢复；可能写信号、事件、worktree 和门证据。 |
| 会话控制 | `nudge`, `wake`, `handshake`, `resume`, `session` | attempt-scoped 或 session-scoped 控制；`session show` 只读，`session set` 修改注册态。 |
| 审查收口 | `review`, `verify`, `verdict`, `seal`, `merge`, `record` | 固定 HEAD 审查、裁决、合并与补记；正常成功路径优先 `seal`。 |
| 运维恢复 | `ledger recover`, `sites` | WAL 严格前缀恢复或租约约束的现场清理；删除类操作不会因方便而放宽所有权判据。 |
| 驱动 | `run`, `step`, `serve`, `run-wave` | 读取同一账本并驱动机械分支；多个驱动者仍受 durable lease 和屏障约束。 |
| 指令队列 | `inbox` | `list` 只读；`add`/`done` 改变项目指令队列。 |

条件型命令必须按具体参数判断：`ledger recover` 默认干跑，只有 `--apply` 回灌；
`snapshot` 默认只读，只有 `--write` 落盘；`bootstrap --copy` 会写剪贴板；`verdict --dry-run`
不落裁决；`wake` 下的 `status` 是只读观察，其他 action 可能改变会话控制事实。

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
| `plan` | 活动轮的 ModeConfig、卡片、绑定和 registry 可静态验证；contract change 还须通过 replan 在飞守卫 | 原子更新派生 ROUND-IR，并在新 revision 追加精确 `TaskValidated`。仅 `actor="verifier:root"` 的 `VerdictIssued(FAIL/BLOCKED)` 按 exact attempt identity 释放 replan 守卫 | `PASS`、非 root actor、未知或缺失 verdict 均 fail-closed 保持 attempt live；修正输入后重跑，IR/revision 漂移时重新 seed 验证并重新 sign-off。此判据不释放 scheduler 容量，也不授权 successor/takeover |
| `round seed-verified` | task 属于 validated IR；seed、目标与 expected-red 身份一致 | 在隔离现场实跑 oracle，成功后追加绑定当前 IR revision 的 `SeedOracleVerified` | 修复 seed/card/门后重新 `plan` 和预验；`--record-only` 只是显式 legacy 逃生舱 |
| `round sign-off` | 当前 validated IR 未漂移，要求重验的 task 已有本 revision 的 seed proof | 仅显式用户动作追加 exact `PlanSignedOff` | 任一输入变化后重新 `plan`/预验/签核；不得复制旧签核 payload |
| `consult` | 当前计划期尚无匹配 `PlanSignedOff`，输入和附件均在允许范围 | 生成有界咨询产物和 provider receipt；不自动修改 IR 或代替用户签核 | 修复 preset/provider/附件后仍只在计划期重试 |
| `approve` | task 在 active signed IR，action 属公开高风险枚举 | 相邻追加 `PermissionRequested` 与用户 `PermissionDecided`；只记录授权，不执行外部动作 | denied 或失败时保持原动作未授权；新动作/新范围需新审批 |
| `dispatch` | signed IR、task/agent/attempt 基线和 Git 身份均满足；普通路径无模糊活 attempt；运行时在 capacity lock 内按 signed agent/quota domain 与 durable in-flight 投影 admission | 创建/复用精确 worktree，追加 attempt/workspace/`DispatchIssued` 与 durable wake 事实，GO 绑定固定 SHA | 先区分 capacity、dead、BLOCKED、stall、ambiguous；项目 slot 可额外收紧，不能放宽 runtime admission，也不能手写 GO |
| `run-task` | 与 dispatch 同类的卡片、agent、Git 和门契约可用，且不存在同名遗留 branch/worktree | legacy Tier S one-shot 驱动 provider、REPORT、机检和 gate；它不具备 Tier F collect/wake 的阶段恢复 | 中断后保全并审计现场；已有 branch/worktree 会直接拒绝，需 planner 明确处置现场或改走 Tier F，不能声称“从最后阶段自动恢复” |
| `await-report` | 存在 current DispatchContext；REPORT/BLOCKED 必须绑定该 attempt | REPORT 路径执行 durable collect、机检、gate 与 receipt；BLOCKED 路径追加 canonical terminal。观察窗从本次命令开始计时，先到时记录非终态 `ReportAwaitExpired`（命令失败审计仍会有 `ActionRejected`）；只有从 newest exact authenticated implementation `WakeIssued.ts` 起算的 `runtimeLimit` 已到，才追加既有 `EscalationRaised` + `AttemptTimedOut` 终态链 | `ReportAwaitExpired` 核对 `observerSecs`/`runtimeLimitSecs` 后可用更长观察窗重跑；它不授权 `retry-dead` 或 takeover。Busy 等 lease；stall/dead/BLOCKED 按恢复矩阵；门红修真实原因，不能把 release 当 PASS |
| `retry-dead` | operator 只应在 `await-report` 已机械判 dead 后调用，且未超有界重试策略；当前命令的 retry 决策只看 dead 计数/activity，不会独立证明“刚发生 dead” | 退避后重验身份，满足时派 successor attempt | 返回 `4` 表示不自动重派；交 planner 选合法改派或修卡，不循环调用，也不把命令可调用性当死亡证明 |
| `bootstrap` | agent 可解析，且提示所需项目事实存在 | stdout 渲染客户端提示；`--copy` 仅写剪贴板，不写 signal、attempt 或 ledger | 修复缺失项目事实后重渲染；复制成功不等于会话已启动 |
| `nudge` | task/agent 必须精确匹配 current DispatchContext；状态在允许集合、wake budget 可用，且默认没有未消费 NUDGE | 写 attempt-scoped NUDGE，再记录 `NudgeIssued` 并尝试真实 wake/POKE；provider/fence 失败时控制文件仍可能待消费 | 非零后查文件和 ledger；`--force` 只允许覆盖控制文件，绝不释放 durable lease |
| `wake <agent>` | active IR 允许该 agent，registry/capacity/消息来源合法；review 参数必须成完整 tuple。`--reissue <SOURCE_WAKE_ID>` 只接受同 agent 的 managed OpenCode formal-review source：它已有唯一 accepted receipt、authenticated `session-death-declared`，且 source identity/当前 review slot/账本链全部精确一致；task、attempt、role、deadline 从 source 推导，禁止调用方再传 review tuple/deadline | 普通真实 wake 后记录 `WakeIssued`。精确重放若复用仍存活的 wake，stdout 明示被复用的 `wakeId` 与“未起新进程”，不会伪装成真实派发。**只有 formal review 席位（`primary`/`secondary`）**的完整 tuple 才在同一受控效果内记录 `ReviewRequested`；**`nongate` 角色被运行时显式拒绝产生 formal review 事件**（它仍照常落 `WakeIssued`、`WorkspaceLeased` 等 wake/site durable 事实）。合法 reissue 精确替换自己的容量槽、仍重跑 agent/shared quota admission，并真 spawn 新 wake；新 `WakeIssued.supersededWakeId` 指向 source，且与同 identity 的新 `ReviewRequested` 原子落账。死亡证据允许首次显式更换 message；同 digest 重放幂等返回唯一 active successor（多跳时返回链尾），不 append 或 spawn 第三个 wake | live 或 pending source、缺/错死亡证据、非 OpenCode provider、身份/引用残链、重复 successor、无关容量或 shared quota 满载均 `ActionRejected` + 非零退出，且不得 spawn；已有活 successor 时再次改 message 也拒绝，绝不把未投递的新消息静默报成幂等成功。spawn 不等于 engaged，不得手补 review 请求。此出口不替换 reviewer，也不实现 nudge/H115 的 live-session supersede |
| `handshake` | agent 已登记且 wake budget/通道可用 | 发起真实 probe wake并留下 wake 事实；exact engaged 返回 `0`，Pending/未咬合/timeout 返回 `3`，不创建实现 attempt | `--no-wake` 必为 Pending `3`；修复通道后重试，不能把 spawn 当握手成功 |
| `resume` | current DispatchContext、GO/ACK、Git 和 attempt 身份可精确重建 | 追加 `ResumeIssued` 并驱动 `ResumeWake` durable chain；不 mint successor、不 kill、不释放 collect | live Busy 或 completed replay 可能仍返回既有 prompt/`0`；`0` 不保证发生了新注入，失败后按 exact lineage 审计 |
| `review deliver` / `review reconcile` | review 文件、role、agent、attempt、固定 HEAD 和 main commit 全部匹配 | deliver 先把产物路由到 canonical/late 路径；reconcile 只为已提交的精确产物补 `ReviewDelivered` | 文件未提交、身份不符或 verdict 已变时按提示走 late monotonic 路径；不伪造 review |
| `verify` | signed IR 允许 legacy verifier，当前模式不是 root-manual 禁用档 | 运行固定 HEAD verifier 并输出/记录其 review 结果；FAIL 返回非零 | 修复实现或 review 证据后重新走正常验证；不得把 verifier 自述当 root PASS |
| `verdict` | exact task/attempt/expected HEAD/expected main、latest collect、reviews/evidence 与 IR 全匹配 | `--dry-run` 只证明可裁决；真实调用追加 root `VerdictIssued`，PASS 留下 fixed authorization tuple；FAIL/BLOCKED 被成功记录也返回 `0` | 漂移即用新身份重算；不要把退出 `0` 等同 PASS。main 写屏障要到后续 canonical `MergeStarted` 才开启 |
| `seal` | 当前 attempt 有 0 或 1 条 exact root PASS，expected HEAD 完全一致；0 时 seal 内创建 PASS，1 时校验并续跑，重复/非 canonical 拒绝 | durable replay 地完成 root PASS、`MergeStarted`/Git merge/`MergeExecuted`/合后门/`TaskRecorded`，并在同一 checked batch 退休有完整锚点的任务现场后清场 | 中断后用同一 tuple 重放；完整锚定的同批 `SiteRetired` 不再触发“未授权事件”假失败，也不得重复追加生命周期事件；其他 suffix 与 `CanonicalRootSuffix` 仍 fail-closed；特殊屏障只用精确 `merge`/`record` 恢复 |
| `merge` / `record` | 仅接受运行时识别的分步恢复 tuple；`record` 还需祖先与 post-merge gate 证明 | `merge` 闭合已有 PASS 的 merge 链；`record` 只在完整授权后补 `TaskRecorded` | 默认不替代 `seal`；`--recover`/`--at-tip` 都要保留额外审计事实，拒绝时不 reset main |
| `round close` | 默认所有 task 均有 canonical `TaskRecorded`，无未闭合屏障，main/IR 未漂移；`--force` 强制非空 note 且只豁免未 Recorded | 闭轮前输出逐项 removed/refused/failed/freedBytes；已释放 site/target 按 durable lease disposition 当轮回收，不以 24h 挂钟保留，再追加 `RoundClosed`，写 DONE 与 BOARD 收轮投影 | receipt 对账失败降级为可见诊断且不禁用 GC；拒收现场与其 target 均保留。清理副作用可能早于闭轮拒绝，force 不豁免坏账本、身份/拓扑漂移或非法 archived chain |
| `session set` | agent 已登记，mode/session id 合法；root-manual active round 机械拒绝修改 registry | 改项目 registry 配置；不会清除 durable fault overlay，也不会迁移 in-flight attempt | 新 generation 的 exact Engaged 证据才能清 overlay；先处理在飞会话 |
| `inbox add` / `inbox done` | active IR 在临界点前后保持一致，文件名/内容安全 | 原子创建 pending 指令或把精确文件推进到 done；不改变 task/attempt | 修复漂移或文件身份后重试，不直接移动目录冒充处理 |
| `sites gc` | active IR 可解析；只回收 released generation | reconcile 可能先补真实 terminal facts；对账失败单独诊断但仍继续逐 site 回收，输出 removed/refused/freedBytes 与逐项原因 | refused 或 registry invariant 红时保留现场并返回非零；不得从 PID/目录年龄推断 release |
| `sites sweep-*` / `rotate-logs` | 目标必须在各自固定 scratch/cache/log 管辖域 | scratch 逐项隔离错误并输出 removed/failed/freedBytes；其他入口按各自 keep/newest-generation/closed-round/TTL 规则清理或压缩 | scratch 仅在零回收且存在失败时返回非零；对开放轮、active lease、debug/keep 对象 fail-closed，不能扩大删除根 |
| `run` / `step` / `serve` / `run-wave` | active signed IR 与驱动模式匹配 | 只是反复选择并调用上述同一机械 transition；没有额外权限；`run-wave` blocked 返回 `1`、awaiting-root 返回 `7` | 停在判断边界交 planner；重启驱动前先重读账本，不能假设上一拍零副作用 |

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

### `wake` 的 positional action

Clap 将下列形式都暴露为一个 `wake` 叶命令，因此 command marker 只有 `wake`；AI 仍须按
第一个 positional 精确分流：

| 形式 | 机械语义 |
|---|---|
| `wake <agent> ...` | 向 active IR 允许且 registry 可注入的 agent 发送一次 wake；真派发首行保持 `orch wake <agent>: 注入已完成`，精确复用则改为含既有 `wakeId` 与 `未起新进程` 的首行且不追加 `WakeIssued`。完整 review tuple 才同事务记录 `ReviewRequested`；同 `(task,role)` 后发请求成为 current（其 attempt 身份随请求精确保留），但不隐藏多活 wake，也不覆盖 canonical review 产物。spawn/channel receipt 不等于 engaged。 |
| `wake <agent> --reissue <SOURCE_WAKE_ID> [--message ...\|--message-file ...]` | 仅重投同 agent 的 managed OpenCode formal review。source 必须已有 accepted receipt、authenticated death declaration，并仍精确绑定当前 task/attempt/role 槽；review tuple/deadline 全部继承，调用方不得覆盖。成功真 spawn 新 wake 并以 `supersededWakeId` 原子链接；首次显式 changed message 由 exact death evidence 豁免 digest fence。若 source 已有合法 successor，同 digest 重放幂等返回唯一 active 链尾，绝不第三次 spawn；此时再次改 message 因 successor 仍活而非零拒绝，不能静默丢消息。live/pending、非 OpenCode、错绑/残链/重复 successor 或容量门失败均落 `ActionRejected` 并非零退出、零 spawn。该能力不换 reviewer，也不关闭 H115。 |
<!-- orch-guide-wake-action:status -->
| `wake status <wakeId> [--json]` | orphan-control 只读面；要求唯一可信的 managed Codex/OpenCode wake 与 authenticated descriptor，输出 redacted 状态。它绕过普通 active/stale preflight，但仍需 canonical 项目事实。 |
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
- `wake` 是通用注册会话控制面；`handshake` 用于派发前探活，失败不应消耗实现 attempt。
- `retry-dead` 只处理 `await-report` 已机械判死后的有界重派；stall、慢门和一次静默不等于 dead。
- 新 attempt/改派必须由 `dispatch` 的 takeover 判据或显式受控入口生成，绝不手写 GO 或事件。
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
- REPORT 提前写、修改冻结 seed、弱化断言、越出 writeSet、绕过 required review/evidence。
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
| `handshake` | `<AGENT>` | — |
| `dispatch` | `<TASK>` | 用 `--new-attempt` 时须 `--reason` |
| `await-report` | `<TASK>` | — |
| `nudge` | `<AGENT>` | `--task`（绝不按 agent 最近任务猜 attempt 身份） |
| `wake` | `<AGENT>` | 作审查请求时 `--review-for` + `--attempt` + `--role` 须成完整 tuple |
| `resume` | `<TASK>` | — |
| `retry-dead` | `<TASK>` | — |
| `review deliver` | `<TASK> <ATTEMPT>` | `--role`、`--agent` |
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
