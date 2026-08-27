//! B283 · Pi/ZCode review wake 纳入 managed custody + 绑定显式 provider/model/effort —— 冻结契约种子。
//!
//! **首红：compile `error[E0583]: file for module `legacy_review_managed_custody_support` not found`。**
//! support 只是零判别力管道 marker（`pub fn contract_loaded() {}`），落点是目录形态
//! `tests/legacy_review_managed_custody_support/mod.rs`，不新增 Cargo target。
//!
//! # 本卡的机械强度来自哪里（必须读懂再审）
//!
//! 本种子钉的是**两件独立的事**，缺一不可：
//!
//! 1. **custody**：pi/zcode 的 rendered argv 必须被判别器认成受托管 topology。
//!    当前它们落到兜底分支 ⇒ `provider_kind = None` ⇒ `backendState="legacy-untracked"`
//!    ⇒ 没有 supervisor、没有 `controlWakeId`、没有可对账 terminal。
//! 2. **signed 路由**：注册表必须能表达 `provider`，且 signed 的 provider/model/effort 必须
//!    **逐字段**到达 wrapper；缺失时运行时不得编造、wrapper 不得回落缺省。
//!
//! ⚠️ **判据是合取**：只做 ①，模型还是错的（r73/B279 就是模型错但进程也没托管）；
//! 只做 ②，进程照样在 launcher 返回后被清理。两半都绿才算修好。
//!
//! # ⚠️ 诚实边界（写死在这里）
//!
//! - **ZCode 侧不承诺 inject。** ZCode CLI 没有 `--model` / `--thought-level` 旗标，模型只能由
//!   仓外 `~/.zcode/cli/config.json` 提供。本种子对 zcode 只断言 **verify + fail-closed**：
//!   wrapper 必须在启动 zcode **之前**把 signed 值与 config 比对，不一致即退出。
//!   任何把 zcode 写成「已实现显式路由」的实现都是虚报。
//! - **接线断言故意读源码，不自调。** `render_invocation` 自己调自己永远能证明「有调用者」，
//!   那是审查的结构性盲区。「谁在启动路径上消费判别器」「wrapper 还有没有缺省回落」这两件事
//!   只能靠读真实源码来判，见 `the_launch_path_consults_the_topology_discriminator`
//!   与两条 wrapper 断言。
//! - **不判审查质量**，也不证明 pi/zcode 能交出好 review。它只保证「跑了就留得下痕迹、
//!   模型是签名指定的那个」。
//!
//! # 合成 provider：不调模型、不联网
//!
//! 全部行为断言只用 `/bin/sh -c …` 合成 provider（`process_group(0)`），沿用
//! `tests/wake_supervisor.rs` 已有的 `supervise_managed_child` / `WakeSupervisorPolicy` 骨架。
//! 本文件内不得出现任何真实 provider 二进制名（`pi`/`zcode` 只作为**路径字符串**出现在
//! argv 判别断言里，不被执行）或网络地址。
//!
//! # M 变异 ↔ 载体 一一对应
//!
//! | M | 注入 | 必红的载体 |
//! |---|---|---|
//! | M1 | 判别器退回按 argv[0] basename 白名单 | `pi_wrapper_argv_resolves_to_managed_custody` + `zcode_wrapper_argv_resolves_to_managed_custody` |
//! | M2 | launcher 返回即 kill 进程树 | `launcher_return_does_not_kill_the_provider_before_terminal` |
//! | M3 | 丢掉 terminal 的 exact exit/signal | `exact_terminal_facts_survive_a_stubborn_provider` |
//! | M4 | 只杀 leader、不收子树 | `exact_terminal_facts_survive_a_stubborn_provider` |
//! | M5 | offset 用字符数推进 | `the_pi_wrapper_advances_stream_offsets_by_bytes` |
//! | M6 | 把 `wake-multica.sh` 一并改坏 | `multica_topology_is_unchanged` |
//! | M7 | 把 `or "deepseek"` / `or "deepseek-v4-pro"` 缺省加回来 | `the_pi_wrapper_has_no_provider_or_model_fallback` |
//! | M8 | zcode wrapper 在 signed 与 config 不一致时照常启动 | `the_zcode_wrapper_verifies_the_signed_pin_before_launch` |
//! | M9 | 为了让 pi/zcode 过，把未知 provider 的兜底改成宽松放行 | `an_unregistered_provider_still_fails_closed` |
//! | M10 | 注册表加 `provider` 时顺手去掉 `deny_unknown_fields` | `an_unknown_registry_field_is_still_rejected` |
//! | M11 | 运行时在没有 signed pin 时替 wrapper 编一个 | `a_legacy_invocation_without_a_pin_does_not_invent_one` |
//!
//! 每条断言写成一个独立 `#[test]`，失败点可单独定位（RUNBOOK 坑 16）。

#![allow(dead_code)]

mod legacy_review_managed_custody_support;

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use orch_host::registry::{load_agent_definitions, render_invocation, signed_invocation_binding};
use orch_host::tierf::TerminationSignal;
use orch_host::wake::{
    backend_receipt_kind_from_argv, durable_identity_kind, supervise_managed_child,
    WakeSupervisorPolicy,
};

// ---------------------------------------------------------------- 公共工具

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn read_repo(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("读 {rel} 失败: {error}"))
}

/// 去掉 `#` 注释行后的源码。防止「把断言要求的字符串写进注释」这类假绿。
fn code_lines(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

fn temp_root(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "orch-b283-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

fn policy(natural_exit_grace_ms: u64, term_grace_ms: u64) -> WakeSupervisorPolicy {
    WakeSupervisorPolicy {
        natural_exit_grace_ms,
        term_grace_ms,
        poll_interval_ms: 5,
    }
}

fn spawn_logged(root: &Path, body: &str) -> (Child, PathBuf) {
    let log_path = root.join("wake.jsonl");
    let stdout = File::create(&log_path).unwrap();
    let stderr = stdout.try_clone().unwrap();
    let child = Command::new("/bin/sh")
        .args(["-c", body])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn()
        .unwrap();
    (child, log_path)
}

/// 写一个只含单个 legacy agent 的最小 fixture 注册表，返回可传给
/// `load_agent_definitions` 的 root。
///
/// 用 fixture 而不是仓内真实 `coordination/agents.yaml`：真实注册表里 pi/zcode 的
/// `provider`/`model` 声明由 planner 在本卡 merge **之后**才提交（卡面 §1.3），
/// 种子不能依赖一个本轮故意不存在的东西。
fn fixture_registry(root: &Path, agent_body: &str) {
    let dir = root.join("coordination");
    fs::create_dir_all(&dir).unwrap();
    let text = format!(
        "apiVersion: orch/v1alpha1\nkind: AgentRegistry\nagents:\n  executor-fixture:\n{agent_body}"
    );
    fs::write(dir.join("agents.yaml"), text).unwrap();
}

const FIXTURE_WAKE: &str =
    "    injectable: true\n    sessionId: \"fresh-session-per-wake\"\n    \
     wake: {argv: [\"sh\", \"orch/scripts/wake-pi-stream.sh\", \"{message}\"]}\n    \
     pokeHint: \"fixture\"\n";

// ------------------------------------------------- ① custody：topology 判别

#[test]
fn pi_wrapper_argv_resolves_to_managed_custody() {
    let rendered = argv(&["sh", "orch/scripts/wake-pi-stream.sh", "MESSAGE"]);

    durable_identity_kind(&rendered).unwrap_or_else(|error| {
        panic!(
            "pi wrapper 的 rendered argv 必须解析出受托管 topology，实际 bail: {error:#}。\
             留在兜底分支 ⇒ provider_kind=None ⇒ backendState=\"legacy-untracked\" ⇒ \
             没有 supervisor、没有 controlWakeId、没有可对账 terminal"
        )
    });

    backend_receipt_kind_from_argv(&rendered).unwrap_or_else(|error| {
        panic!("pi wrapper 的 rendered argv 必须解析出 backend receipt 语法，实际 Err: {error}")
    });
}

#[test]
fn zcode_wrapper_argv_resolves_to_managed_custody() {
    let rendered = argv(&["sh", "orch/scripts/wake-zcode-stream.sh", "MESSAGE"]);

    durable_identity_kind(&rendered).unwrap_or_else(|error| {
        panic!("zcode wrapper 的 rendered argv 必须解析出受托管 topology，实际 bail: {error:#}")
    });

    backend_receipt_kind_from_argv(&rendered).unwrap_or_else(|error| {
        panic!("zcode wrapper 的 rendered argv 必须解析出 backend receipt 语法，实际 Err: {error}")
    });
}

/// 回归护栏：`executor-claw` 是当前唯一在产的 `WakeLogProxy`。
/// 本卡只保证它行为不变，任何「顺手重构 multica」都必须在这里红。
#[test]
fn multica_topology_is_unchanged() {
    let rendered = argv(&["sh", "coordination/scripts/wake-multica.sh", "MESSAGE"]);

    let kind = durable_identity_kind(&rendered)
        .expect("multica 的 topology 是既有在产行为，不得因本卡改造而失效");
    assert_eq!(
        format!("{kind:?}"),
        "WakeLogProxy",
        "multica 必须仍然是 WakeLogProxy；改动它会让 executor-claw 的既有 custody 语义回归"
    );

    let receipt = backend_receipt_kind_from_argv(&rendered)
        .expect("multica 的 backend receipt 语法是既有在产行为");
    assert_eq!(format!("{receipt:?}"), "SmartClaw");
}

/// fail-closed 纪律不得为了让 pi/zcode 过而被放宽。
#[test]
fn an_unregistered_provider_still_fails_closed() {
    for rendered in [
        argv(&["sh", "orch/scripts/not-a-registered-wrapper.sh", "MESSAGE"]),
        argv(&["totally-unknown-provider", "-p", "MESSAGE"]),
        argv(&["sh", "-c", "echo hi"]),
    ] {
        assert!(
            durable_identity_kind(&rendered).is_err(),
            "未登记 provider 必须 fail-closed（topology decision required before pool admission），\
             实际被放行: {rendered:?}"
        );
        assert!(
            backend_receipt_kind_from_argv(&rendered).is_err(),
            "未登记 provider 的 backend receipt 语法必须 fail-closed，实际被放行: {rendered:?}"
        );
    }
}

/// **接线断言（读源码，不自调）**：启动路径必须真的从判别器取 `provider_kind`，
/// 并据此决定 `backendState`；不得把 `backendState` 写死，也不得绕过判别器。
#[test]
fn the_launch_path_consults_the_topology_discriminator() {
    let source = read_repo("orch/crates/orch-host/src/wake.rs");

    assert!(
        source.contains("backend_receipt_kind_from_argv(&rendered_argv)"),
        "wake.rs 的启动路径必须从 rendered argv 经判别器取 provider_kind；\
         绕开判别器就等于 custody 没有真正接上"
    );
    assert!(
        !source.contains(r#""backendState": "legacy-untracked""#),
        "backendState 不得被写死成 legacy-untracked——它必须由 provider_kind 推导"
    );
}

// ------------------------------------------------- ② signed 路由：注册表能力

#[test]
fn the_registry_can_express_provider_model_and_effort() {
    let root = temp_root("registry-provider");
    fixture_registry(
        &root,
        &format!(
            "{FIXTURE_WAKE}    provider: one-dewu-pi-anthropic\n    model: deepseek-v4-flash\n    \
             effort: max\n    observation:\n      source: pi-frames\n      policy: strict\n"
        ),
    );

    let defs = load_agent_definitions(&root).unwrap_or_else(|error| {
        panic!(
            "注册表必须能表达 legacy agent 的 provider/model/effort，实际解析失败: {error:#}。\
             当前 RawAgentDefinition 带 deny_unknown_fields 且没有 provider 字段 ⇒ \
             signed 面根本说不出 r73/B279 里被路由错的那一项"
        )
    });
    let def = defs
        .get("executor-fixture")
        .expect("fixture agent 必须被加载");

    assert_eq!(def.model.as_deref(), Some("deepseek-v4-flash"));
    assert_eq!(def.effort.as_deref(), Some("max"));

    let binding = signed_invocation_binding(def);
    assert_eq!(binding.requested_model.as_deref(), Some("deepseek-v4-flash"));
    assert_eq!(binding.requested_effort.as_deref(), Some("max"));

    fs::remove_dir_all(root).unwrap();
}

/// 负例：加 `provider` 不等于把注册表改成什么都收。
#[test]
fn an_unknown_registry_field_is_still_rejected() {
    let root = temp_root("registry-unknown");
    fixture_registry(
        &root,
        &format!("{FIXTURE_WAKE}    thisFieldDoesNotExist: nonsense\n"),
    );

    assert!(
        load_agent_definitions(&root).is_err(),
        "注册表必须保留 deny_unknown_fields；为了加 provider 而去掉它，\
         等于让任何拼错的字段静默生效"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_legacy_invocation_carries_the_signed_pin_to_the_wrapper() {
    let root = temp_root("render-pin");
    fixture_registry(
        &root,
        &format!(
            "{FIXTURE_WAKE}    provider: one-dewu-pi-anthropic\n    model: deepseek-v4-flash\n    \
             effort: max\n    observation:\n      source: pi-frames\n      policy: strict\n"
        ),
    );

    let defs = load_agent_definitions(&root).expect("fixture 注册表必须可加载");
    let def = defs.get("executor-fixture").unwrap();
    let rendered = render_invocation(def, &root, "SESSION", "MESSAGE").expect("渲染必须成功");

    let joined = format!("{:?} {:?}", rendered.argv, rendered.env);
    for wanted in ["one-dewu-pi-anthropic", "deepseek-v4-flash", "max"] {
        assert!(
            joined.contains(wanted),
            "signed 的 {wanted} 必须逐字段到达 wrapper（argv 或 env 均可），实际 argv/env 里没有。\
             legacy 分支当前 env 恒为空 map ⇒ wrapper 只能吃自己的缺省，\
             这正是 r73/B279 里用户指定的路由根本没被调用的机械原因。实际: {joined}"
        );
    }

    assert_eq!(
        rendered.requested_model.as_deref(),
        Some("deepseek-v4-flash")
    );
    assert_eq!(rendered.requested_effort.as_deref(), Some("max"));

    fs::remove_dir_all(root).unwrap();
}

/// 没有 signed pin 时，运行时**不得替 wrapper 编一个**。
/// fail-closed 是 wrapper 的职责，运行时的职责是不撒谎。
#[test]
fn a_legacy_invocation_without_a_pin_does_not_invent_one() {
    let root = temp_root("render-nopin");
    fixture_registry(&root, FIXTURE_WAKE);

    let defs = load_agent_definitions(&root).expect("fixture 注册表必须可加载");
    let def = defs.get("executor-fixture").unwrap();
    let rendered = render_invocation(def, &root, "SESSION", "MESSAGE").expect("渲染必须成功");

    assert_eq!(rendered.requested_model, None);
    assert_eq!(rendered.requested_effort, None);

    let joined = format!("{:?} {:?}", rendered.argv, rendered.env);
    for forbidden in ["deepseek-v4-pro", "deepseek-v4-flash", "one-dewu-pi-anthropic"] {
        assert!(
            !joined.contains(forbidden),
            "没有 signed 声明时运行时不得凭空注入 {forbidden}；实际: {joined}"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

// ------------------------------------------------- ③ wrapper：缺省回落必须删干净

#[test]
fn the_pi_wrapper_has_no_provider_or_model_fallback() {
    let source = code_lines(&read_repo("orch/scripts/wake-pi-stream.sh"));

    for forbidden in [
        r#"or "deepseek""#,
        r#"or "deepseek-v4-pro""#,
        r#"or 'deepseek'"#,
        r#"or 'deepseek-v4-pro'"#,
    ] {
        assert!(
            !source.contains(forbidden),
            "wake-pi-stream.sh 不得保留 provider/model 的缺省回落 `{forbidden}`。\
             r73/B279 就是靠这条缺省把 signed 的 one-dewu-pi-anthropic/deepseek-v4-flash \
             悄悄换成 deepseek/deepseek-v4-pro，再以 No API key found 失败"
        );
    }
    assert!(
        source.contains("ORCH_PI_PROVIDER") && source.contains("ORCH_PI_MODEL"),
        "wrapper 必须仍然从 signed 通道读 provider/model——删缺省不等于删入口"
    );
}

#[test]
fn the_zcode_wrapper_verifies_the_signed_pin_before_launch() {
    let source = code_lines(&read_repo("orch/scripts/wake-zcode-stream.sh"));

    assert!(
        source.contains("ORCH_ZCODE_MODEL"),
        "zcode wrapper 必须接收 signed model 用于比对。\
         ZCode CLI 没有 --model 旗标 ⇒ 本卡对 zcode 承诺的是 verify + fail-closed，\
         **不是** inject；但没有 signed 值就连比对都做不了"
    );
    assert!(
        source.contains("config.json"),
        "zcode wrapper 必须读 ~/.zcode/cli/config.json 作为有效模型的真值来源"
    );
}

/// UTF-8 offset 必须按字节推进。Python 的 `seek` 以字节为单位，
/// 用 `len(str)`（字符数）去 seek 会在中文跨 chunk 时错位。
#[test]
fn the_pi_wrapper_advances_stream_offsets_by_bytes() {
    let source = code_lines(&read_repo("orch/scripts/wake-pi-stream.sh"));

    assert!(
        !source.contains("offset += len(text)"),
        "流式 offset 不得用字符数推进；必须按 UTF-8 字节数（例如 len(chunk.encode(\"utf-8\"))）"
    );
}

// ------------------------------------------------- ④ supervisor 行为（合成 provider）

#[test]
fn launcher_return_does_not_kill_the_provider_before_terminal() {
    let root = temp_root("launcher-return");
    // 合成 provider：先睡一小会儿（模拟 launcher 已经返回），再写两段结构化输出，
    // 最后以已知 exit code 结束。不调模型、不联网。
    let body = r#"sleep 0.05; printf '%s\n' '{"type":"segment","n":1}'; sleep 0.05; printf '%s\n' '{"type":"segment","n":2}'; exit 0"#;
    let (child, log_path) = spawn_logged(&root, body);

    let outcome = supervise_managed_child(child, "/bin/sh", &log_path, policy(500, 100))
        .expect("supervisor 必须完整收敛");

    assert!(
        outcome.exited_naturally,
        "launcher 返回不等于 provider 死亡：合成 provider 必须活到自然退出"
    );
    assert!(
        outcome.signals.is_empty(),
        "自然退出路径不得发信号，实际: {:?}",
        outcome.signals
    );

    let log = fs::read_to_string(&log_path).unwrap();
    for wanted in [r#""n":1"#, r#""n":2"#] {
        assert!(
            log.contains(wanted),
            "launcher 返回后写出的 {wanted} 必须被完整捕获；丢掉它就等于 r70–r73 那型\
             「日志停在 turn.started」。实际日志: {log}"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn exact_terminal_facts_survive_a_stubborn_provider() {
    let root = temp_root("stubborn");
    // 忽略 TERM 的父进程 + 同样忽略 TERM 的子进程：证明 TERM→宽限→KILL→收割整条路径，
    // 且不留孤儿。
    let body = r#"trap '' TERM; (trap '' TERM; while :; do sleep 10; done) & printf '%s\n' '{"type":"segment","n":1}'; while :; do sleep 10; done"#;
    let (child, log_path) = spawn_logged(&root, body);

    let outcome = supervise_managed_child(child, "/bin/sh", &log_path, policy(30, 30))
        .expect("supervisor 必须完整收敛");

    assert!(
        !outcome.exited_naturally,
        "顽固 provider 不会自然退出，outcome 必须诚实记录这一点"
    );
    assert_eq!(
        outcome.signals,
        vec![TerminationSignal::Term, TerminationSignal::Kill],
        "必须走 TERM→宽限→KILL 的完整升级路径，而不是只发一次 TERM 就当收工"
    );
    assert!(
        outcome.process_tree_terminated,
        "整个受托管进程组必须被收割，不得只杀 leader 留下子树"
    );

    fs::remove_dir_all(root).unwrap();
}

// ------------------------------------------------- ⑤ 管道 marker

#[test]
fn the_support_module_is_wired() {
    legacy_review_managed_custody_support::contract_loaded();
}
