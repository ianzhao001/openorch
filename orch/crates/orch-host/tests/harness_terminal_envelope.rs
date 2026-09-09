#![allow(dead_code)]
//! B295 冻结契约 · 统一终态信封：exit 0 ≠ answered
//!
//! 本文件一旦落位即**永久冻结**（AGENTS.md 铁律 10）。补充测试放 writeSet 内其它落点。
//!
//! ## 判据纪律
//!
//! - wrapper 侧断言必须**真跑 shell**：support 在 scratch 现场内生成受控 stub provider，
//!   按场景吐出「缩进美化终对象 / 单行紧凑终对象 / 零帧 EOF / 有帧无终帧 / 只有进度句」
//!   等形状；种子 spawn 真实 wrapper 并解析其 stdout 与退出码。
//! - Rust 侧断言走真实生产 `pub` 入口，support **不得**自报终态。
//! - 现场落仓内（`orch/target/test-tmp/**`），禁 `/tmp`；`scratch_root` 并行唯一（H190）。
//!
//! ## 这条契约的现场来源（不是假想）
//!
//! r77 开轮前体检：zcode CLI 的 `--json` 终对象是**缩进美化的多行 JSON**，而
//! `wake-zcode-stream.sh` 的终态判据是**逐行** `json.loads` ⇒ 跨行对象没有任何一行能
//! 单独解析，终态永远认不出，稳定 `exit 71`。两次原样复跑完全一致（frames=2，17s/18s），
//! 而上游答卷是完整的。收据：`coordination/rounds/r77/planning/r77-channel-physical.md` §2。
//!
//! ## 负向变异清单（卡面 §5）
//!
//! - M1 zcode wrapper 只认单行 JSON        → `a_pretty_printed_terminal_object_is_recognised`
//! - M2 「stdout 非空」当终态               → `exit_zero_without_a_final_answer_is_empty_not_answered`
//! - M3 `exit 0` 一律判 answered            → 同上（两条互为独立注入，载体相同）
//! - M4 有 activity 即判终态                → `activity_alone_never_terminates_a_wake`
//! - M5 五态加 `_ =>` 兜底                  → `the_five_states_are_a_closed_set`
//! - M6 usage 缺失填 0                      → `a_missing_usage_is_explicit_null_with_a_reason`
//! - M7 Rust 侧映射表漏掉 71                → `the_exit_code_vocabulary_matches_the_wrapper_manifests`
//! - M8 删掉新增公开条目的 `///`            → `every_public_item_added_by_this_card_carries_its_own_doc`
//! - M9 wake.rs 不消费 WRAPPER_EXIT_CODES   → `the_wake_path_consumes_the_single_exit_code_table`
//! - M10 指南不写终态契约                    → `the_guide_documents_the_terminal_contract`
//!
//! ## 退出码为什么改用 manifest 而不是正则扫 `exit`
//!
//! 计划审核实测：三个 wrapper 里 **70/71 全部是动态产生**——
//! `wake-pi-stream.sh:253-257`、`wake-zcode-stream.sh:388-394`、`wake-dsh-stream.sh:506-511`
//! 都是 `code = 7x if … else 7y` 之后 `sys.exit(code)`。字面量并集只有
//! `{0,3,64,65,66,67,72,74}` ⇒ **按字面提取，契约里装不下 71，而 71 正是本卡要修的那个码**。
//! 反过来，用宽松正则扫任意 `exit`，则未来任何合法诊断码新增都会打碎冻结种子（过度冻结）。
//! ⇒ 每个 wrapper 各写一段**机器可读 manifest 注释块**，种子与 Rust 表双向比对。

mod harness_terminal_envelope_support;

use harness_terminal_envelope_support as support;

use orch_host::harness::{self, TerminalState};

const HARNESS_SRC: &str = include_str!("../src/harness.rs");
const WAKE_SRC: &str = include_str!("../src/wake.rs");
const GUIDE_SRC: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

/// 五态是**闭集**：不得有「其余归 X」的兜底。
#[test]
fn the_five_states_are_a_closed_set() {
    let mut names: Vec<&str> = TerminalState::ALL.iter().map(|state| state.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["answered", "canceled", "empty", "failed", "timedOut"],
        "终态必须恰好是这五个"
    );
    for name in &names {
        let parsed = TerminalState::parse(name).expect("已知终态必须可解析");
        assert_eq!(parsed.as_str(), *name, "终态必须逐字往返");
    }
    TerminalState::parse("succeeded").expect_err("未知终态必须拒，不得落进兜底分支");
    TerminalState::parse("").expect_err("空串必须拒");
}

/// zcode 的**缩进美化**终对象必须被识别——这正是今天稳定 exit 71 的根因。
#[test]
fn a_pretty_printed_terminal_object_is_recognised() {
    let root = support::scratch_root("pretty");
    let outcome = support::run_zcode_wrapper(&root, support::Shape::PrettyTerminalObject);
    assert_eq!(
        outcome.exit_code, 0,
        "缩进美化的终对象必须被识别，实际 exit={} stderr={}",
        outcome.exit_code, outcome.stderr
    );
    assert!(
        outcome.stdout.contains("zcode.terminal"),
        "识别后必须吐出 zcode.terminal 帧: {}",
        outcome.stdout
    );
}

/// 单行紧凑形状同样必须成功——不能只修一种形状。
#[test]
fn a_compact_terminal_object_is_still_recognised() {
    let root = support::scratch_root("compact");
    let outcome = support::run_zcode_wrapper(&root, support::Shape::CompactTerminalObject);
    assert_eq!(outcome.exit_code, 0, "单行紧凑终对象必须继续被识别");
    assert!(outcome.stdout.contains("zcode.terminal"));
}

/// 识别终对象后，此前被丢弃的遥测必须一并带出（usage / contextWindow）。
#[test]
fn recognised_terminal_carries_the_provider_telemetry() {
    let root = support::scratch_root("telemetry");
    let outcome = support::run_zcode_wrapper(&root, support::Shape::PrettyTerminalObject);
    let record = support::parse_terminal_frame(&outcome.stdout);
    let usage = record
        .get("usage")
        .expect("终帧必须带 usage（今天因认不出终对象而整块丢失）");
    assert!(
        usage.get("reasoningTokens").is_some(),
        "usage 必须保留 provider 的原始字段: {usage}"
    );
}

/// `exit 0` 而没有稳定最终答卷 ⇒ `empty`，不是 `answered`。
/// 真实反例：r76/B290 的 agy exit 0、只有等待进度句、没有最终 verdict。
#[test]
fn exit_zero_without_a_final_answer_is_empty_not_answered() {
    let root = support::scratch_root("progress_only");
    let record = support::classify(&root, support::Shape::ProgressLinesThenExitZero);
    assert_eq!(
        record.state,
        TerminalState::Empty,
        "只有进度句时必须归 empty，实际 {:?}",
        record.state
    );
    assert!(
        !record.exact_reason.trim().is_empty(),
        "非 answered 终态必须带非空 exactReason"
    );
    assert!(!record.turn_ended, "没有终答就不能声称 turnEnded");
}

/// 零帧 EOF 绝不报 accepted/answered。
#[test]
fn a_zero_frame_eof_never_reports_answered() {
    let root = support::scratch_root("zero_frame");
    let record = support::classify(&root, support::Shape::ZeroFrameEof);
    assert_ne!(record.state, TerminalState::Answered, "零帧 EOF 不得判 answered");
    assert!(!record.exact_reason.trim().is_empty());
    assert!(
        record.final_text_sha256.is_none(),
        "没有最终答卷就不得有 finalTextSha256"
    );
}

/// 工具调用、心跳、日志增长都不构成终态。
#[test]
fn activity_alone_never_terminates_a_wake() {
    let root = support::scratch_root("activity_only");
    let record = support::classify(&root, support::Shape::ToolFramesThenKilled);
    assert_ne!(record.state, TerminalState::Answered, "只有 activity 不得判 answered");
}

/// 超时与取消各有自己的终态，不得都塞进 failed。
#[test]
fn deadline_and_cancel_have_their_own_states() {
    let root = support::scratch_root("deadline");
    let timed_out = support::classify(&root, support::Shape::DeadlineExceeded);
    assert_eq!(timed_out.state, TerminalState::TimedOut);
    let canceled = support::classify(&root, support::Shape::AuthenticatedCancel);
    assert_eq!(canceled.state, TerminalState::Canceled);
}

/// usage 缺失时写显式 `null` + reason，**不伪造数字**。
#[test]
fn a_missing_usage_is_explicit_null_with_a_reason() {
    let root = support::scratch_root("no_usage");
    let record = support::classify(&root, support::Shape::TerminalWithoutUsage);
    assert!(record.usage.is_none(), "缺 usage 必须是 None，不得填 0");
    assert!(
        record
            .usage_absent_reason
            .as_deref()
            .is_some_and(|reason| !reason.trim().is_empty()),
        "缺 usage 必须给出显式原因"
    );
}

/// 退出码词表收成**一份**契约：与三个 wrapper 的 manifest 注释块双向比对。
/// manifest 行形如 `# orch-exit-code: <n> <state> <reason>`，由 wrapper 自己声明，
/// 因此动态产生的 70/71 也能进契约，而未来新增的诊断码不会自动打碎种子。
#[test]
fn the_exit_code_vocabulary_matches_the_wrapper_manifests() {
    let scripts = [
        include_str!("../../../scripts/wake-pi-stream.sh"),
        include_str!("../../../scripts/wake-zcode-stream.sh"),
        include_str!("../../../scripts/wake-dsh-stream.sh"),
    ];
    let mut declared: Vec<i32> = Vec::new();
    for script in scripts {
        for line in script.lines() {
            let Some(rest) = line.trim_start().strip_prefix("# orch-exit-code:") else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let code: i32 = parts
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("manifest 行的退出码必须是整数: {line}"));
            let state = parts.next().unwrap_or_else(|| panic!("manifest 行缺 state: {line}"));
            TerminalState::parse(state)
                .unwrap_or_else(|error| panic!("manifest 行的 state 不在闭集内: {line} ({error})"));
            assert!(
                parts.next().is_some(),
                "manifest 行必须带非空 reason: {line}"
            );
            if !declared.contains(&code) {
                declared.push(code);
            }
        }
    }
    declared.sort_unstable();
    let mut in_table: Vec<i32> = harness::WRAPPER_EXIT_CODES
        .iter()
        .map(|entry| entry.code)
        .collect();
    in_table.sort_unstable();
    assert_eq!(
        declared, in_table,
        "wrapper manifest 与 Rust 侧映射表必须双向严格相等"
    );
    for required in [0, 70, 71, 72] {
        assert!(
            in_table.contains(&required),
            "契约必须覆盖退出码 {required}——71 正是本卡要修的 zcode 终态识别失败码"
        );
    }
}

/// **M9 的真载体（接线）**：唯一的映射表必须被**生产路径**消费，
/// 否则就是「交付了能力但没有调用者」（本仓的 H111 病）。
#[test]
fn the_wake_path_consumes_the_single_exit_code_table() {
    let start = WAKE_SRC.find("fn terminal_record_from_status(").expect("actual terminal mapping entry");
    let end = WAKE_SRC[start..].find("fn validate_unified_channel_terminal_shape(").expect("next terminal validator") + start;
    let production = &WAKE_SRC[start..end];
    assert!(
        production.contains("WRAPPER_EXIT_CODES.iter()"),
        "wake.rs 的生产段必须消费 WRAPPER_EXIT_CODES —— 一份没人读的常量表不算契约"
    );
    let harness_production =
        &HARNESS_SRC[..HARNESS_SRC.find("#[cfg(test)]").unwrap_or(HARNESS_SRC.len())];
    assert_eq!(
        harness_production.matches("pub const WRAPPER_EXIT_CODES").count(),
        1,
        "映射表必须只有一份，不得在 Rust 侧再抄第二张"
    );
}

/// 每个退出码都必须映射到闭集内的终态，且带非空 reason 类别。
#[test]
fn every_mapped_exit_code_resolves_to_a_closed_state() {
    for entry in harness::WRAPPER_EXIT_CODES {
        let state = TerminalState::parse(entry.state)
            .unwrap_or_else(|error| panic!("退出码 {} 映射到未知终态: {error}", entry.code));
        if entry.code == 0 {
            assert_eq!(state, TerminalState::Answered, "0 必须映射到 answered");
        } else {
            assert_ne!(
                state,
                TerminalState::Answered,
                "非零退出码 {} 不得映射到 answered",
                entry.code
            );
        }
        assert!(
            !entry.reason.trim().is_empty(),
            "退出码 {} 缺少 reason 类别",
            entry.code
        );
    }
}

/// 声明 `terminal: native|derived` 的 harness 必须**恰好一条**终态记录。
#[test]
fn a_declared_terminal_capable_harness_emits_exactly_one_terminal() {
    let root = support::scratch_root("one_terminal");
    support::write_descriptor(&root, "alpha", "pi", "derived");
    let records = support::terminal_records_for(&root, "alpha");
    assert_eq!(records.len(), 1, "声明有终态的通道必须恰好落一条，实际 {}", records.len());
}

/// 声明 `terminal: absent` 的 harness 必须**显式记为无机械终态**——
/// 不得沉默（今天 `wake.rs` 对无 controlWakeId 的通道整体跳过），也不得冒充 answered。
#[test]
fn a_declared_terminal_absent_harness_records_an_explicit_absence() {
    let root = support::scratch_root("absent_terminal");
    support::write_descriptor(&root, "alpha", "smartclaw", "absent");
    let records = support::terminal_records_for(&root, "alpha");
    assert_eq!(records.len(), 1, "absent 也要留一条显式记录，不得沉默");
    let record = &records[0];
    assert!(
        record.mechanical_terminal_absent,
        "记录必须显式标注本通道无机械终态"
    );
    assert_ne!(record.state, TerminalState::Answered, "absent 不得冒充 answered");
    assert!(!record.exact_reason.trim().is_empty(), "必须给出原因");
}

/// 指南必须写清终态契约。钉**当前不存在**的专属标记，不得用 `TruncatedNoTerminal` 等旧词凑绿。
#[test]
fn the_guide_documents_the_terminal_contract() {
    for marker in [
        "orch-guide-harness:terminal-envelope",
        "orch-guide-harness:exit-zero-is-not-answered",
        "orch-guide-harness:exit-code-manifest",
    ] {
        assert!(
            GUIDE_SRC.contains(marker),
            "指南必须含专属标记 {marker}（guide --check 只管命令树拓扑，管不到语义漂移）"
        );
    }
    for state in TerminalState::ALL {
        assert!(
            GUIDE_SRC.contains(state.as_str()),
            "指南必须逐个列出终态 {}",
            state.as_str()
        );
    }
}

/// 同 B293/B294：本仓没有 `#![deny(missing_docs)]`（D-01），说明只能由用例扛。
/// **判据放在冻结字节里、逐个点名本卡新增的公开面**——不下放给可编辑的 support，
/// 也不用通用前缀扫描（那认不出公开字段与枚举变体）。
#[test]
fn every_public_item_added_by_this_card_carries_its_own_doc() {
    let lines: Vec<&str> = HARNESS_SRC.lines().collect();
    let mut seen_docs: Vec<String> = Vec::new();
    let mut check = |needle: &str, seen: &mut Vec<String>| {
        let index = lines
            .iter()
            .position(|line| line.trim_start().starts_with(needle))
            .unwrap_or_else(|| panic!("harness.rs 必须交付公开条目: {needle}"));
        let mut cursor = index;
        let mut doc: Option<String> = None;
        while cursor > 0 {
            cursor -= 1;
            let above = lines[cursor].trim_start();
            if above.starts_with("#[") || above.is_empty() {
                continue;
            }
            if let Some(rest) = above.strip_prefix("///") {
                let body = rest.trim();
                if !body.is_empty() {
                    doc = Some(body.to_string());
                }
            }
            break;
        }
        let doc = doc.unwrap_or_else(|| panic!("公开条目缺少非空 /// 说明: {needle}"));
        assert!(!seen.contains(&doc), "占位式重复说明按弱化断言处理: {doc}");
        seen.push(doc);
    };
    for needle in [
        "pub enum TerminalState",
        "pub struct WrapperExitCode",
        "pub const WRAPPER_EXIT_CODES",
        "pub struct TerminalRecord",
    ] {
        check(needle, &mut seen_docs);
    }
    // `TerminalState` 的每个变体、`TerminalRecord` 与 `WrapperExitCode` 的每个公开字段，
    // 逐个向上找说明——通用扫描器认不出这两类，正是 revision 1 的漏洞。
    for (start, label) in [
        ("pub enum TerminalState", "TerminalState 变体"),
        ("pub struct TerminalRecord", "TerminalRecord 字段"),
        ("pub struct WrapperExitCode", "WrapperExitCode 字段"),
    ] {
        let from = HARNESS_SRC.find(start).expect("上一段已断言存在");
        let rest = &HARNESS_SRC[from..];
        let block = &rest[..rest.find("\n}\n").unwrap_or(rest.len())];
        let block_lines: Vec<&str> = block.lines().collect();
        for (index, line) in block_lines.iter().enumerate().skip(1) {
            let trimmed = line.trim_start();
            let is_member = trimmed.starts_with("pub ")
                || trimmed
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase());
            if !is_member || trimmed.starts_with("//") || trimmed.starts_with("#[") {
                continue;
            }
            let mut cursor = index;
            let mut documented = false;
            while cursor > 0 {
                cursor -= 1;
                let above = block_lines[cursor].trim_start();
                if above.starts_with("#[") || above.is_empty() {
                    continue;
                }
                documented = above
                    .strip_prefix("///")
                    .is_some_and(|r| !r.trim().is_empty());
                break;
            }
            assert!(documented, "{label} 必须逐个带非空说明，缺的是: {trimmed}");
        }
    }
}
