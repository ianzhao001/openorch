#!/bin/sh
# wake-dsh-stream.sh <message> -- DeepSeek Harness completed-tool projection.
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
# - a backend receipt or any managed-provider identity;
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


def diag(text):
    print("[wake-dsh-stream] " + text, file=sys.stderr, flush=True)


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
workdir = message_directory(message, "WORKTREE")
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
if target_is_in_workspace or target_is_temporary:
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
patch_payload = (
    json.dumps(
        [{"id": "session-persistence-jsonl", "config": {"root": session_store}}],
        ensure_ascii=False,
        separators=(",", ":"),
    )
    + "\n"
).encode("utf-8")
install_exact_file(patch_path, patch_payload, "DSH session-root patch")

dsh_bin = executable(os.environ.get("ORCH_DSH_BIN") or "dsh", "DSH 入口")
zstd_bin = executable(
    os.environ.get("ORCH_DSH_ZSTD_BIN") or "zstd", "zstd 解码器"
)
try:
    timeout = float(os.environ.get("ORCH_DSH_TIMEOUT") or 7200)
except ValueError:
    diag("ORCH_DSH_TIMEOUT 必须是秒数")
    sys.exit(64)
if timeout <= 0:
    diag("ORCH_DSH_TIMEOUT 必须大于 0")
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
    "cwd=%s sessionRoot=%s profile=%s timeout=%.0fs"
    % (workdir, session_store, profile, timeout)
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


def project_record(record):
    global projected, terminal_seen
    kind = record.get("type")
    if kind == "turn/start":
        print("turn.started", flush=True)
    elif kind == "tool/result":
        text = completed_tool_text(record)
        if text is not None:
            print(bounded_tool_result(text), flush=True)
            projected += 1
    elif kind == "turn/end":
        terminal_seen = True


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
    for record in records[seen_records:]:
        record_frames += 1
        project_record(record)
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

if selection_error is not None:
    diag(
        "frames=%d projected=%d terminal=no cause=identity-error rc=%s exit=74 detail=%s"
        % (record_frames, projected, provider_rc, selection_error)
    )
    sys.exit(74)
if timed_out:
    diag(
        "frames=%d projected=%d terminal=%s cause=timeout rc=%s exit=72"
        % (record_frames, projected, "yes" if terminal_seen else "no", provider_rc)
    )
    sys.exit(72)
if provider_rc != 0:
    code = provider_rc if isinstance(provider_rc, int) and 0 < provider_rc <= 255 else 1
    diag(
        "frames=%d projected=%d terminal=%s cause=provider-exit rc=%s exit=%d"
        % (record_frames, projected, "yes" if terminal_seen else "no", provider_rc, code)
    )
    sys.exit(code)
if not terminal_seen:
    code = 70 if record_frames == 0 else 71
    diag(
        "frames=%d projected=%d terminal=no cause=eof rc=0 exit=%d"
        % (record_frames, projected, code)
    )
    sys.exit(code)

diag(
    "session=%s frames=%d projected=%d terminal=yes cause=terminal rc=0 exit=0"
    % (selected_id, record_frames, projected)
)
sys.exit(0)
PY
