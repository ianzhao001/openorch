#!/bin/sh
# wake-pi-stream.sh <message> -- executor-pi structured proof projection.
#
# Reserved wrapper outcomes. Provider-native non-zero codes are preserved as
# failed exact reasons and therefore deliberately remain outside this manifest.
# orch-exit-code: 0 answered exact-terminal
# orch-exit-code: 3 failed launch-failure
# orch-exit-code: 64 failed invalid-arguments
# orch-exit-code: 65 failed invalid-workspace
# orch-exit-code: 66 failed invalid-envelope
# orch-exit-code: 70 empty zero-frame-eof
# orch-exit-code: 71 empty truncated-no-terminal
# orch-exit-code: 72 timedOut hard-deadline
#
# WHY THIS LIVES UNDER orch/ AND NOT coordination/scripts/:
# this is project-specific glue, but `coordination/**` is a frozen path for
# executors, so a card can never put a wake wrapper there in its writeSet.
# Keeping the streaming wrappers under `orch/scripts/` is what makes them
# deliverable by a normal seeded-red card (B228 precedent). `agents.yaml`
# points at these paths. Do not "tidy" them into coordination/ -- that breaks
# both the writeSet mechanism and the registry.
#
# Production launch shape matches the legacy wrapper: pi -p MESSAGE --mode json
# --provider PROVIDER --model MODEL --thinking EFFORT, in ORCH_PI_CWD. Every
# value is a signed runtime pin; this wrapper deliberately has no local
# provider/model fallback.
# This layer emits only lifecycle markers and completed tool output needed by
# orch's review-consumption proof. Prompt and assistant message frames are never
# copied to the wake log.

msg="$1"
[ -n "$msg" ] || { echo "usage: wake-pi-stream.sh <message>  (env: ORCH_PI_CWD, ORCH_PI_TIMEOUT)" >&2; exit 64; }

MSG="$msg" python3 - <<'PY'
import json
import os
import subprocess
import sys
import threading
import time


def diag(text):
    print("[wake-pi-stream] " + text, file=sys.stderr, flush=True)


ENVELOPE_KEYS = (
    "ORCH_HARNESS_ID",
    "ORCH_HARNESS_ACTION_ID",
    "ORCH_HARNESS_WAKE_ID",
    "ORCH_HARNESS_ROUND",
    "ORCH_HARNESS_TASK_ID",
    "ORCH_HARNESS_ATTEMPT_ID",
    "ORCH_HARNESS_ROLE",
    "ORCH_HARNESS_CWD",
    "ORCH_HARNESS_FIXED_HEAD",
    "ORCH_HARNESS_PROVIDER",
    "ORCH_HARNESS_MODEL",
    "ORCH_HARNESS_EFFORT",
    "ORCH_HARNESS_PROVIDER_BIN",
    "ORCH_HARNESS_REVIEW_OUTPUT_PATH",
    "ORCH_HARNESS_ORCH_BIN",
    "ORCH_HARNESS_DEADLINE_SECS",
)


def exact_executable(value, key):
    if not isinstance(value, str) or not os.path.isabs(value):
        diag("%s 必须是绝对可执行路径: %r" % (key, value))
        sys.exit(66)
    resolved = os.path.realpath(value)
    if not os.path.isfile(resolved) or not os.access(resolved, os.X_OK):
        diag("%s 不存在或不可执行: %s" % (key, value))
        sys.exit(66)
    return resolved


def legacy_conflict(alias, envelope_key, selected):
    legacy = os.environ.get(alias)
    if legacy is not None and legacy != selected:
        diag("legacy alias conflict: %s ignored; %s wins" % (alias, envelope_key))


present_envelope_keys = [key for key in ENVELOPE_KEYS if key in os.environ]
if present_envelope_keys:
    missing = [
        key
        for key in ENVELOPE_KEYS
        if not isinstance(os.environ.get(key), str) or not os.environ[key].strip()
    ]
    if missing:
        diag("incomplete invocation envelope; missing/blank: %s" % ",".join(missing))
        sys.exit(66)
    if os.environ["ORCH_HARNESS_ID"] != "pi":
        diag("ORCH_HARNESS_ID 与 pi wrapper 不匹配")
        sys.exit(66)
    workdir = os.environ["ORCH_HARNESS_CWD"]
    provider = os.environ["ORCH_HARNESS_PROVIDER"]
    model = os.environ["ORCH_HARNESS_MODEL"]
    effort = os.environ["ORCH_HARNESS_EFFORT"]
    provider_bin = exact_executable(
        os.environ["ORCH_HARNESS_PROVIDER_BIN"], "ORCH_HARNESS_PROVIDER_BIN"
    )
    timeout_text = os.environ["ORCH_HARNESS_DEADLINE_SECS"]
    for alias, envelope_key, selected in (
        ("ORCH_PI_CWD", "ORCH_HARNESS_CWD", workdir),
        ("ORCH_PI_PROVIDER", "ORCH_HARNESS_PROVIDER", provider),
        ("ORCH_PI_MODEL", "ORCH_HARNESS_MODEL", model),
        ("ORCH_PI_EFFORT", "ORCH_HARNESS_EFFORT", effort),
        ("ORCH_PI_BIN", "ORCH_HARNESS_PROVIDER_BIN", provider_bin),
        ("ORCH_PI_TIMEOUT", "ORCH_HARNESS_DEADLINE_SECS", timeout_text),
    ):
        legacy_conflict(alias, envelope_key, selected)
else:
    workdir = os.environ.get("ORCH_PI_CWD")
    if not workdir:
        try:
            git_common = subprocess.check_output(
                ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
                text=True,
            ).strip()
        except Exception as exc:
            diag("无法解析 legacy 工作目录: %s" % exc)
            sys.exit(65)
        workdir = os.path.dirname(git_common)
    provider = os.environ.get("ORCH_PI_PROVIDER")
    model = os.environ.get("ORCH_PI_MODEL")
    effort = os.environ.get("ORCH_PI_EFFORT")
    provider_bin = os.environ.get("ORCH_PI_BIN") or "pi"
    timeout_text = os.environ.get("ORCH_PI_TIMEOUT") or "7200"

if not os.path.isdir(workdir):
    diag("ORCH_HARNESS_CWD/ORCH_PI_CWD 不是目录: %s" % workdir)
    sys.exit(65)
try:
    timeout = float(timeout_text)
except ValueError:
    diag("调用 deadline 必须是秒数")
    sys.exit(64)
if timeout <= 0:
    diag("调用 deadline 必须大于 0")
    sys.exit(64)


def bounded_tool_result(text, max_bytes=64 * 1024):
    value = "" if text is None else str(text)
    frame = {"type": "tool_result", "output": value}
    encoded = json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if len(encoded) <= max_bytes:
        return encoded.decode("utf-8")
    marker = " [truncated]"
    low = 0
    high = len(value)
    best = None
    while low <= high:
        middle = (low + high) // 2
        frame = {"type": "tool_result", "output": value[:middle] + marker, "truncated": True}
        encoded = json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        if len(encoded) <= max_bytes:
            best = encoded.decode("utf-8")
            low = middle + 1
        else:
            high = middle - 1
    if best is not None:
        return best
    return json.dumps(
        {"type": "tool_result", "output": marker, "truncated": True},
        ensure_ascii=False,
        separators=(",", ":"),
    )


def completed_text(result):
    if not isinstance(result, dict):
        return None
    content = result.get("content")
    if not isinstance(content, list):
        return None
    chunks = []
    for item in content:
        if isinstance(item, dict) and item.get("type") == "text" and isinstance(item.get("text"), str):
            chunks.append(item["text"])
    return "\n".join(chunks) if chunks else None


missing = [
    label
    for label, value in (("provider", provider), ("model", model), ("effort", effort))
    if not isinstance(value, str) or not value.strip() or value != value.strip()
]
if missing:
    diag("缺少或无效的 signed pin (%s)，在启动 pi 前拒绝运行" % ",".join(missing))
    sys.exit(66)
argv = [
    provider_bin,
    "-p",
    os.environ["MSG"],
    "--mode",
    "json",
    "--provider",
    provider,
    "--model",
    model,
    "--thinking",
    effort,
]

diag("cwd=%s provider=%s model=%s effort=%s timeout=%.0fs" % (workdir, provider, model, effort, timeout))
try:
    proc = subprocess.Popen(
        argv,
        cwd=workdir,
        stdout=subprocess.PIPE,
        stderr=None,
        text=True,
        bufsize=1,
    )
except Exception as exc:
    diag("pi 启动失败: %s" % exc)
    sys.exit(3)

frames = 0
projected = 0
terminal_seen = False
session_id = None
cause = "eof"
deadline = time.time() + timeout
heartbeat_interval = 15.0
heartbeat_started = time.monotonic()
last_provider_frame = heartbeat_started
heartbeat_stop = threading.Event()
state_lock = threading.Lock()
output_lock = threading.Lock()


def emit_stdout(line):
    with output_lock:
        print(line, flush=True)


def kill_on_timeout():
    while proc.poll() is None:
        if time.time() >= deadline:
            diag("读超时 (%.0fs)，终止 pi" % timeout)
            proc.kill()
            return
        time.sleep(1)


threading.Thread(target=kill_on_timeout, daemon=True).start()


def emit_heartbeats():
    next_tick = heartbeat_started + heartbeat_interval
    while True:
        remaining = max(0.0, next_tick - time.monotonic())
        if heartbeat_stop.wait(remaining):
            return
        now = time.monotonic()
        if proc.poll() is not None:
            return
        with state_lock:
            quiet_for = now - last_provider_frame
            observed_frames = frames
            inflight = int(not terminal_seen)
        if quiet_for >= heartbeat_interval:
            emit_stdout(
                "[wake-hb] t=+%ds provider=%d frames=%d inflight=%d"
                % (int(now - heartbeat_started), proc.pid, observed_frames, inflight)
            )
        next_tick = now + heartbeat_interval


heartbeat_thread = threading.Thread(target=emit_heartbeats, daemon=True)
heartbeat_thread.start()

for line in proc.stdout:
    stripped = line.strip()
    if not stripped:
        continue
    with state_lock:
        frames += 1
        last_provider_frame = time.monotonic()
    try:
        event = json.loads(stripped)
    except Exception:
        continue
    if not isinstance(event, dict):
        continue
    kind = event.get("type")
    if kind == "session" and event.get("id"):
        session_id = event["id"]
    elif kind == "turn_start":
        emit_stdout("turn.started")
    elif kind == "tool_execution_end":
        text = completed_text(event.get("result"))
        if text is not None:
            emit_stdout(bounded_tool_result(text))
            projected += 1
    elif kind == "agent_settled":
        with state_lock:
            terminal_seen = True
        # Preserve the B225 review-consumption marker and its established
        # ordering before appending B283's signed backend facts.
        emit_stdout(stripped)
        if session_id:
            emit_stdout(
                json.dumps(
                    {
                        "type": "pi.session",
                        "sessionId": session_id,
                        "provider": provider,
                        "model": model,
                        "effort": effort,
                    },
                    ensure_ascii=False,
                    separators=(",", ":"),
                )
            )
        emit_stdout(
            json.dumps(
                {
                    "type": "pi.terminal",
                    "sessionId": session_id,
                    "provider": provider,
                    "model": model,
                    "effort": effort,
                    "exactReason": "agent_settled",
                    "usage": event.get("usage")
                    if isinstance(event.get("usage"), dict)
                    else None,
                    "usageAbsentReason": None
                    if isinstance(event.get("usage"), dict)
                    else "pi agent_settled frame omitted usage",
                },
                ensure_ascii=False,
                separators=(",", ":"),
            )
        )
        cause = "terminal"

proc.wait()
heartbeat_stop.set()
heartbeat_thread.join()
if time.time() >= deadline and not terminal_seen:
    cause = "timeout"
if session_id:
    diag("session=%s" % session_id)
if terminal_seen and proc.returncode == 0:
    diag("frames=%d projected=%d terminal=yes cause=%s rc=0 exit=0" % (frames, projected, cause))
    sys.exit(0)
if terminal_seen and proc.returncode is not None:
    code = proc.returncode if 0 < proc.returncode <= 255 else 1
    diag("frames=%d projected=%d terminal=yes cause=provider-exit rc=%s exit=%d" % (frames, projected, proc.returncode, code))
    sys.exit(code)
if frames == 0:
    code = 72 if cause == "timeout" else 70
else:
    code = 72 if cause == "timeout" else 71
diag("frames=%d projected=%d terminal=no cause=%s rc=%s exit=%d" % (frames, projected, cause, proc.returncode, code))
sys.exit(code)
PY
