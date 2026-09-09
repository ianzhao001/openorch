#!/bin/sh
# wake-multica.sh <message> —— C(SmartClaw)的 multica.sock 注入通道(r20,非窗口/裁定 A 合规)
# 连 ~/.dewusmartclaw/multica.sock 写一行 NDJSON run 请求 → SmartClaw headless 新建会话执行 → 流式回传。
# 模型=SmartClaw 客户端全局当前模型(startSession 不带 model,由用户在 UI 设定)。
# 会话入 dewusmartclaw.sqlite 的 cowork_sessions(客户端可见);本脚本 stdout=完整对话流(被 wake 日志捕获)。
#
# 2026-07-31 planner 直修（用户全权委托，r59 追认审计）：消除确定性假成功。
#   旧行为：首帧前 EOF / 7200s 超时均 exit 0，上游无法区分「干完了」与「socket 一开就断」。
#   新契约（stdout 逐行透传格式不变，watch-sessions.py 兼容）：
#     exit 0  = 观测到 exact terminal（解析成功且含顶层 "payloads" 的 JSON 帧）
#     exit 70 = 零帧 EOF（服务端未回任何完整帧即关闭——B178 期间的静默失败形态）
#     exit 71 = 有帧但无终帧即 EOF（流被截断）
#     exit 72 = 超时且无终帧
#     exit 3/64/65 沿用（连接失败/缺参/git 失败）
#   诊断走 stderr；ACK 需服务端协议支持，留给 B179/r59。
#   测试座（默认不变）：ORCH_MULTICA_SOCK 覆盖 socket 路径；ORCH_MULTICA_TIMEOUT 覆盖秒数。
msg="$1"
[ -n "$msg" ] || { echo "usage: wake-multica.sh <message>" >&2; exit 64; }
gitcommon="$(git rev-parse --path-format=absolute --git-common-dir)" || exit 65
root="${ORCH_MULTICA_CWD:-$(dirname "$gitcommon")}"
[ -d "$root" ] || { echo "[wake-multica] ORCH_MULTICA_CWD 不是目录: $root" >&2; exit 65; }
MSG="$msg" ROOT="$root" python3 - <<'PY'
import socket, json, os, time, sys

def diag(text):
    print("[wake-multica] " + text, file=sys.stderr, flush=True)

sock = os.environ.get("ORCH_MULTICA_SOCK") or os.path.expanduser("~/.dewusmartclaw/multica.sock")
timeout = float(os.environ.get("ORCH_MULTICA_TIMEOUT") or 7200)
req = {"type": "run",
       # 会话复用（r53 用户裁定，仅本轮应用，默认行为不变）：
       # 设 ORCH_MULTICA_SESSION=<id> 则整轮复用同一 sessionId，SmartClaw 侧可续上下文；
       # 不设则沿用原行为——每次 wake 新建 orch-wake-<ts> 会话。
       "sessionId": os.environ.get("ORCH_MULTICA_SESSION")
                    or "orch-wake-%d" % int(time.time()),
       "prompt": os.environ["MSG"],
       "cwd": os.environ["ROOT"]}
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(timeout)
try:
    s.connect(sock)
except Exception as e:
    diag("multica.sock 连接失败: %s" % e); sys.exit(3)
# The bridge decodes each socket chunk separately. ASCII JSON escapes keep
# multi-byte prompt characters intact even when transport splits every byte.
s.sendall((json.dumps(req, ensure_ascii=True) + "\n").encode("ascii"))

frames = 0          # 完整打印的非空行数（回传证据）
terminal_seen = False
cause = "eof"

def is_terminal(raw):
    # exact terminal：能解析且含顶层 "payloads" 键的 JSON 对象帧
    try:
        obj = json.loads(raw)
    except Exception:
        return False
    return isinstance(obj, dict) and "payloads" in obj

buf = b""
while True:
    try:
        c = s.recv(8192)
    except socket.timeout:
        cause = "timeout"; diag("读超时 (%.0fs)" % timeout); break
    if not c:
        cause = "eof"; break
    # Preserve the received stream byte-for-byte, including blank lines and an
    # unterminated final frame. The native final is projected separately.
    sys.stdout.buffer.write(c)
    sys.stdout.buffer.flush()
    buf += c
    # 逐行透传(供 watch-sessions.py 解析),末尾 payloads 收尾
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        if line.strip():
            text = line.decode("utf-8", "replace")
            frames += 1
            if is_terminal(text):
                terminal_seen = True
# The bridge ends the socket on completion/error. Drain EOF instead of cutting
# the raw stream at the first complete JSON object in a partial read buffer.
s.close()
# EOF 后清算残余缓冲（服务端可能在终帧后立即关连接且终帧无换行）
if buf.strip():
    text = buf.decode("utf-8", "replace")
    frames += 1
    if is_terminal(text):
        terminal_seen = True

if terminal_seen:
    diag("frames=%d terminal=yes cause=%s exit=0" % (frames, cause))
    sys.exit(0)
if frames == 0:
    code = 72 if cause == "timeout" else 70
else:
    code = 72 if cause == "timeout" else 71
diag("frames=%d terminal=no cause=%s exit=%d" % (frames, cause, code))
sys.exit(code)
PY
