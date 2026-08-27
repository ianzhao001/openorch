#![allow(dead_code)]
//! B294 冻结契约 · 统一调用信封（runtime → wrapper），DSH 真正拿到 pin
//!
//! 本文件一旦落位即**永久冻结**（AGENTS.md 铁律 10）。补充测试放 writeSet 内其它落点。
//!
//! ## 判据纪律
//!
//! - **wrapper 侧断言必须真的执行 shell**：support 在 scratch 现场内生成受控 stub provider，
//!   stub 把收到的环境变量**原样写进落盘文件**并自增执行计数，种子读回比对。
//! - **runtime 侧的「拒绝早于 spawn」不能靠纯函数用例证明**（revision 1 的错误：
//!   那类用例里 stub 从未被 spawn，计数恒 0、零信息量）。改为对
//!   `wake.rs` 的 spawn 函数体开窗口，断言**校验位置早于 `Command::spawn`**——
//!   沿用 B265 落位种子 `signed_invocation_binding.rs:180-195` 的既有接线惯用法。
//! - support 只造现场、生成 stub、读回落盘文件，**不得**自报退出码或是否被拒。
//! - 现场落仓内（`orch/target/test-tmp/**`），禁 `/tmp`；`scratch_root` 并行唯一（H190）。
//!
//! ## 兼容分支不是可选项
//!
//! 两份**不可改**的既有测试只给旧变量、不给信封：
//! `dsh_stream_projection.rs`（B266，其 support 只设 `ORCH_DSH_BIN`）与
//! `zcode_stream_projection_contract.rs`（B228）。⇒ wrapper **必须**保留
//! 「信封缺席 ⇒ 走旧变量」的兼容分支；「完整或拒绝」只在**信封存在时**生效。
//! 这条由 `an_absent_envelope_falls_back_to_legacy_variables` 钉死。
//!
//! ## 负向变异清单（卡面 §5）
//!
//! - M1 信封少一个必填键仍构造成功        → `an_incomplete_envelope_is_refused`
//! - M2 校验挪到 `Command::spawn` 之后     → `the_envelope_is_validated_before_spawn`
//! - M3 dsh 候选全落空时回落裸名 `dsh`     → `executable_discovery_is_fail_closed`
//! - M4 不校验 orchBin 可执行              → `an_unbuilt_orch_binary_is_refused`
//! - M5 旧别名覆盖信封                      → `the_envelope_wins_over_legacy_aliases`
//! - M6 `WakeIssued` 不写 digest           → `the_descriptor_digest_is_carried_into_the_wake_payload`
//! - M7 接受相对 cwd / 短 SHA              → `a_relative_cwd_is_refused` / `a_short_fixed_head_is_refused`
//! - M8 删掉新增公开条目的 `///`           → `every_public_item_in_harness_rs_carries_its_own_doc`
//! - M9 指南不写信封契约                    → `the_guide_documents_the_envelope_contract`

mod harness_invocation_envelope_support;

use harness_invocation_envelope_support as support;

use orch_host::harness::{self, InvocationEnvelope};

const HARNESS_SRC: &str = include_str!("../src/harness.rs");
const WAKE_SRC: &str = include_str!("../src/wake.rs");
const GUIDE_SRC: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

fn window<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let from = src
        .find(start)
        .unwrap_or_else(|| panic!("源码里找不到窗口起点: {start}"));
    let rest = &src[from + start.len()..];
    let to = rest.find(end).unwrap_or(rest.len());
    &rest[..to]
}

/// 信封的键集合是一份**契约**，不是各家各写一份。
/// 十六键；**没有 `_PRESET`**——`AgentDefinition` 根本没有 preset 字段
/// （`registry.rs:127-145`），列为必填就永远填不满；preset 属 harness 私有概念，由描述符携带。
/// **有 `_PROVIDER_BIN`**——否则 dsh 的二进制来源仍只能靠手工导出环境变量。
#[test]
fn the_envelope_key_set_is_a_closed_contract() {
    let mut keys = harness::ENVELOPE_KEYS.to_vec();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "ORCH_HARNESS_ACTION_ID",
            "ORCH_HARNESS_ATTEMPT_ID",
            "ORCH_HARNESS_CWD",
            "ORCH_HARNESS_DEADLINE_SECS",
            "ORCH_HARNESS_EFFORT",
            "ORCH_HARNESS_FIXED_HEAD",
            "ORCH_HARNESS_ID",
            "ORCH_HARNESS_MODEL",
            "ORCH_HARNESS_ORCH_BIN",
            "ORCH_HARNESS_PROVIDER",
            "ORCH_HARNESS_PROVIDER_BIN",
            "ORCH_HARNESS_REVIEW_OUTPUT_PATH",
            "ORCH_HARNESS_ROLE",
            "ORCH_HARNESS_ROUND",
            "ORCH_HARNESS_TASK_ID",
            "ORCH_HARNESS_WAKE_ID",
        ],
        "信封键集合必须逐字等于契约"
    );
    assert!(
        !harness::ENVELOPE_KEYS.contains(&"ORCH_HARNESS_PRESET"),
        "_PRESET 不得进必填面：AgentDefinition 没有 preset 字段，填不满"
    );
}

/// 三个受管 wrapper 收到的信封键集合必须**完全相同**——少一个即红。
#[test]
fn all_three_wrappers_receive_the_same_envelope() {
    let root = support::scratch_root("same_envelope");
    let envelope = support::valid_envelope(&root);
    let mut observed: Vec<Vec<String>> = Vec::new();
    for wrapper in support::MANAGED_WRAPPERS {
        observed.push(support::spawn_wrapper_with_stub(&root, wrapper, &envelope));
    }
    let first = observed.first().expect("必须有三个观测结果").clone();
    for (index, seen) in observed.iter().enumerate() {
        assert_eq!(seen, &first, "第 {index} 个 wrapper 收到的信封键集合与其它不一致");
    }
    let mut expected = harness::ENVELOPE_KEYS.to_vec();
    expected.sort_unstable();
    let mut got: Vec<&str> = first.iter().map(String::as_str).collect();
    got.sort_unstable();
    assert_eq!(got, expected, "wrapper 实际收到的键必须等于契约");
}

/// 缺任一必填项 ⇒ 构造失败并点名。
#[test]
fn an_incomplete_envelope_is_refused() {
    let root = support::scratch_root("incomplete");
    for key in harness::ENVELOPE_KEYS.iter().copied() {
        let mut envelope = support::valid_envelope(&root);
        envelope.remove(key);
        let error = match InvocationEnvelope::from_map(envelope) {
            Ok(_) => panic!("缺 {key} 时必须被拒，却构造成功了"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains(key),
            "错误必须点名缺失的键 {key}"
        );
    }
}

/// **M2 的真载体（接线 + 时序）**：校验必须发生在 `Command::spawn` **之前**。
/// revision 1 用「stub 执行计数 == 0」来证明这件事，但那两个用例里 stub 根本没被 spawn，
/// 计数恒 0、证明力为零——这是四家计划审核独立指出的空证明。
#[test]
fn the_envelope_is_validated_before_spawn() {
    let scope = window(WAKE_SRC, "fn spawn_managed_wake_supervisor(", "\nfn ");
    let validate_at = scope.find("InvocationEnvelope::from_map").unwrap_or_else(|| {
        panic!("spawn 路径必须构造并校验信封，否则「完整或拒绝」没有生产承重方")
    });
    let spawn_at = scope
        .find(".spawn()")
        .expect("spawn 路径必须存在 Command::spawn 调用");
    assert!(
        validate_at < spawn_at,
        "信封校验必须早于 Command::spawn —— 晚于 spawn 就等于 provider 已经起来了才拒"
    );
}

/// 空白值等同于缺失——不得用空串占位后照常启动。
#[test]
fn a_blank_envelope_value_is_refused() {
    let root = support::scratch_root("blank_value");
    let mut envelope = support::valid_envelope(&root);
    envelope.insert("ORCH_HARNESS_MODEL".to_string(), "   ".to_string());
    let error = InvocationEnvelope::from_map(envelope).expect_err("空白值必须被拒");
    assert!(format!("{error:#}").contains("ORCH_HARNESS_MODEL"));
}

/// implement 席没有 review 产物落点——用**显式哨兵**表达，不得用空串
/// （空串与「缺失」不可区分，而缺失是必须被拒的）。
#[test]
fn an_implement_role_uses_an_explicit_sentinel_for_the_review_output() {
    let root = support::scratch_root("implement_role");
    let mut envelope = support::valid_envelope(&root);
    envelope.insert("ORCH_HARNESS_ROLE".to_string(), "implement".to_string());
    envelope.insert(
        "ORCH_HARNESS_REVIEW_OUTPUT_PATH".to_string(),
        harness::NO_REVIEW_OUTPUT.to_string(),
    );
    let parsed = InvocationEnvelope::from_map(envelope).expect("implement 席必须能合法构造");
    assert!(
        parsed.review_output_path().is_none(),
        "哨兵必须被解析成 None，而不是一个字面路径"
    );
}

/// cwd 必须绝对。
#[test]
fn a_relative_cwd_is_refused() {
    let root = support::scratch_root("relative_cwd");
    let mut envelope = support::valid_envelope(&root);
    envelope.insert("ORCH_HARNESS_CWD".to_string(), "worktrees/x".to_string());
    let error = InvocationEnvelope::from_map(envelope).expect_err("相对 cwd 必须被拒");
    assert!(format!("{error:#}").contains("ORCH_HARNESS_CWD"));
}

/// fixed HEAD 必须是完整 40 位小写 SHA。
#[test]
fn a_short_fixed_head_is_refused() {
    let root = support::scratch_root("short_head");
    for bad in ["4d2ca5a8", "4D2CA5A800371C67158BDABC0E0FEC10A993EEE7"] {
        let mut envelope = support::valid_envelope(&root);
        envelope.insert("ORCH_HARNESS_FIXED_HEAD".to_string(), bad.to_string());
        let error =
            InvocationEnvelope::from_map(envelope).expect_err("短 SHA / 大写 SHA 必须被拒");
        assert!(format!("{error:#}").contains("ORCH_HARNESS_FIXED_HEAD"));
    }
}

/// DSH 必须从**进程**读到 pin，而不只是签名面上有（H194 的另一半）。
#[test]
fn dsh_reads_its_pin_from_the_process() {
    let root = support::scratch_root("dsh_pin");
    let envelope = support::valid_envelope(&root);
    let seen = support::spawn_wrapper_env(&root, support::DSH_WRAPPER, &envelope);
    for key in [
        "ORCH_HARNESS_PROVIDER",
        "ORCH_HARNESS_MODEL",
        "ORCH_HARNESS_EFFORT",
    ] {
        let value = seen
            .get(key)
            .unwrap_or_else(|| panic!("dsh wrapper 必须在进程环境里看到 {key}"));
        assert_eq!(
            value,
            envelope.get(key).expect("有效信封必须含该键"),
            "{key} 的进程侧取值必须与签名面逐字相同"
        );
    }
}

/// executable discovery 是 fail-closed：候选链只有两级
/// 「信封 `ORCH_HARNESS_PROVIDER_BIN` → 旧的 `ORCH_*_BIN` 兼容别名」。
/// **PATH 不是候选**：裸名回落正是 dsh 今天起不来的机械原因。
#[test]
fn executable_discovery_is_fail_closed() {
    let root = support::scratch_root("discovery");
    let decoy = support::plant_decoy_on_path(&root, "dsh");
    let envelope = support::envelope_without_provider_binary(&root);
    let outcome = support::spawn_wrapper_raw(&root, support::DSH_WRAPPER, &envelope, &decoy);
    assert_ne!(outcome.exit_code, 0, "候选全落空必须非零退出");
    assert_eq!(
        support::stub_execution_count(&root),
        0,
        "拒绝必须发生在 provider 启动之前"
    );
    assert!(
        !outcome.stderr.contains(&decoy.to_string_lossy().to_string()),
        "PATH 上的同名可执行文件不得被当作回落候选"
    );
}

/// runtime 不得把未构建/不可执行的 orch 二进制交给 reviewer（B292 假红的根因）。
#[test]
fn an_unbuilt_orch_binary_is_refused() {
    let root = support::scratch_root("orch_bin");
    let mut envelope = support::valid_envelope(&root);
    envelope.insert(
        "ORCH_HARNESS_ORCH_BIN".to_string(),
        support::nonexistent_path(&root),
    );
    let error =
        InvocationEnvelope::from_map(envelope.clone()).expect_err("orchBin 不存在必须被拒");
    assert!(format!("{error:#}").contains("ORCH_HARNESS_ORCH_BIN"));

    envelope.insert(
        "ORCH_HARNESS_ORCH_BIN".to_string(),
        support::plant_unexecutable_file(&root),
    );
    let error = InvocationEnvelope::from_map(envelope).expect_err("orchBin 不可执行必须被拒");
    assert!(format!("{error:#}").contains("ORCH_HARNESS_ORCH_BIN"));
}

/// 兼容别名是过渡，不是并列的第二条真值来源：冲突时信封赢，且必须披露冲突。
#[test]
fn the_envelope_wins_over_legacy_aliases() {
    let root = support::scratch_root("alias");
    let envelope = support::valid_envelope(&root);
    let outcome = support::spawn_wrapper_with_conflicting_alias(
        &root,
        support::PI_WRAPPER,
        &envelope,
        ("ORCH_PI_MODEL", "stale-model-from-alias"),
    );
    assert_eq!(
        outcome.observed_model.as_deref(),
        envelope.get("ORCH_HARNESS_MODEL").map(String::as_str),
        "冲突时必须以信封为准"
    );
    assert!(
        outcome.stderr.contains("ORCH_PI_MODEL"),
        "冲突必须在诊断行里被披露，而不是静默取胜: {}",
        outcome.stderr
    );
}

/// **信封缺席时必须走旧变量**——`dsh_stream_projection.rs`（B266）与
/// `zcode_stream_projection_contract.rs`（B228）两份不可改测试只给旧变量。
/// 「完整或拒绝」只在信封**存在**时生效。
#[test]
fn an_absent_envelope_falls_back_to_legacy_variables() {
    let root = support::scratch_root("legacy_only");
    let outcome = support::spawn_wrapper_legacy_only(&root, support::DSH_WRAPPER);
    assert_eq!(
        outcome.exit_code, 0,
        "完全没有信封时必须走兼容分支正常启动，否则打红 B266/B228 且不可修: {}",
        outcome.stderr
    );
    assert!(
        support::stub_execution_count(&root) >= 1,
        "兼容分支下 provider 必须真的被启动过"
    );
}

/// 描述符摘要必须进入 wake 的 durable payload——r77 内唯一能事后查出
/// 「轮中偷改描述符」的凭据（IR 签名冻结顺延 D-18）。
#[test]
fn the_descriptor_digest_is_carried_into_the_wake_payload() {
    let root = support::scratch_root("digest_payload");
    support::write_minimal_registry(&root);
    let expected = harness::registry_digest(&root).expect("摘要必须可计算");
    let payload = support::wake_issued_payload(&root);
    let carried = payload
        .get("harnessRegistryDigest")
        .and_then(|value| value.as_str())
        .expect("WakeIssued 必须携带 harnessRegistryDigest");
    assert_eq!(carried, expected, "payload 里的摘要必须与真实摘要逐字相同");
    assert_eq!(carried.len(), 64, "摘要必须是 64 位十六进制");
}

/// 指南必须写清信封契约。钉**当前不存在**的专属标记，不得用旧词凑绿。
#[test]
fn the_guide_documents_the_envelope_contract() {
    for marker in [
        "orch-guide-harness:invocation-envelope",
        "orch-guide-harness:envelope-complete-or-refuse",
        "orch-guide-harness:legacy-alias-precedence",
    ] {
        assert!(
            GUIDE_SRC.contains(marker),
            "指南必须含专属标记 {marker}（guide --check 只管命令树拓扑，管不到语义漂移）"
        );
    }
    for key in harness::ENVELOPE_KEYS.iter().copied() {
        assert!(GUIDE_SRC.contains(key), "指南必须逐个列出信封键 {key}");
    }
}

/// 同 B293：本仓没有 `#![deny(missing_docs)]`（D-01），说明只能由用例扛。
/// **判据放在冻结字节里、逐个点名本卡新增的公开面**——不得下放给可编辑的 support，
/// 也不用通用前缀扫描（那认不出公开字段与枚举变体）。
#[test]
fn every_public_item_added_by_this_card_carries_its_own_doc() {
    let lines: Vec<&str> = HARNESS_SRC.lines().collect();
    let mut seen_docs: Vec<String> = Vec::new();
    for needle in [
        "pub const ENVELOPE_KEYS",
        "pub const NO_REVIEW_OUTPUT",
        "pub struct InvocationEnvelope",
        "pub fn from_map",
        "pub fn review_output_path",
    ] {
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
        assert!(
            !seen_docs.contains(&doc),
            "占位式重复说明按弱化断言处理: {doc}"
        );
        seen_docs.push(doc);
    }
    // `InvocationEnvelope` 的每个公开字段同样承重——**逐字段**向上找说明，
    // 不是「块里存在任意一条 ///」那种看着像覆盖、实际没覆盖的弱断言。
    let block = window(HARNESS_SRC, "pub struct InvocationEnvelope", "\n}\n");
    let block_lines: Vec<&str> = block.lines().collect();
    for (index, line) in block_lines.iter().enumerate() {
        if !line.trim_start().starts_with("pub ") {
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
            documented = above.strip_prefix("///").is_some_and(|r| !r.trim().is_empty());
            break;
        }
        assert!(
            documented,
            "InvocationEnvelope 的公开字段必须逐个带非空说明，缺的是: {}",
            line.trim_start()
        );
    }
}
