#!/bin/sh
# wake-zcode-stream.sh <message> —— executor-zcode 流式转写层
#
# 设计目标（B228）：
# - 保留 launch argv 不变：node <bundle>/zcode.cjs -p "$MSG" --json --cwd "$WORKDIR"。
# - 以 --json 会话落盘文件（~/.zcode/cli/rollout/*.jsonl）为证据输入，不再依赖 stderr。
# - 仅投影 role=="tool" 的内容到 wake 日志（按 messageCount 增量逐条输出单行帧）。
# - 每帧有界；超限打 truncation 标记，避免 prompt/response 泄露与“日志暴风”。
#
# 为什么放在 orch/ 而不是 coordination/scripts/：
# 这是本项目专用的胶水，但 `coordination/**` 对 executor 是冻结路径，卡的 writeSet
# 永远放不进 wake 包装脚本。把流式包装放在 `orch/scripts/` 才能由一张正常的 seeded-red
# 卡交付（B228 先例）；`agents.yaml` 直接指向这些路径。
# 不要把它们“整理”回 coordination/——那会同时打断 writeSet 机制和注册表。

msg="$1"
[ -n "$msg" ] || { echo "usage: wake-zcode-stream.sh <message>  (env: ORCH_ZCODE_CWD, ORCH_ZCODE_ROLLOUT_DIR, ORCH_ZCODE_TIMEOUT)" >&2; exit 64; }

bin="${ORCH_ZCODE_BIN:-/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs}"
[ -f "$bin" ] || { echo "[wake-zcode-stream] 入口不存在: $bin（app 升级会改路径，用 ORCH_ZCODE_BIN 覆盖）" >&2; exit 3; }
[ -f "$HOME/.zcode/cli/config.json" ] || { echo "[wake-zcode-stream] 缺 ~/.zcode/cli/config.json——模型/深度无处钉死，拒绝运行（模板见 research/zcode-cli-probe.md）" >&2; exit 3; }

workdir="$ORCH_ZCODE_CWD"
if [ -z "$workdir" ]; then
  gitcommon="$(git rev-parse --path-format=absolute --git-common-dir)" || exit 65
  workdir="$(dirname "$gitcommon")"
fi
[ -d "$workdir" ] || { echo "[wake-zcode-stream] ORCH_ZCODE_CWD 不是目录: $workdir" >&2; exit 65; }

rollout_dir="${ORCH_ZCODE_ROLLOUT_DIR:-$HOME/.zcode/cli/rollout}"
[ -d "$rollout_dir" ] || { echo "[wake-zcode-stream] roll-out 目录不可读: $rollout_dir" >&2; exit 65; }

MSG="$msg" WORKDIR="$workdir" ROLL_DIR="$rollout_dir" ZBIN="$bin" python3 - <<'PY'
import glob
import json
import os
import subprocess
import sys
import time


def diag(text):
    print("[wake-zcode-stream] " + text, file=sys.stderr, flush=True)


def frame_payload(raw_output, max_bytes):
    text = "" if raw_output is None else str(raw_output)
    frame = {"type": "tool_result", "output": text}
    encoded = json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if len(encoded) <= max_bytes:
        return encoded.decode("utf-8"), False
    marker = " [truncated]"
    while len(text) > 0:
        candidate = {"type": "tool_result", "output": f"{text}{marker}", "truncated": True}
        encoded = json.dumps(candidate, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        if len(encoded) <= max_bytes:
            return encoded.decode("utf-8"), True
        text = text[:-1]
    fallback = {"type": "tool_result", "output": f"{marker}", "truncated": True}
    return json.dumps(fallback, ensure_ascii=False, separators=(",", ":")), True


def session_cwd_from_record(record):
    request = record.get("request")
    if not isinstance(request, dict):
        return None
    for key in ("cwd", "workdir", "workingDirectory"):
        value = request.get(key)
        if isinstance(value, str) and value:
            return os.path.realpath(value)
    return None


workdir = os.environ["WORKDIR"]
bundle = os.environ["ZBIN"]
rollout_dir = os.environ["ROLL_DIR"]
timeout = float(os.environ.get("ORCH_ZCODE_TIMEOUT") or 7200)
argv = ["node", bundle, "-p", os.environ["MSG"], "--json", "--cwd", workdir]
max_frame_bytes = 64 * 1024
deadline = time.time() + timeout
script_started = time.time()

diag("cwd=%s bundle=%s rollout=%s timeout=%.0fs" % (workdir, bundle, rollout_dir, timeout))
baseline_sessions = {}
for path in glob.glob(os.path.join(rollout_dir, "model-io-*.jsonl")):
    stat = os.stat(path)
    baseline_sessions[path] = (stat.st_size, stat.st_mtime)

try:
    process = subprocess.Popen(
        argv,
        cwd=workdir,
        stdout=subprocess.PIPE,
        stderr=None,
        text=True,
        bufsize=1,
    )
except Exception as exc:
    diag(f"zcode 启动失败: {exc}")
    sys.exit(3)

states = {}
seen_turn_started = False
timed_out = False
heartbeat_interval = 15.0
heartbeat_started = time.monotonic()
last_provider_frame = heartbeat_started
next_heartbeat = heartbeat_started + heartbeat_interval
provider_frames = 0


def read_new_records(path, state):
    result = []
    with open(path, "r", encoding="utf-8", errors="replace") as stream:
        if state["offset"] > 0:
            stream.seek(state["offset"])
        else:
            stream.seek(0, os.SEEK_SET)
        chunk = stream.read()
    text = state["tail"] + chunk
    lines = text.splitlines()
    if text and not text.endswith("\n") and lines:
        state["tail"] = lines.pop()
    else:
        state["tail"] = ""
    state["offset"] += len(chunk)
    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue
        try:
            result.append(json.loads(stripped))
        except Exception:
            continue
    return result


def emit_frames(records, state):
    global seen_turn_started
    emitted = False
    accepted = 0
    for record in records:
        if not isinstance(record, dict):
            continue
        request = record.get("request")
        if not isinstance(request, dict):
            continue

        recorded_cwd = session_cwd_from_record(record)
        if recorded_cwd is not None:
            canonical = os.path.realpath(recorded_cwd)
            if canonical != os.path.realpath(workdir):
                continue
        accepted += 1

        count = request.get("messageCount")
        messages = request.get("messages")
        if not isinstance(count, int) or count <= 0:
            continue
        if count <= state["count"]:
            continue
        if not isinstance(messages, list):
            continue
        if len(messages) < count:
            count = len(messages)

        # 新增消息按 messageCount 增量。
        for message in messages[state["count"]:count]:
            if not isinstance(message, dict):
                continue
            if message.get("role") != "tool":
                continue
            content = message.get("content", "")
            if not isinstance(content, str):
                content = json.dumps(content, ensure_ascii=False, separators=(",", ":"))
            line, truncated = frame_payload(content, max_frame_bytes)
            print(line, flush=True)
            emitted = True

        state["count"] = count
    if emitted and not seen_turn_started:
        print("turn.started", flush=True)
        seen_turn_started = True
    return accepted


def select_records():
    accepted = 0
    sessions = set(glob.glob(os.path.join(rollout_dir, "model-io-*.jsonl")))
    for session_file in sessions:
        if session_file in states:
            continue
        stat = os.stat(session_file)
        baseline_size, baseline_mtime = baseline_sessions.get(
            session_file, (0, 0.0)
        )
        if baseline_mtime >= script_started - 1.0:
            offset = 0
        else:
            offset = baseline_size
        states[session_file] = {
            "count": 0,
            "offset": offset,
            "size": stat.st_size,
            "tail": "",
        }

    for path in list(states.keys()):
        state = states[path]
        try:
            size = os.path.getsize(path)
        except OSError:
            continue
        if size < state["size"]:
            state["offset"] = 0
            state["count"] = 0
            state["tail"] = ""
        state["size"] = size
        records = read_new_records(path, state)
        if not records:
            continue
        accepted += emit_frames(records, states[path])
    return accepted


provider_frames += select_records()
if provider_frames:
    last_provider_frame = time.monotonic()
    next_heartbeat = last_provider_frame + heartbeat_interval
try:
    while True:
        poll = process.poll()
        new_frames = select_records()
        provider_frames += new_frames
        now = time.monotonic()
        if new_frames:
            last_provider_frame = now
            next_heartbeat = now + heartbeat_interval
        if poll is not None:
            break
        if time.time() >= deadline:
            timed_out = True
            process.kill()
            break
        if now >= next_heartbeat and now - last_provider_frame >= heartbeat_interval:
            print(
                "[wake-hb] t=+%ds provider=%d frames=%d inflight=1"
                % (int(now - heartbeat_started), process.pid, provider_frames),
                flush=True,
            )
            next_heartbeat = now + heartbeat_interval
        time.sleep(0.05)

finally:
    output, _ = process.communicate()

out = output or ""
terminal = None
if out.strip():
    for line in out.splitlines():
        candidate = line.strip()
        if not candidate:
            continue
        try:
            value = json.loads(candidate)
        except Exception:
            continue
        if (
            isinstance(value, dict)
            and value.get("sessionId") is not None
            and value.get("response") is not None
        ):
            terminal = value
            break

elapsed = time.time() - (deadline - timeout)
session_id = terminal.get("sessionId") if terminal else None
usage = terminal.get("usage") if terminal else None
if terminal is not None:
    diag(
        "session=%s tokens=%s elapsed=%.0fs exit=0"
        % (session_id, usage.get("totalTokens") if isinstance(usage, dict) else None, elapsed)
    )
    sys.exit(0)

if timed_out:
    code = 72
elif not out.strip():
    code = 70
else:
    code = 71
diag("frames=%d terminal=no elapsed=%.0fs exit=%d" % (provider_frames, elapsed, code))
sys.exit(code)
PY
