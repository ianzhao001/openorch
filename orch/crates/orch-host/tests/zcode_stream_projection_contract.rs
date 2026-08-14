//! B228 seeded-red contract: zcode 流式传输（H112 路线 1）——投影帧满足结构化消费证明、
//! 会话中途即出帧、绝不泄露 prompt/响应体、体积有界。
//!
//! Expected red: compile。`orch_host::wake::ZCODE_STREAM_SCRIPT_REL` 在 B228 之前不存在
//! （脚本路径的单一事实源常量，与脚本本体同卡交付；r64 实测发现 assertion 形态的 oracle
//! 会跑全量门，而门在「已 plan 未签核」窗口结构性必红——H101/H85 同族第三窗口，
//! 故本种子采用编译红短路）。
//!
//! 硬约束（裁定⑭，用户 2026-08-04）：不得改变会话创建方式（`node <bundle>/zcode.cjs -p MSG
//! --json --cwd DIR` argv 逐字保留）；GUI 可见性=会话落共享会话库，改造只允许**加一层**
//! 「把已在落盘的帧转写进 orch wake 日志」。
//!
//! 机械判据（wake.rs:12631 `probe_challenge_observed`，pub）：wake 日志逐行解析，
//! `{"type":"tool_result","output":"…challenge…"}` 即命中 ClawToolOutput——本种子直接
//! import 生产判据函数打分，不自造复刻。
//!
//! Negative mutations that must turn the named case red:
//! M1. 投影丢掉 role:"tool" 消息（或整包缓冲到进程退出才输出）
//!     -> `projection_emits_probe_satisfying_frames_while_session_is_still_running` 红。
//! M2. 原样转写 prompt/response 体
//!     -> `projection_never_leaks_prompt_or_response_bodies` 红。
//! M3. 帧体积无界
//!     -> `projection_frames_are_bounded` 红。

use orch_host::util::test_scratch_dir;
use orch_host::wake::{probe_challenge_observed, ReviewProbeSource, ZCODE_STREAM_SCRIPT_REL};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const CHALLENGE: &str = ".orch-review-probe-b228-seed-challenge";

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn script_path() -> std::path::PathBuf {
    // 单一事实源：脚本相对路径常量与脚本本体同卡交付（agents.yaml 指针对照它配置）。
    repo_root().join(ZCODE_STREAM_SCRIPT_REL)
}

/// 造一个假 zcode bundle：睡 6 秒再打终对象（给流式断言留观测窗口）。
fn write_stub_bundle(dir: &std::path::Path) -> std::path::PathBuf {
    let stub = dir.join("zcode-stub.cjs");
    std::fs::write(
        &stub,
        "setTimeout(() => { console.log(JSON.stringify({sessionId: 'stub-sess', response: 'ok'})); }, 6000);\n",
    )
    .expect("写 stub bundle");
    stub
}

/// rollout 记录：request.messages 含 role:"tool"（哨兵输出）与 role:"user"（prompt 体，
/// 绝不可出现在投影里）；response.text 为响应体（同样不可出现）。
fn rollout_record(message_count: u32, tool_content: &str) -> String {
    serde_json::json!({
        "type": "model_io",
        "sessionId": "stub-sess",
        "startedAt": "2026-08-04T00:00:00Z",
        "completedAt": "2026-08-04T00:00:01Z",
        "request": {
            "messageOffset": 0,
            "messageCount": message_count,
            "messages": [
                {"role": "user", "content": "PROMPT_BODY_SECRET_DO_NOT_TRANSCRIBE"},
                {"role": "tool", "content": tool_content},
            ],
        },
        "response": {"text": "RESPONSE_BODY_SECRET_DO_NOT_TRANSCRIBE"},
    })
    .to_string()
}

struct Run {
    child: std::process::Child,
    lines: Vec<String>,
}

/// 启动脚本，边跑边写 rollout，收集「子进程仍在运行期间」读到的 stdout 行。
fn drive_script() -> Run {
    let tmp = test_scratch_dir("b228-seed");
    let rollout_dir = tmp.join("rollout");
    std::fs::create_dir_all(&rollout_dir).expect("建 rollout 夹具目录");
    let stub = write_stub_bundle(&tmp);

    let mut child = Command::new("sh")
        .arg(script_path())
        .arg("do the review")
        .env("ORCH_ZCODE_BIN", &stub)
        .env("ORCH_ZCODE_ROLLOUT_DIR", &rollout_dir)
        .env("ORCH_ZCODE_CWD", &tmp)
        .env("ORCH_ZCODE_TIMEOUT", "30")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 wake-zcode-stream.sh（不存在即红——这是本卡的首红）");

    // 会话「中途」落盘：先一条含哨兵的记录，稍后追加第二条（messageCount 递增重放）。
    let session_file = rollout_dir.join("model-io-sess_stub.jsonl");
    let mut f = std::fs::File::create(&session_file).expect("写 rollout 第 1 行");
    writeln!(f, "{}", rollout_record(2, &format!("1\t{CHALLENGE}\n2\t"))).unwrap();
    f.sync_all().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    writeln!(f, "{}", rollout_record(3, "second tool output, no secrets")).unwrap();
    f.sync_all().unwrap();

    let stdout = child.stdout.take().expect("取 stdout");
    let mut reader = BufReader::new(stdout);
    let mut lines = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                lines.push(line.trim_end().to_string());
                // 拿到首个命中帧就够了：此刻 stub 仍在睡（6s），流式性成立。
                if lines
                    .iter()
                    .any(|l| probe_challenge_observed(l, CHALLENGE).is_some())
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Run { child, lines }
}

#[test]
fn projection_emits_probe_satisfying_frames_while_session_is_still_running() {
    let mut run = drive_script();
    let still_running = run.child.try_wait().expect("try_wait").is_none();
    let hit = run
        .lines
        .iter()
        .find_map(|l| probe_challenge_observed(l, CHALLENGE));
    let _ = run.child.kill();
    let _ = run.child.wait();
    assert!(
        matches!(hit, Some(ReviewProbeSource::ClawToolOutput)),
        "投影必须在会话期内产出满足结构化消费证明的 tool_result 帧，读到的行：{:?}",
        run.lines
    );
    assert!(
        still_running,
        "命中帧必须出现在子进程仍在运行时（M1：整包缓冲到退出才输出即红）"
    );
}

#[test]
fn projection_never_leaks_prompt_or_response_bodies() {
    let mut run = drive_script();
    let _ = run.child.kill();
    let _ = run.child.wait();
    assert!(
        !run.lines.is_empty(),
        "必须先有投影输出才谈得上不泄露（脚本缺失/零输出即红，防空洞通过）"
    );
    let joined = run.lines.join("\n");
    assert!(
        !joined.contains("PROMPT_BODY_SECRET_DO_NOT_TRANSCRIBE"),
        "投影绝不转写 prompt 体（取证面泄露 + 撑爆 wake 日志）"
    );
    assert!(
        !joined.contains("RESPONSE_BODY_SECRET_DO_NOT_TRANSCRIBE"),
        "投影绝不转写 response 体"
    );
}

#[test]
fn projection_frames_are_bounded() {
    // M3：单帧 64KB 上限 + 截断标记。夹具直接喂一条超长 tool 输出。
    let tmp = test_scratch_dir("b228-seed-bounded");
    let rollout_dir = tmp.join("rollout");
    std::fs::create_dir_all(&rollout_dir).expect("建 rollout 夹具目录");
    let stub = write_stub_bundle(&tmp);
    let mut child = Command::new("sh")
        .arg(script_path())
        .arg("bounded check")
        .env("ORCH_ZCODE_BIN", &stub)
        .env("ORCH_ZCODE_ROLLOUT_DIR", &rollout_dir)
        .env("ORCH_ZCODE_CWD", &tmp)
        .env("ORCH_ZCODE_TIMEOUT", "30")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 wake-zcode-stream.sh");
    let big = "X".repeat(200 * 1024);
    let session_file = rollout_dir.join("model-io-sess_stub.jsonl");
    let mut f = std::fs::File::create(&session_file).expect("写 rollout");
    writeln!(f, "{}", rollout_record(2, &big)).unwrap();
    f.sync_all().unwrap();

    let stdout = child.stdout.take().expect("取 stdout");
    let mut reader = BufReader::new(stdout);
    let mut first_frame = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.contains("tool_result") {
                    first_frame = line.trim_end().to_string();
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(!first_frame.is_empty(), "必须产出投影帧");
    assert!(
        first_frame.len() <= 66 * 1024,
        "单帧必须有界（≤64KB+封包开销），实测 {} 字节",
        first_frame.len()
    );
    assert!(
        first_frame.contains("truncated") || first_frame.contains("截断"),
        "超限帧必须带截断标记"
    );
}
