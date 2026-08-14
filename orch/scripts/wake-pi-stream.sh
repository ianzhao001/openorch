#!/bin/sh
# wake-pi-stream.sh <message> -- executor-pi structured proof projection.
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
# --provider PROVIDER --model MODEL --thinking max, in ORCH_PI_CWD.  The r70
# default model is intentionally pinned to deepseek-v4-pro rather than the
# untouched legacy wrapper's flash fallback; the environment override remains.
# This layer emits only lifecycle markers and completed tool output needed by
# orch's review-consumption proof. Prompt and assistant message frames are never
# copied to the wake log.

msg="$1"
[ -n "$msg" ] || { echo "usage: wake-pi-stream.sh <message>  (env: ORCH_PI_CWD, ORCH_PI_TIMEOUT)" >&2; exit 64; }

workdir="$ORCH_PI_CWD"
if [ -z "$workdir" ]; then
  gitcommon="$(git rev-parse --path-format=absolute --git-common-dir)" || exit 65
  workdir="$(dirname "$gitcommon")"
fi
[ -d "$workdir" ] || { echo "[wake-pi-stream] ORCH_PI_CWD 不是目录: $workdir" >&2; exit 65; }

MSG="$msg" WORKDIR="$workdir" python3 - <<'PY'
import json
import os
import subprocess
import sys
import threading
import time


def diag(text):
    print("[wake-pi-stream] " + text, file=sys.stderr, flush=True)


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


workdir = os.environ["WORKDIR"]
timeout = float(os.environ.get("ORCH_PI_TIMEOUT") or 7200)
provider = os.environ.get("ORCH_PI_PROVIDER") or "deepseek"
model = os.environ.get("ORCH_PI_MODEL") or "deepseek-v4-pro"
argv = [
    "pi",
    "-p",
    os.environ["MSG"],
    "--mode",
    "json",
    "--provider",
    provider,
    "--model",
    model,
    "--thinking",
    "max",
]

diag("cwd=%s model=%s timeout=%.0fs" % (workdir, model, timeout))
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
        emit_stdout('{"type":"agent_settled"}')
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
