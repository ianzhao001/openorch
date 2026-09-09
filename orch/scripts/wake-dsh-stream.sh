#!/bin/sh
# wake-dsh-stream.sh <message> -- DeepSeek Harness completed-tool projection.
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
# orch-exit-code: 74 failed identity-drift
#
# This is intentionally a narrow, nongate-review transport.  It parses the
# review site and runtime target from their authoritative message lines.  It
# keeps ambient DSH_HOME for the installed profile/credentials, but uses DSH's
# native --patch layer to redirect session persistence to an attempt-local
# directory under CARGO_TARGET_DIR.  That directory is outside the DSH
# workspace-write root, so tools cannot forge the transcript; a user's web
# daemon and its ambient sessions are never candidates for this invocation.
# The wrapper launches DSH with WORKTREE as cwd, then incrementally decodes
# DSH's own multi-frame zstd session log.
# Only harness-owned completed `tool/result` text is translated to the generic
# `tool_result` grammar consumed by orch.  Prompt, tool/call, assistant text,
# and DSH's final stdout are never evidence and are discarded.
#
# Capability floor -- this wrapper explicitly DOES NOT provide:
# - implement, critical-implement, or takeover authority;
# - serve operation or automatic takeover;
# - formal-review authority (r71 admits DSH as nongate-review only);
# - a backend receipt in legacy/no-envelope mode (managed envelope mode emits
#   one `dsh.session` only after the authenticated request/header pin matches);
# - managed process topology or managed termination semantics;
# - cost/usage accounting;
# - session continuity across wakes; or
# - interactive approval handling.
# `turn/start` -> `turn.started` is an H89-style faithful translation frame,
# not a provider-native receipt.  The wrapper never claims otherwise.
#
# Two observed traps define the implementation shape:
# - Node zstdDecompressSync stops after the first frame.  We use the zstd CLI,
#   re-decode the complete prefix on every poll, and deduplicate by record.
# - zstd can successfully decode a truncated prefix.  Decode success is never
#   completion; only a selected session's `turn/end` permits exit 0.

msg="$1"
[ -n "$msg" ] || {
  echo "usage: wake-dsh-stream.sh <message>  (env: ORCH_DSH_BIN, DSH_HOME, ORCH_DSH_ZSTD_BIN, ORCH_DSH_TIMEOUT)" >&2
  exit 64
}

MSG="$msg" python3 - <<'PY'
import json
import os
import secrets
import shutil
import stat
import subprocess
import sys
import tempfile
import time


MAX_FRAME_BYTES = 64 * 1024
MAX_BUFFERED_PROJECTION_BYTES = 4 * 1024 * 1024


def diag(text):
    print("[wake-dsh-stream] " + text, file=sys.stderr, flush=True)


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


def executable(value, label):
    expanded = os.path.expanduser(value)
    if os.sep in expanded:
        path = os.path.realpath(expanded)
        if os.path.isfile(path) and os.access(path, os.X_OK):
            return path
    else:
        found = shutil.which(expanded)
        if found:
            return found
    diag("%s 不可执行: %s" % (label, value))
    sys.exit(3)


def message_directory(message, key):
    values = []
    for line in message.splitlines():
        prefix = key + "="
        if line.startswith(prefix):
            values.append(line[len(prefix) :].strip())
    if len(values) != 1 or not values[0]:
        diag("消息必须恰含一条非空 %s= 行" % key)
        sys.exit(65)
    value = values[0]
    if not os.path.isabs(value):
        diag("%s= 必须是绝对路径: %s" % (key, value))
        sys.exit(65)
    canonical = os.path.realpath(value)
    if not os.path.isdir(canonical):
        diag("%s= 不是目录: %s" % (key, canonical))
        sys.exit(65)
    return canonical


def private_directory(path, label):
    try:
        os.mkdir(path, 0o700)
    except FileExistsError:
        pass
    except OSError as exc:
        diag("无法创建 %s: %s" % (label, exc))
        sys.exit(65)
    try:
        path_stat = os.lstat(path)
    except OSError as exc:
        diag("无法核验 %s: %s" % (label, exc))
        sys.exit(65)
    if not stat.S_ISDIR(path_stat.st_mode) or stat.S_ISLNK(path_stat.st_mode):
        diag("%s 必须是非符号链接目录: %s" % (label, path))
        sys.exit(65)
    return os.path.realpath(path)


def install_exact_file(path, payload, label):
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        fd = os.open(path, flags, 0o600)
    except FileExistsError:
        try:
            path_stat = os.lstat(path)
            if not stat.S_ISREG(path_stat.st_mode) or stat.S_ISLNK(path_stat.st_mode):
                raise OSError("not a regular non-symlink file")
            with open(path, "rb") as stream:
                current = stream.read()
        except OSError as exc:
            diag("无法核验 %s: %s" % (label, exc))
            sys.exit(65)
        if current != payload:
            diag("%s 已存在但内容漂移: %s" % (label, path))
            sys.exit(65)
        return
    except OSError as exc:
        diag("无法创建 %s: %s" % (label, exc))
        sys.exit(65)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
    except OSError as exc:
        diag("无法写入 %s: %s" % (label, exc))
        sys.exit(65)


def cwd_slug(path):
    # DSH 0.1.0-rc.6 uses --<absolute components joined by hyphens>--.
    body = path.strip(os.sep).replace(os.sep, "-")
    return "--%s--" % body


def bounded_tool_result(value):
    text = "" if value is None else str(value)
    frame = {"type": "tool_result", "output": text}
    encoded = json.dumps(
        frame, ensure_ascii=False, separators=(",", ":")
    ).encode("utf-8")
    if len(encoded) <= MAX_FRAME_BYTES:
        return encoded.decode("utf-8")

    marker = " [truncated]"
    low = 0
    high = len(text)
    best = None
    while low <= high:
        middle = (low + high) // 2
        candidate = {
            "type": "tool_result",
            "output": text[:middle] + marker,
            "truncated": True,
        }
        encoded = json.dumps(
            candidate, ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        if len(encoded) <= MAX_FRAME_BYTES:
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


def completed_tool_text(record):
    if not isinstance(record, dict) or record.get("type") != "tool/result":
        return None
    data = record.get("data")
    message = data.get("message") if isinstance(data, dict) else None
    outer_content = message.get("content") if isinstance(message, dict) else None
    if not isinstance(outer_content, list):
        return None

    chunks = []
    for result in outer_content:
        if not isinstance(result, dict) or result.get("type") != "tool-result":
            continue
        content = result.get("content")
        if not isinstance(content, list):
            continue
        for item in content:
            if (
                isinstance(item, dict)
                and item.get("type") == "text"
                and isinstance(item.get("text"), str)
            ):
                chunks.append(item["text"])
    return "\n".join(chunks) if chunks else None


message = os.environ["MSG"]
present_envelope_keys = [key for key in ENVELOPE_KEYS if key in os.environ]
envelope_mode = bool(present_envelope_keys)
if envelope_mode:
    missing = [
        key
        for key in ENVELOPE_KEYS
        if not isinstance(os.environ.get(key), str) or not os.environ[key].strip()
    ]
    if missing:
        diag("incomplete invocation envelope; missing/blank: %s" % ",".join(missing))
        sys.exit(66)
    if os.environ["ORCH_HARNESS_ID"] != "dsh":
        diag("ORCH_HARNESS_ID 与 dsh wrapper 不匹配")
        sys.exit(66)
    workdir = os.environ["ORCH_HARNESS_CWD"]
    dsh_bin = exact_executable(
        os.environ["ORCH_HARNESS_PROVIDER_BIN"], "ORCH_HARNESS_PROVIDER_BIN"
    )
    pin_provider = os.environ["ORCH_HARNESS_PROVIDER"]
    pin_model = os.environ["ORCH_HARNESS_MODEL"]
    pin_effort = os.environ["ORCH_HARNESS_EFFORT"]
    timeout_text = os.environ["ORCH_HARNESS_DEADLINE_SECS"]
    for alias, envelope_key, selected in (
        ("ORCH_DSH_CWD", "ORCH_HARNESS_CWD", workdir),
        ("ORCH_DSH_BIN", "ORCH_HARNESS_PROVIDER_BIN", dsh_bin),
        ("ORCH_DSH_PROVIDER", "ORCH_HARNESS_PROVIDER", pin_provider),
        ("ORCH_DSH_MODEL", "ORCH_HARNESS_MODEL", pin_model),
        ("ORCH_DSH_EFFORT", "ORCH_HARNESS_EFFORT", pin_effort),
        ("ORCH_DSH_TIMEOUT", "ORCH_HARNESS_DEADLINE_SECS", timeout_text),
    ):
        legacy_conflict(alias, envelope_key, selected)
else:
    workdir = message_directory(message, "WORKTREE")
    dsh_bin = executable(os.environ.get("ORCH_DSH_BIN") or "dsh", "DSH 入口")
    pin_provider = os.environ.get("ORCH_DSH_PROVIDER")
    pin_model = os.environ.get("ORCH_DSH_MODEL")
    pin_effort = os.environ.get("ORCH_DSH_EFFORT")
    timeout_text = os.environ.get("ORCH_DSH_TIMEOUT") or "7200"

if not os.path.isdir(workdir) or not os.path.isabs(workdir):
    diag("ORCH_HARNESS_CWD/WORKTREE 必须是绝对目录: %s" % workdir)
    sys.exit(65)
if envelope_mode:
    # ORCH_HARNESS_ORCH_BIN is an already validated absolute executable such
    # as <target>/debug/orch.  The parent of its profile directory is the
    # runtime-owned Cargo target root; envelope mode must not keep borrowing a
    # second, unauthenticated path from human prompt text.
    runtime_target = os.path.dirname(
        os.path.dirname(os.path.realpath(os.environ["ORCH_HARNESS_ORCH_BIN"]))
    )
else:
    runtime_target = message_directory(message, "CARGO_TARGET_DIR")
try:
    target_is_in_workspace = os.path.commonpath([workdir, runtime_target]) == workdir
    temporary_roots = {
        os.path.realpath("/tmp"),
        os.path.realpath(tempfile.gettempdir()),
    }
    target_is_temporary = any(
        os.path.commonpath([root, runtime_target]) == root for root in temporary_roots
    )
except ValueError:
    target_is_in_workspace = True
    target_is_temporary = True
manual_envelope = envelope_mode and (
    os.environ["ORCH_HARNESS_TASK_ID"] == "MANUAL"
    and os.environ["ORCH_HARNESS_ATTEMPT_ID"] == "MANUAL-A0000"
)
if (target_is_in_workspace and not manual_envelope) or target_is_temporary:
    diag("CARGO_TARGET_DIR 必须位于 WORKTREE 与 /tmp 之外: %s" % runtime_target)
    sys.exit(65)

dsh_home = os.path.realpath(
    os.path.expanduser(os.environ.get("DSH_HOME") or "~/.dsh")
)
if not os.path.isdir(dsh_home):
    diag("ambient DSH_HOME 不是目录: %s" % dsh_home)
    sys.exit(65)
try:
    home_is_in_workspace = os.path.commonpath([workdir, dsh_home]) == workdir
    home_is_temporary = any(
        os.path.commonpath([root, dsh_home]) == root for root in temporary_roots
    )
except ValueError:
    home_is_in_workspace = True
    home_is_temporary = True
if home_is_in_workspace or home_is_temporary:
    diag("ambient DSH_HOME 必须位于 WORKTREE 与 /tmp 之外: %s" % dsh_home)
    sys.exit(65)

launch_nonce = secrets.token_hex(16)
session_store = private_directory(
    os.path.join(runtime_target, ".orch-dsh-sessions-" + launch_nonce),
    "attempt-local DSH session root",
)
patch_path = os.path.join(
    runtime_target, ".orch-dsh-session-patch-" + launch_nonce + ".json"
)
patch_entries = [
    {"id": "session-persistence-jsonl", "config": {"root": session_store}}
]
preset = os.environ.get("ORCH_DSH_PRESET") or "minimal"
if envelope_mode:
    patch_entries.extend(
        [
            {
                "id": "agent-default-model",
                "config": {
                    "provider": pin_provider,
                    "model": pin_model,
                    "reasoningEffort": pin_effort,
                },
            },
            {"id": "agent-presets", "config": {"default": preset}},
        ]
    )
patch_payload = (
    json.dumps(
        patch_entries, ensure_ascii=False, separators=(",", ":")
    )
    + "\n"
).encode("utf-8")
install_exact_file(patch_path, patch_payload, "DSH session-root patch")

zstd_bin = executable(
    os.environ.get("ORCH_DSH_ZSTD_BIN") or "zstd", "zstd 解码器"
)
try:
    timeout = float(timeout_text)
except ValueError:
    diag("调用 deadline 必须是秒数")
    sys.exit(64)
if timeout <= 0:
    diag("调用 deadline 必须大于 0")
    sys.exit(64)

session_root = os.path.join(session_store, cwd_slug(workdir))
try:
    baseline = {
        entry.path
        for entry in os.scandir(session_root)
        if entry.is_dir(follow_symlinks=False) and entry.name.startswith("session-")
    }
except FileNotFoundError:
    baseline = set()

profile = os.environ.get("ORCH_DSH_PROFILE") or "headless"
argv = [dsh_bin, "--profile", profile, "--patch", patch_path, message]
child_env = os.environ.copy()
child_env["DSH_HOME"] = dsh_home
diag(
    "cwd=%s sessionRoot=%s profile=%s preset=%s provider=%s model=%s effort=%s timeout=%.0fs"
    % (workdir, session_store, profile, preset, pin_provider, pin_model, pin_effort, timeout)
)
try:
    process = subprocess.Popen(
        argv,
        cwd=workdir,
        env=child_env,
        stdout=subprocess.DEVNULL,
        # Runtime stores wrapper stdout and stderr in one evidence log.  Raw
        # provider stderr could therefore masquerade as a projected JSON frame
        # or leak prompt/assistant text.  Exit status plus our own prefixed
        # diagnostics are the only stderr evidence this wrapper admits.
        stderr=subprocess.DEVNULL,
    )
except Exception as exc:
    diag("DSH 启动失败: %s" % exc)
    sys.exit(3)

selected_dir = None
selected_id = None
seen_records = 0
record_frames = 0
projected = 0
terminal_seen = False
terminal_record = None
pin_verified = not envelope_mode
receipt_emitted = False
buffered_projection = []
buffered_projection_bytes = 0
pending_writes = []
timed_out = False
selection_error = None
started_wall = time.time()
started_mono = time.monotonic()
last_provider_frame = started_mono
next_heartbeat = started_mono + 15.0


def decode_records(session_dir):
    path = os.path.join(session_dir, "session.jsonl.zstd")
    if not os.path.isfile(path):
        return []
    try:
        decoded = subprocess.run(
            [zstd_bin, "-d", "-c", "-q", path],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=10,
        ).stdout.decode("utf-8", errors="strict")
    except (OSError, subprocess.TimeoutExpired, UnicodeDecodeError):
        return []

    records = []
    for raw in decoded.splitlines():
        if not raw.strip():
            continue
        try:
            value = json.loads(raw)
        except Exception:
            # A live, incomplete tail is not a record.  Stop at the first gap
            # so the next whole-prefix decode can recover it deterministically.
            break
        if not isinstance(value, dict):
            break
        records.append(value)
    return records


def valid_identity(session_dir, records):
    if not records:
        return None
    first = records[0]
    if first.get("type") != "session":
        return None
    session_id = first.get("id")
    recorded_cwd = first.get("cwd")
    if not isinstance(session_id, str) or session_id != os.path.basename(session_dir):
        return None
    if not isinstance(recorded_cwd, str):
        return None
    if os.path.realpath(recorded_cwd) != workdir:
        return None
    return session_id


def new_candidates():
    try:
        entries = list(os.scandir(session_root))
    except FileNotFoundError:
        return []
    return sorted(
        entry.path
        for entry in entries
        if entry.is_dir(follow_symlinks=False)
        and entry.name.startswith("session-")
        and entry.path not in baseline
    )


def projection_for_record(record):
    kind = record.get("type")
    if kind == "turn/start":
        return ["turn.started"]
    if kind == "tool/result":
        text = completed_tool_text(record)
        if text is not None:
            return [bounded_tool_result(text)]
    return []


def request_header_pin(record):
    if record.get("type") != "request/header":
        return None
    data = record.get("data")
    header = data.get("header") if isinstance(data, dict) else None
    config = header.get("config") if isinstance(header, dict) else None
    if not isinstance(config, dict):
        raise RuntimeError("request/header.data.header.config 缺失或畸形")
    values = (
        config.get("provider"),
        config.get("model"),
        config.get("reasoningEffort"),
    )
    if not all(isinstance(value, str) and value for value in values):
        raise RuntimeError("request/header provider/model/reasoningEffort 缺失或畸形")
    return values


def pending_write_from_record(record, ordinal):
    if not envelope_mode or record.get("type") != "tool/call":
        return None
    data = record.get("data")
    if not isinstance(data, dict) or data.get("name") != "write":
        return None
    arguments = data.get("arguments")
    if not isinstance(arguments, dict):
        return None
    path = arguments.get("path")
    content = arguments.get("content")
    expected_path = os.environ["ORCH_HARNESS_REVIEW_OUTPUT_PATH"]
    if path != expected_path or not isinstance(content, str) or not content:
        return None
    return (ordinal, path, content)


def emit_projection(frame):
    global projected
    print(frame, flush=True)
    projected += 1


def project_record(record, ordinal):
    global terminal_seen, terminal_record, pin_verified, receipt_emitted
    global buffered_projection, buffered_projection_bytes

    if envelope_mode and record.get("type") == "request/header":
        observed = request_header_pin(record)
        expected = (pin_provider, pin_model, pin_effort)
        if observed != expected:
            raise RuntimeError(
                "request/header signed pin 漂移: expected=%r observed=%r"
                % (expected, observed)
            )
        if not pin_verified:
            pin_verified = True
            if receipt_emitted:
                raise RuntimeError("dsh.session receipt 重复")
            print(
                json.dumps(
                    {
                        "type": "dsh.session",
                        "sessionId": selected_id,
                        "provider": pin_provider,
                        "model": pin_model,
                        "effort": pin_effort,
                    },
                    ensure_ascii=False,
                    separators=(",", ":"),
                ),
                flush=True,
            )
            receipt_emitted = True
            for frame in buffered_projection:
                emit_projection(frame)
            buffered_projection = []
            buffered_projection_bytes = 0

    candidate = pending_write_from_record(record, ordinal)
    if candidate is not None:
        pending_writes.append(candidate)

    if record.get("type") == "turn/end":
        terminal_seen = True
        terminal_record = record

    for frame in projection_for_record(record):
        if envelope_mode and not pin_verified:
            buffered_projection.append(frame)
            buffered_projection_bytes += len(frame.encode("utf-8")) + 1
            if buffered_projection_bytes > MAX_BUFFERED_PROJECTION_BYTES:
                raise RuntimeError("request/header 前 buffered_projection 超过安全上限")
        else:
            emit_projection(frame)


def capture_last_complete_pending_write():
    if (
        not envelope_mode
        or selected_id is None
        or not pin_verified
        or not pending_writes
    ):
        return False
    _, path, content = pending_writes[-1]
    expected_path = os.environ["ORCH_HARNESS_REVIEW_OUTPUT_PATH"]
    if path != expected_path or not os.path.isabs(path):
        return False
    expected_parent = os.path.realpath(
        os.path.join(workdir, ".cowork-temp", "review-spool")
    )
    parent = os.path.dirname(path)
    if os.path.realpath(parent) != expected_parent or not os.path.isdir(parent):
        return False
    payload = content.encode("utf-8")
    if not payload.endswith(b"\n"):
        payload += b"\n"
    install_exact_file(path, payload, "DSH pending review write")
    return True


def scan_selected_session():
    global selected_dir, selected_id, seen_records, record_frames, last_provider_frame

    if selected_dir is None:
        valid = []
        for candidate in new_candidates():
            records = decode_records(candidate)
            identity = valid_identity(candidate, records)
            if identity is not None:
                valid.append((candidate, identity, records))
        if len(valid) > 1:
            raise RuntimeError(
                "同一 cwd-slug 同时出现多个新会话，拒绝按时间猜身份: %s"
                % ",".join(item[1] for item in valid)
            )
        if len(valid) == 1:
            selected_dir, selected_id, records = valid[0]
        else:
            return 0
    else:
        records = decode_records(selected_dir)
        if valid_identity(selected_dir, records) != selected_id:
            raise RuntimeError("已绑定会话的 session id/cwd 身份漂移")

    if len(records) < seen_records:
        raise RuntimeError("已绑定会话记录前缀缩短，拒绝重放或换档")
    added = len(records) - seen_records
    for ordinal, record in enumerate(records[seen_records:], start=seen_records):
        record_frames += 1
        project_record(record, ordinal)
    seen_records = len(records)
    if added:
        last_provider_frame = time.monotonic()
    return added


while True:
    try:
        scan_selected_session()
    except RuntimeError as exc:
        selection_error = str(exc)
        process.kill()
        process.wait()
        break

    rc = process.poll()
    if rc is not None:
        # DSH has closed the file: one final whole-prefix decode observes every
        # completed frame without treating decoder status as terminal proof.
        try:
            scan_selected_session()
        except RuntimeError as exc:
            selection_error = str(exc)
        break

    now_wall = time.time()
    now_mono = time.monotonic()
    if now_wall - started_wall >= timeout:
        timed_out = True
        process.kill()
        process.wait()
        try:
            scan_selected_session()
        except RuntimeError as exc:
            selection_error = str(exc)
        break
    if now_mono >= next_heartbeat and now_mono - last_provider_frame >= 15.0:
        print(
            "[wake-hb] t=+%ds provider=%d frames=%d inflight=1"
            % (int(now_mono - started_mono), process.pid, record_frames),
            flush=True,
        )
        next_heartbeat = now_mono + 15.0
    time.sleep(0.05)

if process.poll() is None:
    process.wait()
provider_rc = process.returncode

captured_pending_write = False
if selection_error is None and envelope_mode and selected_id is not None and not pin_verified:
    selection_error = (
        "selected DSH session ended without an exact request/header pin; "
        "request/context alone is insufficient"
    )
if selection_error is None and (provider_rc != 0 or not terminal_seen):
    try:
        captured_pending_write = capture_last_complete_pending_write()
    except (OSError, RuntimeError) as exc:
        selection_error = "pending write capture failed closed: %s" % exc

if selection_error is not None:
    diag(
        "frames=%d projected=%d terminal=no cause=identity-error rc=%s exit=74 pendingWrite=%s detail=%s"
        % (record_frames, projected, provider_rc, "yes" if captured_pending_write else "no", selection_error)
    )
    sys.exit(74)
if timed_out:
    diag(
        "frames=%d projected=%d terminal=%s cause=timeout rc=%s exit=72 pendingWrite=%s"
        % (record_frames, projected, "yes" if terminal_seen else "no", provider_rc, "yes" if captured_pending_write else "no")
    )
    sys.exit(72)
if provider_rc != 0:
    code = provider_rc if isinstance(provider_rc, int) and 0 < provider_rc <= 255 else 1
    diag(
        "frames=%d projected=%d terminal=%s cause=provider-exit rc=%s exit=%d pendingWrite=%s"
        % (record_frames, projected, "yes" if terminal_seen else "no", provider_rc, code, "yes" if captured_pending_write else "no")
    )
    sys.exit(code)
if not terminal_seen:
    code = 70 if record_frames == 0 else 71
    diag(
        "frames=%d projected=%d terminal=no cause=eof rc=0 exit=%d pendingWrite=%s"
        % (record_frames, projected, code, "yes" if captured_pending_write else "no")
    )
    sys.exit(code)

usage = terminal_record.get("usage") if isinstance(terminal_record, dict) else None
print(
    json.dumps(
        {
            "type": "dsh.terminal",
            "sessionId": selected_id,
            "exactReason": "turn/end",
            "usage": usage if isinstance(usage, dict) else None,
            "usageAbsentReason": None
            if isinstance(usage, dict)
            else "dsh turn/end record omitted usage",
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ),
    flush=True,
)
diag(
    "session=%s frames=%d projected=%d terminal=yes cause=terminal rc=0 exit=0"
    % (selected_id, record_frames, projected)
)
sys.exit(0)
PY
