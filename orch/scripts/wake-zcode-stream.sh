#!/bin/sh
# wake-zcode-stream.sh <message> —— executor-zcode 流式转写层
#
# Reserved wrapper outcomes. Provider-native non-zero codes are preserved as
# failed exact reasons and therefore deliberately remain outside this manifest.
# orch-exit-code: 0 answered exact-terminal
# orch-exit-code: 3 failed launch-failure
# orch-exit-code: 64 failed invalid-arguments
# orch-exit-code: 65 failed invalid-workspace
# orch-exit-code: 66 failed invalid-envelope
# orch-exit-code: 67 failed pin-mismatch
# orch-exit-code: 70 empty zero-frame-eof
# orch-exit-code: 71 empty truncated-no-terminal
# orch-exit-code: 72 timedOut hard-deadline
# orch-exit-code: 74 failed identity-drift
#
# 设计目标（B228）：
# - 保留 launch argv 不变：node <bundle>/zcode.cjs -p "$MSG" --json --cwd "$WORKDIR"。
# - 以 --json 会话落盘文件（~/.zcode/cli/rollout/*.jsonl）为证据输入，不再依赖 stderr。
# - Review/Execute 仅投影完成的 tool 内容；Consult 另投影完整原生终答与摘要。
# - 每帧有界；超限打 truncation 标记，避免 prompt/response 泄露与“日志暴风”。
#
# 为什么放在 orch/ 而不是 coordination/scripts/：
# 这是本项目专用的胶水，但 `coordination/**` 对 executor 是冻结路径，卡的 writeSet
# 永远放不进 wake 包装脚本。把流式包装放在 `orch/scripts/` 才能由一张正常的 seeded-red
# 卡交付（B228 先例）；`agents.yaml` 直接指向这些路径。
# 不要把它们“整理”回 coordination/——那会同时打断 writeSet 机制和注册表。

msg="$1"
[ -n "$msg" ] || { echo "usage: wake-zcode-stream.sh <message>  (env: ORCH_ZCODE_CWD, ORCH_ZCODE_ROLLOUT_DIR, ORCH_ZCODE_TIMEOUT, ORCH_ZCODE_PROVIDER, ORCH_ZCODE_MODEL, ORCH_ZCODE_EFFORT)" >&2; exit 64; }

config_path="${ORCH_ZCODE_CONFIG:-$HOME/.zcode/cli/config.json}"
rollout_dir="${ORCH_ZCODE_ROLLOUT_DIR:-$HOME/.zcode/cli/rollout}"
if [ -z "${ORCH_FUSION_PRIVATE_ROOT:-}" ]; then
  [ -d "$rollout_dir" ] || { echo "[wake-zcode-stream] roll-out 目录不可读: $rollout_dir" >&2; exit 65; }
fi

MSG="$msg" ROLL_DIR="$rollout_dir" ZCONFIG="$config_path" python3 - <<'PY'
import glob
import datetime
import re
import stat
import threading
import hashlib
import json
import os
import subprocess
import sys
import time


def diag(text):
    print("[wake-zcode-stream] " + text, file=sys.stderr, flush=True)


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
envelope_mode = bool(present_envelope_keys)
consult_mode = envelope_mode and os.environ.get("ORCH_HARNESS_ROLE") == "consult"
if envelope_mode:
    missing = [
        key
        for key in ENVELOPE_KEYS
        if not isinstance(os.environ.get(key), str) or not os.environ[key].strip()
    ]
    if missing:
        diag("incomplete invocation envelope; missing/blank: %s" % ",".join(missing))
        sys.exit(66)
    if os.environ["ORCH_HARNESS_ID"] != "zcode":
        diag("ORCH_HARNESS_ID 与 zcode wrapper 不匹配")
        sys.exit(66)
    workdir = os.environ["ORCH_HARNESS_CWD"]
    bundle = exact_executable(
        os.environ["ORCH_HARNESS_PROVIDER_BIN"], "ORCH_HARNESS_PROVIDER_BIN"
    )
    provider = os.environ["ORCH_HARNESS_PROVIDER"]
    model = os.environ["ORCH_HARNESS_MODEL"]
    effort = os.environ["ORCH_HARNESS_EFFORT"]
    timeout_text = os.environ["ORCH_HARNESS_DEADLINE_SECS"]
    for alias, envelope_key, selected in (
        ("ORCH_ZCODE_CWD", "ORCH_HARNESS_CWD", workdir),
        ("ORCH_ZCODE_BIN", "ORCH_HARNESS_PROVIDER_BIN", bundle),
        ("ORCH_ZCODE_PROVIDER", "ORCH_HARNESS_PROVIDER", provider),
        ("ORCH_ZCODE_MODEL", "ORCH_HARNESS_MODEL", model),
        ("ORCH_ZCODE_EFFORT", "ORCH_HARNESS_EFFORT", effort),
        ("ORCH_ZCODE_TIMEOUT", "ORCH_HARNESS_DEADLINE_SECS", timeout_text),
    ):
        legacy_conflict(alias, envelope_key, selected)
else:
    provider = os.environ.get("ORCH_ZCODE_PROVIDER") or ""
    model = os.environ.get("ORCH_ZCODE_MODEL") or ""
    effort = os.environ.get("ORCH_ZCODE_EFFORT") or ""
    bundle = os.environ.get("ORCH_ZCODE_BIN") or "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs"
    if not os.path.isfile(bundle):
        diag("入口不存在: %s（app 升级会改路径，用 ORCH_ZCODE_BIN 覆盖）" % bundle)
        sys.exit(3)
    workdir = os.environ.get("ORCH_ZCODE_CWD")
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
    timeout_text = os.environ.get("ORCH_ZCODE_TIMEOUT") or "7200"

if not os.path.isdir(workdir):
    diag("ORCH_HARNESS_CWD/ORCH_ZCODE_CWD 不是目录: %s" % workdir)
    sys.exit(65)
try:
    timeout = float(timeout_text)
except ValueError:
    diag("调用 deadline 必须是秒数")
    sys.exit(64)
if timeout <= 0:
    diag("调用 deadline 必须大于 0")
    sys.exit(64)


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


def fail_closed(reason):
    diag("signed pin verification failed before zcode launch: " + reason)
    sys.exit(67)


def nonempty_string(value):
    return isinstance(value, str) and value and value == value.strip()


def configured_main_model(value):
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        main = value.get("main")
        return main if isinstance(main, str) else None
    return None


# B228's pre-existing stream contract invokes a local, no-model Node fixture
# directly rather than through a signed runtime wake. Keep that deterministic
# test surface, but admit only its exact inert fixture bytes: an unpinned real
# ZCode bundle still reaches the fail-closed path below before subprocess.Popen.
B228_LOCAL_STUB_SHA256 = "504bc21296fbf8a28471bdf21ca4ca7ac334d0305803f47995face0d9062e39f"


def is_b228_local_stub(bundle, provider, model, effort):
    if any(value for value in (provider, model, effort)):
        return False
    if not os.environ.get("ORCH_ZCODE_ROLLOUT_DIR"):
        return False
    if os.path.basename(bundle) != "zcode-stub.cjs":
        return False
    try:
        with open(bundle, "rb") as fixture_file:
            digest = hashlib.sha256(fixture_file.read()).hexdigest()
    except OSError:
        return False
    return digest == B228_LOCAL_STUB_SHA256


# Inserted into the code-owned ZCode wrapper; only local native evidence is read.
def bounded_native_bytes(path, limit=2 * 1024 * 1024):
    before = os.lstat(path)
    if not stat.S_ISREG(before.st_mode) or before.st_size > limit:
        raise ValueError("native file must be bounded and regular")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(fd, "rb") as handle:
        opened = os.fstat(handle.fileno())
        data = handle.read(limit + 1)
        after = os.fstat(handle.fileno())
    named = os.lstat(path)
    identity = lambda m: (m.st_dev, m.st_ino, m.st_size, m.st_mtime_ns, m.st_mode)
    if len(data) > limit or identity(before) != identity(opened) or identity(opened) != identity(after) or identity(after) != identity(named):
        raise ValueError("native file changed during read")
    return data


def prepare_fusion_settings(config):
    private = os.environ.get("ORCH_FUSION_PRIVATE_ROOT")
    if not private:
        return None
    if not consult_mode or os.environ.get("ORCH_FUSION_ROLE") != "1":
        fail_closed("private role settings require a Fusion Consult envelope")
    try:
        listing = subprocess.check_output(["git", "-C", workdir, "worktree", "list", "--porcelain", "-z"], timeout=5)
        first = listing.split(b"\0", 1)[0]
        if not first.startswith(b"worktree "):
            raise ValueError("missing primary Git root")
        main = os.path.realpath(first[len(b"worktree "):].decode("utf-8"))
        expected = os.path.join(main, ".orch", "fusion-runs")
        if not os.path.isabs(private) or os.path.realpath(private) != private or os.path.commonpath([private, expected]) != expected:
            raise ValueError("private settings root is outside this project's Fusion runs")
        cursor = private
        while cursor != main:
            if not stat.S_ISDIR(os.lstat(cursor).st_mode):
                raise ValueError("private settings ancestor is not a real directory")
            cursor = os.path.dirname(cursor)
        if os.listdir(private):
            raise ValueError("private settings root is not empty")
        top = subprocess.check_output(["git", "-C", workdir, "rev-parse", "--show-toplevel"], text=True, timeout=5).strip()
        ancestors = []
        cursor = os.path.realpath(workdir)
        top = os.path.realpath(top)
        while True:
            ancestors.append(cursor)
            if cursor == top:
                break
            parent = os.path.dirname(cursor)
            if parent == cursor:
                raise ValueError("invocation cwd is outside Git root")
            cursor = parent
        for directory in reversed(ancestors):
            for name in ("zcode.json", os.path.join(".zcode", "config.json")):
                candidate = os.path.join(directory, name)
                if not os.path.lexists(candidate):
                    continue
                project_config = json.loads(bounded_native_bytes(candidate))
                if not isinstance(project_config, dict) or any(key in project_config for key in ("model", "provider", "modelCatalog")):
                    raise ValueError("project model/provider override is not modeled for a private role")
        os.umask(0o077)
        if isinstance(config.get("model"), dict):
            config["model"]["main"] = provider + "/" + model
        else:
            config["model"] = provider + "/" + model
        model_config = config["provider"][provider]["models"][model]
        reasoning = model_config["reasoning"]
        if effort not in reasoning.get("levels", []):
            raise ValueError("selected effort is absent from the native model")
        reasoning["defaultLevel"] = effort
        settings_path = os.path.join(private, "settings.json")
        data = (json.dumps(config, ensure_ascii=False) + "\n").encode("utf-8")
        fd = os.open(settings_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
            identity = os.fstat(handle.fileno())
        storage = os.path.join(private, "storage")
        os.makedirs(os.path.join(storage, "cli", "db"), mode=0o700)
        native_rollout = os.path.join(storage, "cli", "rollout")
        os.mkdir(native_rollout, 0o700)
        print(json.dumps({"type":"zcode.private-settings","path":settings_path,"dev":identity.st_dev,"ino":identity.st_ino,"sha256":hashlib.sha256(data).hexdigest()}, separators=(",", ":")), flush=True)
        return {"settings":settings_path,"storage":storage,"db":os.path.join(storage,"cli","db","db.sqlite"),"rollout":native_rollout}
    except Exception:
        fail_closed("private role settings or higher-priority project configuration could not be verified")


def native_timestamp_ns(value):
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
        return int(parsed.timestamp() * 1000000000) if parsed.tzinfo is not None else None
    except (ValueError, OverflowError):
        return None


def exact_consult_pin(terminal, child_start_ns, child_end_ns):
    session = terminal.get("sessionId")
    trace = terminal.get("traceId")
    if not isinstance(session, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,160}", session):
        return False
    if not isinstance(trace, str) or not trace.strip() or len(trace) > 256:
        return False
    try:
        path = os.path.join(rollout_dir, "model-io-" + session + ".jsonl")
        data = bounded_native_bytes(path, 16 * 1024 * 1024)
        metadata = os.lstat(path)
        if metadata.st_nlink != 1 or metadata.st_mtime_ns + 5000000000 < child_start_ns or metadata.st_mtime_ns > child_end_ns + 5000000000:
            return False
        if not data.endswith(b"\n"):
            return False
        records = [json.loads(line) for line in data.splitlines() if line.strip()]
    except Exception:
        return False
    rows = [r for r in records if isinstance(r, dict) and r.get("querySource") == "main_turn" and r.get("traceId") == trace]
    if not rows:
        return False
    turns = set()
    matches = 0
    response_model = None
    for row in rows:
        if row.get("type") != "model_io" or row.get("sessionId") != session:
            return False
        turn = row.get("turnId")
        if not isinstance(turn, str) or not turn:
            return False
        turns.add(turn)
        start, end = native_timestamp_ns(row.get("startedAt")), native_timestamp_ns(row.get("completedAt"))
        if start is None or end is None or start > end or start + 5000000000 < child_start_ns or end > child_end_ns + 5000000000:
            return False
        selected = row.get("model") or {}
        if selected.get("providerId") != provider or selected.get("modelId") != model or selected.get("variant") != effort:
            return False
        response = row.get("response") or {}
        if row.get("error") is None and isinstance(response.get("modelId"), str) and response.get("modelId") and response.get("text") == terminal.get("response"):
            matches += 1
            response_model = response["modelId"]
    if len(turns) != 1 or matches != 1:
        return False
    return {"provider":provider,"model":model,"effort":effort,"responseModel":response_model,
            "sessionId":session,"traceId":trace,"turnId":next(iter(turns)),"sourceSha256":hashlib.sha256(data).hexdigest(),"sourceBytes":len(data)}


rollout_dir = os.environ["ROLL_DIR"]
config_path = os.environ["ZCONFIG"]
synthetic_b228_fixture = is_b228_local_stub(bundle, provider, model, effort)
private_settings = None
if not synthetic_b228_fixture:
    if not all(nonempty_string(value) for value in (provider, model, effort)):
        fail_closed("provider/model/effort must be non-empty exact strings")
    try:
        config = json.loads(bounded_native_bytes(config_path))
    except Exception as exc:
        fail_closed("config.json is unreadable or invalid JSON: %s" % exc)
    if not isinstance(config, dict):
        fail_closed("config.json root must be an object")
    providers = config.get("provider")
    if not isinstance(providers, dict):
        fail_closed("config.json provider table is missing")
    provider_config = providers.get(provider)
    if not isinstance(provider_config, dict):
        fail_closed("configured provider differs from signed provider")
    private_settings = prepare_fusion_settings(config)
    if private_settings:
        rollout_dir = private_settings["rollout"]
    if configured_main_model(config.get("model")) != "%s/%s" % (provider, model):
        fail_closed("configured main model differs from signed provider/model")
    models = provider_config.get("models")
    model_config = models.get(model) if isinstance(models, dict) else None
    if not isinstance(model_config, dict):
        fail_closed("configured provider does not contain the signed model")
    reasoning = model_config.get("reasoning")
    if not isinstance(reasoning, dict):
        fail_closed("configured signed model has no reasoning policy")
    levels = reasoning.get("levels")
    if not isinstance(levels, list) or effort not in levels:
        fail_closed("configured reasoning levels omit the signed effort")
    if reasoning.get("defaultLevel") != effort:
        fail_closed("configured default reasoning effort differs from signed effort")
    options = reasoning.get("providerOptionsByLevel")
    if not isinstance(options, dict) or not isinstance(options.get(effort), dict):
        fail_closed("configured signed effort lacks providerOptionsByLevel")
    print(
        json.dumps(
            {
                "type": "zcode.pin-verified",
                "provider": provider,
                "model": model,
                "effort": effort,
            },
            ensure_ascii=False,
            separators=(",", ":"),
        ),
        flush=True,
    )
argv = ([bundle] if envelope_mode else ["node", bundle]) + [
    "-p",
    os.environ["MSG"],
    "--json",
    "--cwd",
    workdir,
]
if consult_mode:
    argv.extend(["--mode", "plan"])
child_env = os.environ.copy()
if private_settings:
    argv.extend(["--settings", private_settings["settings"]])
    for key in ("ZCODE_MODEL", "ZCODE_BASE_URL", "ZCODE_STORAGE_DIR", "ZCODE_SESSION_DB", "ZCODE_SESSION_DB_PATH"):
        child_env.pop(key, None)
    child_env["ZCODE_STORAGE_DIR"] = private_settings["storage"]
    child_env["ZCODE_SESSION_DB_PATH"] = private_settings["db"]
max_frame_bytes = 64 * 1024
deadline = time.time() + timeout
script_started = time.time()

diag("cwd=%s provider=%s model=%s effort=%s bundle=%s rollout=%s timeout=%.0fs" % (workdir, provider, model, effort, bundle, rollout_dir, timeout))
baseline_sessions = {}
for path in glob.glob(os.path.join(rollout_dir, "model-io-*.jsonl")):
    stat = os.stat(path)
    baseline_sessions[path] = (stat.st_size, stat.st_mtime)

child_started_ns = time.time_ns()
try:
    process = subprocess.Popen(
        argv,
        cwd=workdir,
        stdout=subprocess.PIPE,
        stderr=None,
        env=child_env,
        bufsize=0,
    )
except Exception as exc:
    diag(f"zcode 启动失败: {exc}")
    sys.exit(3)

# Drain while the child is running: waiting for exit before reading can deadlock
# a valid large native JSON terminal on the pipe capacity. Never retain excess.
stdout_parts = []
stdout_state = {"bytes": 0, "overflow": False, "eof": False, "error": False}
NATIVE_STDOUT_LIMIT = 8 * 1024 * 1024

def drain_native_stdout():
    try:
        while True:
            chunk = process.stdout.read(65536)
            if not chunk:
                stdout_state["eof"] = True
                break
            remaining = max(0, NATIVE_STDOUT_LIMIT - stdout_state["bytes"])
            if remaining:
                stdout_parts.append(chunk[:remaining])
            stdout_state["bytes"] += len(chunk)
            if stdout_state["bytes"] > NATIVE_STDOUT_LIMIT:
                stdout_state["overflow"] = True
    except Exception:
        stdout_state["error"] = True

stdout_reader = threading.Thread(target=drain_native_stdout, daemon=True)
stdout_reader.start()
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
    with open(path, "rb") as stream:
        if state["offset"] > 0:
            stream.seek(state["offset"])
        else:
            stream.seek(0, os.SEEK_SET)
        chunk = stream.read()
    text = state["tail"] + chunk
    lines = text.split(b"\n")
    if text and not text.endswith(b"\n") and lines:
        state["tail"] = lines.pop()
    else:
        state["tail"] = b""
    state["offset"] += len(chunk)
    for raw_line in lines:
        line = raw_line.decode("utf-8", errors="replace")
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
            "tail": b"",
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
            state["tail"] = b""
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
    if process.poll() is None:
        process.kill()
    process.wait()
    stdout_reader.join(timeout=2)

child_ended_ns = time.time_ns()
if stdout_reader.is_alive() or not stdout_state["eof"] or stdout_state["overflow"] or stdout_state["error"]:
    diag("native stdout was not complete and bounded exit=71")
    sys.exit(71)
if timed_out:
    diag("native deadline reached; no consult answer exit=72")
    sys.exit(72)
try:
    out = b"".join(stdout_parts).decode("utf-8")
except UnicodeDecodeError:
    diag("native stdout is not complete UTF-8 exit=71")
    sys.exit(71)
terminal = None
terminal_candidates = 0
malformed_native_stdout = False
if out.strip():
    # ZCode's --json terminal object is commonly pretty-printed across many
    # lines. Parse the complete stdout object first; retain the compact
    # line-delimited fallback for older providers and local fixtures.
    try:
        whole_value = json.loads(out.strip())
    except Exception:
        whole_value = None
    if (
        isinstance(whole_value, dict)
        and whole_value.get("sessionId") is not None
        and whole_value.get("response") is not None
    ):
        terminal = whole_value
        terminal_candidates += 1
    else:
        for line in out.splitlines():
            candidate = line.strip()
            if not candidate:
                continue
            try:
                value = json.loads(candidate)
            except Exception:
                malformed_native_stdout = True
                continue
            if (
                isinstance(value, dict)
                and value.get("sessionId") is not None
                and value.get("response") is not None
            ):
                terminal = value
                terminal_candidates += 1
                if not consult_mode:
                    break

elapsed = time.time() - (deadline - timeout)
if consult_mode and malformed_native_stdout:
    diag("incomplete native stdout; no consult answer exit=71")
    sys.exit(71)
if consult_mode and terminal_candidates > 1:
    diag("multiple native terminal/session objects; no consult answer exit=74")
    sys.exit(74)

session_id = terminal.get("sessionId") if terminal else None
usage = terminal.get("usage") if terminal else None
if terminal is not None:
    pin_evidence = None
    response = terminal.get("response")
    response_sha256 = (
        hashlib.sha256(response.encode("utf-8")).hexdigest()
        if isinstance(response, str) and response.strip()
        else None
    )
    if consult_mode and (
        not isinstance(session_id, str) or not session_id.strip() or response_sha256 is None
    ):
        diag("native terminal has no complete bound consult text exit=71")
        sys.exit(71)
    # Native role overrides additionally require the exact model-IO receipt.
    # Legacy CLI Consult retains its existing prelaunch pin and terminal contract.
    if consult_mode and os.environ.get("ORCH_FUSION_ROLE") == "1" and not synthetic_b228_fixture:
        pin_evidence = exact_consult_pin(terminal, child_started_ns, child_ended_ns)
        if not pin_evidence:
            diag("exact native main-turn provider/model/effort/response proof is absent or mismatched exit=74")
            sys.exit(74)
    projection = terminal.get("projection")
    context_window = (
        projection.get("contextWindow") if isinstance(projection, dict) else None
    )
    trace_id = terminal.get("traceId")
    terminal_frame = {
        "type": "zcode.terminal",
        "sessionId": session_id,
        "exactReason": "zcode --json terminal object",
        "finalTextSha256": response_sha256,
        "usage": usage if isinstance(usage, dict) else None,
        "usageAbsentReason": None
        if isinstance(usage, dict)
        else "zcode terminal object omitted usage",
        "contextWindow": context_window,
        "traceId": trace_id,
    }
    if consult_mode:
        terminal_frame["finalText"] = response
        terminal_frame["pinEvidence"] = pin_evidence
    if not synthetic_b228_fixture:
        terminal_frame.update({"provider": provider, "model": model, "effort": effort})
    print(json.dumps(terminal_frame, ensure_ascii=False, separators=(",", ":")), flush=True)
    diag(
        "session=%s tokens=%s elapsed=%.0fs provider_rc=%s exit=%s"
        % (
            session_id,
            usage.get("totalTokens") if isinstance(usage, dict) else None,
            elapsed,
            process.returncode,
            process.returncode,
        )
    )
    sys.exit(process.returncode if process.returncode is not None else 3)

if timed_out:
    code = 72
elif not out.strip():
    code = 70
else:
    code = 71
diag("frames=%d terminal=no elapsed=%.0fs exit=%d" % (provider_frames, elapsed, code))
sys.exit(code)
PY
