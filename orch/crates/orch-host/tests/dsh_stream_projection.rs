#![allow(dead_code)]
//! B266 seeded-red contract：DeepSeek Harness 审查通道一期——
//! `orch/scripts/wake-dsh-stream.sh` 的投影契约（用户 2026-08-14 追加目标）。
//!
//! Expected red: **compile**。本文件顶部声明的 `support_dsh` 模块尚不存在
//! （目标 `orch/crates/orch-host/tests/support_dsh/mod.rs`），rustc 报
//! `error[E0583]: file not found for module `support_dsh``。
//! 沿用 B195 `wake_multica_contract.rs` 的先例形态。
//! **不得**以建空壳、改本文件、或把本测试排除出门的方式伪造红绿。
//!
//! ── 本卡的事实基础全部来自 planner 的真实 POC（r71 Q2 裁决书）──
//!
//! 安装：`@deepseek-ai/dsh@0.1.0-rc.6`，lockfile + `npm ci`（588 包 / 199 个 `@deepseek-ai/*`），
//! 顶层 integrity 与调研包 `sources.yaml` 记录逐字节一致。实测结论：
//!
//! 1. **DSH 产出 harness 自有的 completed tool 输出**：一次四工具任务后，随机口令
//!    （只写在 fixture 的 `AGENTS.md` 里）的命中分布为
//!    `tool/result` **1** / `tool/call` 1 / `assistant/message` 3 / `assistant/chunk` 5
//!    ⇒ 口令确由 `read` 工具**真读**得到，不是模型复述。
//!    `tool/result` 精确形状：`data.message.content[].content[].text`，同层带
//!    `isError` 与 `toolCallId`。
//! 2. **增量刷盘已实测**（claw 标的「选型生死项」）：任务 `echo → sleep 45 → echo`，
//!    运行途中每 10 秒采样同一会话文件——**t+10 s（sleep 仍在跑）已有 59 条完整记录、
//!    `tool/result`=1、marker 命中 8 次**；终态 102 条记录、`tool/result`=3、`turn/end`=1、
//!    `exit=0`、`stderr` 0 字节。⇒ 投影可在会话进行中成立。
//! 3. **多 frame zstd 截断安全**：25/50/75/90/100% 截断分别解出 7/16/85/108/137 行
//!    **完整**记录，末行永远可解析，从不产生半条 JSON。
//! 4. 模型三元组运行期生效：`request/context` =
//!    `{"provider":"opencode-go","model":"deepseek-v4-pro","contextWindow":1000000}`。
//!
//! ── 三个必须被本种子钉死的「假成功陷阱」（planner 实测踩到）──
//!
//! T1. `zstd -d` 对**截断文件不报错**（exit 0），只静默停在最后一个完整帧
//!     ⇒ **「解码成功」不能当「流已完整」**；完整性只能由 `turn/end` 判定。
//! T2. Node 内置 `zstdDecompressSync` **只解首帧**：同一 43154 B 文件用它只得 268 B
//!     （仅 `session` 头一条），用 `zstd -d` CLI 得 65389 B
//!     ⇒ 解码器选错会让 wrapper **静默只看到会话头**，表现得像「对端什么都没干」。
//! T3. 用户自有的 `dsh web` daemon 与 orch **共享同一个 `$DSH_HOME`**
//!     ⇒ **绝不能用「最新 session 文件」定位本次调用**，必须 cwd-slug + session id 精确绑定。
//!     这与 r70/H165「时间邻近不是身份」同族。
//!
//! ── 忠实翻译 vs 伪造的判据（Q2 裁决 §1，claw 提出、planner 采纳）──
//!
//! 只投 `tool/result`，**不投 `tool/call`**（契约措辞是 harness-owned **completed**-tool field）；
//! 绝不投 `assistant/*`（`wake.rs` 明写 assistant 正文永远不是证据）；
//! 绝不投成 `item.completed/command_execution`（那会冒领 Codex managed receipt 类别）。
//!
//! ── Negative mutations that must turn the named case red ──
//!
//! M1. wrapper 改用只解首帧的解码器（T2 回归）
//!     -> `a_tool_result_in_a_later_frame_is_still_projected` 红。
//! M2. wrapper 把 `assistant/*` 或 prompt 正文也投出去
//!     -> `neither_prompt_nor_assistant_text_reaches_stdout` 红。
//! M3. wrapper 把 `tool/call` 也投成 `tool_result`（放宽 completed-tool 契约）
//!     -> `only_completed_tool_results_are_projected` 红。
//! M4. **接线变异**：投影帧形状偏离 `probe_challenge_observed` 认得的文法
//!     -> `the_projected_stream_satisfies_the_review_consumption_probe` 红。
//! M5. wrapper 在 provider 非零退出时兜底为 0（确定性假成功）
//!     -> `provider_exit_code_is_propagated_not_laundered` 红。
//! M6. wrapper 用「最新 session 文件」定位本次调用（T3 回归）
//!     -> `an_ambient_neighbour_session_is_never_selected` 红。
//! M7. 超大工具输出不做有界截断或不标 `truncated`
//!     -> `oversized_tool_output_is_bounded_and_marked` 红。
//! M8. wrapper 把「解码成功」当「流已完整」，缺 `turn/end` 仍报 exit 0（T1 回归）
//!     -> `a_truncated_stream_without_turn_end_is_not_reported_as_complete` 红。
//! M9. wrapper 忽略消息里的 `WORKTREE=` 行，退回主仓根或某个 env 缺省
//!     -> `the_review_site_is_derived_from_the_worktree_line_in_the_message` 红。

mod support_dsh;

use std::path::{Path, PathBuf};

use orch_host::wake::{probe_challenge_observed, ReviewProbeSource};
use support_dsh::{run_wake_dsh, DshSession, FakeDsh};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn script_path() -> PathBuf {
    repo_root().join("orch/scripts/wake-dsh-stream.sh")
}

/// 现场由**探针消息正文的 `WORKTREE=` 行**推导，不靠 env——
/// 运行时的 `spawn_registered_wake` 只把 cwd 设为主仓根（`wake.rs:16995` 传的是 `root`），
/// 现场只以文本行进消息（`review_probe_message_body`）。
#[test]
fn the_review_site_is_derived_from_the_worktree_line_in_the_message() {
    let fake = FakeDsh::new("b266-cwd").with_session(DshSession::minimal());
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    assert_eq!(
        out.observed_cwd.as_deref(),
        Some(fake.site_path().to_str().expect("site path utf8")),
        "B266: wrapper 必须把 cwd 设为消息 WORKTREE= 行给出的审查现场；实际 stderr={}",
        out.stderr
    );
}

/// T2 回归：`tool/result` 落在**第二个及以后**的 zstd frame 里也必须被投影。
#[test]
fn a_tool_result_in_a_later_frame_is_still_projected() {
    let fake = FakeDsh::new("b266-multiframe")
        .with_session(DshSession::with_tool_result_in_frame(3, "B266-LATER-FRAME"));
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    assert!(
        out.stdout.contains("B266-LATER-FRAME"),
        "B266 M1: 只解首帧的解码器会静默只看到会话头——必须解全部 frame。stdout={}",
        out.stdout
    );
}

/// T1 回归：解码成功 ≠ 流已完整。缺 `turn/end` 时不得报成功。
#[test]
fn a_truncated_stream_without_turn_end_is_not_reported_as_complete() {
    let fake = FakeDsh::new("b266-truncated").with_session(DshSession::truncated_without_turn_end());
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    assert_ne!(
        out.code, 0,
        "B266 T1: zstd -d 对截断不报错，wrapper 不得据此宣称成功；stderr={}",
        out.stderr
    );
    assert!(
        out.stderr.contains("terminal=no"),
        "B266: 诊断行必须显式声明未观测到终帧；stderr={}",
        out.stderr
    );
}

/// prompt 与 assistant 正文永不进 stdout（M2）。
#[test]
fn neither_prompt_nor_assistant_text_reaches_stdout() {
    let fake = FakeDsh::new("b266-leak").with_session(DshSession::with_secrets(
        "B266-PROMPT-SECRET",
        "B266-ASSISTANT-SECRET",
    ));
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    assert!(
        !out.stdout.contains("B266-PROMPT-SECRET"),
        "B266 M2: prompt 正文绝不进 wake 日志"
    );
    assert!(
        !out.stdout.contains("B266-ASSISTANT-SECRET"),
        "B266 M2: assistant 正文绝不是证据，也绝不投影"
    );
}

/// 只投 completed 工具输出，`tool/call` 不算（M3）。
#[test]
fn only_completed_tool_results_are_projected() {
    let fake = FakeDsh::new("b266-callonly").with_session(DshSession::tool_call_without_result(
        "B266-CALL-ONLY",
    ));
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    assert!(
        !out.stdout.contains("B266-CALL-ONLY"),
        "B266 M3: 契约措辞是 harness-owned **completed**-tool field；\
         投 tool/call 等于放宽契约。stdout={}",
        out.stdout
    );
}

/// **接线变异（M4）**：投影出来的流必须真的能被生产判据消费。
#[test]
fn the_projected_stream_satisfies_the_review_consumption_probe() {
    let challenge = "B266-CHALLENGE-TOKEN";
    let fake =
        FakeDsh::new("b266-probe").with_session(DshSession::with_tool_result_text(challenge));
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    assert_eq!(
        probe_challenge_observed(&out.stdout, challenge),
        Some(ReviewProbeSource::ClawToolOutput),
        "B266 M4: 投影只有被 probe_challenge_observed 认下才承重——\
         帧形状对不上就等于这条通道的消费证明根本不成立。stdout={}",
        out.stdout
    );
}

/// 非零退出不得被兜底洗成成功（M5）。
#[test]
fn provider_exit_code_is_propagated_not_laundered() {
    let fake = FakeDsh::new("b266-exit").with_session(DshSession::minimal()).with_exit_code(23);
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    assert_eq!(
        out.code, 23,
        "B266 M5: provider 退出码必须原样传播；确定性假成功是本仓反复咬人的形态。stderr={}",
        out.stderr
    );
}

/// T3 回归：同一 `$DSH_HOME` 下存在他人会话时，绝不能取「最新文件」（M6）。
#[test]
fn an_ambient_neighbour_session_is_never_selected() {
    let fake = FakeDsh::new("b266-ambient")
        .with_session(DshSession::with_tool_result_text("B266-MINE"))
        .with_neighbour_session_written_later("B266-NEIGHBOUR");
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    assert!(
        out.stdout.contains("B266-MINE"),
        "B266: 必须投影本次调用自己的会话。stdout={}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("B266-NEIGHBOUR"),
        "B266 M6: 用户自有的 dsh web daemon 与 orch 共享同一个 $DSH_HOME——\
         「最新文件」不是身份，必须 cwd-slug + session id 精确绑定"
    );
}

/// 超大工具输出必须有界并显式标注截断（M7）。
#[test]
fn oversized_tool_output_is_bounded_and_marked() {
    let fake = FakeDsh::new("b266-bounded")
        .with_session(DshSession::with_tool_result_text(&"X".repeat(200 * 1024)));
    let out = run_wake_dsh(&script_path(), &fake, &fake.message_with_worktree_line());

    let longest = out
        .stdout
        .lines()
        .map(|line| line.len())
        .max()
        .unwrap_or_default();
    assert!(
        longest <= 64 * 1024,
        "B266 M7: 单帧必须有界（≤64 KiB），实际最长 {longest}"
    );
    assert!(
        out.stdout.contains("\"truncated\":true") || out.stdout.contains("\"truncated\": true"),
        "B266 M7: 截断必须显式标注，不得静默丢弃。stdout={}",
        out.stdout
    );
}
