//! B174 seed (H37 重做 + H42 修正) — consult 答案提取的三态契约。
//!
//! **本 seed 是对 r57 那份种子的修正。** 上一版断言「能解析成功的 codex 流 + 80 KB 填充
//! 也必须 raw_fallback」（注释写着 *even if it parsed*），唯一能满足它的实现就是量
//! **原始输入**长度。种子冻结且机械受检，于是无声胜出——而卡面写的是「**答案体** > 64 KB」。
//!
//! 后果（r56 活体日志实测，纯算术）：codex 680,969 B 日志 → 3,167 B 干净答案、
//! opencode 273,709 B → 1,453 B。任何真实日志都远超 64 KB ⇒ 两家在生产上**都**被标成
//! raw-fallback、告警恒亮、该字段零信息量。**唯一一直正常的那家成了永久告警源。**
//!
//! 判据只有一条：**量答案体，不量日志。** 日志大是健康常态（推理与工具轨迹），
//! 答案体大才可疑。
//!
//! M1：主判据量 `raw.len()` 而非 `answer.text.len()`
//!     ⇒ `a_large_log_with_a_small_answer_is_structured` 红（**H42 的直接回归**）。
//! M2：把非 JSON provider 的散文也标成 `RawTranscript`
//!     ⇒ `plain_prose_is_plain_text_not_a_failure` 红。
//! M3：把未识别的 JSON 流标成 `PlainText`
//!     ⇒ `an_unrecognised_json_stream_is_a_raw_transcript` 红。

use orch_host::fusion::{extract_member_answer, AnswerExtraction, ANSWER_BODY_LIMIT};

/// codex 形态：`item.completed` + `item.type=="agent_message"` → `item.text`。
fn codex_stream(answer: &str) -> String {
    format!(
        "{}\n{}\n{}\n",
        r#"{"type":"thread.started","thread_id":"t1"}"#,
        r#"{"type":"item.started","item":{"type":"reasoning"}}"#,
        serde_json::json!({
            "type": "item.completed",
            "item": {"type": "agent_message", "text": answer}
        })
    )
}

/// opencode 形态：文本在 `{"type":"text","part":{"text":…}}`。
/// r57 实测其日志里 25 条 `step_finish` **无一带 text**——原实现找错了地方，
/// 于是 427 KB 转录被整份当成答案。
fn opencode_stream(answer: &str) -> String {
    format!(
        "{}\n{}\n{}\n",
        r#"{"type":"step_start","part":{"type":"step-start"}}"#,
        serde_json::json!({"type": "text", "part": {"type": "text", "text": answer}}),
        r#"{"type":"step_finish","part":{"reason":"stop"}}"#
    )
}

/// 模拟真实日志的体量：大量工具调用与推理轨迹，最后才是那句答案。
/// r56 现场 codex 是 680,969 B 日志 / 3,167 B 答案。
fn noisy_log_around(answer: &str) -> String {
    let noise: String = (0..400)
        .map(|i| {
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "item.completed",
                    "item": {"type": "command_execution", "output": "x".repeat(200), "seq": i}
                })
            )
        })
        .collect();
    format!("{noise}{}", codex_stream(answer))
}

#[test]
fn a_large_log_with_a_small_answer_is_structured() {
    // M1 —— **H42 的直接回归，本 seed 存在的理由**。
    // 日志几十万字节、答案三千字节：这是 codex 每一次**正常**运行的样子。
    // 量 raw.len() 的实现会把它标成 raw-transcript，于是告警恒亮、信号退化为噪音。
    let answer = "结论：r58 首件应做 H46 的 merge 授权放宽。理由如下……";
    let raw = noisy_log_around(answer);
    assert!(
        raw.len() > 64 * 1024,
        "夹具本身必须超过 64 KiB，否则测不到 H42"
    );

    let extracted = extract_member_answer(&raw);
    assert_eq!(extracted.text, answer, "必须提取出最终 assistant 文本");
    assert_eq!(
        extracted.extraction,
        AnswerExtraction::Structured,
        "日志大、答案小是健康常态——量的必须是答案体，不是日志"
    );
}

#[test]
fn opencode_text_parts_are_extracted() {
    // r57 实测有效的那处修法：427 KB 原始转录 → 1,453 字节可读答案。
    let answer = "## 排序与理由\n\n1. 先修编排……";
    let extracted = extract_member_answer(&opencode_stream(answer));
    assert_eq!(
        extracted.text, answer,
        "opencode 的文本在 type==\"text\" 的 part.text，不在 step_finish"
    );
    assert_eq!(extracted.extraction, AnswerExtraction::Structured);
}

#[test]
fn plain_prose_is_plain_text_not_a_failure() {
    // M2：agy 吐纯文本、不吐 JSON。逐行解析全失败 ⇒ 回落原文，
    // 而**原文本身就是答案**。这是合法降级，不是故障，不该告警。
    // r57 把它和真正的提取失败共用一个标签，于是 agy 每次运行都报警。
    let prose = "AGY FINAL ANSWER\n\n我建议先做 H46，因为它阻塞其余全部。";
    let extracted = extract_member_answer(prose);
    assert_eq!(extracted.text, prose, "纯文本 provider 的原文即答案");
    assert_eq!(
        extracted.extraction,
        AnswerExtraction::PlainText,
        "非 JSON provider 的散文是合法降级，必须与提取失败区分开"
    );
}

#[test]
fn an_unrecognised_json_stream_is_a_raw_transcript() {
    // M3：**是**结构化流，却没有一条记录被认出来——这才是真正的提取失败。
    // 与 PlainText 的区别在于：首行可解析为带 type 的 JSON，说明本该能解析。
    // r56 那次 427 KB 灌进 judge 正是这一格，而当时 status 仍是 ok、零报警。
    let unknown = opencode_stream("x").replace(r#""type":"text""#, r#""type":"unknown_shape""#);
    let extracted = extract_member_answer(&unknown);
    assert_eq!(
        extracted.extraction,
        AnswerExtraction::RawTranscript,
        "认不出的结构化流必须响亮标记，不得静默当成答案"
    );
}

#[test]
fn an_oversized_answer_body_is_a_raw_transcript() {
    // 答案体本身超限 ⇒ 可疑，无论形状。
    // 注意这与 M1 的区别：那里大的是**日志**，这里大的是**答案**。
    let huge_answer = "y".repeat(ANSWER_BODY_LIMIT + 1);
    let extracted = extract_member_answer(&codex_stream(&huge_answer));
    assert_eq!(
        extracted.extraction,
        AnswerExtraction::RawTranscript,
        "答案体超限必须标记——量的对象是它，不是日志"
    );
}

#[test]
fn a_plain_text_body_over_the_limit_is_also_a_transcript() {
    // 纯文本路径同样受答案体上限约束：500 KB 的「散文」实际上就是一份转录。
    // 否则 PlainText 会变成绕过上限的后门。
    let huge = "z".repeat(ANSWER_BODY_LIMIT + 1);
    let extracted = extract_member_answer(&huge);
    assert_eq!(
        extracted.extraction,
        AnswerExtraction::RawTranscript,
        "超限的纯文本不得借 PlainText 绕过上限"
    );
}

#[test]
fn only_a_raw_transcript_is_loud() {
    // 告警口径：三态里**只有** RawTranscript 该响亮。
    // PlainText 写进 meta 供审计但不告警——这直接兑现 r57 执行者在 REPORT §6.2
    // 提出而当时无权自行修改的那条疑虑（它拒绝偷偷改口径是正确处置）。
    assert!(
        AnswerExtraction::RawTranscript.is_loud(),
        "提取失败必须响亮"
    );
    assert!(
        !AnswerExtraction::PlainText.is_loud(),
        "合法的纯文本降级不得告警，否则 agy 每次运行都报警"
    );
    assert!(!AnswerExtraction::Structured.is_loud());
}
