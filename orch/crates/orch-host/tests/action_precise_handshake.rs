//! B110 生产路径集成测试：action 精确通道事实与 handshake 窗口。
//!
//! 覆盖任务卡要求的全部场景：
//! - dispatch / nudge / resume 三路把 actionId/attemptId/attemptNo/pid/logPath/
//!   probeOffset/probeEnd 固化进 durable 事件链，同 action 状态事件身份相同；
//! - 两个相邻 action 不串台（per-attempt 专属 logPath，不读 legacy mtime 文件）；
//! - 旧日志噪声不进入判定（exact_probe_slice 窗口）；
//! - spawn-success / engaged 分离（Delivered 只表示 spawn 成功）；
//! - POKE / no-wake 无 pid/log 时显式 Pending，不伪造 Delivered；
//! - action facts 缺字段或类型错 fail-closed。
//!
//! scratch 一律落在本 worktree `orch/target/test-tmp`（协议约束，不污染工作树）；
//! fake provider 一律用 /bin/echo，绝不调用真实模型 CLI。

mod support_legacy_plan;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use orch_core::EventRecord;
use orch_host::attempt::AttemptRef;
use orch_host::wake::{
    capture_probe_window, evaluate_captured_probe, exact_probe_slice, ActionChannelFacts,
    DeliveryState,
};
use orch_host::{ledger, tierf};

const ROUND: &str = "r47";
const AGENT: &str = "executor-desktop";

struct Site {
    root: PathBuf,
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// fake agent 形态：EchoMark（echo 咬合标记 → spawn 成功且窗口内可见 provider
/// 活动）、EchoNoise（echo 普通文本 → spawn 成功但永不咬合）、EchoMessage
/// （echo 注入消息本文 → 日志携带 attempt 级身份，供串台判定）、
/// NonInjectable（不可注入 → POKE 备用道）。
enum AgentKind {
    EchoMark,
    EchoNoise,
    EchoMessage,
    NonInjectable,
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git 启动失败");
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn make_site(tag: &str, kind: AgentKind) -> Site {
    let root = orch_host::util::test_scratch_dir(&format!("b110-{tag}"));
    git(&root, &["init", "-q"]);
    fs::write(root.join("README.md"), "base\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    git(&root, &["branch", "-M", "main"]);

    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/modes")).unwrap();
    fs::create_dir_all(root.join(".worktrees")).unwrap();
    fs::create_dir_all(root.join(format!("coordination/rounds/{ROUND}/tasks"))).unwrap();
    fs::write(
        root.join(".gitignore"),
        "coordination/rounds/*/dispatch/\ncoordination/runtime/\n.worktrees/\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "scope: {protectedPaths: [\"coordination/**\"]}\n\
         git: {pushPolicy: forbidden}\n\
         commands:\n  testGate: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        "preset: relay\n\
         agents:\n  \
           planner: {adapter: root, tier: none}\n  \
           executor: {adapter: codex-desktop, tier: F, agentId: executor-desktop}\n  \
           verifier: {adapter: root-manual, tier: none}\n\
         hitl: {planSignoff: required, mergeGate: auto}\n\
         verification: {mode: root-manual-fixed-head}\n\
         liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}\n\
         scheduling:\n  \
           allowedAgents: [executor-desktop, executor-claw, executor-opencode]\n  \
           capacities:\n    \
             executor-desktop: {agent: 1, quota: 1, roles: [implement]}\n    \
             executor-claw: {agent: 1, quota: 1, roles: [primary-review]}\n    \
             executor-opencode: {agent: 1, quota: 1, roles: [implement, secondary-review]}\n\
         budgets: {round: {maxUsd: 12, wallMinutes: 1500, maxModelWakes: 40}}\n\
         git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();
    fs::write(
        root.join(format!("coordination/rounds/{ROUND}/tasks/B110.md")),
        "---\ntaskId: B110\nround: r47\nagent: executor-desktop\nwriteSet: []\nfrozenPaths: []\ngates: {fast: [testGate]}\nbudgets: {wallMinutes: 30}\nrequiredReviews:\n  - {role: primary, agent: executor-claw}\nrequiredEvidence: [fixture-evidence]\n---\n# B110 test\n",
    )
    .unwrap();
    for task in ["B110A", "B110B"] {
        fs::write(
            root.join(format!("coordination/rounds/{ROUND}/tasks/{task}.md")),
            format!(
                "---\ntaskId: {task}\nround: r47\nagent: executor-desktop\nwriteSet: []\nfrozenPaths: []\ngates: {{fast: [testGate]}}\nbudgets: {{wallMinutes: 30}}\nrequiredReviews:\n  - {{role: primary, agent: executor-claw}}\nrequiredEvidence: [fixture-evidence]\n---\n# {task} test\n"
            ),
        )
        .unwrap();
    }
    let agent_spec = match kind {
        AgentKind::EchoMark => serde_json::json!({
            "injectable": true,
            "sessionId": "test-session",
            "wake": {"argv": ["/bin/echo", "{\"type\":\"turn.started\"}"]}
        }),
        AgentKind::EchoNoise => serde_json::json!({
            "injectable": true,
            "sessionId": "test-session",
            "wake": {"argv": ["/bin/echo", "provider booting, no engagement mark"]}
        }),
        AgentKind::EchoMessage => serde_json::json!({
            "injectable": true,
            "sessionId": "test-session",
            "wake": {"argv": ["/bin/echo", "{message}"]}
        }),
        AgentKind::NonInjectable => serde_json::json!({
            "injectable": false,
            "sessionId": "",
            "wake": {"argv": []},
            "pokeHint": "请处理 {round}"
        }),
    };
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_string_pretty(&serde_json::json!({"agents": {AGENT: agent_spec}})).unwrap(),
    )
    .unwrap();
    support_legacy_plan::materialize(&root).unwrap();
    orch_host::round::run_sign_off(&root, Some("action handshake fixture")).unwrap();
    Site { root }
}

/// 追加一条现代 DispatchIssued（含 GO + ACK 文件），返回 attempt/go_rel/base_sha。
fn append_dispatch(
    site: &Site,
    task: &str,
    attempt_id: &str,
    ordinal: usize,
) -> (AttemptRef, String, String) {
    let base_sha = git(&site.root, &["rev-parse", "main"]);
    let attempt = AttemptRef {
        task_id: task.to_string(),
        ordinal,
        attempt_id: attempt_id.to_string(),
    };
    let go_rel = format!("coordination/rounds/{ROUND}/dispatch/{AGENT}/GO-{attempt_id}.md");
    let go = site.root.join(&go_rel);
    fs::create_dir_all(go.parent().unwrap()).unwrap();
    fs::write(&go, format!("# GO {task}\n")).unwrap();
    fs::write(format!("{}.ack", go.display()), format!("# ACK {task}\n")).unwrap();
    ledger::append(
        &site.root,
        ROUND,
        &[ledger::event(
            "DispatchIssued",
            "runtime:test",
            Some(task),
            Some(ROUND),
            serde_json::json!({
                "agent": AGENT,
                "baseSha": base_sha,
                "goPath": go_rel,
                "attemptId": attempt_id,
                "attemptNo": ordinal,
                "wakePending": true
            }),
        )],
    )
    .unwrap();
    (attempt, go_rel, base_sha)
}

/// Close one implementation lifecycle without erasing its historical
/// DispatchIssued.  B179's continuation fence deliberately rejects two live
/// owners for one agent, so fixtures that exercise sequential actions must
/// make the hand-off explicit in the ledger.
fn append_task_recorded(site: &Site, task: &str, attempt_id: &str, ordinal: usize) {
    ledger::append(
        &site.root,
        ROUND,
        &[ledger::event(
            "TaskRecorded",
            "runtime:test",
            Some(task),
            Some(ROUND),
            serde_json::json!({
                "attemptId": attempt_id,
                "attemptNo": ordinal,
            }),
        )],
    )
    .unwrap();
}

fn read_events(root: &Path) -> Vec<EventRecord> {
    let text = fs::read_to_string(root.join(format!("coordination/rounds/{ROUND}/events.jsonl")))
        .unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("events.jsonl 行必须是合法 EventRecord"))
        .collect()
}

/// 取某 task 某 kind 的唯一事件的 payload（恰好一条，否则断言失败）。
fn payload_of<'a>(events: &'a [EventRecord], kind: &str, task: &str) -> &'a serde_json::Value {
    let matches: Vec<&EventRecord> = events
        .iter()
        .filter(|event| event.kind == kind && event.task_id.as_deref() == Some(task))
        .collect();
    assert_eq!(matches.len(), 1, "{kind}({task}) 应恰好一条");
    matches[0].payload.as_ref().expect("事件缺 payload")
}

fn count_kind(events: &[EventRecord], kind: &str) -> usize {
    events.iter().filter(|event| event.kind == kind).count()
}

fn wait_until(label: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("等待超时: {label}");
}

const CHANNEL_KEYS: [&str; 7] = [
    "actionId",
    "attemptId",
    "attemptNo",
    "pid",
    "logPath",
    "probeOffset",
    "probeEnd",
];

#[test]
fn dispatch_delivered_carries_exact_channel_facts() {
    let site = make_site("dispatch-facts", AgentKind::EchoMark);
    let (attempt, go_rel, base_sha) = append_dispatch(&site, "B110", "B110-A0001", 1);
    tierf::finish_dispatch_wake_with_hook(
        &site.root,
        ROUND,
        "B110",
        AGENT,
        &attempt,
        &go_rel,
        false,
        true,
        &mut |_| Ok(()),
    )
    .unwrap();

    let events = read_events(&site.root);
    let delivered = payload_of(&events, "DispatchWakeDelivered", "B110");
    let completed = payload_of(&events, "DispatchWakeCompleted", "B110");

    // 事实可解析且身份精确绑定本 action。
    let facts = ActionChannelFacts::from_payload(delivered).unwrap();
    let expected_action = orch_host::attempt::dispatch_wake_action_id(
        ROUND, "B110", &attempt, AGENT, &base_sha, &go_rel,
    );
    assert_eq!(facts.action_id, expected_action);
    assert_eq!(facts.attempt_id, "B110-A0001");
    assert_eq!(facts.attempt_no, 1);
    assert!(facts.pid > 0);
    assert!(facts.probe_end >= facts.probe_offset);
    // logPath 必须是 per-attempt 专属文件（不是按 mtime 轮换的 legacy 名）。
    assert!(
        facts.log_path.contains(&format!("wake-{AGENT}-")),
        "logPath 应为 per-attempt 文件: {}",
        facts.log_path
    );
    assert!(!facts.log_path.ends_with(&format!("wake-{AGENT}.log")));
    // probeEnd 是投递时刻水位：文件只增不减，不得超过当前长度。
    let now_len = fs::metadata(&facts.log_path).unwrap().len();
    assert!(facts.probe_end <= now_len);

    // 同 action 状态事件身份相同：Delivered 与 Completed 的通道七元组逐项相等。
    for key in CHANNEL_KEYS {
        assert_eq!(
            delivered.get(key),
            completed.get(key),
            "Delivered/Completed 的 {key} 必须相同"
        );
    }
    // Claimed/Launching（spawn 前）的身份字段一致；pid/logPath 尚不可能存在。
    for kind in ["DispatchWakeClaimed", "DispatchWakeLaunching"] {
        let payload = payload_of(&events, kind, "B110");
        assert_eq!(payload["actionId"], delivered["actionId"]);
        assert_eq!(payload["attemptId"], delivered["attemptId"]);
        assert_eq!(payload["attemptNo"], delivered["attemptNo"]);
    }

    // echo turn.started → 精确窗口内最终可见 provider 活动（咬合另判为真）。
    wait_until("provider 咬合标记落盘", || {
        capture_probe_window(Path::new(&facts.log_path), facts.probe_offset)
            .and_then(evaluate_captured_probe)
            .unwrap_or(false)
    });

    // fail-closed：缺字段 / 类型错一律拒绝。
    let mut missing = delivered.clone();
    missing.as_object_mut().unwrap().remove("pid");
    assert!(ActionChannelFacts::from_payload(&missing).is_err());
    let mut wrong_type = delivered.clone();
    wrong_type["probeOffset"] = serde_json::json!("0");
    assert!(ActionChannelFacts::from_payload(&wrong_type).is_err());
    let mut empty_id = delivered.clone();
    empty_id["actionId"] = serde_json::json!("");
    assert!(ActionChannelFacts::from_payload(&empty_id).is_err());
}

#[test]
fn delivered_only_means_spawn_succeeded() {
    // (a) spawn 成功但 provider 尚未咬合：Delivered 落账，窗口判定仍未咬合——
    //     Delivered 绝不冒充 Engaged。
    let site = make_site("spawn-only", AgentKind::EchoNoise);
    let (attempt, go_rel, _) = append_dispatch(&site, "B110", "B110-A0001", 1);
    tierf::finish_dispatch_wake_with_hook(
        &site.root,
        ROUND,
        "B110",
        AGENT,
        &attempt,
        &go_rel,
        false,
        true,
        &mut |_| Ok(()),
    )
    .unwrap();
    let events = read_events(&site.root);
    let facts =
        ActionChannelFacts::from_payload(payload_of(&events, "DispatchWakeDelivered", "B110"))
            .unwrap();
    assert_eq!(DeliveryState::from_spawn(true), DeliveryState::Delivered);
    wait_until("echo 输出落盘", || {
        fs::metadata(&facts.log_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    });
    assert!(
        !evaluate_captured_probe(
            capture_probe_window(Path::new(&facts.log_path), facts.probe_offset).unwrap()
        )
        .unwrap(),
        "spawn 成功 ≠ 已咬合：无 provider 活动标记必须判未咬合"
    );

    // (b) POKE 备用道（agent 不可注入，无 pid/log）：显式 Pending——
    //     不伪造 DispatchWakeDelivered，claim 被 release，派发失败可见。
    let poke = make_site("poke-pending", AgentKind::NonInjectable);
    let (attempt, go_rel, _) = append_dispatch(&poke, "B110", "B110-A0001", 1);
    let error = tierf::finish_dispatch_wake_with_hook(
        &poke.root,
        ROUND,
        "B110",
        AGENT,
        &attempt,
        &go_rel,
        false,
        true,
        &mut |_| Ok(()),
    )
    .expect_err("POKE 备用道不得冒充 Delivered");
    let text = format!("{error:#}");
    assert!(
        text.contains("pending") || text.contains("POKE"),
        "错误须明示 Pending/POKE: {text}"
    );
    assert_eq!(DeliveryState::from_spawn(false), DeliveryState::Pending);
    let events = read_events(&poke.root);
    assert_eq!(
        count_kind(&events, "DispatchWakeDelivered"),
        0,
        "无 pid/log 不得落 Delivered"
    );
    assert_eq!(count_kind(&events, "DispatchWakeCompleted"), 0);
    assert_eq!(
        count_kind(&events, "DispatchWakeReleased"),
        1,
        "claim 必须被显式释放"
    );
    assert!(
        poke.root
            .join(format!("coordination/runtime/POKE-{AGENT}.txt"))
            .exists(),
        "POKE 备用信标必须落盘"
    );
}

#[test]
fn adjacent_actions_do_not_cross_talk() {
    // 两个相邻 action 共用同一 agent（共享 legacy 探活名）：串台向量 =
    // legacy `wake-<agent>.log` 会被后一次注入重链。精确通道事实必须各判各的。
    let site = make_site("cross-talk", AgentKind::EchoMessage);
    let (attempt_a, go_rel_a, _) = append_dispatch(&site, "B110A", "B110A-A0001", 1);
    tierf::finish_dispatch_wake_with_hook(
        &site.root,
        ROUND,
        "B110A",
        AGENT,
        &attempt_a,
        &go_rel_a,
        false,
        true,
        &mut |_| Ok(()),
    )
    .unwrap();
    append_task_recorded(&site, "B110A", "B110A-A0001", 1);
    let (attempt_b, go_rel_b, _) = append_dispatch(&site, "B110B", "B110B-A0001", 1);
    tierf::finish_dispatch_wake_with_hook(
        &site.root,
        ROUND,
        "B110B",
        AGENT,
        &attempt_b,
        &go_rel_b,
        false,
        true,
        &mut |_| Ok(()),
    )
    .unwrap();

    let events = read_events(&site.root);
    let facts_a =
        ActionChannelFacts::from_payload(payload_of(&events, "DispatchWakeDelivered", "B110A"))
            .unwrap();
    let facts_b =
        ActionChannelFacts::from_payload(payload_of(&events, "DispatchWakeDelivered", "B110B"))
            .unwrap();
    assert_ne!(
        facts_a.log_path, facts_b.log_path,
        "相邻 action 必须有各自专属 logPath"
    );
    assert_ne!(facts_a.action_id, facts_b.action_id);

    // echo {message}：各自日志只含本 action 身份。
    wait_until("action A 消息落盘", || {
        fs::read_to_string(&facts_a.log_path)
            .map(|text| text.contains("B110A-A0001"))
            .unwrap_or(false)
    });
    wait_until("action B 消息落盘", || {
        fs::read_to_string(&facts_b.log_path)
            .map(|text| text.contains("B110B-A0001"))
            .unwrap_or(false)
    });
    let log_a = fs::read(&facts_a.log_path).unwrap();
    assert!(
        !String::from_utf8_lossy(&log_a).contains("B110B-A0001"),
        "A 的通道不得混入 B 的字节"
    );

    // 向 B 的日志投毒（咬合标记）：A 的事实判定绝不受 B 影响。
    use std::io::Write;
    let mut poison = fs::OpenOptions::new()
        .append(true)
        .open(&facts_b.log_path)
        .unwrap();
    writeln!(poison, "{{\"type\":\"turn.started\"}}").unwrap();
    drop(poison);
    assert!(
        !evaluate_captured_probe(
            capture_probe_window(Path::new(&facts_a.log_path), facts_a.probe_offset).unwrap()
        )
        .unwrap(),
        "B 的咬合标记不得串进 A 的判定"
    );
    assert!(
        evaluate_captured_probe(
            capture_probe_window(Path::new(&facts_b.log_path), facts_b.probe_offset).unwrap()
        )
        .unwrap(),
        "B 自己的窗口应判咬合"
    );

    // 对照：legacy 探活名此刻被重链到 B 的文件——按 legacy 判 A 必然串台，
    // 这正是任务卡禁止的 mtime/legacy 读法。
    let legacy = site
        .root
        .join(format!("coordination/runtime/logs/wake-{AGENT}.log"));
    let via_legacy = fs::read_to_string(&legacy).unwrap_or_default();
    assert!(
        via_legacy.contains("B110B-A0001"),
        "legacy 名应被后一次注入重链（串台向量的存在性证明）"
    );
    assert!(!via_legacy.contains("B110A-A0001"));
}

#[test]
fn old_log_noise_excluded() {
    // 旧日志噪声：legacy 名与历史 per-attempt 文件都预先埋入旧 fault/咬合标记。
    let site = make_site("old-noise", AgentKind::EchoNoise);
    let logs = site.root.join("coordination/runtime/logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("wake-executor-test.log"),
        "old-fault\n{\"type\":\"turn.started\",\"ts\":\"prev-round\"}\n",
    )
    .unwrap();
    fs::write(
        logs.join("wake-executor-test-111-1-0.jsonl"),
        "{\"type\":\"thread.started\",\"ts\":\"stale\"}\n",
    )
    .unwrap();

    let (attempt, go_rel, _) = append_dispatch(&site, "B110", "B110-A0001", 1);
    tierf::finish_dispatch_wake_with_hook(
        &site.root,
        ROUND,
        "B110",
        AGENT,
        &attempt,
        &go_rel,
        false,
        true,
        &mut |_| Ok(()),
    )
    .unwrap();
    let events = read_events(&site.root);
    let facts =
        ActionChannelFacts::from_payload(payload_of(&events, "DispatchWakeDelivered", "B110"))
            .unwrap();
    assert!(!facts.log_path.ends_with("wake-executor-test.log"));
    assert!(!facts.log_path.ends_with("wake-executor-test-111-1-0.jsonl"));

    wait_until("本次 echo 输出落盘", || {
        fs::metadata(&facts.log_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    });
    assert!(
        !evaluate_captured_probe(
            capture_probe_window(Path::new(&facts.log_path), facts.probe_offset).unwrap()
        )
        .unwrap(),
        "旧日志噪声（legacy/历史文件中的咬合标记）不得进入本次判定"
    );
    //  sanity：预埋的旧噪声确实含咬合标记（若是整份/legacy 读法必误判）。
    let stale = fs::read(logs.join("wake-executor-test-111-1-0.jsonl")).unwrap();
    assert!(orch_host::wake::wake_engaged(&String::from_utf8_lossy(
        &stale
    )));
}

#[test]
fn engaged_in_uses_captured_probe_end_not_log_len() {
    // T1：marker 位于 captured end 之后；即使传入 bytes 已更长也不得判咬合。
    let prefix = b"provider booting\n";
    let mut log = prefix.to_vec();
    log.extend_from_slice(b"{\"type\":\"turn.started\"}\n");
    let facts = ActionChannelFacts::new(
        "nudge-r47-B110-A0003",
        "B110-A0003",
        3,
        7,
        "captured.log",
        0,
        prefix.len() as u64,
    )
    .unwrap();
    assert!(!facts.engaged_in(&log, prefix.len() as u64).unwrap());
}

#[test]
fn engaged_in_rejects_log_len_as_implicit_right_end() {
    // T2 durable regression：相同 bytes 下，捕获右端不含 marker 时为 false；只有
    // 调用方显式传入更大的另一轮 captured end 才能变 true。engaged_in 自身无
    // bytes.len() 兜底，因此把实现退化回 log.len() 会让第一条断言转红。
    let prefix = b"provider booting\n";
    let mut log = prefix.to_vec();
    log.extend_from_slice(b"turn.started\n");
    let facts = ActionChannelFacts::new(
        "probe-action",
        "B110-A0003",
        3,
        7,
        "captured.log",
        0,
        prefix.len() as u64,
    )
    .unwrap();
    assert!(!facts.engaged_in(&log, prefix.len() as u64).unwrap());
    assert!(facts.engaged_in(&log, log.len() as u64).unwrap());
}

#[test]
fn capture_probe_window_returns_current_len_snapshot() {
    // T3：每轮可捕获更大的 end，但前一轮 observation 的 end 不漂移。
    let site = make_site("capture-growth", AgentKind::EchoNoise);
    let path = site.root.join("probe.log");
    fs::write(&path, b"boot\n").unwrap();
    let first = capture_probe_window(&path, 0).unwrap();
    let first_end = first.probe_end;
    fs::write(&path, b"boot\nturn.started\n").unwrap();
    assert_eq!(first.probe_end, first_end);
    assert!(!evaluate_captured_probe(first).unwrap());
    let second = capture_probe_window(&path, 0).unwrap();
    assert!(second.probe_end > first_end);
    assert!(evaluate_captured_probe(second).unwrap());
}

#[test]
fn capture_probe_window_fail_closed_on_stat_error() {
    // T4：missing/stat error 不能折叠为 (0,0)。
    let site = make_site("capture-missing", AgentKind::EchoNoise);
    let missing = site.root.join("missing-probe.log");
    assert!(capture_probe_window(&missing, 0).is_err());
}

#[test]
fn handshake_uses_captured_probe_end_per_observation() {
    // T5：第一轮 capture 看不到延迟 marker；下一轮重新 capture 才能看到。
    let site = make_site("capture-two-polls", AgentKind::EchoNoise);
    let path = site.root.join("two-polls.log");
    fs::write(&path, b"boot\n").unwrap();
    let first = capture_probe_window(&path, 0).unwrap();
    fs::write(&path, b"boot\nturn.started\n").unwrap();
    assert!(!evaluate_captured_probe(first).unwrap());
    assert!(evaluate_captured_probe(capture_probe_window(&path, 0).unwrap()).unwrap());
}

#[test]
fn handshake_does_not_read_beyond_captured_probe_end() {
    // T6：capture 后追加 marker；本 observation 必须保持 false。
    let site = make_site("capture-poison", AgentKind::EchoNoise);
    let path = site.root.join("poison.log");
    fs::write(&path, b"boot\n").unwrap();
    let captured = capture_probe_window(&path, 0).unwrap();
    fs::write(&path, b"boot\nthread.started\n").unwrap();
    assert!(!evaluate_captured_probe(captured).unwrap());
}

#[test]
fn durable_probe_end_remains_delivery_snapshot_across_polls() {
    // T7：ledger 里的 delivery snapshot 不刷新；内存 capture 可增长。
    let site = make_site("durable-end", AgentKind::EchoNoise);
    let (attempt, go_rel, _) = append_dispatch(&site, "B110", "B110-A0001", 1);
    tierf::finish_dispatch_wake_with_hook(
        &site.root,
        ROUND,
        "B110",
        AGENT,
        &attempt,
        &go_rel,
        false,
        true,
        &mut |_| Ok(()),
    )
    .unwrap();
    let events = read_events(&site.root);
    let facts =
        ActionChannelFacts::from_payload(payload_of(&events, "DispatchWakeDelivered", "B110"))
            .unwrap();
    let durable_end = facts.probe_end;
    use std::io::Write as _;
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(&facts.log_path)
        .unwrap();
    writeln!(log, "turn.started").unwrap();
    drop(log);
    let captured = capture_probe_window(Path::new(&facts.log_path), facts.probe_offset).unwrap();
    assert!(captured.probe_end > durable_end);
    assert_eq!(
        ActionChannelFacts::from_payload(payload_of(
            &read_events(&site.root),
            "DispatchWakeDelivered",
            "B110"
        ))
        .unwrap()
        .probe_end,
        durable_end
    );
    assert!(evaluate_captured_probe(captured).unwrap());
}

#[test]
fn exact_probe_slice_rejects_probe_end_beyond_log_len() {
    // T8：stat/read 截短竞态与直接越界均 fail-closed。
    assert!(exact_probe_slice(b"abc", 0, 4).is_err());
    let site = make_site("capture-truncate", AgentKind::EchoNoise);
    let path = site.root.join("truncate.log");
    fs::write(&path, b"abcdef").unwrap();
    let captured = capture_probe_window(&path, 0).unwrap();
    fs::write(&path, b"").unwrap();
    assert!(evaluate_captured_probe(captured).is_err());
}



#[test]
fn retired_nudge_force_preserves_live_report_collect_lease() {
    let site = make_site("nudge-preserves-collect", AgentKind::NonInjectable);
    let (attempt, go_rel, base_sha) = append_dispatch(&site, "B110", "B110-A0001", 1);
    let action_id = "collect-r47-B110-B110-A0001-test";
    let owner = "collect-owner-test";
    let generation = "collect-generation-test";
    let common = serde_json::json!({
        "actionId": action_id,
        "attemptId": attempt.attempt_id,
        "attemptNo": attempt.ordinal,
        "agent": AGENT,
        "baseSha": base_sha,
        "goPath": go_rel,
        "owner": owner,
        "leaseGeneration": generation,
        "leaseUntil": "2999-01-01T00:00:00Z"
    });
    ledger::append(
        &site.root,
        ROUND,
        &[
            ledger::event(
                "ReportCollectClaimed",
                "runtime:orch",
                Some("B110"),
                Some(ROUND),
                common.clone(),
            ),
            ledger::event(
                "ReportCollectExecuting",
                "runtime:orch",
                Some("B110"),
                Some(ROUND),
                common,
            ),
        ],
    )
    .unwrap();

    let expectation = orch_host::attempt::DurableActionExpectation {
        round: ROUND.to_string(),
        task_id: "B110".to_string(),
        attempt_id: attempt.attempt_id.clone(),
        attempt_no: attempt.ordinal,
        agent: AGENT.to_string(),
        base_sha,
        go_path: go_rel,
        action_id: action_id.to_string(),
        evidence_sha256: None,
        evidence_len: None,
        control_epoch: None,
        branch_sha: None,
    };
    let before = read_events(&site.root);
    assert_eq!(
        orch_host::attempt::fold_durable_action(
            &before,
            orch_host::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )
        .unwrap(),
        orch_host::attempt::DurableActionPhase::Executing,
    );
    let collect_before = before
        .iter()
        .filter(|event| event.kind.starts_with("ReportCollect"))
        .map(|event| serde_json::to_value(event).unwrap())
        .collect::<Vec<_>>();

    // The retired command is rejected by the actual parser before it can touch the lease.
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap()
        .join("target/debug/orch");
    let out = Command::new(binary).arg("--root").arg(&site.root)
        .args(["nudge", AGENT, "--task", "B110", "--force", "--no-wake"])
        .output().unwrap();
    assert_eq!(out.status.code(), Some(2));

    let after = read_events(&site.root);
    assert_eq!(count_kind(&after, "NudgeIssued"), 0);
    assert_eq!(count_kind(&after, "ReportCollectReleased"), 0);
    let collect_after = after
        .iter()
        .filter(|event| event.kind.starts_with("ReportCollect"))
        .map(|event| serde_json::to_value(event).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        collect_after, collect_before,
        "nudge changed collect lineage"
    );
    assert_eq!(
        orch_host::attempt::fold_durable_action(
            &after,
            orch_host::attempt::DurableActionKind::ReportCollect,
            &expectation,
        )
        .unwrap(),
        orch_host::attempt::DurableActionPhase::Executing,
    );
}
