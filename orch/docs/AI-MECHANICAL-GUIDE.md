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

直接 managed 调用沿用控制 descriptor：v2 保存脱敏 requested/effective tuple、固定摘要和路径；v1 仍可读取。完整 LF-framed 控制记录不得超过 64 KiB，启动前按同一序列化格式核验；这是本地产物表示边界，不是 provider/model 的能力上限。缺绑定、未知版本或越界路径拒绝控制请求。

默认构建公开 6 叶：guide、doctor、harness list/lint、wake、consult；selfhost 构建追加任务机械，
共 30 叶。默认 CLI 单包构建不编译任务/轮次/账本 writer。orch-ui 保留，只读功能和测试不变，
其 host 依赖显式启用 selfhost；工作区缺省成员为 CLI。以下任务链、租约、WAL 与签核规则仅适用于
selfhost，实际可调用表面以当前构建的 help/guide --check 为准。

独立项目只需已有提交的 Git 仓库和被 Git 忽略的 .orch/harnesses.yaml，不要求 binding、CURRENT-ROUND
或 IR。发现 selfhost 标记（包括坏内容/悬空标记）时默认构建拒绝调用，不以解析失败降级成独立模式。
普通直调使用同一 managed supervisor；控制描述符 v2 保存非密调用元组/摘要、cwd、日志与发布时刻，
不复制 env/argv，也不创建 Task/round/WAL。旧 v1 描述符继续可由 selfhost 精确对账。
独立项目的 wrapper 从本工具安装/源码位置取得并与编译内置字节匹配，不执行目标仓同名脚本。
独立分发可将原样 wrapper 放在二进制旁的 scripts/，配置变化只影响下一次调用。

AI 接管一个项目时按以下顺序读取：

1. 运行 `orch guide --check`；先读 truth/quick-reference，再按当前动作读取所需 section。
2. selfhost 项目再读项目 RUNBOOK；它只能增加本地约束。独立调用按配置与当前 help 进行。
3. selfhost 项目运行 `orch current` 并读 CURRENT，取得动态状态；独立调用读其控制描述符与原生产物。
4. 只有异常恢复或审计时才读取事件尾部、证据、日志和源码。

本文不替代 executor 的 `coordination/PROTOCOL.md`，也不授权任何合并、发布、删除、
审批或绕过门的动作。`orch guide` 本身只向 stdout 输出内嵌文本，不要求项目存在
`coordination/`，也不读写事件账本。
<!-- orch-guide-section-end:scope -->

<!-- orch-guide-section:truth -->
## 真值、投影与观察信号

独立调用以固定 Git/config/request 身份、受认证控制描述符与原生回执为准；controller 退出不等于持久后端结束。
selfhost 按以下机械权威排序理解状态：

1. 当前轮 `events.jsonl` 中通过解析和身份校验的事件是 durable 事实；状态名称只是事件折叠结果。
2. Git 提交、refs、固定 HEAD 的 REPORT/review/evidence 是事件所绑定的可复核证据。
3. WAL 是账本的逐字节恢复镜像；它只能修复严格前缀缺失，不能创造业务事实。
4. `CURRENT.md`、`status`、`snapshot` 和 UI 是派生观察面；刷新投影不等于改变任务事实。
5. heartbeat、PID、日志增长和客户端窗口是 liveness 证据，不是 durable action 的所有权证明。

所有 attempt、review 和 durable action 都必须绑定当前 round/task/attempt 以及相应的固定
Git/证据身份。字段缺失、类型错误、代际不一致、账本坏行或并发状态模糊时均拒绝猜测。

### 本机 harness 配置

`.orch/harnesses.yaml` 是主仓本机、gitignored 的调用配置，不是 Git、card 或 ROUND-IR 真值。
`harness list` 与 `harness lint` 会从任意 linked worktree 经 git common-dir 回到主仓，只读取一次
该文件：`.orch` 与 leaf 必须 no-symlink，leaf 必须是 regular file，且 `git check-ignore` 必须确认
忽略规则。快照绑定绝对路径、原始 bytes 与 SHA-256；解析后不得在同一 action 中重读。配置缺失、
相对 executable、raw argv/env、未知字段或不可用 alias 都不触发 PATH/env/其它 alias fallback。

Orch 不创建或改写该文件，也不探测账户 token。配置与协作编排变化不触发 replan；在途 action 保持
原快照，下一 action 读取新快照。归档只保存 config digest 和必要非密事实，不把配置原字节写入 Git/card/IR。

### 等待、通知与继续执行

必须区分四个不同事实：provider 已终态、产物字节已稳定、完成通知已入队、后续 driver 已被调度。
前一项不自动推出后一项。后台 monitor 启动成功或 callback/工具结果进入队列，只证明观察/投递动作；
若没有活的消费者或可核验的 continuation receipt，完成结果可以永久停在队列中而不推进状态机。

需要 terminal 后继续执行的调用方，应保持有界前台消费与 final-drain；若由外部系统接续，必须有
真实可核验的消费回执。本产品不提供 daemon/scheduler 来自动调度后续动作，不把通知入队当成已接续。
宿主接口的分段让出不等于本门终态：同一调用仍有效时继续等待，不能据此读取中途答卷、重启期限或另发 action。
调用方应保存原始作用域/固定输入、实际等待者和完成判据，并在实际返回时附回原上下文、核验身份后记录消费。
退出码按各命令声明解释；逐席返回与整波完成不同，通知到达也不等于判断型调用方已处理。缺少有效消费者时不保证续接。

反复查询 provider 不是替代方案；单一机械 wait 可以保持安静，观察窗到期仍按
`ReportAwaitExpired` 等 typed 非终态语义处理。进程退出时必须 final-drain，并把 provider terminal、
稳定产物、round/task/attempt/role/fixed HEAD 一起核验；PID 消失、一次空读或通知已排队都不能冒充交卷。
接口没有暴露 continuation-scheduled/consumed 事实时，调用方不得承诺“完成后会自动唤醒并续跑”。
<!-- orch-guide-section-end:truth -->

<!-- orch-guide-section:lifecycle -->
## selfhost 的 Round、task 与 seal 生命周期

新建 round 使用 schema 3：`RoundOpened.payload.contractSchemaVersion=3`。schema 3 的 card/ROUND-IR
只签代码修改授权（card/seed/write/frozen/entry/gate/evidence/dependency 与 source digest），不签
implementer、reviewer、fusion member、provider/model/effort/mode、roster、timeout 或票数政策。
这些 action 事实改变时不 replan；写入任何 retired/未知调度键即使值为 null/空集合也拒绝。

schema 3 实现路径 exact-XOR `dispatch <task> --local` 或 `dispatch <task> --harness <alias>`：
两路都创建独立 task branch/worktree/GO，任务 lifecycle owner 始终是 `local`；harness alias 只是一次
channel action 地址，不形成 seat、roster 或 capacity 实体。`--harness` 与 schema 3 `wake` 均从主仓
gitignored 配置捕获一次 config/attachment snapshot，经 code-owned driver render、binary/cwd/HEAD preflight
后启动；不得从 tracked AgentRegistry/HarnessRegistry/AdapterSpec/preset 取得 executable、argv/env、cwd、
provider/model/effort/mode 或能力真值，也不得从 PATH/ambient pin 回落。consult 成员只来自当次重复
`--harness`，保持原顺序并拒绝重复；preset/judge/AdapterSpec 均不进入 live admission 或 invocation
snapshot，bootstrap source 只供旧 validator 只读取证。
schema 3 的 harness execute 与普通 one-shot 调用使用同一个 7200 秒通道缺省期限，仍受 managed-wake
安全上限约束；不得把 actorless IR 中已退役的 `wallMinutes=0` 换算成 1 秒。它不是卡面预算或新调度政策。
每个 execute/review/consult override 可含 `limits: {maxPromptBytes, timeoutSeconds}`；字段可省略，
给出时须为正整数，null/未知键/零值/负数/溢出拒绝。`defaults` 仍只接受 tuple，不接受 limits。
完整渲染后的 prompt（含指令、review上下文与附件）按 UTF-8 字节核上限，等于上限允许，超限在
provider spawn 前拒绝且不截断。`harness list --action execute|review|consult` 按真实 driver 能力、
tuple、enabled 与 executable 输出逐 alias 可用性，缺省 list 保持原行为；它不调用模型，也不按
配置节是否存在猜能力。limits 与 requested/effective tuple 属同一个 config snapshot，变化不 replan。
有效期限由一个入口按“显式 CLI → action 配置 → workload/default”选择后受原 hard ceiling 限制；
consult 的 total-wall 是显式可调的总上限，review/execute 保留 managed safety ceiling。review 必须
保留 CLI 是否显式指定的事实，不能把已填入的 workload fallback 当成用户值。consult 的 channelFacts
同时显示 limits 与最终 deadlineSecs；wake 的 config/request digest 和 runtimeLimit 绑定同一次选择。
Agy的code-owned driver将该有效期限同时传给原生CLI的`--print-timeout`，避免其默认print等待
先于外层预算结束；不另设按alias/模型命名的固定秒数，也不改变provider自身的请求限制。
limits 是该 alias/action 与 provider/model tuple 快照的本地保护值，不声明 harness 的固有上限。
UTF-8 prompt bytes 不等于模型 context/output tokens 或 HTTP body bytes；总执行 seconds 不等于
上游请求/空闲超时。外部限制由实际 provider/model 配置及证据确定；切换组合须重新核对保护值，
不能把某次失败外推到整个 driver，也禁止把字节按固定系数换算为token。显式 CLI 不豁免真实外部硬上限。
共同provider的请求校验/协议兼容由实际请求层统一处理；driver仅映射自己的接口，不能按席位或alias
复制网关策略。本产品通过native CLI/bridge调用，不声明已机械获取任意外部provider/model的上限。
consult 成员结束后由单一写入者立即保存完整 answer/manifest；整波仍等所有成员终态后才写 summary，
不因快席已交卷或慢席静默而取消调用。原子首次发布的产物不可覆盖，随后观察失败不抹掉已保存的快席。
同步调用的 execution 事实区分 exited/hard-deadline/observation-failed，记录实际 OS status、总期限、
首完整帧/leader退出/elapsed 时间、进程组结束证明与原始捕获路径。直驱exit72和耗时本身不证明timeout；
受信code-owned wrapper的WRAPPER_EXIT_CODES保留72=timedOut，诊断source为wrapper-exit-status，
不把它改记为宿主HardDeadline。upstreamTermination=unconfirmed明确表示本地退出/信号不证明远端
provider作业已结束或已取消；它不替代已声明driver的终态能力或独立native作业观察。
只有硬期限及当前 owned leader 证明可触发精确进程清理；静默或观察失败没有取消权限。无法证明原生
结束时保留 raw、unclosed/HOLD，不写 ConsultationCompleted 或授予 GC 权。诊断只解析相应 driver 的
可信顶层错误帧，保存脱敏摘要、帧摘要及可用的 reportedHttpStatus/reportedErrorType；source 表示
证据解析来源，不是根因归属。模型正文/工具回显中的错误示例不分类；未知仍为 unknown。
唯一普通stderr例外是空stdout+实际非零退出+非空stderr：记录startup-diagnostic，类别protocol，
只保留脱敏摘要与原日志指针，不从文本推auth/quota或特定provider根因；exit未知不满足该判据。
在途managed作业的进程/census/credential观察错误写入有界observationDiagnostics并保持等待，
按既有采样节奏退避；错误不选择stop winner。认证取消、真实独立终态与总硬期限仍生效，后续安全回收
仍需fresh ownership与完整结束证明。历史观察诊断不把已独立正常结束的调用改记为OperationalError。
分类拼写保持兼容：新增upstream-stream、observation-failed；既有quotaExhausted、rateLimited、
serviceUnavailable、authentication、permission、timeout、protocol、unknown不改名。
spawn 继续 `env_clear`：通用环境只保留固定 PATH 与 HOME/TMPDIR/locale/TERM 等非敏感运行身份；
`USER` 只传给 Claude driver，它是 macOS Claude Code 读取已登录订阅所需的最小键。API key、OAuth
token、SSH socket 与其它 ambient env 对所有 driver 一律不透传。
旧 `run_dispatch*` 与 automatic-successor host 入口也在任何 storage/GO/worktree/provider 效果前核对
current/open/schema3；schema1/2、closed 或坏账本直接拒绝，不能以 ActionRejected 改写旧账本。
approved-reattempt 的同一检查位于外层 protocol lease 之前。v3 使用明确的 local/harness API；
保留的历史六站独占门只证明十样本的纯决策、canonical event 映射及缺站/人工/near-miss 反例，
不再声称旧自动派发与墙钟 GO 等待可运行。H75 等真实进程/信号门保持不变。
`await-report` 不安装 provider liveness。review wire role 固定为 `review`，唯一身份是
`(task, attempt, harness, wakeId)`；Rust 只验证 fixed HEAD、cwd、request/config/attachment digest、
terminal、artifact SHA/bytes 与所有已发起请求均终态，不数 PASS、不应用 veto。新 live request 只认
`unified-channel-v1`：每条可信 managed terminal 后必须紧邻同一 lease/wake 的 exact
`WorkspaceReleased`；typed `ActionRejected` 是另一种 adjudication 闭合，但 post-spawn/effect-unknown
rejection 不释放 managed process/workspace 容量，attempt/root/TaskRecorded 也不得替代逐 wake 物理闭合。
answered/failed/empty/timedOut/canceled 均在 exact release 后释放调用容量，但只有 answered + substantive artifact 可生成 `ReviewDelivered`，backend receipt
本身不能替代 live terminal。

缺失 backend acceptance receipt 时，仅原生退出失败、已认证 hard-deadline 或 manual-cancel 的非自然退出终态，
在明确记录 receipt absent、managed scope 已终止、无 answer/final/output 字节且无交付后可结清为零票；
timeout 还要求没有取消请求；cancel 须为 canceled/StoppedByAuthenticatedCancel/manual-cancel、
exitedNaturally=false、hardDeadlineReached=false 与 canonical cancelRequestId。所有 request/channel binding
与相邻 exact release 继续严格核验。取消认证在 supervisor/status 写入链完成；归档读取信任 runtime 铸造
的终态，不存在独立的账本 cancel 事件，ID 形状本身不证明认证，也不授予 answer 或投票资格。
这不生成 accepted receipt 或 ReviewDelivered，不把超时追认为成功；其它缺收据形状仍拒绝。

`review deliver` 的 schema 3 形状是
`--harness <ALIAS> --wake-id <ID>`；首次交付 no-clobber 安装并提交 canonical bytes，exact replay 先从
当前 commit 重新验证全部 provenance/terminal/bytes，即使 runtime inbox 已 GC 也不追加事件或提交。
只有 `refs/heads/main` 同时含 exact delivery event 与 artifact 才属于该零写 replay；两者在 main 均缺、
但 working ledger/canonical bytes 均 exact 时，重试只补同一 scoped accounting commit；任一单边存在或
字节漂移均按 split-brain 拒绝。recovery 还要求 working ledger/WAL 相等且是 captured main ledger 的
canonical LF 严格扩展；main artifact 必须是 exact regular blob mode，symlink/gitlink 不可冒充。
storage census 捕获的 main commit 与 ledger/artifact bytes 也是提交 CAS 输入；临界区外 main 移动、
活文件漂移、重复 eventId、foreign uncommitted `ReviewDelivered` 或 temporary-index/tree bytes 不同均在
update-ref 前拒绝。
`review deliver`（包括零写 replay）只在该 attempt 的 root verdict 前调用；root 后的同一事实由
verdict/seal/archive validator 重放，不重新进入 delivery 命令。
schema 1/2 的 fixed-seat、Panel、Quorum、runtime-policy 与永久 seed freeze 仅用于 closed-round replay。

schema 3 seed 从本卡 seed commit 到本卡 `TaskRecorded` 仍严格执行逐字搬运、先红、转绿、REPORT cross-check 与负向变异；
`SeedRelocated.freezePolicy=evolvable` 表示 `TaskRecorded` 后它成为可由后继受审卡演进或删除的普通测试，
不会形成新的永久 landed anchor。历史事件、source、TaskRecorded 和提交字节保持只读。
B328 已物理删除 live 永久 landed/prefix/source-shape 准入、公开 srcshape 模块、三个旧预检脚本
及新 supersession 构造/发射器。`plan`、candidate 机检和 `seal` 不再要求历史 target 的永久冻结授权；
本卡 seed 的 SHA、containment、regular-blob/no-symlink、逐字搬运与先红/转绿门保持有效。
旧 descriptor、source/event/commit 不变；legacy 仅按完整 Git commit 重算 schema1/2 历史 reader
选择及 descriptor/base/ordered-command 摘要，拒绝 schema3 和 movable ref。没有工作树 reader 准入入口，
不能把历史审计失败改成固定空摘要，也不能从 legacy 重新生成 supersession。新 `TaskRecorded` 批次
仍受原有根裁决、相邻事件、祖先关系和收口门检查约束。

schema 3 的 `consult` 在 PlanSignedOff 前后均可调用，形状为一个或多个重复 `--harness <ALIAS>` 与
可重复 `--attach`。timeout 只来自显式 CLI 覆盖或 code-owned 默认；所有成员共享一次捕获的 config 与
ordered attachment snapshot，完成预检后并发运行于各自 process group。deadline 到达即终止对应整组；
只有 driver-specific terminal、已结束且已回收的 managed scope、substantive final 与相同 final bytes
digest 同时成立才计有效。exit 0、tool-only、raw transcript 或 terminal-absent 均为逐席 invalid，不能
抹掉有效兄弟。每席 raw answer 与独立 manifest 绑定 fixed HEAD/cwd/request/config/attachment digest、
artifact SHA/bytes 和 terminal；`summary.md` 只是索引，不是 judge、票或综合结论。成员属于当次 action
snapshot，不进入 IR，preset/judge/roster 已无 live 或 CLI 入口。IR/card/binding 损坏仍 fail-closed。
B319/B320 自举期的 live
invocation bridge 在 B320 Recorded 后永久关闭；历史 bootstrap source digest 仅可作为旧 validator 的
只读过渡取证，绝不是 v3 spawn 真值。该私有历史 decoder 只覆盖既有 closed/recorded B319 字节；
任何新 request、B320 以后事实或 live replay 都不能进入它。

正常生产链如下：

```text
RoundOpened
  → plan / SeedOracleVerified / PlanSignedOff
  → DispatchIssued(agent=local,wakePending=false) / WorkspaceLeased / AttemptStarted
  → executor seeded-red / implementation / gates / mutations / REPORT-last
  → ReportObserved
  → ReportCollectClaimed → ReportCollectExecuting → ReportCollectExecuted
  → ReportCollectCompleted
  → 0..N dynamic generic reviews（所有已发起请求均终态）+ evidence
  → VerdictIssued(PASS, fixed HEAD)
  → MergeStarted → MergeExecuted → post-merge gates → TaskRecorded + SiteRetired × 0..N
  → all tasks terminal → RoundClosed
```

- 一个 task 可以有多个单调递增 attempt；新 attempt 不得冒充旧 attempt 的事实。
- `ReportObserved` 只证明 immutable REPORT 已被收取，不证明 collect 门已经成功。
- `VerdictIssued(PASS)` 必须绑定当前 attempt、latest collect、固定 HEAD、签核 IR、审查和证据。
- 正常收口与其中断重放使用 `seal`；旧 merge/record CLI 已退役，历史库级恢复审计不授权恢复旧入口。
- `TaskRecorded` 可与该任务现场的 `SiteRetired` 在同一 checked batch 落账；post-merge
  授权只接纳 runtime actor、round/task、唯一 `TaskRecorded` 锚点、既有 `WorkspaceLeased`
  的 `siteId/generation/attemptId/role/agent` 和单次退休全部匹配的事件，绝不按裸
  `SiteRetired` kind 放行。
- `MergeExecuted` 后门红不得回滚或伪装为 `TaskRecorded`；只走运行时已有的恢复/补记语义。
- `round close` 只在任务集合满足闭轮判据时执行；`--force` 是显式审计动作，不是默认路径。
- schema 3 收轮先逐卡验证完整 Recorded 链；冻结替换计数只在确认整轮没有
  `FrozenContractSuperseded` 后返回零，不进入旧永久冻结批次审计。存在旧冻结事件或
  代际标记不合法仍拒绝；legacy 计数验证和任务完成链校验不因此放宽。

---

> ### ⛔ live 契约到此结束
>
> 本节以下只解释 schema 1/2 已闭合轮的原始字节，**不指导任何 v3 live action**；
> happy path 与正常恢复都不需要读它。

### Legacy schema 1/2 closed-replay appendix（只读）

schema 1/2 的 runtime-policy、Formal、Panel、Quorum、fallback、nongate 与 fixed-seat 数据只作为
closed-round 证据。validator 必须从历史事件绑定的 committed tree 读取当时的 binding、ROUND-IR、ledger
与 artifact；当前工作树的 tracked registry/mode/adapter 以及 `.orch/harnesses.yaml` 都不能参与复算。

现行 CLI/runtime 不再创建、补齐、恢复或转换上述事实：runtime-policy transition、Panel select/retry/backfill、
formal/nongate delivery、spool promotion、fallback/reissue 和自动 review tick 均在副作用前拒绝。历史 decoder、
Panel/Quorum evaluator、policy resolver 与 accounting-suffix classifier 只回答“旧字节是否自洽”，不得追加事件、
安装 artifact、提交 recovery commit 或派发 provider。

<!-- orch-guide-review:accounting-suffix-recovery -->
历史 review accounting suffix 按 legacy singleton 与 Panel batch 的原始形状做只读分类；singleton
核对 exact request binding，Panel 分支执行各自的形状检查，其中 spool/delivery/terminal 三连批仅核
kind 顺序，不能把分类成功当作完整 provenance/artifact 校验。分类结果不再授权 recovery writer；
完整旧轮取证应走 doctor/status/archive audit，不能在活轮补写历史事实。

带非空 frozen-contract supersession 的 post-merge barrier 永远不允许
`post-merge-gate-released`；该声明只供历史审计，已没有生成新 supersession 的 live record
或恢复出口。seal 的瞬时 owner intent 只允许携 exact token 的 orch
no-ff merge 更新 main；Drop 后 intent 消失，因此 H29 的后续修复提交仍可达。
<!-- orch-guide-section-end:lifecycle -->

<!-- orch-guide-section:commands -->
## 命令总表

以下隐藏标记由 `orch guide --check` 与当前 Clap 命令树精确比对；新增、删除或重命名公开
叶子命令而未更新指南会使检查失败。

<!-- orch-guide-command:doctor -->
<!-- orch-guide-selfhost-command:ledger recover -->
<!-- orch-guide-selfhost-command:review deliver -->
<!-- orch-guide-selfhost-command:sites cache run -->
<!-- orch-guide-selfhost-command:sites cache status -->
<!-- orch-guide-selfhost-command:sites cache sweep -->
<!-- orch-guide-selfhost-command:sites gc -->
<!-- orch-guide-selfhost-command:sites sweep-scratch -->
<!-- orch-guide-selfhost-command:sites sweep-trial-cache -->

<!-- orch-guide-selfhost-command:sites sweep-targets -->

<!-- orch-guide-command:harness list -->
<!-- orch-guide-command:harness lint -->
<!-- orch-guide-selfhost-command:status -->

<!-- orch-guide-command:guide -->
<!-- orch-guide-selfhost-command:check -->

<!-- orch-guide-selfhost-command:verdict -->
<!-- orch-guide-selfhost-command:seal -->

<!-- orch-guide-selfhost-command:snapshot -->
<!-- orch-guide-selfhost-command:dispatch -->
<!-- orch-guide-selfhost-command:await-report -->

<!-- orch-guide-selfhost-command:round open -->
<!-- orch-guide-selfhost-command:round sign-off -->
<!-- orch-guide-selfhost-command:round seed-verified -->
<!-- orch-guide-selfhost-command:round close -->
<!-- orch-guide-command:wake -->

<!-- orch-guide-selfhost-command:cost -->
<!-- orch-guide-selfhost-command:stall-check -->
<!-- orch-guide-selfhost-command:plan -->
<!-- orch-guide-command:consult -->

<!-- orch-guide-selfhost-command:current -->

| 构建 | 命令 | 机械效果 |
|---|---|---|
| 默认与 selfhost | guide、doctor、harness list/lint | 契约、Git/config 体检与能力枚举；不发模型请求。 |
| 默认与 selfhost | wake、consult | 显式调用；managed wake 绑定控制身份，consult 逐席稳定保存，整波终态后写 summary。 |
| selfhost | current/status/snapshot/cost/stall-check | 派生状态与只读诊断；current 和 snapshot --write 只更新派生产物。 |
| selfhost | round open/sign-off/seed-verified/close、plan | 显式轮次与签名输入机械；没有自动编排。 |
| selfhost | dispatch、await-report、check、review deliver、verdict、seal | 固定任务链、审查与完整收口；根授权与既有安全门保留。 |
| selfhost | ledger recover、sites gc/sweep-scratch/sweep-trial-cache/sweep-targets | 严格 WAL 恢复或租约约束的安全清理。 |

ledger recover 默认干跑，--apply 才回灌；snapshot --write 才落盘；verdict --dry-run 不落裁决。
旧 init/bind/agent/schema/verify/merge/record/bootstrap/handshake/approve/mcp/session/inbox 及 sites rotate-logs
均不可解析，也无 hidden/alias 恢复入口。日志轮转仍是 round close 的内部安全动作。

selfhost 的 current.planSignedOff 仅在 latest canonical TaskValidated 的 revision/digest 匹配真实 user sign-off 时为 yes；刷新派生文件不产生签核。

以下为 selfhost 保留的存储准入与停滞诊断判据：

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
| `ledger recover --apply` | 指定 round 的 tracked ledger 是 WAL 的逐字节严格前缀，或两者已一致 | 仅原子回灌 WAL 中缺失的原字节并写恢复 receipt；一致时零写入 | 分叉即停写并审计两份输入；不得借它修 lease、门或 attempt |
| `round open` | round id 合法；默认不存在未闭合活动轮；公开的 legacy `--signed-off` 当前 fail-closed，必须先 plan 再单独 sign-off | 建目录，追加 `RoundOpened`，更新 `CURRENT-ROUND` 与 BOARD 开版投影 | 只对“`RoundOpened` 已落、但 `CURRENT-ROUND` 尚未指向它”的特定 crash window 可同参补指针；普通 replay 会拒绝，`--force` 只用于明确审计的跨轮决策 |
| `plan` | schema 3 只读取 exact PROJECT-BINDING、cards 与 seed source；不读取 ModeConfig、agent/harness registry、consult preset 或 landed-anchor 编排政策；binding gate floor、write/frozen/dependency/evidence 与 source digest 必须成立 | 原子更新 actorless ROUND-IR，并在新 revision 追加精确 `TaskValidated`；只在 writeSet/seed/gate/dependency/evidence 合同改变时形成 replan | 修正输入后重跑；参与者、provider/model/effort/mode 或 fusion/review 组合变化不触发 replan |
| `round seed-verified` | task 属于 validated IR；seed、目标与 expected-red 身份一致 | 在隔离现场实跑 oracle，成功后追加绑定当前 IR revision 的 `SeedOracleVerified` | 修复 seed/card/门后重新 `plan` 和预验；`--record-only` 只是显式 legacy 逃生舱 |
| `round sign-off` | 当前 validated IR 未漂移，要求重验的 task 已有本 revision 的 seed proof | 仅显式用户动作追加 exact `PlanSignedOff` | 任一输入变化后重新 `plan`/预验/签核；不得复制旧签核 payload |
| `consult` | schema 3 在签核前后均可；至少一个重复 `--harness`，保持顺序并拒绝重复；附件可重复且按序冻结 | 单次读取本机 config/附件快照，逐席预检后并发调用统一 driver；分别保留 raw answer + manifest，root summary 仅索引；只有可信 terminal + substantive byte-bound final 有效，不自动修改 IR 或代替用户签核 | 任一无效成员只标该席；全席无效返回非零但保留产物。preset/judge/AdapterSpec/roster 不可达，不得把 action 编组写回 card/IR |
| `dispatch` | 当前开放 schema 3 要求 exact-XOR `--local` / `--harness <alias>`，且 task、attempt base、依赖与 Git 身份精确 | 创建/复用 local-owned worktree；harness 路由另追加绑定单快照的 channel `WakeIssued`，不形成调度 actor；保留的 legacy `run_dispatch*` 对 legacy/bad/closed generation 在 storage/GO/worktree/provider 前拒绝 | `BlockedByDependency` 与 `ForwardBaselineRequired` 分别处置；legacy 参数解析兼容不授予派发权限，schema 3 不接受旧 reassignment/wake flags，不手写 GO |
| `await-report` | 存在 current DispatchContext；REPORT/BLOCKED 必须绑定该 attempt | REPORT 路径执行 durable collect、机检、gate 与 receipt；BLOCKED 路径追加 canonical terminal。观察窗从本次命令开始计时，先到时记录非终态 `ReportAwaitExpired`（命令失败审计仍会有 `ActionRejected`）；只有从 newest exact authenticated implementation `WakeIssued.ts` 起算的 `runtimeLimit` 已到，才追加既有 `EscalationRaised` + `AttemptTimedOut` 终态链 | `ReportAwaitExpired` 核对 `observerSecs`/`runtimeLimitSecs` 后可用更长观察窗重跑；它不授权自动重派或 takeover。Busy 等 lease；stall/dead/BLOCKED 按恢复矩阵；门红修真实原因，不能把 release 当 PASS |
| `wake <alias>` | schema 3 review 只接受 task+attempt，harness 来自 positional alias，role 由代码固定为 `review`；不读取 IR roster/capacity 或 tracked registry，不存在 formal role/reissue 参数 | schema 3 原子记录 generic `WakeIssued + ReviewRequested`，并记录固定 HEAD、单次 config/request/attachment/command snapshot、requested/effective tuple、driver、observation source 与 executable identity；这些 action facts不改变 IR | pending/空答/错 receipt/cwd/digest/binary identity 均拒绝；不得手补请求或把 legacy seat/policy 带回 v3 |
| `review deliver` | 要求 task/attempt/harness/wakeId 与 unified request、可信 terminal、紧邻 exact release、固定 HEAD/cwd/digests 和 bytes 全匹配 | 首次调用 no-clobber 安装并以 scoped commit 同时绑定 artifact + `ReviewDelivered`；exact replay 从 committed canonical bytes 复验且零 append/commit，不依赖已 GC inbox | pending/空答/receipt-only、错 HEAD/cwd/digest/channel binding/release/symlink/hardlink 均拒绝；旧 role/agent/reconcile/Panel 参数与命令不可达 |
| `verdict` | 当前开放 exact schema 3；exact task/attempt/expected HEAD/expected main、latest collect、reviews/evidence 与 IR 全匹配；normal 路径要求当前签核早于 dispatch，受签 exact-attempt bootstrap permit 仅允许下述两条闭合迁移链 | `--dry-run` 只证明可裁决；真实调用追加 root `VerdictIssued`，PASS 留下 fixed authorization tuple；FAIL/BLOCKED 被成功记录也返回 `0` | legacy/closed host 入口在副作用前拒绝；漂移即用新身份重算，不把退出 `0` 等同 PASS。main 写屏障要到后续 canonical `MergeStarted` 才开启 |
| `seal` | 当前开放 exact schema 3，锁前检查并在锁内重查同一 round；当前 attempt 有 0 或 1 条 exact root PASS，expected HEAD 完全一致；0 时 seal 内创建 PASS，1 时校验并续跑，重复/非 canonical 拒绝 | durable replay 地完成 root PASS、`MergeStarted`/Git merge/`MergeExecuted`/合后门/`TaskRecorded`，并在同一 checked batch 退休有完整锚点的任务现场后清场 | legacy/closed host 入口在副作用前拒绝；中断后用同一 tuple 重放；完整锚定的同批 `SiteRetired` 不再触发“未授权事件”假失败，也不得重复追加生命周期事件；其他 suffix 与 `CanonicalRootSuffix` 仍 fail-closed；专用旧恢复 CLI 不再暴露；未建模屏障保持拒绝与证据，不伪造完成 |
| `round close` | 默认 active IR task 均有 canonical `TaskRecorded`，无未闭合屏障，main/IR 未漂移；更高 revision 签名移出的历史 task 只在其已于该 validation 前 `blocked/changes_requested`、且之后仅有受管终态清理事实时忽略，任何再派发或状态推进仍拒绝；`--force` 强制非空 note 且只豁免 active task 未 Recorded | 闭轮前执行统一存储维护，输出逐项 removed/held/failed、逻辑占用及各文件系统可用空间前后值；完整核验已释放 site/target 后当轮回收，不以 24h 挂钟保留，再追加 `RoundClosed`，写 DONE 与 BOARD 收轮投影 | receipt 对账失败降级为可见诊断且不禁用 GC；拒收现场与其 target 均保留。清理副作用可能早于闭轮拒绝，force 不豁免坏账本、身份/拓扑漂移或非法 archived chain |
| `sites gc` | 不要求活轮或 active IR；尚未闭合的 schema 1/2 轮次继续只读拒绝，可信闭轮后才可维护；`--round` 可限定历史现场，诊断缓存独立核验 | `--dry-run` 严格只读，逐项列完整删除判据与需在实际删除时重核的锁/目录身份；当前活轮 apply 保留显式真实回执收取；host 在读 sidecar 前拒绝闭轮/legacy 对账，指定其他轮不对账。不补造历史终态、不改历史 ledger/WAL。各轮独立核验，跨轮重叠或无法界定归属时保护受影响对象；受管诊断锁独立于历史轮错误 | removed/held/failed 与原因可回读；failed 或报告持久化失败返回4，held 保留。`gc 幂等重放` 不重复计删除；`freedBytes 由动作前后测量` 仅描述旧底层回执的逻辑差值。新报告使用 removedLogicalBytes，另测文件系统 availableBytes。完整 journal 重现对象仍须 canonical release、无跨代路径共享及全部物理判据，不凭旧收据删除或改历史收据（`COMPLETE_JOURNAL_ABA_CONTRACT_V1`） |
| `sites sweep-*` 与内部日志轮转 | 不改变既有明确所有者的底层管辖域 | 前台 scratch/trial-cache 入口只读报告未登记目录，不按名称或 TTL 接管；targets 按统一现场判据和跨轮归属核验，输出统一报告；内部日志轮转保留原有日志策略 | active、unknown、dirty、blocked、共享 debug 和超过隔离上限现场保留；不提高隔离上限。缺失/非法身份拒绝，不以空账本替代坏账本 |

**plan 派发准入 floors。** schema3 对 exact binding/cards/seeds 核 writeSet、冻结路径、
依赖、入口、证据及门强度；已 Recorded target 不形成永久 veto。历史 source/event/commit 只读，
本卡 seed 到 Recorded 前仍逐字冻结。只读历史回放不以移动后的 main 重新分类已签授权。
真实 Rust-only 项目还要求
`commands.check`，mixed Rust+Node 项目要求 `commands.rustCheck`；所选命令必须存在且非空，并在
首个 `--` 终止符前携带独立 `--all-targets` token；长字符串拼接或 terminator
后的 token 均拒绝。该错误与既有 `--locked` floor 一次聚合；非 Rust binding 保持历史语义。

Generic review 只以 `(task, attempt, harness, wakeId)` 绑定；role 固定为 `review`，同一 attempt/harness
不得重复发起。provider receipt、进程 activity 或 exit 0 都不能单独构成答卷；review action 还必须有稳定的
regular/no-symlink output artifact。legacy formal capacity、fallback、reissue、Panel 与 magic evidence 不再参与
live admission。

完整 generic review tuple 省略 `--deadline-secs` 时，`wake` 从当前 actorless IR 的 `requiredEvidence` 数量
推导 deadline；显式值优先，结果受 managed-wake safety ceiling 约束。

<!-- orch-guide-harness:smartclaw-session-terminal -->
旧 SmartClaw observationSource 保留 LF-complete payload、exact runtime session 与 accepted receipt 语义。
新 `dewusmartclaw-native-final-and-stream-v1` 代际使用 ASCII 转义 JSON 请求，回传 raw 逐字节保留；
仅在客户端已收殓且受管写入范围确认结束后，单次只读原生存储，要求唯一 session、exact cwd、完整
渲染请求 SHA、completed 状态、无待完成工具和唯一 boolean isFinal 非空正文。保存原生 id/sequence/
hash 与 closed raw hash 的独立投影；不得从过程文本或 PASS/INPUT 字符串猜终答。数据库路径取发起时
捕获的运行环境，后续读取不改用新的 HOME。native记录独立决定final投影，raw的缺尾帧、截断或过程
文本不提供或否决投影权限；其原字节与hash仍保全。旧代际live parser继续只接受LF-complete终帧。
原生仍运行、绑定歧义或观察不可用均 HOLD，receiptless 也不能跳过原生结束核验。已证明原生结束但
缺有效 final 的调用只保留零票结果；有原生 final 也不能豁免 accepted receipt 或正式 review artifact。
单个请求中的工具调用 ID 必须唯一，未知 ID 的结果或无结果调用仍拒绝。已声明 ID 的多条结果仅在
全部为严格 boolean isError=true 时作为重复错误回执；成功、混合或非布尔重复仍 HOLD。重复原生行
ID 与数量记录为 duplicateErrorReceipts；普通投影不添加空字段，原始数据库和回执逐字保留。
新代际有效native final可覆盖socket的EOF诊断（wrapper70/71），但原退出码原样保留；72、显式取消、
身份/请求不一致或未闭合状态不因已有正文而升级。旧wrapper终态ABI本身不变，只有新native证据源
选择独立的作业终态；只改投影helper而让后续EOF分类继续否决终答不算交付。
supervisor 的 hard-deadline/信号证据不会因晚到的 native final 变成自然结束或 answered。
只有有效独立投影首次发布；相同字节 replay 幂等，已发布身份/final/raw 变化拒绝。已退役的 Panel
spool、fixed-primary magic 或 descriptor-less bridge 不能产生新的 terminal。

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

`ORCH_HARNESS_ROLE` 只接受 legacy 的 `primary|secondary|nongate|implement` 或 schema-3 channel 的
`review|consult`；implement/consult 的 review 落点必须用
显式 `NO_REVIEW_OUTPUT` 哨兵。`ORCH_HARNESS_CWD` 与 review 落点必须是绝对路径，fixed head 必须是
完整 40 位小写 SHA；provider/orch binary 必须是绝对、存在且可执行的常规文件。

<!-- orch-guide-harness:envelope-complete-or-refuse -->
只要出现任一信封键，十六键就必须全部非空并在任何 provider `spawn` 之前完成校验；缺键、空白、
相对路径、短/大写 SHA、未构建或不可执行的 binary 都直接拒绝，不用空串或本机缺省继续启动。
`ORCH_HARNESS_ORCH_BIN` 指向运行中的已构建 orch，无法提供时 reviewer 不会被派发。

<!-- orch-guide-harness:legacy-alias-precedence -->
旧 `ORCH_PI_*` / `ORCH_ZCODE_*` / DSH identity 别名只作兼容输入：信封存在时信封逐字段取胜，
冲突必须打印诊断；信封完全缺席时才进入旧变量兼容分支。DSH 的 legacy/no-channel managed 路径仍在
应用 rendered env map 后清除 `ORCH_DSH_PRESET` / `ORCH_DSH_PROFILE` / `ORCH_DSH_ZSTD_BIN`；schema-3
channel 则保留 config snapshot 派生的 `minimal` preset 或 `headless` profile，使 effective child env 与
command digest 相同；parent render 把 complete env 纳入 command digest，未建模的
`ORCH_DSH_ZSTD_BIN` 在 launch-spec spawn 前拒绝，supervisor 不再作 post-digest 删键。DSH schema-3
调用要求运行中 Orch 位于 exact `debug|release` Cargo profile，其祖父
target 必须有 canonical regular/no-symlink `CACHEDIR.TAG` 且可写，并位于 invocation cwd 与
`/tmp`/`TMPDIR` 之外；不满足时在 supervisor/provider spawn 前拒绝。仅 `MANUAL/MANUAL-A0000` 可把
target 放在 cwd 内，临时目录禁令仍不豁免。自举开发需从仓外非临时 Cargo target 构建并运行该 binary。
provider executable 的受控候选只允许
信封 `ORCH_HARNESS_PROVIDER_BIN`，兼容分支才可读取对应 `ORCH_*_BIN`；信封模式绝不从 `PATH`
回落同名 provider。每条 `WakeIssued` 同时携带本次 spawn 前读取的 `harnessRegistryDigest`，使描述符
轮中漂移可事后核对。

DSH managed receipt 不是首条 `session` record 的副产物。该记录只验 selected session id/cwd；
`turn/start`/`tool/result` projection 在首个 exact `request/header.data.header.config` 到来前进入
`buffered_projection`。header 的 provider/model/reasoningEffort 与 signed envelope 三元组逐字一致后，
wrapper 先恰一次输出 `dsh.session`，再按 transcript 原序释放缓冲；后续 header 漂移即 fail-closed。
`request/context` 缺 reasoningEffort，不能单独授权 receipt。这个顺序只约束 wrapper projection，
不声称 provider process/model work 尚未开始；legacy/no-envelope 模式绝不输出 `dsh.session`。

### `wake` 的 positional action

Clap 将下列形式都暴露为一个 `wake` 叶命令，因此 command marker 只有 `wake`；AI 仍须按
第一个 positional 精确分流：

| 形式 | 机械语义 |
|---|---|
| `wake <harness> ...` | 从当次本机 config 解析 harness alias 并发送一次独立 wake，生成新的 `wakeId`；真派发首行为 `orch wake <harness>: 注入已完成`。`--review-for` + `--attempt` 完整成组时同事务记录 role=`review` 的 generic `ReviewRequested`，同 attempt/harness 重复请求明确拒绝，不承诺复用旧 wakeId；没有 formal role、reissue、seat 或 roster 参数。spawn/channel receipt 不等于 substantive 答卷。 |
<!-- orch-guide-wake-action:status -->
| `wake status <wakeId> [--json]` | orphan-control 只读面；要求唯一可信的 managed Codex/OpenCode/Pi/ZCode wake 与 authenticated descriptor，输出 redacted 状态。它绕过普通 active/stale preflight，但仍需 canonical 项目事实。 |
<!-- orch-guide-wake-action:attach -->
| `wake attach <wakeId> --mode <resume\|fork> --reason ... [--deadline-secs <1..=21600>]` | 参数形状暂留给 control 兼容，但 legacy formal attach 已退役；当前 driver 未提供 provider-neutral attach 时在任何 session/provider/ledger 副作用前明确拒绝。 |
<!-- orch-guide-wake-action:cancel -->
| `wake cancel <wakeId> --reason ...` | 对 authenticated managed wake 首写者获胜地提交/重查 cancel control；CLI 自身不发进程信号。Accepted/AlreadyCanceling 的 `0` 只表示请求已确认，不表示 scope 已终止；Pending/CleanupPending 用同一命令或 status 重查。 |
<!-- orch-guide-wake-action:declare-dead -->
| `wake declare-dead <wakeId>` | 参数形状暂留给 control 兼容；旧 review-scoped session-death writer 已退役并明确拒绝。状态观察使用 `wake status`，停止请求使用 `wake cancel`。 |

---

> ### ⛔ live 契约到此结束
>
> 以下内容**只在异常恢复、跨轮取证或审计时需要读**，happy path 不需要：
> legacy schema 1/2 review evidence、`verdict` 的 bootstrap permit、phase-scoped gate log、
> `candidate-lanes-v1` / `root-reuse-v1` / `final-tree-v1` 门策略、`WorkspaceFullPermit`、
> `GateExecuted` 十键、`complete seal` 的 post-Recorded 重放、`landedSeedBaseline` 迁移口径，
> 以及 frozen-contract supersession 的授权臂与字节形状。
>
> 它们仍是**现行运行时行为**，不是废弃文本——只是按需读取，不必在冷启动时加载。

<!-- orch-guide-seal:post-recorded-managed-terminal -->
`complete seal` 的幂等重放仍要求 current IR 已验证并签核；首封尚未 committed `TaskRecorded` 时，
目标任务授权绑定其历史 root `mainHeadSha` 与已接受 lifecycle suffix；current main 已 Recorded 后，
重放的可见前缀改为逐字节绑定 current-main ledger blob。该前缀已包含
目标任务唯一、同轮且 `postMergeGates=all-green` 的 `TaskRecorded` 后，working ledger 只可多出
一个受管清理批次：此前必须恰有一个身份完全相同的 canonical `WakeIssued`（包括存在时类型正确的
model-wake reservation），随后只能是同 wake 的 canonical
`ManagedWakeTerminated`；若此前有对应 `WorkspaceLeased`，还必须紧邻 runtime 按该 terminal
重算出的唯一、同批 provenance `WorkspaceReleased`。缺 wake/record 锚、重复 terminal、身份或
closed outcome 漂移、
漏/伪造 release 都拒绝。此许可只属于 complete/post-merge replay；verdict、root、storage 与 exact
suffix mode 的拒绝边界不变。

<!-- orch-guide-review:dsh-legacy-spool -->
### Legacy review evidence（只读）

DSH spool、nongate receipt、signed fallback、closed quorum 与 `primaryPassAloneSatisfies` 只解释已关闭轮的
原始证据。它们不再是新任务的编排输入，也不生成 seat、票数或替补实体；新一轮的票数与否决政策只由
RUNBOOK/skill 执行，Rust 只验证 generic review 的 exact request、可信 terminal、固定 HEAD、cwd、digest 与
artifact bytes。旧 artifact/receipt 保持原字节，不复制到当前 registry，也不因缺少 live writer 而补账。

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

<!-- orch-guide-policy:production-reader-target -->
source-reader closure 逐字校验 binding 内的 descriptor 指针、descriptor SHA、其 base descriptor
指针与 base SHA，再将 candidate 实际改动的 production subject 反向闭合到 runnable integration
tests。policy `ownerTask` 必须是 canonical task id，并在 immutable policy base 内有唯一先行
`MergeExecuted` + `TaskRecorded(all-green)`，且 merge 是 policy-base ancestor；任意字符串或固定
写死的旧 owner 都没有权限。seed 参数只能来自本卡 seed 落点。integration reader 继续从 canonical
`orch/crates/<package>/tests/<stem>.rs` 派生 target；`src/` 下的 production reader 必须在 descriptor
显式声明同 package 的独立 `runnableTarget`，并且对应 integration target 已存在于 policy base。
错 package/test、不存在 target、未知 literal/dynamic/macro reader、descriptor 或 reader 字节漂移、
未知 commandRef 都不会退化成只跑 `check`：runtime 先追加 canonical
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

round close 内部日志轮转对含 exact round 的格式按轮归属；历史无轮号文件继续走 task-prefix 兼容识别，
若被多轮主张就保留并报告冲突，不猜测或迁移。root-verdict 的幂等读取对新 verdict 使用其绑定的
`gateRunId` 精确回读 phase-scoped 路径；旧 verdict 只有 scoped 文件不存在、且恰有一份
regular-file legacy candidate，其 SHA-256 与长度都和账本绑定一致时才兼容读取。
scoped/legacy 并存、多候选、symlink、缺失或字节绑定不符都 fail closed。

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
只有 merge 后门全绿，唯一 canonical helper 才在普通 `seal` 与历史库级 record-at-tip/H48 green recovery
三路原子追加 `TaskRecorded → FrozenContractSuperseded*`（按 target 排序）`→ SiteRetired* →`
可选 `RecordGateRelaxed`。普通 `append`/`append_checked` 无 authority；expected-main suffix 会重建完整
signed payload（含原授权变体）、事件顺序、retirement 集合与 recovery tail；planner arm 不合成
blocked/replacement。`FrozenContractSuperseded` 是 known inert
audit fact，不替代 `TaskRecorded` 状态边；其按 initiator 的 Effective 次数进入 `RoundClosed`，
Authorized 但未落该事件的不计数。

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

### 受管 Cargo 诊断缓存

`sites cache run --cwd <绝对干净 worktree> --purpose <单行用途> -- <binding 中的绝对 cargo> test --locked --manifest-path orch/Cargo.toml ...`
为本地前台 Cargo build/check/test/doc/clippy 分配唯一私有 `CARGO_TARGET_DIR`。不运行 provider/远端作业，
不接受覆盖 target/config 或 `--fix`；源必须为项目内干净且固定 HEAD 的主/linked worktree。
诊断使用独立的 Diagnostic 容量票，复用真实文件系统预留和门的估算/底线；不伪造 task、round 或门审计事件，也不能刷新为门票。
登记和目录在 spawn 前 fsync；日志保留在 `coordination/runtime/diagnostic-cache/<id>/`，target 单独删除。
真实 child wait、进程组和打开文件检查、源与目录身份、稳定日志摘要均通过后才回收；不发送任何信号。
正常非零退出仍保留实际 exitCode 与日志；CLI run 在 child exit0 时返回0，否则返回1，回收失败不会改写 child 结果。
`--keep-cache` 创建 `.keep` 调试标记；删除该标记只解除偏好，`sites cache sweep` 仍重核所有安全条件。
控制器在持久退出证据之前崩溃时保持 unknown/held，不凭 PID 消失推断完成。已记录退出但尚有使用者时保留，
之后可由 sweep 完成稳定日志保全和回收。重复 sweep 不重复记已释放 bytes。
如果 removing 回执已落盘而 target/retiring 都已不存在，核验已有退出与日志摘要后仅补回执，
持久标记“按不存在推断”；不声称由本次删除或知道原始删除量，本次新增删除 bytes 为0。
`sites cache status` 严格只读；`--last-maintenance` 回读最近报告。run/sweep 与 gc 在已闭轮也可用，
无需新建轮次。run 的原有结果附 maintenance 报告；sweep 输出统一报告，有 failed 项或报告保存失败返回4，held 不是删除成功。
这些数字是逻辑字节，不冒充 APFS 物理释放；共享主 debug 与未登记旧现场不由本入口接管。
需要默认 target 的 CLI/CAS 根推导验证继续使用既有受管任务/门现场，不把隔离 target 当作默认路径的证明。

### 统一维护、触发与空间准入

可信原生终态已保全答卷、TaskRecorded 后、收轮前及开轮前调用统一维护。审查答卷可由唯一匹配的
ReviewDelivered 保全，也可由受认证 ManagedWakeTerminated 指向项目 runtime/review-inbox 内稳定
摘要匹配的答卷；答卷在待删除现场内部不构成保全。最后一个使用者未释放、原生终态不明均 HOLD。
最近维护报告保存在 `coordination/runtime/storage-maintenance/latest.json`，不写历史账本。
本地合并尾务不再提前直接删除工作目录或任务分支；统一维护先测量并核验后回收工作目录，任务分支保留。
seal 的内部补记路径不另起一次维护，避免内层先删除而外层报告零计量。
开轮展示未解决问题并重试安全回收，再按现有 floor 新鲜探测；不足或探测失败时，在创建新轮目录前拒绝。
派发、审查、诊断和门继续用各自估算与预留，并探测实际缓存所在文件系统。清理错误不撤销任务完成事实，
也不单独阻止空间充足的新操作；清理入口不受低空间准入限制。
报告的逻辑占用按路径并集计量，不把父子路径重复相加；删除逻辑字节排除隔离搬移和既往删除回执。
同一次报告对相同目录复用一次占用快照；该快照只用于报告，不跨调用保留，也不用于删除前核验或空间准入。
无法测量的路径记为未知，仍统计其他可安全测量的路径。
文件系统可用空间是独立前后观测，受同时发生的其他写入影响，不能用逻辑删除量替代。

### 终态后的资源回收边界

缓存扫描与现场租约共用角色解析：`primary`、`secondary`、`nongate`、`review`、`implement`。
字段缺失、类型错误和未知角色分别报错，均在删除前拒绝；合法 `review` 不再被误报为缺失。
角色合法不代表已经终态，活跃 lease 与身份错配仍须保留。

durable terminal、`TaskRecorded` 或 `SiteRetired` 只证明协议身份已终止/退休；除非另有进程回收 receipt，
它们不自动证明 wrapper、process group、fsmonitor 或文件缓存已经释放。`sites gc`/`sweep-*` 的成功只绑定
各自管辖的文件系统对象，也不等于 RSS 已下降。

任何自动内存/磁盘回收器都必须消费 authenticated ownership：无 active lease/wake、进程身份未复用、
worktree clean，且提交已被 main 包含或有保全 ref。runtime-owned 对象应在其 terminal 后尽快回收，
而不是默认等待 TTL 或闭轮；shared/user-owned、active、dirty、blocked、debug 或身份不明对象必须保留。
本实现只管理磁盘，幂等报告 removed/held/failed、逻辑占用与各文件系统可用空间；不探测或回收 RSS，
不终止进程，不增加后台服务。禁止用 broad 进程名匹配或目录年龄代替 ownership 证明。
<!-- orch-guide-section-end:leases -->

<!-- orch-guide-section:nudge-resume -->
## 控制能力边界

status/cancel 只按实际 driver 与受认证产物执行；控制器 terminal 不单独证明持久原生作业结束。
attach/declare-dead 保留明确 unsupported 拒绝语义，当前 provider-neutral writer 未接线；不因名称可解析
或历史纯规划 helper 存在而宣称支持。OpenCode 的控制能力表仅标记实际 status/cancel。

## Managed wake 控制与手动接续

<!-- orch-guide-invariant:nudge-not-release-collect -->
旧 `nudge` 已无可调用入口，更不可能释放 collect lease。当前派发、收取、审查与 `seal`
仍是显式动作；工作中的慢调用继续等待，观察异常、静默或另一席完成都不授予停止权。

B329 已物理删除 `serve`/`runloop`/`runtask`/`wave`、自动 action planner、fresh planner
spawn 家族和 task-level nudge/resume/automatic successor 写路径。九个退役命令
run-task/retry-dead/nudge/resume/schedule/run/step/serve/run-wave 不能通过 hidden 或 alias 恢复。
保留的依赖准入归 plan，generic review 身份归 generic_review，短时发布锁归 channel，
只读负载与旧签名准入归 legacy；没有新增调度实体或票数政策。

- 受支持的 managed wake 控制继续绑定 exact wakeId、request、会话与原生终态；不支持的
  action 明确拒绝，不退回旧 task resume，不以文件存在声称 transport 已接线。
- `wake/resume_dispatch.rs::project()` 只读取历史替换关系并核 receipt、terminal、顺序、
  身份与唯一性。新调用不再生成 resumedFromWakeId/resumeActionId/resumeOwner/
  resumeLeaseGeneration；保留读取不是授权重建 writer。
- canonical BLOCKED/FAIL 后，保全原工作树/提交。若卡面或 requiredEvidence 需要变动，先
  固定修订方案并咨询裁决，再 plan、提交相互匹配的 cards/IR/TaskValidated、seed 预验与签核，
  然后通过显式 dispatch 建立 successor；不手写 GO 或事件，不把旧 attempt 重新当成可写授权。
- `NudgeIssued`/`ResumeIssued` 等旧事件仍可用于历史控制 epoch 的只读解释；没有新的控制文件
  消费者或自动重试。手动 await/lease、WAL 与安全回收的原有判据保持。

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

历史带 `resumedFromWakeId` 的 `WakeIssued` 仍须保留原 descriptor-derived harnessId、
terminalCapability 与 harnessRegistryDigest。历史投影不产生新的替换事件；终态对账继续按
exact wakeId 闭合，另一调用的合法终态不能替它释放生命周期。

<!-- orch-guide-harness:exit-zero-is-not-answered -->
`exit 0` **不等于** `answered`。只有已认证的 turn-end 加稳定 final text 摘要或稳定输出
artifact 摘要，才可判 `answered`；exit 0 但只有进度句、工具活动或零帧 EOF 一律为
`empty`。进程退出前必须 final-drain，退出本身不补造终帧。

Claude 与 CodeBuddy 共用严格成功帧：`type=result`、`subtype=success`、`is_error=false`、
非空 `result`，且 `error` 只能缺省/null/false。监督进程与终态回读使用同一判据。
监督状态漏记 terminalSeen 时，仅同族 driver 已声明支持的 unified action 可补足：consult 必须无 task；
review/execute 须有非空 task 与 attemptId，review 还须声明非空输出路径。derived terminal、明确 null providerKind、
exact wakeId、自然 exit 0、完整终止且无 cancel/deadline/error/signal，才可由唯一成功帧补足该事实；
日志还须为 logs 直属 regular/non-symlink/single-link 文件，字节数与监督状态完全一致。
这不修改旧 status 字节，也不放宽终态矩阵或将该补足规则扩展到其它 driver。
补足只证明原生 turn 结束，不等于实现验收；review 缺稳定产物仍为 empty、不能计票，内容契约继续独立校验。

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
| heartbeat/日志持续推进 | 当前 attempt、generation、最近事件 | 继续 `await-report` | 运行时间长不等于 stall |
| 无 REPORT、heartbeat fresh | GO ack、进程/managed wake 状态 | 等待并核实实际进展 | 无输出不等于 dead |
| `ReportAwaitExpired` / 观察窗到期 | exact attempt、`observerSecs`、`runtimeLimitSecs`、provider 进展；`runtimeLimitSecs=0` 表示没有已认证期限 | 继续等待，或用合适的更长观察窗重跑 `await-report` | 观察者停止等待不等于 runtime deadline、dead、stall 或 takeover 授权 |
| `await-report` 返回 stall | snapshot、heartbeat 代际、provider receipt | 先诊断并保全现场；仅确认故障后使用已支持的 exact wake 控制 | stall 不授权 takeover |
| `await-report` 返回 dead | terminal 事件、当前 attempt | 核原生结束和 canonical terminal 后由 planner 决定显式接续/BLOCKED | PID 不在不能释放其他 durable action |
| executor 给出 BLOCKED | canonical BLOCKED、AttemptBlocked、卡面/IR | 若卡面/IR 需修改则重新 plan、验证和签核；结束旧会话后按现有或修订后的合法计划派 successor | 不在旧 attempt 上偷偷扩大 writeSet |
| collect 报 live owner/Busy | 最新同 action 的 owner、generation、`leaseUntil` | 等租约到期后重跑 `await-report` | 不 kill、不伪造 release、不改时钟 |
| collect 门红 | `GateExecuted`、完整日志、`ReportCollectReleased` | 修复真实原因或按 attempt 规则返工；再次收取会重新跑门 | 租约已释放不代表门会绿 |
| review FAIL / root verdict FAIL·BLOCKED / PASS | 固定 HEAD、review/evidence binding、IR；root verdict 还须核对 exact actor、attempt identity 与 verdict payload | exact root FAIL/BLOCKED 可进入卡面定义的 repair/takeover 流程，并在修卡后重跑 `plan`；PASS 继续既有 `seal`/merge barrier | 非 root 或畸形 verdict 不释放 replan 守卫；不改审查产物冒充 PASS，也不以 replan 绕过 PASS merge barrier |
| backend receipt degraded | exact wake、current `ReviewRequested`、immutable log window 与 degraded/rejection identity | 修复合法 request lineage 后只对 exact wake 做 late reconcile；保持现场 | 非零不证明 provider 未启动；不再 wake 双开、不手写 accepted receipt、不把 degraded 当 review/reissue 授权 |
| stale-binary 拒绝状态变更 | build imprint 与 main 差异 | 用当前源码 locked rebuild，再重试原命令 | `--allow-stale-binary` 不是默认恢复 |
| merge 后门红 | MergeStarted/MergeExecuted、失败门、main 拓扑 | 同一 tuple 重放 seal；旧专用恢复 CLI 已退役，额外历史逃生情形须保全证据并明确裁定 | 不 reset main，不伪造 TaskRecorded |
| ledger/WAL 不一致 | `orch doctor` 与 `ledger recover` 干跑 | 仅 strict-prefix 情形才 `--apply` | WAL 工具不修 lease、门、attempt |

<!-- orch-guide-invariant:ledger-recover-wal-only -->
`ledger recover` 只处理 WAL 与账本的逐字节严格前缀关系；它不释放业务租约、不补门结果、
不结束 attempt，也不替代 `await-report`、`seal` 或人工裁决。

### 两个事故场景的机械推演

**collect 持有者被 kill：** 先从账本定位 exact `ReportCollectExecuting` 的 owner、generation
和 `leaseUntil`。租约仍活时，即使 PID 已消失也只等待；不运行 `ledger recover` 来解锁、
不伪造 release 或改时钟。到期后重新执行同一 task 的 `await-report`：运行时在锁内为旧代追加
`ReportCollectReleased(outcomeUnknown: true)`，再 claim 新代并重跑门。恢复只证明可以重跑；
新门仍可能因真实测试失败而红。

**BLOCKED attempt 手动接续：** 先确认文件已成为 current attempt 的 canonical
`AttemptBlocked`，保全 partial commits 和第一笔 seed/red 证据。若需修卡，重新 plan 后先
提交与卡面一致的 IR/TaskValidated，再做必要 seed 预验、sign-off 和显式 successor dispatch。
旧 attempt 的 BLOCKED 保持历史事实；新 attempt 不凭旧控制文件、静默或 PID 猜测获得权限。
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
## selfhost 正常一轮的完整调用序列

下面是 schema 3 本地自举正常成功路径的**真实参数形状**（占位符用尖括号）。它只覆盖 happy path：
任何一步非零都回到「恢复矩阵」，不要顺着往下走。参数细节仍以 `orch <command> --help` 为准。

```text
# ① 开轮（--signed-off 是 legacy 语法位，当前 fail-closed：必须先 plan 再单独 sign-off）
orch round open <ROUND> --purpose "<one line>"

# ② 写任务卡与种子 → git commit → 编译并静态校验 ROUND-IR
orch plan

# ③ 逐卡真跑 oracle 预验（隔离现场落位种子跑门，核对 expected-red 身份）
orch round seed-verified <TASK> --expected-red "<assertion: N failed；compile: error[Edddd]>"

# ④ 计划签核（HITL#1，须用户显式授权或明确委托；note 写真实依据，永不隐式触发）
orch round sign-off --note "<why this plan>"

# ⑤ 本轮 root 本地派发 + 收取（产品也支持 exact `--harness <alias>`；二者互斥）
orch dispatch <TASK> --local
orch await-report <TASK> --timeout-secs <SECS>

# ⑥ 请审查：harness 不在 card/IR；wire role 固定 review
orch wake <HARNESS> --review-for <TASK> --attempt <ATTEMPT> \
  --message-file <PATH> [--deadline-secs <SECS>]

# ⑦ 交付 exact wake 的 generic review
orch review deliver <TASK> <ATTEMPT> --harness <HARNESS> --wake-id <WAKE_ID>

# ⑧ 先干跑证明可裁决（票数由项目 RUNBOOK 执行，Rust 只验事实）
orch verdict <TASK> --attempt <ATTEMPT> --expected-head <BRANCH_SHA> \
  --expected-main <MAIN_SHA> --verdict pass --dry-run

# ⑨ 正常收口：一条命令走完 root PASS → merge → 合后门 → TaskRecorded → 清场
#    seal 自己抓取 main，因此它没有 --expected-main
orch seal <TASK> --attempt <ATTEMPT> --expected-head <BRANCH_SHA>

# ⑩ 全部 task 都有 canonical TaskRecorded 后收轮
orch round close --note "<round summary>"
```

⑧ 与 ⑨ 不是两次裁决：`seal` 在当前 attempt 有 0 条 exact root PASS 时自己创建 PASS，
有 1 条时校验并续跑。所以 ⑧ 只需 `--dry-run` 证明可裁决，不必单独落一次真实 verdict。

### 状态变更命令的必填参数

| 命令 | 必填 positional | 必填 flag |
|---|---|---|
| `round open` | `<ID>` | — |
| `round seed-verified` | `<TASK>` | `--expected-red` |
| `round sign-off` | — | —（`--note` 可选） |
| `round close` | — | —（用 `--force` 时 note 须非空） |
| `plan` | — | — |
| `dispatch` | `<TASK>` | 当前开放 schema 3 必须 exact-XOR `--local` / `--harness <ALIAS>`；legacy flags 的解析兼容不授予调用权限 |
| `await-report` | `<TASK>` | — |
| `wake` | `<HARNESS>` | review 用 `--review-for` + `--attempt`；`--role`/`--reissue` 不可达 |
| `review deliver` | `<TASK> <ATTEMPT>` | `--harness`、`--wake-id`；`--role`/`--agent` 不可达 |
| `verdict` | `<TASK>` | `--attempt`、`--expected-head`、`--expected-main`、`--verdict` |
| `seal` | `<TASK>` | `--attempt`、`--expected-head` |
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
核依赖和授权    读当前 card/IR；由 planner 显式选择下一张卡
schema3派发/收取 orch dispatch <TASK> --local|--harness <ALIAS> ; orch await-report <TASK>
正常在做        root 在 task worktree 实现；不 handshake/nudge/resume
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
