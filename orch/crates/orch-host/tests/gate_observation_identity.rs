//! 红种子契约 · B297 · 门观测唯一化（telemetry-only，零行为改变）。
//!
//! 预期红：**compile** —— `error[E0432]: unresolved import`，种子 `use` 了
//! `orch_host::gate::{GatePhase, phase_scoped_log_tag, capture_gate_environment_fingerprint,
//! gate_toolchain_digest, gate_environment_digest}`，五者当前都不存在。
//!
//! 为什么这张卡先于一切优化：`gate_log_sinks` 用 `truncate(true)` 打开
//! `<tag>-gate-<name>.log`，而 collect 的 tag 只有 taskId 不含 attemptId
//! （r77/B295 的 A0001 与 A0002 因此共用一个文件），`verdict --dry-run` 与 seal 的
//! root verdict 共用 root tag。**在这之前，"门优化前后各花了多久"在本仓不可回答**——
//! r77 调研第一次用 birth→mtime 估时得出 B295 collect「461 分钟」这种不可能值，
//! 正是被这一型碰撞反向暴露的。
//!
//! 负向变异清单（卡面 §5，逐条注入 → 指名载体红 → 精确逆编辑撤销 → 全绿）：
//!   M1: 日志名去掉 gateRunId          -> two_runs_of_one_gate_keep_two_logs 红
//!   M2: 日志名去掉 attemptId          -> two_attempts_of_one_task_keep_two_logs 红
//!   M3: gate.rs 不再消费 round_scoped_log_tag -> the_round_scoped_tag_is_kept 红
//!   M4: root phase 不落 durationMs    -> all_six_phases_are_distinct_and_closed 红
//!   M5: trial phase 完全不落账        -> the_phase_tag_is_consumed_at_every_call_site 红
//!   M6: 指纹改成编译期常量            -> the_fingerprint_is_computed_from_the_real_toolchain 红
//!   M7: close.rs 的 postmerge 调用点改回旧 tag -> the_phase_tag_is_consumed_at_every_call_site 红
//!   M8: 从 collect attestation 删掉 toolchainDigest/gateRunId -> the_collect_attestation_carries_the_digests 红
//!   M9: 放行判据去掉「状态惰性」 -> a_gate_observation_passes_the_barrier_without_moving_it 红
//!   M10: 放行判据放宽到任意 kind/actor/task -> the_barrier_still_refuses_everything_else 红
//!   M11: 删掉任一新增 pub 条目的 ///   -> every_public_item_added_by_this_card_carries_its_own_doc 红

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use orch_core::EventRecord;
use orch_host::gate::{
    capture_gate_environment_fingerprint, gate_environment_digest, gate_toolchain_digest,
    phase_scoped_log_tag, GatePhase, BUILD_ENV_KEYS,
};
use orch_host::ledger::{classify_barrier_append, BarrierAppendVerdict};

const GATE_SRC: &str = include_str!("../src/gate.rs");
const COLLECT_SRC: &str = include_str!("../src/collect.rs");
const VERIFY_SRC: &str = include_str!("../src/verify.rs");
const CLOSE_SRC: &str = include_str!("../src/close.rs");
const ORACLE_SRC: &str = include_str!("../src/oracle.rs");
const TIERF_SRC: &str = include_str!("../src/tierf.rs");
const GUIDE: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

const ULID_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const ULID_B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("B297: 仓根必须可从 CARGO_MANIFEST_DIR 上溯三级")
        .to_path_buf()
}

/// 取 `needle` 所在行**上方紧邻**的一段 `///` 说明（不含 `///` 前缀）。
/// 找不到条目即 panic —— 那说明公开面根本没交付，比"文档为空"更严重。
fn doc_comment_above(src: &str, needle: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let idx = lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("B297: 源码里找不到公开条目 {needle}"));
    let mut doc = Vec::new();
    for line in lines[..idx].iter().rev() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("///") {
            doc.push(rest.trim().to_string());
            continue;
        }
        if trimmed.starts_with("#[") || trimmed.is_empty() {
            // 属性行与空行不打断说明块；但空行之后若不是 /// 就停。
            if trimmed.is_empty() && !doc.is_empty() {
                break;
            }
            continue;
        }
        break;
    }
    doc.reverse();
    doc.join(" ")
}

fn all_phases() -> [GatePhase; 6] {
    [
        GatePhase::RedReplay,
        GatePhase::Collect,
        GatePhase::Trial,
        GatePhase::Root,
        GatePhase::PostMerge,
        GatePhase::Recovery,
    ]
}

// ── 日志唯一性 ──────────────────────────────────────────────────────────────

#[test]
fn two_runs_of_one_gate_keep_two_logs() {
    let first = phase_scoped_log_tag("r78", "B297", "B297-A0001", GatePhase::Collect, ULID_A);
    let second = phase_scoped_log_tag("r78", "B297", "B297-A0001", GatePhase::Collect, ULID_B);
    assert_ne!(
        first, second,
        "B297 M1: 同一门的两次运行必须落在两个文件里。gate_log_sinks 用 truncate(true)，\
         tag 相同就意味着第二次把第一次的证据抹掉——失败日志尤其没有 CAS 兜底。\
         实得 first={first:?} second={second:?}"
    );
    for tag in [&first, &second] {
        assert!(
            tag.contains(ULID_A) || tag.contains(ULID_B),
            "B297 M1: gateRunId 必须真的出现在 tag 里，实得 {tag:?}"
        );
    }
}

#[test]
fn two_attempts_of_one_task_keep_two_logs() {
    let a0001 = phase_scoped_log_tag("r78", "B297", "B297-A0001", GatePhase::Collect, ULID_A);
    let a0002 = phase_scoped_log_tag("r78", "B297", "B297-A0002", GatePhase::Collect, ULID_A);
    assert_ne!(
        a0001, a0002,
        "B297 M2: r77/B295 的 A0001 与 A0002 共用了 `B295-round-r77-*`，\
         第二个 attempt 的门日志直接盖掉第一个。attemptId 必须进 tag。\
         实得 a0001={a0001:?} a0002={a0002:?}"
    );
    assert!(
        a0002.contains("B297-A0002"),
        "B297 M2: attemptId 必须逐字出现，实得 {a0002:?}"
    );
}

#[test]
fn the_round_scoped_tag_is_kept() {
    // ⚠️ 不能只查"文件里有这个符号"——保留定义但生产不再调用它，存在性断言照样绿。
    // 必须证明它被**新 tag 函数自己**调用。
    let def = GATE_SRC
        .find("fn phase_scoped_log_tag")
        .expect("B297: phase_scoped_log_tag 必须存在");
    let body_end = (def + 2400).min(GATE_SRC.len());
    assert!(
        GATE_SRC[def..body_end].contains("round_scoped_log_tag"),
        "B297 M3: 新 tag 必须**包住** round_scoped_log_tag 而不是取代它——\
         已落位冻结种子 reclaim_and_attribution_dimensions.rs 断言 gate.rs 含该符号（B272 M6）。\
         保留一个没人调用的定义能骗过存在性断言，骗不过这条"
    );
    let tag = phase_scoped_log_tag("r78", "B297", "B297-A0001", GatePhase::Root, ULID_A);
    assert!(
        tag.contains("r78"),
        "B297 M3: 跨轮同名卡会让同一文件名被两轮同时主张（r69 收轮实撞 23 条 CONFLICT）。\
         轮号必须仍在，实得 {tag:?}"
    );
    assert!(
        tag.contains("B297"),
        "B297 M3: 加轮号/阶段不得吞掉 taskId，否则归因反而更难。实得 {tag:?}"
    );
}

#[test]
fn all_six_phases_are_distinct_and_closed() {
    let tags = all_phases()
        .iter()
        .map(|phase| phase_scoped_log_tag("r78", "B297", "B297-A0001", *phase, ULID_A))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        tags.len(),
        6,
        "B297 M4: 六个 phase 必须彼此可区分——今天 root 只有 exit/log SHA、\
         trial 与 postmerge 连逐门 durable 事实都没有，跨设备无法回答\
         「全轮在 root/postmerge 究竟花了多少」。实得 {tags:?}"
    );
}

/// 每一个**门执行调用点**附近都必须出现新 tag。
///
/// ⚠️ 这条**刻意不用文件级 `contains`**：`close.rs` 有四个门调用点（postmerge、
/// record recovery ×2、tip recovery），`collect.rs` 有 trial 段与 collect 段两处。
/// 只要任意一处还在用新 tag，文件级存在性就绿 ⇒ M5（trial 不落账）与
/// M7（postmerge 改回旧 tag）**都不会红**。按调用点逐个开窗口才有判别力。
#[test]
fn the_phase_tag_is_consumed_at_every_gate_call_site() {
    const WINDOW: usize = 1600;
    for (name, src, gate_call) in [
        ("collect.rs", COLLECT_SRC, "run_gate_with_permit_and_identity"),
        ("collect.rs", COLLECT_SRC, "run_trial_gate_with_permit_and_identity"),
        ("verify.rs", VERIFY_SRC, "run_gate_with_audit_identity"),
        ("close.rs", CLOSE_SRC, "run_gate_with_audit_identity"),
    ] {
        let mut naked = Vec::new();
        for (offset, _) in src.match_indices(gate_call) {
            let start = offset.saturating_sub(WINDOW);
            let end = (offset + WINDOW).min(src.len());
            if !src[start..end].contains("phase_scoped_log_tag") {
                naked.push(src[..offset].lines().count() + 1);
            }
        }
        assert!(
            naked.is_empty(),
            "B297 M5/M7: {name} 的 {gate_call} 调用点 {naked:?} 附近没有新 tag ⇒ \
             那几处仍写进会被 truncate 覆盖的旧日志名。文件级 contains 挡不住这一型\
             （H111「交付的能力没人调用」/ B272 M6 同型）"
        );
    }
}

/// 六个 phase 的 `GateExecuted` payload 必须**同形**。
///
/// ⚠️ 这条是 M4（root phase 不落 `durationMs`）的**唯一机械载体**——
/// 只断言「六个 tag 互不相同」是测 `phase_scoped_log_tag` 这个纯函数，
/// 与某个 phase 有没有落某个字段完全无关。
#[test]
fn every_gate_executed_append_carries_the_same_shaped_payload() {
    const WINDOW: usize = 1400;
    const REQUIRED: [&str; 7] = [
        "phase",
        "gateRunId",
        "durationMs",
        "exitCode",
        "subjectTreeSha",
        "toolchainDigest",
        "environmentDigest",
    ];
    let mut sites = 0usize;
    for (name, src) in [
        ("collect.rs", COLLECT_SRC),
        ("verify.rs", VERIFY_SRC),
        ("close.rs", CLOSE_SRC),
        ("oracle.rs", ORACLE_SRC),
    ] {
        for (offset, _) in src.match_indices("\"GateExecuted\"") {
            sites += 1;
            let end = (offset + WINDOW).min(src.len());
            let window = &src[offset..end];
            for key in REQUIRED {
                assert!(
                    window.contains(key),
                    "B297 M4: {name} 第 {} 行的 GateExecuted 落账缺字段 {key:?} ⇒ \
                     六个 phase 记的东西不同形，跨设备就回答不了「全轮在 root/postmerge \
                     究竟花了多少」——那正是本卡存在的唯一理由",
                    src[..offset].lines().count() + 1
                );
            }
        }
    }
    assert!(
        sites >= 4,
        "B297 M5: 只找到 {sites} 处 GateExecuted 落账点。六个 phase 必须都落账；\
         trial 或 postmerge 整个不落账时这条必须红"
    );
}

// ── 工具链与环境指纹 ────────────────────────────────────────────────────────

/// 采集到的 digest 必须**等于**用真实工具链输入手工算出来的 digest。
///
/// ⚠️ 这条取代了原来的「源码里含 `-vV`」。那种写法挡不住"capture 直接返回常量、
/// 同时把 `-vV` 留在注释里、纯 digest helper 照样对输入敏感"——三项断言全绿而
/// 指纹恒定，B298 于是把任何两次门都判成同一工具链。
/// 这里改为**端到端对质**：种子自己跑 `cargo -V` / `rustc -vV`，喂给纯函数，
/// 结果必须与 `capture_gate_environment_fingerprint` 逐字相同。常量实现必然红。
#[test]
fn the_fingerprint_is_computed_from_the_real_toolchain() {
    let root = repo_root();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let out = |program: &str, args: &[&str]| -> String {
        let o = std::process::Command::new(program)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("B297: 无法执行 {program} {args:?}: {e}"));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let cargo_version = out(&cargo, &["-V"]);
    let rustc_vv = out("rustc", &["-vV"]);
    let target_triple = rustc_vv
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or_else(|| panic!("B297: `rustc -vV` 里没有 host: 行"))
        .trim()
        .to_string();
    let cargo_realpath = std::fs::canonicalize(
        which_like(&cargo).unwrap_or_else(|| std::path::PathBuf::from(&cargo)),
    )
    .map(|p| p.display().to_string())
    .unwrap_or_else(|_| cargo.clone());

    let expected =
        gate_toolchain_digest(&cargo_realpath, &cargo_version, &rustc_vv, &target_triple);
    let actual = capture_gate_environment_fingerprint(&root)
        .expect("B297: 同机采集指纹必须成功；采集失败必须是 Err，不得回落成空串");
    assert_eq!(
        actual.toolchain_digest, expected,
        "B297 M6: 现场采集的 toolchain digest 与「用真实 cargo/rustc 输出手工算出的」\
         不一致 ⇒ capture 没有真的去问工具链（常量/env!/缓存）。\
         这会让 B298 把两个不同工具链上的绿判成可采用"
    );
}

/// 会改变构建/测试语义的环境变量必须进 environment digest。
///
/// ⚠️ 现实依据：gate runner **继承整个父进程环境**（`gate.rs` 的 `Command` 没有
/// `env_clear`）。于是 collect 在设了 `RUSTFLAGS=--cfg foo` 的 shell 里全绿、
/// root 在没设的进程里本该编译红——而 OS/arch/sandbox/overlay 四项完全相同。
/// 少了这一维，B298 的七项判据在这条路径上一个字都不成立。
#[test]
fn the_build_environment_is_inside_the_digest() {
    for key in ["RUSTFLAGS", "CARGO_HOME", "RUSTC_WRAPPER", "RUSTC"] {
        assert!(
            BUILD_ENV_KEYS.contains(&key),
            "B297: {key} 不在 BUILD_ENV_KEYS 里 ⇒ 它能改变构建语义却不进指纹"
        );
    }
    let base = |env: &[(String, String)]| {
        gate_environment_digest("Darwin 25.5.0", "arm64", "none", None, env)
    };
    let empty: Vec<(String, String)> = Vec::new();
    let with_flags = vec![("RUSTFLAGS".to_string(), "--cfg collect_green".to_string())];
    let other_flags = vec![("RUSTFLAGS".to_string(), "--cfg something_else".to_string())];
    assert_ne!(
        base(&empty),
        base(&with_flags),
        "B297: 设了 RUSTFLAGS 与没设必须给出不同 digest"
    );
    assert_ne!(
        base(&with_flags),
        base(&other_flags),
        "B297: RUSTFLAGS 取值不同必须给出不同 digest"
    );
}

/// 在 PATH 上定位一个裸程序名；已是绝对路径时原样返回。
fn which_like(program: &str) -> Option<std::path::PathBuf> {
    let candidate = std::path::Path::new(program);
    if candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|dir| {
            let full = dir.join(program);
            full.is_file().then_some(full)
        })
    })
}

#[test]
fn the_fingerprint_is_stable_across_two_captures() {
    let root = repo_root();
    let first = capture_gate_environment_fingerprint(&root)
        .expect("B297: 同机采集指纹必须成功；采集失败必须是 Err，不得回落成空串");
    let second = capture_gate_environment_fingerprint(&root)
        .expect("B297: 第二次采集同样必须成功");
    assert_eq!(
        first.toolchain_digest, second.toolchain_digest,
        "B297: 同一机器同一工具链的两次采集必须逐字相等，否则 B298 永远命不中"
    );
    assert_eq!(
        first.environment_digest, second.environment_digest,
        "B297: environment digest 必须稳定，不得混入时间戳/随机数/pid"
    );
    for digest in [&first.toolchain_digest, &first.environment_digest] {
        assert_eq!(
            digest.len(),
            64,
            "B297: digest 必须是 SHA-256 十六进制，实得 {digest:?}"
        );
        assert!(
            digest.chars().all(|c| c.is_ascii_hexdigit()),
            "B297: digest 必须全为十六进制字符，实得 {digest:?}"
        );
    }
}

#[test]
fn a_changed_toolchain_changes_the_digest() {
    let base = gate_toolchain_digest(
        "/opt/cargo/bin/cargo",
        "cargo 1.90.0 (abcdef 2026-01-01)",
        "rustc 1.90.0 (abcdef 2026-01-01)",
        "aarch64-apple-darwin",
    );
    for (label, changed) in [
        (
            "cargo realpath",
            gate_toolchain_digest(
                "/usr/local/bin/cargo",
                "cargo 1.90.0 (abcdef 2026-01-01)",
                "rustc 1.90.0 (abcdef 2026-01-01)",
                "aarch64-apple-darwin",
            ),
        ),
        (
            "cargo version",
            gate_toolchain_digest(
                "/opt/cargo/bin/cargo",
                "cargo 1.91.0 (fedcba 2026-02-02)",
                "rustc 1.90.0 (abcdef 2026-01-01)",
                "aarch64-apple-darwin",
            ),
        ),
        (
            "rustc -vV",
            gate_toolchain_digest(
                "/opt/cargo/bin/cargo",
                "cargo 1.90.0 (abcdef 2026-01-01)",
                "rustc 1.91.0 (fedcba 2026-02-02)",
                "aarch64-apple-darwin",
            ),
        ),
        (
            "target triple",
            gate_toolchain_digest(
                "/opt/cargo/bin/cargo",
                "cargo 1.90.0 (abcdef 2026-01-01)",
                "rustc 1.90.0 (abcdef 2026-01-01)",
                "x86_64-apple-darwin",
            ),
        ),
    ] {
        assert_ne!(
            base, changed,
            "B297 M6: {label} 变了 digest 必须变——否则 B298 会把一份在别的工具链上\
             产生的绿当成本次的绿"
        );
    }
}

#[test]
fn a_changed_environment_changes_the_digest() {
    let base = gate_environment_digest("Darwin 25.5.0", "arm64", "none", None, &[]);
    for (label, changed) in [
        (
            "os build",
            gate_environment_digest("Darwin 25.6.0", "arm64", "none", None, &[]),
        ),
        (
            "arch",
            gate_environment_digest("Darwin 25.5.0", "x86_64", "none", None, &[]),
        ),
        (
            "sandbox class",
            gate_environment_digest("Darwin 25.5.0", "arm64", "codex-workspace", None, &[]),
        ),
        (
            "machine overlay",
            gate_environment_digest("Darwin 25.5.0", "arm64", "none", Some("deadbeef"), &[]),
        ),
    ] {
        assert_ne!(
            base, changed,
            "B297: {label} 变了 environment digest 必须变。H69 的 Unix socket EPERM 在\
             Codex sandbox 内稳定复现、在非 sandbox 立即绿——sandbox class 不进 digest，\
             就等于允许把沙箱里的结论搬到沙箱外用"
        );
    }
}

/// 两个 digest 与 gateRunId 必须真的进 **collect attestation 的生产序列化**。
///
/// ⚠️ 没有这一条，本卡就是一张自说自话的 telemetry 卡：
/// `CollectReceiptAttestation` / `CollectAttestedGate` 都带 `deny_unknown_fields`，
/// 不改结构体定义就加不进字段。而 B298 的判据第 4 项要从 collect receipt 读这两个 digest
/// ——读不到 ⇒ 判据永不成立 ⇒ root **必然真跑** ⇒ B298 一分钱省不到，
/// 且纯函数种子永远绿、**没有任何断言会红**。
/// 这正是 RUNBOOK 坑 2 / H111 说的那型：接线看着完成而实际不通。
#[test]
fn the_collect_attestation_carries_the_digests() {
    let attestation = struct_body(TIERF_SRC, "struct CollectReceiptAttestation");
    for field in ["toolchain_digest", "environment_digest"] {
        assert!(
            attestation.contains(field),
            "B297 M8: CollectReceiptAttestation 缺 {field} ⇒ B298 的判据第 4 项\
             永远读不到值，root 必然真跑，B298 的收益直接归零"
        );
    }
    let gate = struct_body(TIERF_SRC, "struct CollectAttestedGate");
    assert!(
        gate.contains("gate_run_id"),
        "B297 M8: CollectAttestedGate 缺 gate_run_id ⇒ 采用时无法追回\
         「采用的是哪一次门运行」，`adoptedFrom` 只能指向事件而非具体 run"
    );
}

/// 截出一个结构体定义的源码窗口（从 `struct <name>` 到第一个右花括号行）。
fn struct_body(src: &'static str, signature: &str) -> &'static str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("B297: 源码中找不到 {signature:?}"));
    let rest = &src[start..];
    let end = rest.find("\n}").map(|offset| offset + 2).unwrap_or(rest.len());
    &rest[..end]
}

// ── 屏障期的观测放行（A0001 实撞后新增）────────────────────────────────────

fn event(kind: &str, actor: &str, task: &str, round: &str) -> EventRecord {
    EventRecord {
        event_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
        ts: "2026-08-22T16:00:00Z".to_string(),
        actor: actor.to_string(),
        kind: kind.to_string(),
        task_id: Some(task.to_string()),
        round: Some(round.to_string()),
        payload: None,
        extra: Default::default(),
    }
}

/// 纯观测事实必须能穿过 active 屏障，**且绝不改变屏障状态**。
///
/// ⚠️ 这条是 B297-A0001 BLOCKED 的直接产物：postmerge 与 recovery 的门跑在
/// `MergeStarted` 屏障 active 期间，而屏障原本只放行
/// canonical MergeExecuted / merge escalation / TaskRecorded ⇒ 这两个 phase 的
/// 耗时与红/绿**永远没有 durable 记录**，而"红门的耗时无处可查"正是本卡要解决的问题。
///
/// 放行的安全依据：屏障保护的是 merge 生命周期的**状态转换**（谁能推进/闭合它），
/// 而 `GateExecuted` 在 `orch-core` 里只出现在事件目录中、**不参与任何 TaskState 投影**。
#[test]
fn a_gate_observation_passes_the_barrier_without_moving_it() {
    for merge_executed in [false, true] {
        let verdict = classify_barrier_append(
            &event("GateExecuted", "runtime:orch", "B297", "r78"),
            "B297",
            "r78",
            merge_executed,
        );
        assert!(
            matches!(verdict, BarrierAppendVerdict::TransparentObservation),
            "B297 M9: 同 task/round 的 runtime GateExecuted 必须以\
             **状态惰性**方式通过（merge_executed={merge_executed}），实得 {verdict:?}"
        );
    }
}

/// 屏障对其它一切的拒绝**逐字不变**。
#[test]
fn the_barrier_still_refuses_everything_else() {
    let cases = [
        ("别的 kind", event("ReviewDelivered", "runtime:orch", "B297", "r78")),
        ("非 runtime actor", event("GateExecuted", "executor-desktop", "B297", "r78")),
        ("跨 task", event("GateExecuted", "runtime:orch", "B298", "r78")),
        ("跨 round", event("GateExecuted", "runtime:orch", "B297", "r77")),
    ];
    for (label, candidate) in cases {
        let verdict = classify_barrier_append(&candidate, "B297", "r78", false);
        match verdict {
            BarrierAppendVerdict::Refused { reason } => assert!(
                !reason.trim().is_empty(),
                "B297 M10: {label} 被拒时必须给出确切原因"
            ),
            other => panic!(
                "B297 M10: {label} 必须被拒绝——放行判据只针对同 task/round 的 runtime\
                 GateExecuted 这一个 kind，放宽它等于把屏障变成摆设。实得 {other:?}"
            ),
        }
    }
    // 生命周期事件仍走原来的臂，本卡不得改动它们
    assert!(
        matches!(
            classify_barrier_append(
                &event("MergeExecuted", "runtime:orch", "B297", "r78"),
                "B297",
                "r78",
                false
            ),
            BarrierAppendVerdict::LifecycleAdvance
        ),
        "B297: MergeExecuted 仍必须是推进屏障的生命周期臂"
    );
}

// ── 文档与指南 ──────────────────────────────────────────────────────────────

#[test]
fn every_public_item_added_by_this_card_carries_its_own_doc() {
    for item in [
        "pub fn phase_scoped_log_tag",
        "pub fn capture_gate_environment_fingerprint",
        "pub fn gate_toolchain_digest",
        "pub fn gate_environment_digest",
        "pub enum GatePhase",
        "pub struct GateEnvironmentFingerprint",
    ] {
        let doc = doc_comment_above(GATE_SRC, item);
        assert!(
            !doc.trim().is_empty(),
            "B297 M8: {item} 缺 ///（PROTOCOL 铁律 12）。\
             注意本仓的 #![deny(missing_docs)] 从未落地（D-01），编译器不会替你发现"
        );
        assert!(
            doc.chars().count() >= 24,
            "B297 M8: {item} 的说明只有 {} 个字符，属复述条目名。\
             说明要写「为什么存在、调用者依赖它的哪个性质、什么情况下它会拒绝」，\
             实得 {doc:?}",
            doc.chars().count()
        );
    }
}

#[test]
fn the_guide_documents_the_phase_observation_contract() {
    for marker in [
        "gateRunId",
        "phase-scoped gate log",
        "toolchainDigest",
    ] {
        assert!(
            GUIDE.contains(marker),
            "B297: AI-MECHANICAL-GUIDE.md 缺标记 {marker:?}。\
             改门的可观测契约必须同卡更新指南（AGENTS.md 文档维护约定）"
        );
    }
}
