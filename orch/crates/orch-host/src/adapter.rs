//! Tier S 适配器宿主：spawn + 日志落盘 + 超时 + usage 提取（规格来源：m0/compatibility/*）。
//! Tier S 语义（design/03 §1）：进程退出即完成信号；棒间等待结构性不存在。

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::redact;

use anyhow::{bail, Context, Result};
use wait_timeout::ChildExt;

const BUILTIN_ADAPTERS: [&str; 6] = ["opencode", "codex", "claude", "cursor", "mimo", "codebuddy"];

pub struct RunResult {
    pub exit_code: i32,
    pub duration_secs: u64,
    /// 从 JSONL 日志尽力提取的 usage（各家形状不同，原样保留）
    pub usage: Option<serde_json::Value>,
    pub log_path: String,
    /// stderr 脱敏日志路径
    pub stderr_log_path: String,
    /// 结构化执行证据（requested/observed model、session、usage、terminal_result）
    pub evidence: AdapterExecutionEvidence,
}

/// All Tier S profiles compiled into orch. The dispatcher below uses this list to decide whether
/// an adapter is built-in; tests then require every listed name to have a command constructor.
pub fn supported_builtins() -> Vec<&'static str> {
    BUILTIN_ADAPTERS.to_vec()
}

/// 内置 Tier S profile（AdapterSpec 声明式格式的硬编码前身；M1 后续外置）
/// `extra_writable`：worktree 任务须加白主仓 `.git`（errata E11：codex workspace-write
/// 沙箱默认禁写 worktree 关联的主仓 index.lock，实测 `Operation not permitted`）
pub fn build_command(
    adapter: &str,
    prompt: &str,
    workdir: &Path,
    extra_writable: Option<&Path>,
    spec_root: Option<&Path>,
) -> Result<Command> {
    let builtins = supported_builtins();
    let mut cmd = if builtins.contains(&adapter) {
        build_builtin_command(adapter, prompt, workdir, extra_writable)
            .with_context(|| format!("内置适配器清单与构造器漂移: {adapter}"))?
    } else {
        // AdapterSpec 外置化：coordination/adapters/<name>.yaml（用户自定义工具，决策 4）
        match spec_root.and_then(|r| load_spec(r, adapter).ok()) {
            Some(spec) => {
                let sub = |s: &str| {
                    s.replace("{prompt}", prompt)
                        .replace("{workdir}", &workdir.display().to_string())
                };
                let argv: Vec<String> = spec.launch.argv.iter().map(|a| sub(a)).collect();
                let (prog, rest) = argv.split_first().context("AdapterSpec argv 为空")?;
                let mut c = Command::new(prog);
                c.args(rest);
                if spec.launch.cwd_is_workdir {
                    c.current_dir(workdir);
                }
                c
            }
            None => bail!(
                "未知适配器: {adapter}（内置 {}，或提供 coordination/adapters/{adapter}.yaml）",
                builtins.join("/")
            ),
        }
    };
    cmd.stdin(Stdio::null());
    Ok(cmd)
}

fn build_builtin_command(
    adapter: &str,
    prompt: &str,
    workdir: &Path,
    extra_writable: Option<&Path>,
) -> Option<Command> {
    let cmd = match adapter {
        // m0/compatibility/opencode-1.18.4.md
        "opencode" => {
            let mut c = Command::new("opencode");
            c.args(["run", prompt, "--format", "json", "--dir"])
                .arg(workdir);
            c
        }
        // m0/compatibility/codex-cli-0.144.1.md（stdin 必须关闭）
        "codex" => {
            let mut c = Command::new("codex");
            c.args(["exec", prompt, "--json", "--skip-git-repo-check"]);
            if let Some(w) = extra_writable {
                c.arg("-c").arg(format!(
                    "sandbox_workspace_write.writable_roots=[\"{}\"]",
                    w.display()
                ));
            }
            c.current_dir(workdir);
            c
        }
        // m0/compatibility/claude-code-2.1.216.md
        "claude" => {
            let mut c = Command::new("claude");
            c.args(["-p", prompt, "--output-format", "stream-json", "--verbose"]);
            c.current_dir(workdir);
            c
        }
        // research/r13-R2-cursor-cli-capability.md（Cursor 官方参数文档实核版）
        "cursor" => {
            let mut c = Command::new("cursor-agent");
            c.args(["-p", prompt, "--output-format", "stream-json", "--force"]);
            c.current_dir(workdir);
            c
        }
        // B94 取证 + 卡 §本机已实核 argv：fresh one-shot，禁 session/continue
        "mimo" => {
            let mut c = Command::new("mimo");
            c.args([
                "run",
                prompt,
                "--model",
                "xiaomi/mimo-v2.5-pro",
                "--variant",
                "high",
                "--format",
                "json",
                "--pure",
                "--dangerously-skip-permissions",
                "--dir",
            ])
            .arg(workdir);
            c.current_dir(workdir);
            c
        }
        // B94 取证 + 卡 §本机已实核 argv：fresh one-shot，禁 fallback/resume/session
        "codebuddy" => {
            let mut c = Command::new("codebuddy");
            c.args([
                "-p",
                prompt,
                "--output-format",
                "stream-json",
                "--model",
                "glm-5.2",
                "--effort",
                "high",
                "--no-session-persistence",
                "--dangerously-skip-permissions",
            ]);
            c.current_dir(workdir);
            c
        }
        _ => return None,
    };
    Some(cmd)
}

#[derive(serde::Deserialize)]
struct AdapterSpecFile {
    launch: LaunchSpec,
}
#[derive(serde::Deserialize)]
struct LaunchSpec {
    argv: Vec<String>,
    #[serde(default = "default_true")]
    cwd_is_workdir: bool,
}
fn default_true() -> bool {
    true
}
fn load_spec(root: &Path, name: &str) -> Result<AdapterSpecFile> {
    let p = root.join(format!("coordination/adapters/{name}.yaml"));
    let text =
        fs::read_to_string(&p).with_context(|| format!("读 AdapterSpec 失败: {}", p.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("解析 AdapterSpec 失败: {}", p.display()))
}

fn redact_stream<R: Read, W: Write>(input: R, output: W) -> std::io::Result<()> {
    let mut reader = BufReader::new(input);
    let mut writer = BufWriter::new(output);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let (body, ending) = if let Some(body) = line.strip_suffix("\r\n") {
            (body, "\r\n")
        } else if let Some(body) = line.strip_suffix('\n') {
            (body, "\n")
        } else {
            (line.as_str(), "")
        };
        writer.write_all(redact::redact_full(body).as_bytes())?;
        writer.write_all(ending.as_bytes())?;
        writer.flush()?;
    }
    Ok(())
}

pub fn run(
    adapter: &str,
    prompt: &str,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
    timeout: Duration,
    extra_writable: Option<&Path>,
    spec_root: Option<&Path>,
) -> Result<RunResult> {
    fs::create_dir_all(log_dir)?;
    let log_path = log_dir.join(format!("{tag}.jsonl"));
    let err_path = log_dir.join(format!("{tag}.stderr.log"));
    let mut cmd = build_command(adapter, prompt, workdir, extra_writable, spec_root)?;
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let log_file = File::create(&log_path)?;
    let err_file = File::create(&err_path)?;

    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {adapter} 失败"))?;

    // stdout/stderr 都是落盘日志：分别并发排水，逐行打码且保留各自行结构与顺序。
    let stdout = child.stdout.take().context("stdout pipe 未配置")?;
    let stderr = child.stderr.take().context("stderr pipe 未配置")?;
    let stdout_thread = thread::spawn(move || redact_stream(stdout, log_file));
    let stderr_thread = thread::spawn(move || redact_stream(stderr, err_file));

    let (status, timed_out) = match child.wait_timeout(timeout)? {
        Some(status) => (status, false),
        None => {
            child.kill().ok();
            let status = child.wait().context("等待超时子进程退出失败")?;
            (status, true)
        }
    };
    stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stdout redact 线程 panic"))?
        .context("stdout 日志打码落盘失败")?;
    stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stderr redact 线程 panic"))?
        .context("stderr 日志打码落盘失败")?;
    if timed_out {
        bail!(
            "{adapter} 超时（>{}s）已 kill；日志: {}",
            timeout.as_secs(),
            log_path.display()
        );
    }
    let duration_secs = start.elapsed().as_secs();
    let evidence = extract_execution_evidence(adapter, &log_path);

    // 非零 exit 的真实 failure 证据不得被"缺 model"覆盖：
    // 只在 exit==0 且 adapter 有 pinned model 时调 validate_model_identity；
    // 非零 exit 或无 pinned model（既有 adapter/AdapterSpec）的跳过 identity 校验。
    if status.success() && pinned_model(adapter).is_some() {
        validate_model_identity(adapter, &evidence).with_context(|| {
            format!(
                "{adapter} 模型身份校验失败（exit 0）；日志: {}",
                log_path.display()
            )
        })?;
    }

    Ok(RunResult {
        exit_code: status.code().unwrap_or(-1),
        duration_secs,
        usage: evidence.usage.clone(),
        log_path: log_path.display().to_string(),
        stderr_log_path: err_path.display().to_string(),
        evidence,
    })
}

/// 尽力扫描 JSONL 日志提取 usage：兼容三家已实测形状；未知形状返回 None（容错 R6）。
/// B96 后被 extract_execution_evidence 取代，但内联测试仍直接引用此函数验证既有形状。
#[cfg(test)]
fn extract_usage(adapter: &str, log_path: &Path) -> Option<serde_json::Value> {
    // Cursor stream-json 的 usage 形状尚未真机实测；待真机实测回填，严禁借用别家字段名。
    if adapter == "cursor" {
        return None;
    }
    let text = fs::read_to_string(log_path).ok()?;
    let mut last: Option<serde_json::Value> = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // codex: {"type":"turn.completed","usage":{...}}
        if v.get("type").and_then(|t| t.as_str()) == Some("turn.completed") {
            if let Some(u) = v.get("usage") {
                last = Some(u.clone());
            }
        }
        // claude: {"type":"result",...,"total_cost_usd":..,"usage":{..}}
        if v.get("type").and_then(|t| t.as_str()) == Some("result") {
            last = Some(serde_json::json!({
                "usage": v.get("usage"),
                "total_cost_usd": v.get("total_cost_usd"),
            }));
        }
        // opencode: {"type":"step_finish","part":{"tokens":{...},"cost":..}}
        if v.get("type").and_then(|t| t.as_str()) == Some("step_finish") {
            if let Some(tok) = v.pointer("/part/tokens") {
                last = Some(serde_json::json!({
                    "tokens": tok,
                    "cost": v.pointer("/part/cost"),
                }));
            }
        }
    }
    last
}

/// agent 名 → adapter 名的集中映射（r44/B96）。
/// `executor-cli→opencode`、`executor-mimo→mimo`、`executor-codebuddy→codebuddy`；
/// 其余直接同名（如 `codex`→`codex`）。
pub fn adapter_name_for_agent(agent: &str) -> &str {
    match agent {
        "executor-cli" => "opencode",
        "executor-mimo" => "mimo",
        "executor-codebuddy" => "codebuddy",
        other => other,
    }
}

/// 每个 adapter 的 pinned 模型（argv `--model` 值，用于 requested_model）。
fn pinned_model(adapter: &str) -> Option<&'static str> {
    match adapter {
        "mimo" => Some("xiaomi/mimo-v2.5-pro"),
        "codebuddy" => Some("glm-5.2"),
        _ => None,
    }
}

/// 结构化执行证据：从 CLI JSON 输出提取的只读字段（r44/B96）。
/// 普通 assistant 正文/REPORT §0/argv requested 都不得冒充 observed。
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterExecutionEvidence {
    pub requested_model: Option<String>,
    pub observed_model: Option<String>,
    pub session_id: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub terminal_result: Option<String>,
}

fn trimmed_non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 从 CLI JSON 输出文件提取结构化执行证据。
/// 同时接受 NDJSON（每行一个 JSON 对象）和单 JSON array（`[{...},{...}]`）。
/// 只读明确结构化 terminal/provider 字段，不扫普通正文。
pub fn extract_execution_evidence(adapter: &str, log_path: &Path) -> AdapterExecutionEvidence {
    let requested_model = pinned_model(adapter).map(String::from);
    let text = match fs::read_to_string(log_path) {
        Ok(t) => t,
        Err(_) => {
            return AdapterExecutionEvidence {
                requested_model,
                observed_model: None,
                session_id: None,
                usage: None,
                terminal_result: None,
            };
        }
    };

    // 解析为 JSON 值序列（NDJSON 逐行 或 单 JSON array）
    let values: Vec<serde_json::Value> = if text.trim_start().starts_with('[') {
        // JSON array
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(serde_json::Value::Array(arr)) => arr,
            _ => Vec::new(),
        }
    } else {
        // NDJSON
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .collect()
    };

    let mut observed_models: Vec<String> = Vec::new();
    let mut session_id = None;
    let mut usage = None;
    let mut terminal_result = None;
    let provider = adapter.strip_prefix("consult-").unwrap_or(adapter);

    for v in &values {
        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        // MiMo: step_finish.part.model/modelID/providerID（结构化 terminal event）
        if adapter == "mimo" && event_type == "step_finish" {
            let model = trimmed_non_empty(v.pointer("/part/model").and_then(|m| m.as_str()))
                .or_else(|| trimmed_non_empty(v.pointer("/part/modelID").and_then(|m| m.as_str())))
                .or_else(|| {
                    trimmed_non_empty(v.pointer("/part/providerID").and_then(|m| m.as_str()))
                });
            if let Some(model) = model {
                observed_models.push(model);
            }
            if let Some(tok) = v.pointer("/part/tokens") {
                usage = Some(serde_json::json!({
                    "tokens": tok,
                    "cost": v.pointer("/part/cost"),
                }));
            }
            if let Some(text) = v.pointer("/part/text").and_then(|t| t.as_str()) {
                terminal_result = Some(text.to_string());
            }
        }

        // CodeBuddy: result.providerData.model（结构化 terminal event）
        if adapter == "codebuddy" && event_type == "result" {
            if let Some(model) =
                trimmed_non_empty(v.pointer("/providerData/model").and_then(|m| m.as_str()))
            {
                observed_models.push(model);
            }
            if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                session_id = Some(sid.to_string());
            }
            if let Some(u) = v.get("usage") {
                usage = Some(serde_json::json!({
                    "usage": u,
                    "total_cost_usd": v.get("total_cost_usd"),
                }));
            }
            if let Some(result) = v.get("result").and_then(|r| r.as_str()) {
                terminal_result = Some(result.to_string());
            }
        }

        // Consult model identity is trusted only from provider envelopes whose
        // exact event shape has been measured. Wrong event types, generic JSON
        // pointers, prose, and AGY self-report remain untrusted.
        let observed_model = match provider {
            "codex" if event_type == "thread.started" => {
                trimmed_non_empty(v.get("model").and_then(serde_json::Value::as_str))
            }
            "opencode"
                if event_type == "step_start"
                    && v.pointer("/part/type").and_then(serde_json::Value::as_str)
                        == Some("step-start") =>
            {
                trimmed_non_empty(
                    v.pointer("/part/modelID")
                        .and_then(serde_json::Value::as_str),
                )
            }
            "claude"
                if event_type == "system"
                    && v.get("subtype").and_then(serde_json::Value::as_str) == Some("init") =>
            {
                trimmed_non_empty(v.get("model").and_then(serde_json::Value::as_str))
            }
            _ => None,
        };
        if let Some(model) = observed_model {
            observed_models.push(model);
        }

        // 既有 adapter 的 usage 提取（codex/claude/opencode 兼容）
        if adapter != "mimo" && adapter != "codebuddy" {
            if event_type == "turn.completed" {
                if let Some(u) = v.get("usage") {
                    usage = Some(u.clone());
                }
            }
            if event_type == "result" {
                usage = Some(serde_json::json!({
                    "usage": v.get("usage"),
                    "total_cost_usd": v.get("total_cost_usd"),
                }));
            }
            if event_type == "step_finish" {
                if let Some(tok) = v.pointer("/part/tokens") {
                    usage = Some(serde_json::json!({
                        "tokens": tok,
                        "cost": v.pointer("/part/cost"),
                    }));
                }
            }
        }
    }

    // observed_model: 去重后若唯一则 Some；多个不同（mixed）→ None（validate 判 missing → Err）
    let observed_model = if observed_models.is_empty() {
        None
    } else {
        let set: std::collections::HashSet<&str> =
            observed_models.iter().map(|s| s.as_str()).collect();
        if set.len() == 1 {
            Some(observed_models[0].clone())
        } else {
            None
        }
    };

    AdapterExecutionEvidence {
        requested_model,
        observed_model,
        session_id,
        usage,
        terminal_result,
    }
}

/// 模型身份 fail-closed 校验（r44/B96）。
/// - observed_model 缺失 → Err
/// - observed_model 与 requested_model 不同 → Err
/// （mixed 已在 extract 时被压为 None → 触发 missing 分支）
pub fn validate_model_identity(_adapter: &str, evidence: &AdapterExecutionEvidence) -> Result<()> {
    let observed = evidence.observed_model.as_deref();
    let requested = evidence.requested_model.as_deref();

    match observed {
        None => bail!("模型身份校验失败：无 observed model（identity protocol failure）"),
        Some(obs) => {
            if let Some(req) = requested {
                if obs != req {
                    bail!("模型身份校验失败：observed={obs} ≠ requested={req}（model mismatch）");
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|part| part.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn builtin_list_and_command_dispatch_stay_in_sync() {
        assert_eq!(
            supported_builtins(),
            vec!["opencode", "codex", "claude", "cursor", "mimo", "codebuddy"]
        );
        for adapter in supported_builtins() {
            build_command(adapter, "probe", Path::new("/tmp"), None, None)
                .unwrap_or_else(|error| panic!("{adapter} listed but not constructible: {error}"));
        }
    }

    #[test]
    fn existing_builtin_argv_contracts_do_not_regress() {
        let workdir = Path::new("/tmp");

        let opencode = build_command("opencode", "probe", workdir, None, None).unwrap();
        assert_eq!(
            argv(&opencode),
            ["opencode", "run", "probe", "--format", "json", "--dir", "/tmp"]
        );

        let codex = build_command("codex", "probe", workdir, None, None).unwrap();
        assert_eq!(
            argv(&codex),
            ["codex", "exec", "probe", "--json", "--skip-git-repo-check"]
        );
        assert_eq!(codex.get_current_dir(), Some(workdir));

        let claude = build_command("claude", "probe", workdir, None, None).unwrap();
        assert_eq!(
            argv(&claude),
            [
                "claude",
                "-p",
                "probe",
                "--output-format",
                "stream-json",
                "--verbose"
            ]
        );
        assert_eq!(claude.get_current_dir(), Some(workdir));
    }

    #[test]
    fn cursor_usage_is_none_until_its_stream_shape_is_measured() {
        let scratch = crate::util::test_scratch_dir("cursor-usage-shape");
        let log = scratch.join("usage.jsonl");
        fs::write(
            &log,
            "{\"type\":\"result\",\"usage\":{\"invented\":1},\"total_cost_usd\":99}\n",
        )
        .unwrap();

        assert_eq!(extract_usage("cursor", &log), None);
        assert!(extract_usage("claude", &log).is_some());
        fs::remove_dir_all(scratch).unwrap();
    }

    #[test]
    fn redact_stream_preserves_line_endings_and_unterminated_tail() {
        let input = b"API_TOKEN=abc\r\nsafe\nBearer secret";
        let mut output = Vec::new();
        redact_stream(&input[..], &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "API_TOKEN=***\r\nsafe\nBearer ***"
        );
    }

    #[test]
    fn run_redacts_both_stdout_and_stderr_logs() {
        let root = crate::util::test_scratch_dir("adapter-redact");
        let spec_dir = root.join("coordination/adapters");
        let log_dir = root.join("logs");
        fs::create_dir_all(&spec_dir).unwrap();
        fs::write(
            spec_dir.join("redact-probe.yaml"),
            "launch:\n  argv:\n    - /bin/sh\n    - -c\n    - \"echo API_TOKEN=out; echo 'Bearer errsecret' >&2\"\n",
        )
        .unwrap();

        let result = run(
            "redact-probe",
            "unused",
            &root,
            &log_dir,
            "probe",
            Duration::from_secs(5),
            None,
            Some(&root),
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(
            fs::read_to_string(log_dir.join("probe.jsonl")).unwrap(),
            "API_TOKEN=***\n"
        );
        assert_eq!(
            fs::read_to_string(log_dir.join("probe.stderr.log")).unwrap(),
            "Bearer ***\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn evidence_from(adapter: &str, body: &str) -> AdapterExecutionEvidence {
        let root = crate::util::test_scratch_dir("b96-inline");
        let path = root.join("out.jsonl");
        fs::write(&path, body).unwrap();
        let ev = extract_execution_evidence(adapter, &path);
        fs::remove_dir_all(&root).unwrap();
        ev
    }

    #[test]
    fn mimo_step_finish_modelid_is_accepted_as_observed() {
        let body = "{\"type\":\"step_finish\",\"part\":{\"modelID\":\"xiaomi/mimo-v2.5-pro\",\"tokens\":{\"input\":1,\"output\":1},\"text\":\"ok\"}}\n";
        let ev = evidence_from("mimo", body);
        assert_eq!(ev.observed_model.as_deref(), Some("xiaomi/mimo-v2.5-pro"));
    }

    #[test]
    fn mimo_step_finish_providerid_is_accepted_as_observed() {
        let body = "{\"type\":\"step_finish\",\"part\":{\"providerID\":\"xiaomi/mimo-v2.5-pro\",\"tokens\":{\"input\":1,\"output\":1},\"text\":\"ok\"}}\n";
        let ev = evidence_from("mimo", body);
        assert_eq!(ev.observed_model.as_deref(), Some("xiaomi/mimo-v2.5-pro"));
    }

    #[test]
    fn consult_provider_models_only_come_from_structured_fields() {
        let codex = evidence_from(
            "consult-codex",
            "{\"type\":\"thread.started\",\"thread_id\":\"t1\",\"model\":\"gpt-5.6-codex\"}\n",
        );
        assert_eq!(codex.observed_model.as_deref(), Some("gpt-5.6-codex"));

        let opencode = evidence_from(
            "consult-opencode",
            "{\"type\":\"step_start\",\"part\":{\"type\":\"step-start\",\"modelID\":\"glm-5.2\"}}\n",
        );
        assert_eq!(opencode.observed_model.as_deref(), Some("glm-5.2"));

        let claude = evidence_from(
            "consult-claude",
            "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-opus-4-6\"}\n",
        );
        assert_eq!(claude.observed_model.as_deref(), Some("claude-opus-4-6"));

        let missing = evidence_from(
            "consult-codex",
            "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"I used model guessed-by-prose\"}}\n",
        );
        assert_eq!(
            missing.observed_model, None,
            "assistant prose and absent fields must stay null"
        );

        let plain_agy = evidence_from("consult-agy", "AGY FINAL ANSWER\n");
        assert_eq!(
            plain_agy.observed_model, None,
            "plain-text agy output cannot be promoted to observed identity"
        );
    }

    #[test]
    fn consult_provider_models_reject_wrong_envelopes_and_agy_self_report() {
        let wrong_codex = evidence_from(
            "consult-codex",
            "{\"type\":\"item.completed\",\"model\":\"forged\",\"item\":{\"type\":\"agent_message\",\"text\":\"model is forged\"}}\n",
        );
        assert_eq!(wrong_codex.observed_model, None);

        let wrong_opencode = evidence_from(
            "consult-opencode",
            "{\"type\":\"step_start\",\"modelID\":\"top-level\",\"part\":{\"type\":\"wrong\",\"modelID\":\"nested\"}}\n",
        );
        assert_eq!(wrong_opencode.observed_model, None);

        let wrong_claude = evidence_from(
            "consult-claude",
            "{\"type\":\"system\",\"subtype\":\"status\",\"model\":\"forged\"}\n{\"type\":\"assistant\",\"message\":{\"model\":\"nested\"}}\n",
        );
        assert_eq!(wrong_claude.observed_model, None);

        let agy_self_report = evidence_from(
            "consult-agy",
            "AGY FINAL ANSWER\n{\"type\":\"system\",\"model\":\"self-reported\",\"modelID\":\"also-self-reported\"}\n",
        );
        assert_eq!(agy_self_report.observed_model, None);
    }

    #[test]
    fn consult_provider_models_trim_drop_blanks_dedupe_and_fail_closed_on_conflict() {
        let blank = evidence_from(
            "consult-codex",
            "{\"type\":\"thread.started\",\"model\":\"   \"}\n",
        );
        assert_eq!(blank.observed_model, None);

        let duplicate = evidence_from(
            "consult-opencode",
            "{\"type\":\"step_start\",\"part\":{\"type\":\"step-start\",\"modelID\":\" glm-5.2 \"}}\n{\"type\":\"step_start\",\"part\":{\"type\":\"step-start\",\"modelID\":\"glm-5.2\"}}\n",
        );
        assert_eq!(duplicate.observed_model.as_deref(), Some("glm-5.2"));

        let conflict = evidence_from(
            "consult-claude",
            "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-a\"}\n{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-b\"}\n",
        );
        assert_eq!(
            conflict.observed_model, None,
            "different trusted envelope values must fail closed"
        );
    }
}
